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
//!   connections cannot make `ping` fail. When the unread table is full the
//!   oldest unread connection is dropped, by name, to make room for the new
//!   one (S3-8): a client that reconnects without ever sending a verb loses its
//!   own connections, never a real client's turn;
//! - once the verb is parsed it takes a slot by class: `ping` and `status`
//!   have reserved slots that nothing else can use, `stop` has two of its own
//!   (a stop waits up to `wait_s`, so it must not hold the ping slots, S3-9),
//!   `wait` and `mutate` are capped within the general slots, and everything
//!   else takes a general one;
//! - a verb waiting on the control thread watches its peer: a client that has
//!   gone away frees its slot, and the listener logs `ctl_peer_gone`.
//!
//! Linux only.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
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
/// Slots only `ping` and `status` can use, beyond the general ones, so neither
/// CLI waiters nor a flood of other verbs can starve them (F6s).
const MAX_PRIORITY_REQUESTS: usize = 8;
/// Slots only `stop` can use (S3-9). A stop waits for the daemon to wind down,
/// up to its `wait_s`, so eight of them held the whole shared reserve and made
/// ping and status answer `ctl_busy`. A stop over these is told so by name.
const MAX_STOP_REQUESTS: usize = 2;
/// Connections that have not yet sent a whole request line. Each is a thread
/// cut at [`READ_DEADLINE`]; past this the oldest is dropped for the newest.
const MAX_UNREAD_CONNECTIONS: usize = 128;
/// How long an episode of dropped empty connections, or of evictions, goes
/// without a new one before it is over and its count is logged (S3-8, S3-10).
const EPISODE_QUIET: Duration = Duration::from_secs(30);
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

/// What an [`Episode`] reports when it is over.
struct EpisodeEnd {
    count: u64,
    lasted: Duration,
}

/// What [`Episode::note`] found.
enum Noted {
    /// A new episode began; the caller logs this event in full. `previous` is
    /// an earlier episode that went quiet and was not reported yet.
    Begins { previous: Option<EpisodeEnd> },
    /// The episode was already under way: counted, not logged.
    Counted,
}

/// A run of the same event, logged once (S3-8, S3-10): the first is written in
/// full, the rest are counted, and once no new one has come for `quiet` the
/// episode's count is written by [`Episode::ended`]. Nothing is dropped: what
/// is not written at the time is in the count.
struct Episode {
    quiet: Duration,
    state: Mutex<Option<EpisodeState>>,
}

struct EpisodeState {
    began: Instant,
    last: Instant,
    count: u64,
}

impl Episode {
    fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            state: Mutex::new(None),
        }
    }

    fn note(&self) -> Noted {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match state.as_mut() {
            Some(open) if now.duration_since(open.last) < self.quiet => {
                open.last = now;
                open.count += 1;
                Noted::Counted
            }
            _ => {
                let previous = state.take().map(|old| EpisodeEnd {
                    count: old.count,
                    lasted: old.last.duration_since(old.began),
                });
                *state = Some(EpisodeState {
                    began: now,
                    last: now,
                    count: 1,
                });
                Noted::Begins { previous }
            }
        }
    }

    /// The episode, once it has been quiet for `quiet`.
    fn ended(&self) -> Option<EpisodeEnd> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .as_ref()
            .is_some_and(|open| open.last.elapsed() >= self.quiet)
        {
            return state.take().map(|old| EpisodeEnd {
                count: old.count,
                lasted: old.last.duration_since(old.began),
            });
        }
        None
    }
}

fn answer(stream: &UnixStream, value: &Value, log: &CtlLog, slots: &Slots) {
    let mut line = value.to_string();
    line.push('\n');
    let mut writer = stream;
    // A client that went away loses its reply, but the loss is named.
    if let Err(error) = writer.write_all(line.as_bytes()) {
        let code = value["code"].as_str().unwrap_or("ok");
        // S3-10: a connect-and-close with no request is one line per episode,
        // not one per connection (a probe or a port scan can make thousands).
        if code == "empty_request" {
            match slots.empty_requests.note() {
                Noted::Counted => return,
                Noted::Begins { previous } => {
                    if let Some(end) = previous {
                        log_episode_end(log, "ctl_empty_requests_ended", &end);
                    }
                    log(
                        Level::Info,
                        "ctl_reply_dropped",
                        format!(
                            "code={code} cause={error}; further connections that send no \
                             request are counted, not logged, until none comes for {} s \
                             (ctl_empty_requests_ended)",
                            slots.empty_requests.quiet.as_secs()
                        ),
                    );
                    return;
                }
            }
        }
        log(
            Level::Info,
            "ctl_reply_dropped",
            format!("code={code} cause={error}"),
        );
    }
}

fn log_episode_end(log: &CtlLog, event: &'static str, end: &EpisodeEnd) {
    log(
        Level::Info,
        event,
        format!("count={} over_ms={}", end.count, end.lasted.as_millis()),
    );
}

/// Where a connection stands, and so which places it holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// Connected, verb not yet parsed.
    Unread,
    /// `ping`, `status`: reserved slots.
    Priority,
    /// `stop`: two reserved slots of its own.
    Stop,
    /// `wait`, `mutate`: a general slot and a long one.
    Long,
    /// Everything else: a general slot.
    General,
}

fn place_of(request: &Request) -> Place {
    match request {
        Request::Ping | Request::Status => Place::Priority,
        Request::Stop => Place::Stop,
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

/// An unread connection the table can drop to make room (S3-8).
struct UnreadEntry {
    id: u64,
    stream: Arc<UnixStream>,
    evicted: Arc<AtomicBool>,
}

/// The connections that have not yet sent their request, oldest first.
#[derive(Default)]
struct UnreadTable {
    next_id: u64,
    entries: VecDeque<UnreadEntry>,
}

/// How many places each kind holds right now.
struct Slots {
    unread: Mutex<UnreadTable>,
    general: AtomicUsize,
    long: AtomicUsize,
    priority: AtomicUsize,
    stop: AtomicUsize,
    /// Dropped replies to connections that sent no request (S3-10).
    empty_requests: Episode,
    /// Unread connections dropped for a newer one (S3-8).
    evictions: Episode,
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
    fn new(episode_quiet: Duration) -> Self {
        Self {
            unread: Mutex::new(UnreadTable::default()),
            general: AtomicUsize::new(0),
            long: AtomicUsize::new(0),
            priority: AtomicUsize::new(0),
            stop: AtomicUsize::new(0),
            empty_requests: Episode::new(episode_quiet),
            evictions: Episode::new(episode_quiet),
        }
    }

    /// An unread place for `stream`. A full table drops its oldest entry for
    /// this one and returns it: the caller shuts it down and says so (S3-8).
    fn take_unread(self: &Arc<Self>, stream: &Arc<UnixStream>) -> (Held, Option<UnreadEntry>) {
        let mut table = self.unread.lock().unwrap_or_else(PoisonError::into_inner);
        let victim = if table.entries.len() >= MAX_UNREAD_CONNECTIONS {
            table.entries.pop_front()
        } else {
            None
        };
        if let Some(victim) = &victim {
            victim.evicted.store(true, Ordering::SeqCst);
        }
        let id = table.next_id;
        table.next_id += 1;
        let evicted = Arc::new(AtomicBool::new(false));
        table.entries.push_back(UnreadEntry {
            id,
            stream: Arc::clone(stream),
            evicted: Arc::clone(&evicted),
        });
        let held = Held {
            slots: Arc::clone(self),
            place: Place::Unread,
            ticket: Some(Ticket { id, evicted }),
        };
        (held, victim)
    }

    /// Log the end of an episode that has gone quiet (called whenever a
    /// request is served, so the count is written soon after the last event).
    fn report_ended_episodes(&self, log: &CtlLog) {
        if let Some(end) = self.empty_requests.ended() {
            log_episode_end(log, "ctl_empty_requests_ended", &end);
        }
        if let Some(end) = self.evictions.ended() {
            log_episode_end(log, "ctl_unread_eviction_ended", &end);
        }
    }

    /// The places for `place`, all or none; they are given back when the
    /// returned guard drops. (An unread place is [`Slots::take_unread`].)
    fn take(self: &Arc<Self>, place: Place) -> Option<Held> {
        let taken = match place {
            Place::Unread => false,
            Place::Priority => increment(&self.priority, MAX_PRIORITY_REQUESTS),
            Place::Stop => increment(&self.stop, MAX_STOP_REQUESTS),
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
            ticket: None,
        })
    }
}

/// An unread connection's claim on the table, and whether it was dropped.
struct Ticket {
    id: u64,
    evicted: Arc<AtomicBool>,
}

/// Places taken for one connection, given back when it drops.
struct Held {
    slots: Arc<Slots>,
    place: Place,
    ticket: Option<Ticket>,
}

impl Held {
    /// Whether the table dropped this unread connection for a newer one.
    fn was_evicted(&self) -> bool {
        self.ticket
            .as_ref()
            .is_some_and(|ticket| ticket.evicted.load(Ordering::SeqCst))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let slots = &self.slots;
        match self.place {
            Place::Unread => {
                if let Some(ticket) = &self.ticket {
                    let mut table = slots.unread.lock().unwrap_or_else(PoisonError::into_inner);
                    // Already gone when it was evicted.
                    if let Some(at) = table.entries.iter().position(|e| e.id == ticket.id) {
                        table.entries.remove(at);
                    }
                }
            }
            Place::Priority => {
                slots.priority.fetch_sub(1, Ordering::SeqCst);
            }
            Place::Stop => {
                slots.stop.fetch_sub(1, Ordering::SeqCst);
            }
            Place::General => {
                slots.general.fetch_sub(1, Ordering::SeqCst);
            }
            Place::Long => {
                slots.long.fetch_sub(1, Ordering::SeqCst);
                slots.general.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

/// Why a verb of `place` was refused.
fn full_detail(place: Place) -> String {
    match place {
        Place::Unread => format!(
            "evicted: {MAX_UNREAD_CONNECTIONS} connections were waiting to send their request, \
             and this one was the oldest"
        ),
        Place::Priority => {
            format!("{MAX_PRIORITY_REQUESTS} ping or status requests are already waiting")
        }
        Place::Stop => format!(
            "{MAX_STOP_REQUESTS} stop requests are already waiting for the daemon to stop; \
             one of them is answered when it has"
        ),
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
fn read_request_line(stream: &UnixStream, within: Duration) -> io::Result<String> {
    let deadline = Instant::now() + within;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut reader = stream;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request deadline"));
        }
        stream.set_read_timeout(Some(left))?;
        let read = match reader.read(&mut chunk) {
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
    stream: Arc<UnixStream>,
    uid: u32,
    tx: &Sender<Msg>,
    mutation_deadline: Duration,
    slots: &Arc<Slots>,
    log: &CtlLog,
    unread: Held,
) {
    let stream: &UnixStream = &stream;
    match sys::peer_uid(stream) {
        Ok(peer) if peer == uid => {}
        Ok(peer) => {
            answer(
                stream,
                &json!({"ok": false, "code": "peer_refused", "detail": format!("uid {peer} is not the package uid")}),
                log,
                slots,
            );
            return;
        }
        Err(error) => {
            answer(
                stream,
                &json!({"ok": false, "code": "peer_unverified", "detail": error.to_string()}),
                log,
                slots,
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
    let line = read_request_line(stream, READ_DEADLINE);
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
    // An episode of empty connections or evictions that has gone quiet is
    // reported by the next request read, before its reply (S3-8, S3-10).
    if parsed.is_ok() {
        slots.report_ended_episodes(log);
    }
    // S3-8: a connection the table dropped for a newer one had its input shut
    // down, so what it sent is cut short. A request that did arrive whole is
    // served; anything else is answered as the eviction it was, not as an
    // empty or bad request.
    let parsed = match parsed {
        Err(_) if unread.was_evicted() => {
            Err(json!({"ok": false, "code": "ctl_busy", "detail": full_detail(Place::Unread)}))
        }
        other => other,
    };
    // The verb is known (or the request has failed): the connection no longer
    // takes an unread place, and takes the slot its verb needs instead.
    drop(unread);
    let reply = match parsed {
        Err(reply) => Some(reply),
        Ok((request, deadline)) => {
            let place = place_of(&request);
            match slots.take(place) {
                Some(_held) => ask_while_connected(stream, tx, request, deadline, log),
                None => {
                    Some(json!({"ok": false, "code": "ctl_busy", "detail": full_detail(place)}))
                }
            }
        }
    };
    if let Some(reply) = reply {
        answer(stream, &reply, log, slots);
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
    listen_with(path, uid, tx, mutation_deadline, log, EPISODE_QUIET)
}

/// [`listen`] with the quiet period that ends an episode of dropped empty
/// connections or evictions (tests shorten it).
pub(crate) fn listen_with(
    path: &Path,
    uid: u32,
    tx: Sender<Msg>,
    mutation_deadline: Duration,
    log: CtlLog,
    episode_quiet: Duration,
) -> io::Result<thread::JoinHandle<()>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let slots = Arc::new(Slots::new(episode_quiet));
    let socket = path.to_path_buf();
    thread::Builder::new()
        .name("sdsyncd-ctl".into())
        .spawn(move || {
            // S3-4: an accept or spawn failure is named once per episode and
            // the listener pauses rather than spinning on it.
            let accept_failing = AtomicBool::new(false);
            let spawn_failing = AtomicBool::new(false);
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(stream) => {
                        if accept_failing.swap(false, Ordering::SeqCst) {
                            log(
                                Level::Info,
                                "ctl_accept_recovered",
                                format!("path={}", socket.display()),
                            );
                        }
                        Arc::new(stream)
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
                // never a verb slot (F6s). A full table drops its oldest entry
                // for this one (S3-8): refusing the new connection let a client
                // that reconnected without ever sending a verb keep every place.
                let (unread, victim) = slots.take_unread(&stream);
                if let Some(victim) = victim {
                    evict(&slots, &log, &socket, &victim);
                }
                let tx = tx.clone();
                let thread_slots = Arc::clone(&slots);
                let thread_log = Arc::clone(&log);
                let thread_stream = Arc::clone(&stream);
                let spawned = thread::Builder::new()
                    .name("sdsyncd-ctl-conn".into())
                    .spawn(move || {
                        serve(
                            thread_stream,
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
                        if let Err(option) = stream.set_write_timeout(Some(READ_DEADLINE)) {
                            log(
                                Level::Warn,
                                "ctl_socket_option_failed",
                                format!("option=write_timeout cause={option}"),
                            );
                        }
                        answer(
                            &stream,
                            &json!({"ok": false, "code": "ctl_busy", "detail": format!("no thread to serve it: {error}")}),
                            &log,
                            &slots,
                        );
                        thread::sleep(ACCEPT_BACKOFF);
                    }
                }
            }
        })
}

/// Drop the oldest unread connection to make room (S3-8). Its input is shut
/// down so its thread wakes at once; the thread answers `ctl_busy`. The first
/// eviction of an episode is logged in full, the rest are counted.
fn evict(slots: &Slots, log: &CtlLog, socket: &Path, victim: &UnreadEntry) {
    if let Err(error) = victim.stream.shutdown(Shutdown::Read) {
        log(
            Level::Warn,
            "ctl_socket_option_failed",
            format!(
                "option=shutdown path={} cause={error}; the evicted connection is held until \
                 its {} s read deadline",
                socket.display(),
                READ_DEADLINE.as_secs()
            ),
        );
    }
    match slots.evictions.note() {
        Noted::Counted => {}
        Noted::Begins { previous } => {
            if let Some(end) = previous {
                log_episode_end(log, "ctl_unread_eviction_ended", &end);
            }
            log(
                Level::Warn,
                "ctl_unread_evicted",
                format!(
                    "path={} cause={MAX_UNREAD_CONNECTIONS} connections were waiting to send a \
                     request, so the oldest is dropped for each new one; further evictions are \
                     counted until none comes for {} s (ctl_unread_eviction_ended)",
                    socket.display(),
                    slots.evictions.quiet.as_secs()
                ),
            );
        }
    }
}

/// "cannot connect to <path>: <cause>", the path said once: a full backlog already names the
/// socket in its own words (`connect_with_timeout`), and repeating it read as two sockets.
fn connect_failure(path: &Path, error: &io::Error) -> String {
    let cause = error.to_string();
    if cause.contains(&path.display().to_string()) {
        format!("cannot connect: {cause}")
    } else {
        format!("cannot connect to {}: {cause}", path.display())
    }
}

// The client side: send one request, wait at most `deadline` for the reply.
pub(crate) fn request(path: &Path, request: &Value, deadline: Duration) -> Result<Value, String> {
    // A connect is bounded too: a listener whose backlog is full would hold a
    // blocking connect (H5).
    let mut stream =
        super::super::linux_socket::connect_with_timeout(path, CONNECT_TIMEOUT.min(deadline))
            .map_err(|error| connect_failure(path, &error))?;
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
    fn a_connect_failure_names_the_socket_path_once() {
        let path = Path::new("/var/run/ctl.sock");
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert_eq!(
            connect_failure(path, &refused),
            format!("cannot connect to {}: {refused}", path.display())
        );
        let full = io::Error::new(
            io::ErrorKind::TimedOut,
            "/var/run/ctl.sock: the listener's backlog stayed full for 5000 ms (connect: x)",
        );
        let said = connect_failure(path, &full);
        assert_eq!(said.matches("/var/run/ctl.sock").count(), 1, "{said}");
        assert!(said.contains("backlog stayed full"), "{said}");
    }

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

    /// A stand-in control thread that answers ping and status at once and holds
    /// every stop (a daemon that is slow to wind down) and every other request.
    fn stop_holding_control() -> Sender<Msg> {
        let (tx, rx) = mpsc::channel::<Msg>();
        thread::spawn(move || {
            let mut held = Vec::new();
            for message in rx {
                if let Msg::Ctl(request, reply) = message {
                    if matches!(request, Request::Ping | Request::Status) {
                        let _ = reply.send(json!({"ok": true, "pong": true}));
                    } else {
                        held.push(reply);
                    }
                }
            }
        });
        tx
    }

    /// One text reply line, or what went wrong reading it.
    fn reply_line(stream: &UnixStream, within: Duration) -> String {
        stream.set_read_timeout(Some(within)).unwrap();
        let mut text = String::new();
        match BufReader::new(stream).read_line(&mut text) {
            Ok(_) => text,
            Err(error) => format!("<no reply: {error}>"),
        }
    }

    /// Ask until the verb is served (a client that is told `ctl_busy` asks
    /// again, as the CLI does); `(attempts, reply)` or the last failure.
    fn ask_until_served(
        path: &Path,
        verb: &Value,
        within: Duration,
    ) -> Result<(u32, Value), String> {
        let started = Instant::now();
        let mut attempts = 0;
        loop {
            attempts += 1;
            let outcome = request(path, verb, Duration::from_secs(8));
            match &outcome {
                Ok(reply) if reply["pong"] == true => return Ok((attempts, reply.clone())),
                _ if started.elapsed() > within => {
                    return Err(format!(
                        "{verb} was not served in {within:?} ({attempts} attempts); last: {outcome:?}"
                    ));
                }
                _ => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// S3-8: a full unread table refused every new connection, so a same-uid
    /// client that reconnected without ever sending a verb starved ping and
    /// stop. The oldest unread connection is dropped, by name, instead.
    #[test]
    fn a_full_unread_table_evicts_the_oldest_instead_of_refusing_ping() {
        let dir = socket_dir("evict");
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
        let idle: Vec<UnixStream> = (0..MAX_UNREAD_CONNECTIONS)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();
        thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        let reply = request(&path, &json!({"verb":"ping"}), Duration::from_secs(5)).unwrap();
        assert_eq!(
            reply["pong"],
            true,
            "ping with {MAX_UNREAD_CONNECTIONS} silent connections: {reply} after {:?}",
            started.elapsed()
        );
        // The oldest silent connection was told why it went; the newest was kept.
        let first = reply_line(&idle[0], Duration::from_secs(2));
        assert!(
            first.contains("ctl_busy") && first.contains("evicted"),
            "the oldest connection's reply: {first:?}"
        );
        let last = reply_line(
            &idle[MAX_UNREAD_CONNECTIONS - 1],
            Duration::from_millis(200),
        );
        assert!(
            last.starts_with("<no reply"),
            "the newest silent connection is still waiting for its turn, got {last:?}"
        );
        let seen = lines.lock().unwrap().clone();
        assert!(
            seen.iter().any(|l| l.starts_with("ctl_unread_evicted")),
            "the eviction is named in the log: {seen:?}"
        );
        drop(idle);
        remove_dir(&dir);
    }

    /// S3-8, the reviewer's flood: 160 same-uid clients reconnect without
    /// sending anything. 0 of 142 pings got through; each is served now.
    #[test]
    fn a_reconnect_flood_cannot_starve_ping_or_stop() {
        let dir = socket_dir("flood");
        let path = dir.join("ctl.sock");
        listen(
            &path,
            sys::geteuid(),
            holding_control(),
            Duration::from_secs(1),
            quiet_log(),
        )
        .unwrap();
        let over = Arc::new(AtomicBool::new(false));
        let flooders: Vec<_> = (0..MAX_UNREAD_CONNECTIONS + 32)
            .map(|_| {
                let (path, over) = (path.clone(), Arc::clone(&over));
                thread::spawn(move || {
                    while !over.load(Ordering::SeqCst) {
                        match UnixStream::connect(&path) {
                            Ok(mut stream) => {
                                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                                let mut chunk = [0_u8; 256];
                                while !over.load(Ordering::SeqCst)
                                    && stream.read(&mut chunk).is_err()
                                {
                                }
                            }
                            Err(_) => thread::sleep(Duration::from_millis(20)),
                        }
                    }
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        let (mut pings, mut stops, mut worst_attempts) = (0, 0, 0);
        let mut failures = Vec::new();
        while started.elapsed() < Duration::from_secs(4) {
            for (verb, served) in [
                (json!({"verb":"ping"}), &mut pings),
                (json!({"verb":"stop","wait_s":5}), &mut stops),
            ] {
                match ask_until_served(&path, &verb, Duration::from_secs(5)) {
                    Ok((attempts, _)) => {
                        *served += 1;
                        worst_attempts = worst_attempts.max(attempts);
                    }
                    Err(failure) => failures.push(failure),
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        over.store(true, Ordering::SeqCst);
        for flooder in flooders {
            flooder.join().unwrap();
        }
        assert!(
            failures.is_empty() && pings > 5 && stops > 5,
            "pings served {pings}, stops served {stops}, worst attempts {worst_attempts}, \
             failures: {failures:?}"
        );
        remove_dir(&dir);
    }

    /// S3-9: eight waiting stops held the shared reserve for up to `wait_s`,
    /// so ping and status failed behind them. Stop has slots of its own, and a
    /// stop over them is told so by name.
    #[test]
    fn waiting_stops_cannot_block_ping_or_status() {
        let dir = socket_dir("stops");
        let path = dir.join("ctl.sock");
        listen(
            &path,
            sys::geteuid(),
            stop_holding_control(),
            Duration::from_secs(1),
            quiet_log(),
        )
        .unwrap();
        let mut stops = Vec::new();
        for _ in 0..MAX_PRIORITY_REQUESTS + 4 {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream
                .write_all(b"{\"verb\":\"stop\",\"wait_s\":30}\n")
                .unwrap();
            stops.push(stream);
        }
        thread::sleep(Duration::from_millis(500));
        for verb in [json!({"verb":"ping"}), json!({"verb":"status"})] {
            let reply = request(&path, &verb, Duration::from_secs(5)).unwrap();
            assert_eq!(
                reply["pong"],
                true,
                "{verb} behind {} waiting stops: {reply}",
                stops.len()
            );
        }
        // The stops over the reserve were told by name; the reserved ones wait.
        let mut refused = 0;
        for stop in &stops {
            let text = reply_line(stop, Duration::from_millis(50));
            if text.contains("ctl_busy") {
                assert!(text.contains("stop"), "the refusal names stop: {text}");
                refused += 1;
            }
        }
        assert_eq!(
            refused,
            stops.len() - MAX_STOP_REQUESTS,
            "{MAX_STOP_REQUESTS} stops wait, the other {} are told so",
            stops.len() - MAX_STOP_REQUESTS
        );
        drop(stops);
        remove_dir(&dir);
    }

    /// S3-10: a connect-and-close with no request wrote one Info line per
    /// connection. The first of an episode is logged in full, the rest are
    /// counted, and the end of the episode is reported with the count.
    #[test]
    fn empty_connections_are_logged_once_per_episode() {
        let dir = socket_dir("empty");
        let path = dir.join("ctl.sock");
        let (log, lines) = recording_log();
        listen_with(
            &path,
            sys::geteuid(),
            holding_control(),
            Duration::from_secs(1),
            log,
            Duration::from_millis(400),
        )
        .unwrap();
        const EMPTY: usize = 60;
        for _ in 0..EMPTY {
            drop(UnixStream::connect(&path).unwrap());
        }
        // Let every connection thread finish its (failed) reply.
        thread::sleep(Duration::from_millis(600));
        let dropped = |lines: &[String]| {
            lines
                .iter()
                .filter(|l| l.starts_with("ctl_reply_dropped") && l.contains("code=empty_request"))
                .count()
        };
        let seen = lines.lock().unwrap().clone();
        assert!(
            (1..=2).contains(&dropped(&seen)),
            "{EMPTY} empty connections wrote {} drop lines: {seen:?}",
            dropped(&seen)
        );
        // A served request after the quiet period ends the episode, with its count.
        let reply = request(&path, &json!({"verb":"ping"}), Duration::from_secs(5)).unwrap();
        assert_eq!(reply["pong"], true, "{reply}");
        let seen = lines.lock().unwrap().clone();
        let ended: Vec<&String> = seen
            .iter()
            .filter(|l| l.starts_with("ctl_empty_requests_ended"))
            .collect();
        assert_eq!(ended.len(), 1, "the episode's end is reported: {seen:?}");
        let counted: usize = ended[0]
            .split("count=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no count in {:?}", ended[0]));
        assert!(
            counted >= EMPTY - 2,
            "the end line counts the episode's connections ({EMPTY} made): {:?}",
            ended[0]
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
