//! The dashboard's API server under the daemon engine (`--serve-api`).
//!
//! The bridge's server (`sdsync-dsm-api --serve`: `api.sock`, its bounded
//! worker pool and every request handler) clears the environment and keeps
//! process-wide state that assumes it is alone, so it runs as its own child
//! process, not as a thread of the daemon. The daemon keeps it alive:
//! - start: the daemon is not ready (no `ctl.sock`, no `daemon.ready`) until
//!   the server has activated `api.sock` (`0000` -> `0600`, which it does only
//!   once its workers run), within a deadline; otherwise the start fails with
//!   the child's own last words. Nothing connects to probe it, so no probe is
//!   ever recorded as a failed request (H10);
//! - before each spawn, a socket left by a server that is gone is removed,
//!   named: the daemon holds `daemon.lock` and has reaped its own server, so a
//!   `0000` socket (killed between bind and activation, F8) or a refused `0600`
//!   one is stale unless `api.pid` names a live shell-engine server;
//! - a child that exits is named with its status and last stderr line and
//!   restarted after a backoff (1 s doubling to 30 s, reset after 10 minutes
//!   of service), each restart named;
//! - stop: TERM, then KILL after the grace; the child also dies with the
//!   daemon (parent-death signal), so it never outlives it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
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
        api_pid: PathBuf,
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
            api_pid,
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
                    "api.sock was not activated within {} s",
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
    /// The shell engine's `api.pid`: a live server there owns `api.sock`.
    api_pid: PathBuf,
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
            // The previous server (if any) is reaped: what it left is stale.
            self.clear_stale_socket();
            let before = socket_identity(&self.socket);
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
                    Err(error) => {
                        // S3-5: never respawn over a child that may live on.
                        self.put_down(&mut child, &error);
                        break None;
                    }
                }
                if ready.is_some()
                    && activated_since(&self.socket, before)
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

    /// A child that cannot be waited for: KILL it and reap it within a bound,
    /// each step named, so a respawn never runs beside it unawares.
    fn put_down(&self, child: &mut Child, error: &io::Error) {
        let pid = child.id();
        let killed = sys::kill_process(pid, libc::SIGKILL);
        let deadline = Instant::now() + Duration::from_secs(5);
        let reaped = loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => break false,
            }
        };
        (self.log)(
            Level::Error,
            "api_server_unwaitable",
            format!(
                "pid={pid} cause={error} killed={} reaped={reaped}",
                killed
                    .as_ref()
                    .map_or_else(|e| e.to_string(), |()| "yes".into())
            ),
        );
    }

    /// Remove `api.sock` when it is provably stale (F8): this daemon holds
    /// `daemon.lock`, its own previous server is reaped, and `api.pid` names no
    /// live shell-engine server. A socket owned by this uid with one link, at
    /// `0000` (a server killed between bind and activation) or at `0600` with
    /// nobody accepting, is removed and named; anything else is left and named.
    fn clear_stale_socket(&self) {
        let path = &self.socket;
        let Ok(before) = fs::symlink_metadata(path) else {
            return;
        };
        let mode = before.mode() & 0o7777;
        let named = |level: Level, event: &'static str, cause: String| {
            (self.log)(
                level,
                event,
                format!("path={} mode={mode:04o} cause={cause}", path.display()),
            );
        };
        if !before.file_type().is_socket()
            || before.uid() != sys::geteuid()
            || before.nlink() != 1
            || !matches!(mode, 0o000 | 0o600)
        {
            named(
                Level::Error,
                "api_socket_stale_kept",
                "not a socket of this package (type, owner, links or mode)".into(),
            );
            return;
        }
        if let Some(pid) = live_shell_server(&self.api_pid) {
            named(
                Level::Error,
                "api_socket_stale_kept",
                format!(
                    "{} names live PID {pid}, a shell-engine API server",
                    self.api_pid.display()
                ),
            );
            return;
        }
        if mode == 0o600 {
            match UnixStream::connect(path) {
                Ok(_) => {
                    named(
                        Level::Error,
                        "api_socket_stale_kept",
                        "a server accepts on it".into(),
                    );
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                Err(error) => {
                    named(Level::Error, "api_socket_stale_kept", error.to_string());
                    return;
                }
            }
        }
        // The same inode still: nothing replaced it while it was judged.
        let same = fs::symlink_metadata(path)
            .is_ok_and(|after| after.dev() == before.dev() && after.ino() == before.ino());
        if !same {
            return;
        }
        match fs::remove_file(path) {
            Ok(()) => named(
                Level::Warn,
                "api_socket_stale_removed",
                if mode == 0 {
                    "a prepared socket whose server is gone".into()
                } else {
                    "a socket nobody accepts on".into()
                },
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => named(Level::Error, "api_socket_stale_kept", error.to_string()),
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

/// The socket's device and inode, if there is one.
fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_socket())
        .map(|m| (m.dev(), m.ino()))
}

/// Whether `path` is an activated (`0600`) socket other than `before`: the
/// server activates its listener only once its workers run.
fn activated_since(path: &Path, before: Option<(u64, u64)>) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| {
        m.file_type().is_socket()
            && m.mode() & 0o7777 == 0o600
            && Some((m.dev(), m.ino())) != before
    })
}

/// The PID in the shell engine's `api.pid` when it names a live process.
fn live_shell_server(api_pid: &Path) -> Option<u32> {
    let text = fs::read_to_string(api_pid).ok()?;
    let pid: u32 = text
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
        .filter(|p| *p > 1)?;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let state = stat.rsplit_once(") ")?.1.split_whitespace().next()?;
    (!matches!(state, "Z" | "X" | "x")).then_some(pid)
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

    /// Bind `socket` the way the server does once its workers run: `0600`.
    fn activate(socket: &Path) -> UnixListener {
        let listener = UnixListener::bind(socket).unwrap();
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600)).unwrap();
        listener
    }

    /// A server that dies before it binds fails the start with its own words.
    #[test]
    fn a_server_that_dies_before_binding_fails_the_start_by_name() {
        let (exe, dir) = script("dies", "echo 'bind refused: address in use' >&2; exit 3");
        let (log, _) = quiet();
        let error = match ApiKeeper::start(
            exe,
            dir.join("api.sock"),
            dir.join("api.pid"),
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
            dir.join("api.pid"),
            Vec::new(),
            log,
            Duration::from_millis(500),
        )
        .err()
        .unwrap();
        assert!(error.contains("not activated"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An activated socket makes the start succeed, and nothing connects to
    /// find out (H10: a probe's connect-and-drop was recorded by the server as
    /// a failed request); a crash is named and the server restarted; stop ends
    /// it within the grace.
    #[test]
    fn a_crashed_server_is_named_and_restarted_and_readiness_never_connects() {
        let (_, dir) = script("serve", "");
        let socket = dir.join("api.sock");
        let (exe, exe_dir) = script("serve-exe", "sleep 0.3; echo 'worker panicked' >&2; exit 9");
        let (log, events) = quiet();
        // The "server" activates the socket a moment after it starts; what
        // connects to it before the start returns is the readiness probe.
        let started = Arc::new(AtomicBool::new(false));
        let binder = {
            let socket = socket.clone();
            let started = Arc::clone(&started);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                let listener = activate(&socket);
                listener.set_nonblocking(true).unwrap();
                let mut accepted = 0;
                let until = Instant::now() + Duration::from_secs(5);
                while !started.load(Ordering::SeqCst) && Instant::now() < until {
                    if listener.accept().is_ok() {
                        accepted += 1;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                // Anything the probe sent is still queued: drain it too.
                while listener.accept().is_ok() {
                    accepted += 1;
                }
                (listener, accepted)
            })
        };
        let keeper = ApiKeeper::start(
            exe,
            socket.clone(),
            dir.join("api.pid"),
            Vec::new(),
            log,
            Duration::from_secs(5),
        )
        .unwrap();
        started.store(true, Ordering::SeqCst);
        let (listener, accepted) = binder.join().unwrap();
        assert_eq!(accepted, 0, "readiness connected to the server");
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

    fn keeper_at(dir: &Path, log: KeeperLog) -> Keeper {
        Keeper {
            exe: dir.join("server"),
            socket: dir.join("api.sock"),
            api_pid: dir.join("api.pid"),
            env: Vec::new(),
            log,
            stop: Arc::new(AtomicBool::new(false)),
            child: Arc::new(AtomicU32::new(0)),
        }
    }

    /// F8: a server killed between bind and activation leaves `api.sock` at
    /// `0000`, which every later server refused as unsafe, forever. The keeper
    /// (holding daemon.lock, its own server reaped) removes it, named; a socket
    /// someone may still own is kept, named.
    #[test]
    fn a_prepared_socket_left_by_a_dead_server_is_removed_by_name() {
        let (_, dir) = script("stale", "");
        let (log, events) = quiet();
        let keeper = keeper_at(&dir, log);
        let socket = dir.join("api.sock");
        // 0000, nobody behind it.
        drop(UnixListener::bind(&socket).unwrap());
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o000)).unwrap();
        keeper.clear_stale_socket();
        assert!(!socket.exists(), "{:?}", events.lock().unwrap());
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("api_socket_stale_removed") && e.contains("mode=0000")),
            "{:?}",
            events.lock().unwrap()
        );
        // 0600 and refused: stale too.
        drop(activate(&socket));
        keeper.clear_stale_socket();
        assert!(!socket.exists());
        // 0000 while api.pid names a live shell-engine server: kept, named.
        drop(UnixListener::bind(&socket).unwrap());
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o000)).unwrap();
        fs::write(dir.join("api.pid"), format!("{}\n", std::process::id())).unwrap();
        keeper.clear_stale_socket();
        assert!(fs::symlink_metadata(&socket).is_ok());
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("api_socket_stale_kept") && e.contains("shell-engine")),
            "{:?}",
            events.lock().unwrap()
        );
        fs::remove_file(dir.join("api.pid")).unwrap();
        fs::remove_file(&socket).unwrap();
        // 0600 with a server accepting: kept.
        let live = activate(&socket);
        keeper.clear_stale_socket();
        assert!(socket.exists());
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
