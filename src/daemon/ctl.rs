//! `ctl.sock`: the daemon's local control socket (design §2.7).
//!
//! A `0600` unix socket owned by the package uid; every connection is checked
//! with `SO_PEERCRED` and refused unless the peer is that uid. One request per
//! connection, one JSON object per line each way:
//!
//! ```text
//! {"verb":"status"}
//! {"verb":"run","scope":"photos","action":"sync","allow_delete":false}
//! {"verb":"cancel","id":12}         {"verb":"stop","wait_s":30}
//! {"verb":"mutate","op":"configure-profile","profile":"photos","args":[...]}
//! ```
//!
//! Every wait is bounded: a connection has one deadline for its whole request
//! line (a client trickling bytes is cut at it, H5), a client connect has a
//! timeout, and a control thread that does not answer in time yields a named
//! `daemon_busy` rather than a hang. Slots (F6s):
//!
//! - a connection that has not yet sent its verb takes only a cheap "unread"
//!   place (its thread, cut at the read deadline), never a verb slot, so idle
//!   connections cannot make `ping` fail;
//! - once the verb is parsed it takes a slot by class: `ping`, `status` and
//!   `stop` have reserved slots that nothing else can use, `wait` and `mutate`
//!   are capped within the general slots, and everything else takes a general
//!   one;
//! - a verb waiting on the control thread watches its peer: a client that has
//!   gone away frees its slot, and the listener logs `ctl_peer_gone`.
//!
//! Linux only.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::control::{Msg, Mutation, Request};
use super::routines::{Action, Level, valid_name};
use super::sys;

/// Where the listener's own failures go: level, event, detail.
pub(crate) type CtlLog = Arc<dyn Fn(Level, &'static str, String) + Send + Sync>;

/// The largest request line accepted.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// How long a client has to send its whole request line.
const READ_DEADLINE: Duration = Duration::from_secs(5);
/// How long a quick verb may wait for the control thread.
const ANSWER_DEADLINE: Duration = Duration::from_secs(5);
/// General verbs served at once (parsed, waiting on the control thread); more
/// are refused by name.
const MAX_CONNECTIONS: usize = 32;
/// Of those, how many may be long verbs (`wait`, `mutate`); the rest stay free
/// for quick ones (F6s).
const MAX_LONG_REQUESTS: usize = 16;
/// Slots only `ping`, `status` and `stop` can use, beyond the general ones, so
/// neither CLI waiters nor a flood of other verbs can starve them (F6s).
const MAX_PRIORITY_REQUESTS: usize = 8;
/// Connections that have not yet sent a whole request line. Each is a thread
/// cut at [`READ_DEADLINE`]; more are refused by name.
const MAX_UNREAD_CONNECTIONS: usize = 128;
/// How often a verb waiting on the control thread looks at its peer.
const PEER_CHECK: Duration = Duration::from_millis(250);
/// How long a client waits for its connection to be accepted.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// After a failed accept (EMFILE, ENFILE, ENOMEM), the listener pauses this
/// long instead of spinning (S3-4).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(200);

/// Parse one request line into a control request and its answer deadline.
pub(crate) fn parse_request(
    line: &str,
    mutation_deadline: Duration,
) -> Result<(Request, Duration), String> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| format!("the request is not JSON: {error}"))?;
    let verb = value["verb"].as_str().ok_or("the request has no verb")?;
    let text = |key: &str| value[key].as_str().map(str::to_owned);
    let request = match verb {
        "ping" => Request::Ping,
        "status" => Request::Status,
        "reload" => Request::Reload,
        "wake" => Request::Wake,
        "stop" => {
            let wait = value["wait_s"].as_u64().unwrap_or(30).clamp(1, 120);
            return Ok((Request::Stop, Duration::from_secs(wait)));
        }
        "cancel" => Request::Cancel {
            id: value["id"].as_u64(),
        },
        "wait" => {
            let id = value["id"].as_u64().ok_or("wait needs the run id")?;
            let wait = value["wait_s"].as_u64().unwrap_or(3600).clamp(1, 86_400);
            return Ok((
                Request::Wait {
                    id,
                    within: Duration::from_secs(wait),
                },
                Duration::from_secs(wait + 5),
            ));
        }
        "run" | "plan" => {
            let scope = text("scope").unwrap_or_else(|| "all".into());
            if scope != "all" && !valid_name(&scope) {
                return Err("scope must be a profile name or all".into());
            }
            let action = match (verb, value["action"].as_str()) {
                ("plan", _) | (_, Some("plan")) => Action::Plan,
                (_, None | Some("sync")) => Action::Sync,
                _ => return Err("action must be plan or sync".into()),
            };
            Request::Run {
                scope,
                action,
                allow_delete: value["allow_delete"].as_bool().unwrap_or(false),
                max_total_delete: value["max_total_delete"].as_u64(),
            }
        }
        "routine-now" => {
            let profile = text("profile")
                .filter(|p| valid_name(p))
                .ok_or("profile must be a routine name")?;
            Request::RoutineNow { profile }
        }
        "mutate" => {
            let op = text("op")
                .filter(|op| {
                    !op.is_empty() && op.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                })
                .ok_or("op must be a manager command")?;
            let profile = match text("profile") {
                Some(profile) if valid_name(&profile) => Some(profile),
                Some(_) => return Err("profile must be a profile name".into()),
                None => None,
            };
            let args = match &value["args"] {
                Value::Null => Vec::new(),
                Value::Array(items) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .ok_or("args must be strings")
                    })
                    .collect::<Result<_, _>>()?,
                _ => return Err("args must be an array".into()),
            };
            return Ok((
                Request::Mutate(Mutation { op, profile, args }),
                mutation_deadline + Duration::from_secs(5),
            ));
        }
        other => return Err(format!("unknown verb {other}")),
    };
    Ok((request, ANSWER_DEADLINE))
}

fn answer(stream: &mut UnixStream, value: &Value, log: &CtlLog) {
    let mut line = value.to_string();
    line.push('\n');
    // A client that went away loses its reply, but the loss is named.
    if let Err(error) = stream.write_all(line.as_bytes()) {
        log(
            Level::Info,
            "ctl_reply_dropped",
            format!(
                "code={} cause={error}",
                value["code"].as_str().unwrap_or("ok")
            ),
        );
    }
}

/// Where a connection stands, and so which places it holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// Connected, verb not yet parsed.
    Unread,
    /// `ping`, `status`, `stop`: reserved slots.
    Priority,
    /// `wait`, `mutate`: a general slot and a long one.
    Long,
    /// Everything else: a general slot.
    General,
}

fn place_of(request: &Request) -> Place {
    match request {
        Request::Ping | Request::Status | Request::Stop => Place::Priority,
        Request::Wait { .. } | Request::Mutate(_) => Place::Long,
        _ => Place::General,
    }
}

/// The verb's word, for the log.
fn verb_word(request: &Request) -> &'static str {
    match request {
        Request::Ping => "ping",
        Request::Status => "status",
        Request::Stop => "stop",
        Request::Wait { .. } => "wait",
        Request::Mutate(_) => "mutate",
        Request::Reload => "reload",
        Request::Wake => "wake",
        Request::Cancel { .. } => "cancel",
        Request::Run { .. } => "run",
        Request::RoutineNow { .. } => "routine-now",
    }
}

/// How many places each kind holds right now.
#[derive(Default)]
struct Slots {
    unread: AtomicUsize,
    general: AtomicUsize,
    long: AtomicUsize,
    priority: AtomicUsize,
}

/// One more in `counter` if it is under `cap`.
fn increment(counter: &AtomicUsize, cap: usize) -> bool {
    if counter.fetch_add(1, Ordering::SeqCst) >= cap {
        counter.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    true
}

impl Slots {
    /// The places for `place`, all or none; they are given back when the
    /// returned guard drops.
    fn take(self: &Arc<Self>, place: Place) -> Option<Held> {
        let taken = match place {
            Place::Unread => increment(&self.unread, MAX_UNREAD_CONNECTIONS),
            Place::Priority => increment(&self.priority, MAX_PRIORITY_REQUESTS),
            Place::General => increment(&self.general, MAX_CONNECTIONS),
            Place::Long => {
                if increment(&self.general, MAX_CONNECTIONS) {
                    if increment(&self.long, MAX_LONG_REQUESTS) {
                        true
                    } else {
                        self.general.fetch_sub(1, Ordering::SeqCst);
                        false
                    }
                } else {
                    false
                }
            }
        };
        taken.then(|| Held {
            slots: Arc::clone(self),
            place,
        })
    }
}

/// Places taken for one connection, given back when it drops.
struct Held {
    slots: Arc<Slots>,
    place: Place,
}

impl Drop for Held {
    fn drop(&mut self) {
        let slots = &self.slots;
        match self.place {
            Place::Unread => slots.unread.fetch_sub(1, Ordering::SeqCst),
            Place::Priority => slots.priority.fetch_sub(1, Ordering::SeqCst),
            Place::General => slots.general.fetch_sub(1, Ordering::SeqCst),
            Place::Long => {
                slots.long.fetch_sub(1, Ordering::SeqCst);
                slots.general.fetch_sub(1, Ordering::SeqCst)
            }
        };
    }
}

/// Why a verb of `place` was refused.
fn full_detail(place: Place) -> String {
    match place {
        Place::Unread => format!(
            "{MAX_UNREAD_CONNECTIONS} connections are already waiting to send their request"
        ),
        Place::Priority => {
            format!("{MAX_PRIORITY_REQUESTS} ping, status or stop requests are already waiting")
        }
        Place::Long => format!(
            "{MAX_LONG_REQUESTS} long requests (wait, mutate) are already waiting, or \
             {MAX_CONNECTIONS} requests are already being served"
        ),
        Place::General => format!("{MAX_CONNECTIONS} requests are already being served"),
    }
}

/// Ask the control thread for `request`, watching `stream`'s peer while waiting.
/// `None` when the peer hung up first: nobody is left to answer, and the caller
/// gives the slot back (the request itself stays with the control thread).
fn ask_while_connected(
    stream: &UnixStream,
    tx: &Sender<Msg>,
    request: Request,
    deadline: Duration,
    log: &CtlLog,
) -> Option<Value> {
    let verb = verb_word(&request);
    let (reply, answer) = mpsc::channel();
    if tx.send(Msg::Ctl(request, reply)).is_err() {
        return Some(
            json!({"ok": false, "code": "daemon_stopped", "detail": "the control thread is gone"}),
        );
    }
    let started = Instant::now();
    let give_up = started + deadline;
    let mut watching = true;
    loop {
        let left = give_up.saturating_duration_since(Instant::now());
        match answer.recv_timeout(left.min(PEER_CHECK)) {
            Ok(value) => return Some(value),
            Err(RecvTimeoutError::Disconnected) => {
                return Some(json!({"ok": false, "code": "daemon_stopped", "detail":
                    "the control thread stopped before answering"}));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if left <= PEER_CHECK {
            return Some(json!({"ok": false, "code": "daemon_busy", "detail":
                format!("no answer within {} ms", deadline.as_millis())}));
        }
        if !watching {
            continue;
        }
        match sys::peer_hung_up(stream) {
            Ok(false) => {}
            Ok(true) => {
                log(
                    Level::Info,
                    "ctl_peer_gone",
                    format!(
                        "verb={verb} waited_ms={} cause=the client closed the connection before \
                         the reply; its slot is free",
                        started.elapsed().as_millis()
                    ),
                );
                return None;
            }
            Err(error) => {
                // Cannot tell whether the client is there: keep serving it (the
                // deadline still bounds the wait), but say that it is unwatched.
                watching = false;
                log(
                    Level::Warn,
                    "ctl_peer_unchecked",
                    format!("verb={verb} cause={error}; this wait cannot notice a client hang-up"),
                );
            }
        }
    }
}

// Read one request line within one deadline for the whole line (H5): each
/// read gets only what is left of it. `Ok("")` at end of input.
fn read_request_line(stream: &mut UnixStream, within: Duration) -> io::Result<String> {
    let deadline = Instant::now() + within;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request deadline"));
        }
        stream.set_read_timeout(Some(left))?;
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            break;
        }
        if let Some(end) = chunk[..read].iter().position(|b| *b == b'\n') {
            bytes.extend_from_slice(&chunk[..end]);
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() as u64 > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the request is over {MAX_REQUEST_BYTES} bytes"),
            ));
        }
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the request is not UTF-8"))
}

/// One connection: verify the peer, read its request line, take the slot its
/// verb needs, ask the control thread and write the reply. `unread` is the
/// connection's place before its verb is known; it is given back the moment the
/// request line is parsed (or has failed), so an idle connection never holds a
/// verb slot.
fn serve(
    mut stream: UnixStream,
    uid: u32,
    tx: &Sender<Msg>,
    mutation_deadline: Duration,
    slots: &Arc<Slots>,
    log: &CtlLog,
    unread: Held,
) {
    match sys::peer_uid(&stream) {
        Ok(peer) if peer == uid => {}
        Ok(peer) => {
            answer(
                &mut stream,
                &json!({"ok": false, "code": "peer_refused", "detail": format!("uid {peer} is not the package uid")}),
                log,
            );
            return;
        }
        Err(error) => {
            answer(
                &mut stream,
                &json!({"ok": false, "code": "peer_unverified", "detail": error.to_string()}),
                log,
            );
            return;
        }
    }
    if let Err(error) = stream.set_write_timeout(Some(READ_DEADLINE)) {
        log(
            Level::Warn,
            "ctl_socket_option_failed",
            format!("option=write_timeout cause={error}; a reply to this client may block"),
        );
    }
    let line = read_request_line(&mut stream, READ_DEADLINE);
    let parsed = match &line {
        Ok(line) if line.trim().is_empty() => {
            Err(json!({"ok": false, "code": "empty_request", "detail": "no request line"}))
        }
        Ok(line) => parse_request(line.trim_end(), mutation_deadline)
            .map_err(|detail| json!({"ok": false, "code": "bad_request", "detail": detail})),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(
                json!({"ok": false, "code": "request_timeout", "detail": "no whole request line within 5 s"}),
            )
        }
        Err(error) => Err(json!({"ok": false, "code": "bad_request", "detail": error.to_string()})),
    };
    // The verb is known (or the request has failed): the connection no longer
    // takes an unread place, and takes the slot its verb needs instead.
    drop(unread);
    let reply = match parsed {
        Err(reply) => Some(reply),
        Ok((request, deadline)) => {
            let place = place_of(&request);
            match slots.take(place) {
                Some(_held) => ask_while_connected(&stream, tx, request, deadline, log),
                None => {
                    Some(json!({"ok": false, "code": "ctl_busy", "detail": full_detail(place)}))
                }
            }
        }
    };
    if let Some(reply) = reply {
        answer(&mut stream, &reply, log);
    }
}

/// Bind `path` (removing a stale socket: the caller holds the daemon lock, so
/// no live daemon can own it) and serve it on a thread of its own.
pub(crate) fn listen(
    path: &Path,
    uid: u32,
    tx: Sender<Msg>,
    mutation_deadline: Duration,
    log: CtlLog,
) -> io::Result<thread::JoinHandle<()>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let slots = Arc::new(Slots::default());
    let socket = path.to_path_buf();
    thread::Builder::new()
        .name("sdsyncd-ctl".into())
        .spawn(move || {
            // S3-4: an accept or spawn failure is named once per episode and
            // the listener pauses rather than spinning on it.
            let accept_failing = AtomicBool::new(false);
            let spawn_failing = AtomicBool::new(false);
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => {
                        if accept_failing.swap(false, Ordering::SeqCst) {
                            log(
                                Level::Info,
                                "ctl_accept_recovered",
                                format!("path={}", socket.display()),
                            );
                        }
                        stream
                    }
                    Err(error) => {
                        if !accept_failing.swap(true, Ordering::SeqCst) {
                            log(
                                Level::Error,
                                "ctl_accept_failed",
                                format!(
                                    "path={} cause={error} retry_ms={}",
                                    socket.display(),
                                    ACCEPT_BACKOFF.as_millis()
                                ),
                            );
                        }
                        thread::sleep(ACCEPT_BACKOFF);
                        continue;
                    }
                };
                // A connection that has not sent its verb takes an unread place,
                // never a verb slot (F6s).
                let Some(unread) = slots.take(Place::Unread) else {
                    if let Err(error) = stream.set_write_timeout(Some(READ_DEADLINE)) {
                        log(
                            Level::Warn,
                            "ctl_socket_option_failed",
                            format!("option=write_timeout cause={error}"),
                        );
                    }
                    answer(
                        &mut stream,
                        &json!({"ok": false, "code": "ctl_busy", "detail": full_detail(Place::Unread)}),
                        &log,
                    );
                    continue;
                };
                let tx = tx.clone();
                let thread_slots = Arc::clone(&slots);
                let thread_log = Arc::clone(&log);
                let refused = stream.try_clone();
                let spawned = thread::Builder::new()
                    .name("sdsyncd-ctl-conn".into())
                    .spawn(move || {
                        serve(
                            stream,
                            uid,
                            &tx,
                            mutation_deadline,
                            &thread_slots,
                            &thread_log,
                            unread,
                        );
                    });
                match spawned {
                    Ok(_) => {
                        spawn_failing.store(false, Ordering::SeqCst);
                    }
                    Err(error) => {
                        // The closure (and the unread place it owned) is gone
                        // with the failed spawn.
                        if !spawn_failing.swap(true, Ordering::SeqCst) {
                            log(
                                Level::Error,
                                "ctl_accept_failed",
                                format!(
                                    "path={} cause=a connection thread could not start: {error}",
                                    socket.display()
                                ),
                            );
                        }
                        match refused {
                            Ok(mut stream) => {
                                if let Err(option) = stream.set_write_timeout(Some(READ_DEADLINE)) {
                                    log(
                                        Level::Warn,
                                        "ctl_socket_option_failed",
                                        format!("option=write_timeout cause={option}"),
                                    );
                                }
                                answer(
                                    &mut stream,
                                    &json!({"ok": false, "code": "ctl_busy", "detail": format!("no thread to serve it: {error}")}),
                                    &log,
                                );
                            }
                            Err(clone) => log(
                                Level::Error,
                                "ctl_accept_failed",
                                format!(
                                    "path={} cause=the refused connection could not be answered: {clone}",
                                    socket.display()
                                ),
                            ),
                        }
                        thread::sleep(ACCEPT_BACKOFF);
                    }
                }
            }
        })
}

// The client side: send one request, wait at most `deadline` for the reply.
pub(crate) fn request(path: &Path, request: &Value, deadline: Duration) -> Result<Value, String> {
    // A connect is bounded too: a listener whose backlog is full would hold a
    // blocking connect (H5).
    let mut stream =
        super::super::linux_socket::connect_with_timeout(path, CONNECT_TIMEOUT.min(deadline))
            .map_err(|error| format!("cannot connect to {}: {error}", path.display()))?;
    stream
        .set_read_timeout(Some(deadline))
        .and_then(|()| stream.set_write_timeout(Some(deadline)))
        .map_err(|error| error.to_string())?;
    let mut line = request.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|error| format!("cannot send to {}: {error}", path.display()))?;
    let mut reply = String::new();
    BufReader::new(stream.take(MAX_REQUEST_BYTES * 16))
        .read_line(&mut reply)
        .map_err(|error| {
            format!(
                "no reply from {} within {} s: {error}",
                path.display(),
                deadline.as_secs()
            )
        })?;
    serde_json::from_str(reply.trim_end())
        .map_err(|error| format!("the reply is not JSON: {error}"))
}

/// `--ctl VERB [key=value …]`: build the request a shell or operator asks for.
pub(crate) fn request_from_args(verb: &str, pairs: &[String]) -> Result<(Value, Duration), String> {
    let mut request = json!({"verb": verb});
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("{pair}: expected key=value"))?;
        let parsed = if key == "args" {
            Value::Array(
                value
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.into()))
                    .collect(),
            )
        } else if let Ok(number) = value.parse::<u64>() {
            json!(number)
        } else if let Ok(flag) = value.parse::<bool>() {
            json!(flag)
        } else {
            json!(value)
        };
        request[key] = parsed;
    }
    let deadline = match verb {
        "stop" => Duration::from_secs(request["wait_s"].as_u64().unwrap_or(30).clamp(1, 120) + 5),
        "mutate" => Duration::from_secs(130),
        "wait" => {
            Duration::from_secs(request["wait_s"].as_u64().unwrap_or(3600).clamp(1, 86_400) + 10)
        }
        _ => Duration::from_secs(10),
    };
    Ok((request, deadline))
}

/// Where the control socket lives under a package `var`.
pub(crate) fn socket_path(var: &Path) -> PathBuf {
    var.join("run").join("ctl.sock")
}

#[cfg(test)]
mod tests {
    use super::super::control::ask;
    use super::*;
    use std::time::Instant;

    #[test]
    fn requests_parse_into_bounded_verbs() {
        let d = Duration::from_secs(120);
        assert_eq!(
            parse_request(r#"{"verb":"status"}"#, d).unwrap().0,
            Request::Status
        );
        let (stop, wait) = parse_request(r#"{"verb":"stop","wait_s":500}"#, d).unwrap();
        assert_eq!((stop, wait), (Request::Stop, Duration::from_secs(120)));
        assert_eq!(
            parse_request(r#"{"verb":"plan","scope":"photos"}"#, d)
                .unwrap()
                .0,
            Request::Run {
                scope: "photos".into(),
                action: Action::Plan,
                allow_delete: false,
                max_total_delete: None
            }
        );
        assert!(parse_request(r#"{"verb":"run","scope":"../etc"}"#, d).is_err());
        assert!(parse_request(r#"{"verb":"mutate","op":"rm -rf"}"#, d).is_err());
        assert!(
            parse_request(r#"{"verb":"launch"}"#, d)
                .unwrap_err()
                .contains("unknown verb")
        );
        assert!(parse_request("not json", d).is_err());
        let (mutate, wait) = parse_request(
            r#"{"verb":"mutate","op":"remove-profile","profile":"a","args":["--yes"]}"#,
            d,
        )
        .unwrap();
        assert_eq!(wait, Duration::from_secs(125));
        assert!(
            matches!(mutate, Request::Mutate(m) if m.conflicts_with_a_run() && m.args == ["--yes"])
        );
    }

    #[test]
    fn cli_pairs_become_typed_requests() {
        let (request, _) = request_from_args(
            "run",
            &[
                "scope=photos".into(),
                "allow_delete=true".into(),
                "max_total_delete=5".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            request,
            json!({"verb":"run","scope":"photos","allow_delete":true,"max_total_delete":5})
        );
        assert!(request_from_args("run", &["oops".into()]).is_err());
    }

    /// The socket is 0600, serves its own uid, and a client that never sends a
    /// line is dropped with a named timeout instead of holding a thread.
    #[test]
    fn the_socket_is_private_and_bounded() {
        let dir = std::env::temp_dir().join(format!("sdsyncd-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ctl.sock");
        // A stale socket file from a dead daemon is replaced.
        std::fs::write(&path, b"stale").unwrap();
        let (tx, rx) = mpsc::channel::<Msg>();
        // A stand-in control thread answering pings.
        thread::spawn(move || {
            for message in rx {
                if let Msg::Ctl(_, reply) = message {
                    let _ = reply.send(json!({"ok": true, "pong": true}));
                }
            }
        });
        listen(
            &path,
            sys::geteuid(),
            tx,
            Duration::from_secs(1),
            Arc::new(|_, _, _| {}),
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let start = Instant::now();
        let reply = request(&path, &json!({"verb":"ping"}), Duration::from_secs(5)).unwrap();
        assert_eq!(reply["pong"], true);
        assert!(start.elapsed() < Duration::from_secs(1));
        let reply = request(&path, &json!({"verb":"bogus"}), Duration::from_secs(5)).unwrap();
        assert_eq!(reply["code"], "bad_request");
        // A silent client: the server answers with a named timeout.
        let mut silent = UnixStream::connect(&path).unwrap();
        silent
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut text = String::new();
        BufReader::new(&mut silent).read_line(&mut text).unwrap();
        assert!(text.contains("request_timeout"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stand-in control thread: answers ping, status and stop at once and
    /// holds every other request's reply (a CLI `wait` on a long run).
    fn holding_control() -> Sender<Msg> {
        let (tx, rx) = mpsc::channel::<Msg>();
        thread::spawn(move || {
            let mut held = Vec::new();
            for message in rx {
                if let Msg::Ctl(request, reply) = message {
                    if matches!(request, Request::Ping | Request::Status | Request::Stop) {
                        let _ = reply.send(json!({"ok": true, "pong": true}));
                    } else {
                        held.push(reply);
                    }
                }
            }
        });
        tx
    }

    /// A log that records `event detail` lines.
    fn recording_log() -> (CtlLog, Arc<std::sync::Mutex<Vec<String>>>) {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: CtlLog = Arc::new(move |_, event, detail| {
            sink.lock().unwrap().push(format!("{event} {detail}"));
        });
        (log, lines)
    }

    fn wait_line() -> &'static [u8] {
        b"{\"verb\":\"wait\",\"id\":1,\"wait_s\":60}\n"
    }

    fn socket_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sdsyncd-ctl-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Remove a test directory; a failure is printed, not swallowed.
    fn remove_dir(dir: &Path) {
        if let Err(error) = std::fs::remove_dir_all(dir) {
            eprintln!("could not remove {}: {error}", dir.display());
        }
    }

    fn quiet_log() -> CtlLog {
        Arc::new(|_, _, _| {})
    }

    /// H5: the request deadline is one per connection, not one per read: a
    /// client trickling a byte a second is cut at the deadline, not served
    /// for as long as it keeps trickling.
    #[test]
    fn a_trickling_client_is_cut_at_one_deadline() {
        let dir = socket_dir("trickle");
        let path = dir.join("ctl.sock");
        listen(
            &path,
            sys::geteuid(),
            holding_control(),
            Duration::from_secs(1),
            quiet_log(),
        )
        .unwrap();
        let mut slow = UnixStream::connect(&path).unwrap();
        slow.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let started = Instant::now();
        let mut answered = String::new();
        while started.elapsed() < Duration::from_secs(12) {
            let mut chunk = [0_u8; 512];
            match slow.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    answered.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    break;
                }
                Err(_) => {}
            }
            // One byte a second: each read on the server side succeeds well
            // within its own timeout.
            let _ = slow.write_all(b" ");
            thread::sleep(Duration::from_millis(900));
        }
        let took = started.elapsed();
        assert!(
            took < READ_DEADLINE + Duration::from_secs(2),
            "served for {took:?}"
        );
        assert!(answered.contains("request_timeout"), "{answered:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F6s: CLI waiters cannot take every slot: ping (and status, stop)
    /// still answer while the long verbs wait.
    #[test]
    fn ping_answers_while_waiters_hold_their_slots() {
        let dir = socket_dir("waiters");
        let path = dir.join("ctl.sock");
        listen(
            &path,
            sys::geteuid(),
            holding_control(),
            Duration::from_secs(1),
            quiet_log(),
        )
        .unwrap();
        let mut waiters = Vec::new();
        for _ in 0..20 {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream
                .write_all(b"{\"verb\":\"wait\",\"id\":1,\"wait_s\":60}\n")
                .unwrap();
            waiters.push(stream);
        }
        thread::sleep(Duration::from_millis(300));
        let reply = request(&path, &json!({"verb":"ping"}), Duration::from_secs(5)).unwrap();
        assert_eq!(reply["pong"], true, "{reply}");
        drop(waiters);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F6s: `stop` was in the long class, so sixteen CLI waiters left the one
    /// verb that ends the daemon answering `ctl_busy`. Ping, status and stop have
    /// slots of their own.
    #[test]
    fn stop_status_and_ping_answer_while_every_long_slot_is_taken() {
        let dir = socket_dir("reserved");
        let path = dir.join("ctl.sock");
        listen(
            &path,
            sys::geteuid(),
            holding_control(),
            Duration::from_secs(1),
            quiet_log(),
        )
        .unwrap();
        let mut waiters = Vec::new();
        for _ in 0..MAX_LONG_REQUESTS + 4 {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(wait_line()).unwrap();
            waiters.push(stream);
        }
        thread::sleep(Duration::from_millis(300));
        for verb in [
            json!({"verb":"ping"}),
            json!({"verb":"status"}),
            json!({"verb":"stop","wait_s":5}),
        ] {
            let reply = request(&path, &verb, Duration::from_secs(5)).unwrap();
            assert_eq!(reply["pong"], true, "{verb}: {reply}");
        }
        // The waiters beyond the long cap were refused by name, not held.
        let mut refused = 0;
        for waiter in &mut waiters {
            waiter
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let mut text = String::new();
            if BufReader::new(&*waiter).read_line(&mut text).is_ok() && text.contains("ctl_busy") {
                refused += 1;
            }
        }
        assert_eq!(
            refused, 4,
            "{MAX_LONG_REQUESTS} waiters fit, the rest are told so"
        );
        drop(waiters);
        remove_dir(&dir);
    }

    /// F6s: a slot was taken at accept, before the verb was read, so forty idle
    /// connections made `ping` fail for the five seconds they were allowed to
    /// sit silent. A connection counts once its verb is parsed.
    #[test]
    fn idle_connections_do_not_take_a_verb_slot() {
        let dir = socket_dir("idle");
        let path = dir.join("ctl.sock");
        listen(
            &path,
            sys::geteuid(),
            holding_control(),
            Duration::from_secs(1),
            quiet_log(),
        )
        .unwrap();
        let idle: Vec<UnixStream> = (0..MAX_CONNECTIONS + 8)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();
        thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        let reply = request(&path, &json!({"verb":"ping"}), Duration::from_secs(5)).unwrap();
        assert_eq!(reply["pong"], true, "{reply} after {:?}", started.elapsed());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "ping waited {:?} behind idle connections",
            started.elapsed()
        );
        drop(idle);
        remove_dir(&dir);
    }

    /// F6s: a `wait` whose client died kept its slot for the whole wait (up to
    /// an hour). The listener notices the hang-up, frees the slot and says so.
    #[test]
    fn a_waiter_whose_peer_hung_up_frees_its_slot() {
        let dir = socket_dir("hangup");
        let path = dir.join("ctl.sock");
        let (log, lines) = recording_log();
        listen(
            &path,
            sys::geteuid(),
            holding_control(),
            Duration::from_secs(1),
            log,
        )
        .unwrap();
        let mut waiters = Vec::new();
        for _ in 0..MAX_LONG_REQUESTS {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(wait_line()).unwrap();
            waiters.push(stream);
        }
        thread::sleep(Duration::from_millis(300));
        drop(waiters);
        // A new waiter is admitted (and then held) once the dead ones are freed:
        // it must not be told `ctl_busy`.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut kept = Vec::new();
        loop {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(wait_line()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(600)))
                .unwrap();
            let mut text = String::new();
            let busy = BufReader::new(&stream).read_line(&mut text).is_ok();
            if !busy {
                kept.push(stream);
                break;
            }
            assert!(text.contains("ctl_busy"), "{text}");
            assert!(
                Instant::now() < deadline,
                "dead waiters still hold the long slots after 5 s: {:?}",
                lines.lock().unwrap()
            );
        }
        let seen = lines.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|l| l.starts_with("ctl_peer_gone") && l.contains("verb=wait")),
            "the abandoned waiters are named: {seen:?}"
        );
        remove_dir(&dir);
    }

    /// A client that sends its request and half-closes (`shutdown(WR)`, as a
    /// shell `socat` or a script would) still awaits the reply: that is not a
    /// hang-up, and the listener must not drop it.
    #[test]
    fn a_half_closed_client_still_gets_its_reply() {
        let dir = socket_dir("halfclose");
        let path = dir.join("ctl.sock");
        let (log, lines) = recording_log();
        // A control thread that answers after a delay longer than the peer check.
        let (tx, rx) = mpsc::channel::<Msg>();
        thread::spawn(move || {
            for message in rx {
                if let Msg::Ctl(_, reply) = message {
                    thread::sleep(PEER_CHECK * 3);
                    reply.send(json!({"ok": true, "late": true})).unwrap();
                }
            }
        });
        listen(&path, sys::geteuid(), tx, Duration::from_secs(1), log).unwrap();
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"{\"verb\":\"status\"}\n").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut text = String::new();
        BufReader::new(&stream).read_line(&mut text).unwrap();
        assert!(
            text.contains("\"late\":true"),
            "{text} {:?}",
            lines.lock().unwrap()
        );
        assert!(
            !lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.starts_with("ctl_peer_gone")),
            "{:?}",
            lines.lock().unwrap()
        );
        remove_dir(&dir);
    }

    /// A control thread that never answers yields `daemon_busy`, never a hang.
    #[test]
    fn an_unanswered_request_is_a_named_busy() {
        let (tx, _rx) = mpsc::channel::<Msg>();
        let start = Instant::now();
        let reply = ask(&tx, Request::Status, Duration::from_millis(200));
        assert_eq!(reply["code"], "daemon_busy");
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
