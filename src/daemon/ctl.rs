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
//! Every wait is bounded: a connection that sends nothing is dropped after its
//! read deadline, and a control thread that does not answer in time yields a
//! named `daemon_busy` rather than a hang. Linux only.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::control::{Msg, Mutation, Request, ask};
use super::routines::{Action, valid_name};
use super::sys;

/// The largest request line accepted.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// How long a client has to send its request.
const READ_DEADLINE: Duration = Duration::from_secs(5);
/// How long a quick verb may wait for the control thread.
const ANSWER_DEADLINE: Duration = Duration::from_secs(5);
/// Connections served at once; more are refused by name.
const MAX_CONNECTIONS: usize = 16;

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

fn answer(stream: &mut UnixStream, value: &Value) {
    let mut line = value.to_string();
    line.push('\n');
    // A client that went away is not an error worth more than a dropped reply.
    let _ = stream.write_all(line.as_bytes());
}

fn serve(mut stream: UnixStream, uid: u32, tx: &Sender<Msg>, mutation_deadline: Duration) {
    match sys::peer_uid(&stream) {
        Ok(peer) if peer == uid => {}
        Ok(peer) => {
            answer(
                &mut stream,
                &json!({"ok": false, "code": "peer_refused", "detail": format!("uid {peer} is not the package uid")}),
            );
            return;
        }
        Err(error) => {
            answer(
                &mut stream,
                &json!({"ok": false, "code": "peer_unverified", "detail": error.to_string()}),
            );
            return;
        }
    }
    let _ = stream.set_read_timeout(Some(READ_DEADLINE));
    let _ = stream.set_write_timeout(Some(READ_DEADLINE));
    let mut line = String::new();
    let read = match stream.try_clone() {
        Ok(reader) => BufReader::new(reader.take(MAX_REQUEST_BYTES)).read_line(&mut line),
        Err(error) => Err(error),
    };
    let reply = match read {
        Ok(0) => json!({"ok": false, "code": "empty_request", "detail": "no request line"}),
        Ok(_) => match parse_request(line.trim_end(), mutation_deadline) {
            Ok((request, deadline)) => ask(tx, request, deadline),
            Err(detail) => json!({"ok": false, "code": "bad_request", "detail": detail}),
        },
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            json!({"ok": false, "code": "request_timeout", "detail": "no request within 5 s"})
        }
        Err(error) => json!({"ok": false, "code": "bad_request", "detail": error.to_string()}),
    };
    answer(&mut stream, &reply);
}

/// Bind `path` (removing a stale socket: the caller holds the daemon lock, so
/// no live daemon can own it) and serve it on a thread of its own.
pub(crate) fn listen(
    path: &Path,
    uid: u32,
    tx: Sender<Msg>,
    mutation_deadline: Duration,
) -> io::Result<thread::JoinHandle<()>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let busy = Arc::new(AtomicUsize::new(0));
    thread::Builder::new()
        .name("sdsyncd-ctl".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                if busy.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    answer(
                        &mut stream,
                        &json!({"ok": false, "code": "ctl_busy", "detail": "too many connections"}),
                    );
                    continue;
                }
                busy.fetch_add(1, Ordering::SeqCst);
                let tx = tx.clone();
                let counter = Arc::clone(&busy);
                let spawned = thread::Builder::new()
                    .name("sdsyncd-ctl-conn".into())
                    .spawn(move || {
                        serve(stream, uid, &tx, mutation_deadline);
                        counter.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    busy.fetch_sub(1, Ordering::SeqCst);
                }
            }
        })
}

/// The client side: send one request, wait at most `deadline` for the reply.
pub(crate) fn request(path: &Path, request: &Value, deadline: Duration) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path)
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
    use super::*;
    use std::sync::mpsc;
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
        listen(&path, sys::geteuid(), tx, Duration::from_secs(1)).unwrap();
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
