//! The dashboard's API server under the daemon engine (`--serve-api`).
//!
//! The bridge's server (`sdsync-dsm-api --serve`: `api.sock`, its bounded
//! worker pool and every request handler) clears the environment and keeps
//! process-wide state that assumes it is alone, so it runs as its own child
//! process, not as a thread of the daemon. The daemon keeps it alive:
//! - start: the daemon is not ready (no `ctl.sock`, no `daemon.ready`) until
//!   `api.sock` accepts a connection, within a deadline; otherwise the start
//!   fails with the child's own last words;
//! - a child that exits is named with its status and last stderr line and
//!   restarted after a backoff (1 s doubling to 30 s, reset after 10 minutes
//!   of service), each restart named;
//! - stop: TERM, then KILL after the grace; the child also dies with the
//!   daemon (parent-death signal), so it never outlives it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::routines::Level;
use super::sys;

/// Where the keeper's events go: level, event name, detail.
pub(crate) type KeeperLog = Arc<dyn Fn(Level, &'static str, String) + Send + Sync>;

/// The running keeper.
pub(crate) struct ApiKeeper {
    stop: Arc<AtomicBool>,
    child: Arc<AtomicU32>,
    thread: thread::JoinHandle<()>,
}

/// How long a restarted child must serve before the backoff resets.
const STABLE_AFTER: Duration = Duration::from_secs(600);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

impl ApiKeeper {
    /// Start the server and wait until `socket` accepts a connection.
    pub(crate) fn start(
        exe: PathBuf,
        socket: PathBuf,
        env: Vec<(OsString, OsString)>,
        log: KeeperLog,
        ready_within: Duration,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(AtomicU32::new(0));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let keeper = Keeper {
            exe,
            socket,
            env,
            log,
            stop: Arc::clone(&stop),
            child: Arc::clone(&child),
        };
        let thread = thread::Builder::new()
            .name("sdsyncd-api".into())
            .spawn(move || keeper.run(ready_tx))
            .map_err(|error| format!("the API keeper thread could not start: {error}"))?;
        let started = Self {
            stop,
            child,
            thread,
        };
        match ready_rx.recv_timeout(ready_within) {
            Ok(Ok(())) => Ok(started),
            Ok(Err(cause)) => {
                started.stop(Duration::from_secs(2));
                Err(cause)
            }
            Err(_) => {
                started.stop(Duration::from_secs(2));
                Err(format!(
                    "api.sock did not accept a connection within {} s",
                    ready_within.as_secs()
                ))
            }
        }
    }

    /// Stop the server: TERM, then KILL after `grace`. Bounded.
    pub(crate) fn stop(self, grace: Duration) {
        self.stop.store(true, Ordering::SeqCst);
        let pid = self.child.load(Ordering::SeqCst);
        if pid > 1 {
            let _ = sys::kill_process(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + grace;
        while !self.thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if !self.thread.is_finished() {
            let pid = self.child.load(Ordering::SeqCst);
            if pid > 1 {
                let _ = sys::kill_process(pid, libc::SIGKILL);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while !self.thread.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
        if self.thread.is_finished() {
            let _ = self.thread.join();
        }
        // Otherwise the keeper is abandoned; the child dies with this process.
    }
}

struct Keeper {
    exe: PathBuf,
    socket: PathBuf,
    env: Vec<(OsString, OsString)>,
    log: KeeperLog,
    stop: Arc<AtomicBool>,
    child: Arc<AtomicU32>,
}

impl Keeper {
    #[allow(clippy::type_complexity)]
    fn spawn(
        &self,
    ) -> std::io::Result<(
        Child,
        Arc<Mutex<VecDeque<u8>>>,
        Option<thread::JoinHandle<()>>,
    )> {
        let mut command = Command::new(&self.exe);
        command
            .arg("--serve")
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let parent = sys::getpid();
        // SAFETY: child_setup is async-signal-safe and allocates nothing.
        unsafe {
            command.pre_exec(move || sys::child_setup(parent, None));
        }
        let mut child = command.spawn()?;
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let mut reader = None;
        if let Some(mut stderr) = child.stderr.take() {
            let tail = Arc::clone(&tail);
            reader = thread::Builder::new()
                .name("sdsyncd-api-stderr".into())
                .spawn(move || {
                    let mut buffer = [0_u8; 1024];
                    while let Ok(read) = stderr.read(&mut buffer) {
                        if read == 0 {
                            break;
                        }
                        if let Ok(mut tail) = tail.lock() {
                            tail.extend(&buffer[..read]);
                            while tail.len() > 4096 {
                                tail.pop_front();
                            }
                        }
                    }
                })
                .ok();
        }
        Ok((child, tail, reader))
    }

    fn run(self, ready: mpsc::Sender<Result<(), String>>) {
        let mut ready = Some(ready);
        let mut backoff = Duration::from_secs(1);
        let mut attempt: u32 = 0;
        while !self.stop.load(Ordering::SeqCst) {
            let (mut child, tail, reader) = match self.spawn() {
                Ok(spawned) => spawned,
                Err(error) => {
                    let cause = format!("{}: {error}", self.exe.display());
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(format!("the API server could not start: {cause}")));
                        return;
                    }
                    (self.log)(
                        Level::Error,
                        "api_server_unavailable",
                        format!("cause={cause}"),
                    );
                    if self.pause(backoff) {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            };
            self.child.store(child.id(), Ordering::SeqCst);
            let started = Instant::now();
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                if ready.is_some()
                    && UnixStream::connect(&self.socket).is_ok()
                    && let Some(ready) = ready.take()
                {
                    let _ = ready.send(Ok(()));
                }
                thread::sleep(Duration::from_millis(50));
            };
            self.child.store(0, Ordering::SeqCst);
            // The child's last words arrive after its exit: give the reader
            // a bounded moment to drain the pipe (a grandchild holding it open
            // cannot hold this thread).
            if let Some(reader) = reader {
                let until = Instant::now() + Duration::from_millis(500);
                while !reader.is_finished() && Instant::now() < until {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            let last = tail
                .lock()
                .map(|tail| {
                    let bytes: Vec<u8> = tail.iter().copied().collect();
                    String::from_utf8_lossy(&bytes)
                        .lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .map(|line| super::state::sanitize_detail(line, 200))
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let code = status
                .map(|s| {
                    s.code()
                        .or_else(|| s.signal().map(|n| 128 + n))
                        .unwrap_or(-1)
                })
                .unwrap_or(-1);
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(format!(
                    "the API server exited (status {code}) before api.sock answered{}",
                    if last.is_empty() {
                        String::new()
                    } else {
                        format!(": {last}")
                    }
                )));
                return;
            }
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            (self.log)(
                Level::Error,
                "api_server_exited",
                format!(
                    "status={code} served_seconds={} cause={}",
                    started.elapsed().as_secs(),
                    if last.is_empty() { "none" } else { &last }
                ),
            );
            if started.elapsed() >= STABLE_AFTER {
                backoff = Duration::from_secs(1);
            }
            if self.pause(backoff) {
                return;
            }
            attempt += 1;
            (self.log)(
                Level::Warn,
                "api_server_restarted",
                format!("attempt={attempt} after_seconds={}", backoff.as_secs()),
            );
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// Sleep `total` in slices; `true` when told to stop.
    fn pause(&self, total: Duration) -> bool {
        let deadline = Instant::now() + total;
        while Instant::now() < deadline {
            if self.stop.load(Ordering::SeqCst) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        self.stop.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn script(label: &str, body: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sdsyncd-api-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("server");
        std::fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        (exe, dir)
    }

    fn quiet() -> (KeeperLog, Arc<Mutex<Vec<String>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let log: KeeperLog = Arc::new(move |_, event, detail| {
            sink.lock().unwrap().push(format!("{event} {detail}"));
        });
        (log, events)
    }

    /// A server that dies before it binds fails the start with its own words.
    #[test]
    fn a_server_that_dies_before_binding_fails_the_start_by_name() {
        let (exe, dir) = script("dies", "echo 'bind refused: address in use' >&2; exit 3");
        let (log, _) = quiet();
        let error = match ApiKeeper::start(
            exe,
            dir.join("api.sock"),
            Vec::new(),
            log,
            Duration::from_secs(5),
        ) {
            Ok(_) => panic!("started"),
            Err(error) => error,
        };
        assert!(
            error.contains("status 3") && error.contains("bind refused"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server that never binds fails the start at the deadline.
    #[test]
    fn a_server_that_never_binds_fails_at_the_deadline() {
        let (exe, dir) = script("silent", "exec sleep 30");
        let (log, _) = quiet();
        let started = Instant::now();
        let error = ApiKeeper::start(
            exe,
            dir.join("api.sock"),
            Vec::new(),
            log,
            Duration::from_millis(500),
        )
        .err()
        .unwrap();
        assert!(error.contains("did not accept"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A served socket makes the start succeed; a crash is named and the
    /// server restarted; stop ends it within the grace.
    #[test]
    fn a_crashed_server_is_named_and_restarted() {
        let (_, dir) = script("serve", "");
        let socket = dir.join("api.sock");
        // The test owns the socket; the "server" exits after a moment.
        let listener = UnixListener::bind(&socket).unwrap();
        let (exe, exe_dir) = script("serve-exe", "sleep 0.3; echo 'worker panicked' >&2; exit 9");
        let (log, events) = quiet();
        let keeper =
            ApiKeeper::start(exe, socket, Vec::new(), log, Duration::from_secs(5)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = events.lock().unwrap().clone();
            if seen.iter().any(|e| {
                e.starts_with("api_server_exited status=9") && e.contains("worker panicked")
            }) && seen
                .iter()
                .any(|e| e.starts_with("api_server_restarted attempt=1"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "{seen:?}");
            thread::sleep(Duration::from_millis(50));
        }
        let stopping = Instant::now();
        keeper.stop(Duration::from_secs(2));
        assert!(stopping.elapsed() < Duration::from_secs(5));
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&exe_dir);
    }
}
