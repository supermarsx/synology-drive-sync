//! Process lifecycle (design §2.8, §2.6, R5): identity and layout checked once,
//! a `flock` singleton, signals as messages, a ready file, a bounded stop, and
//! an optional respawning parent. Linux only.
//!
//! Entry points (flags of `sdsync-dsm-api`):
//! - `--daemon`: run the daemon in the foreground.
//! - `--launch-daemon`: start a detached supervisor (`--supervise`) and return
//!   once `ctl ping` answers (≤ 30 s), for `start-stop-status start`.
//! - `--supervise`: run `--daemon` as a child and restart it after a crash, at
//!   most 5 times in 10 minutes, each restart named (`service.restarted`).
//! - `--ctl VERB [key=value…]`: one control request.
//!
//! Each accepts `--home DIR --var DIR --core PATH --manager PATH` so tests can
//! point it at a private tree; the defaults are the package paths.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

use super::control::{
    self, Launcher, Msg, Mutation, MutationOutcome, MutationRunner, Parts, Paths, RunControl,
    Settings,
};
use super::ctl;
use super::events::{ActivityRow, EventSink, FileSink, LogEvent};
use super::executor::{self, ExecEvent, RunSpec, StopCause};
use super::routines::Level;
use super::state::sanitize_detail;
use super::sys;

/// Paths a lifecycle command works on.
#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub(crate) home: PathBuf,
    pub(crate) var: PathBuf,
    pub(crate) core: PathBuf,
    pub(crate) manager: PathBuf,
    /// Watchdog and stop timings (`--stall-warn-seconds`, `--stall-kill-seconds`,
    /// `--cancel-grace-seconds`); the design defaults otherwise.
    pub(crate) timings: executor::Timings,
    /// `--serve-api`: keep the dashboard's API server (`--serve`, api.sock)
    /// running as a child; the daemon is not ready until it answers.
    pub(crate) serve_api: bool,
    /// `--ready-within-seconds`: how long `--launch-daemon` waits for the
    /// daemon to answer `ctl ping` (default 30 s).
    pub(crate) ready_within: Duration,
}

impl Options {
    /// Parse `--home/--var/--core/--manager`; anything else is refused.
    pub(crate) fn parse(arguments: &[OsString]) -> Result<(Self, Vec<String>), String> {
        let mut options = Self {
            home: PathBuf::from(super::super::PACKAGE_HOME),
            var: PathBuf::from(super::super::PACKAGE_VAR),
            core: PathBuf::from(super::super::BINARY_PATH),
            manager: PathBuf::from(super::super::MANAGER_PATH),
            timings: executor::Timings::default(),
            serve_api: false,
            ready_within: Duration::from_secs(30),
        };
        let mut rest = Vec::new();
        let mut items = arguments.iter();
        while let Some(item) = items.next() {
            let text = item.to_str().ok_or("arguments must be UTF-8")?;
            let seconds = |value: Option<&OsString>| -> Result<Duration, String> {
                value
                    .and_then(|v| v.to_str())
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|v| (1..=86_400).contains(v))
                    .map(Duration::from_secs)
                    .ok_or_else(|| format!("{text} needs a number of seconds (1..86400)"))
            };
            match text {
                "--stall-warn-seconds" => {
                    options.timings.stall_warn = seconds(items.next())?;
                    continue;
                }
                "--stall-kill-seconds" => {
                    options.timings.stall_kill = seconds(items.next())?;
                    continue;
                }
                "--cancel-grace-seconds" => {
                    options.timings.cancel_grace =
                        seconds(items.next())?.min(Duration::from_secs(110));
                    continue;
                }
                "--serve-api" => {
                    options.serve_api = true;
                    continue;
                }
                "--ready-within-seconds" => {
                    options.ready_within = seconds(items.next())?.min(Duration::from_secs(300));
                    continue;
                }
                _ => {}
            }
            let slot = match text {
                "--home" => &mut options.home,
                "--var" => &mut options.var,
                "--core" => &mut options.core,
                "--manager" => &mut options.manager,
                other => {
                    rest.push(other.to_owned());
                    continue;
                }
            };
            *slot = PathBuf::from(items.next().ok_or_else(|| format!("{text} needs a path"))?);
        }
        if options.timings.stall_kill <= options.timings.stall_warn {
            return Err("--stall-kill-seconds must exceed --stall-warn-seconds".into());
        }
        Ok((options, rest))
    }

    fn forward(&self) -> Vec<OsString> {
        let t = &self.timings;
        let mut forwarded: Vec<OsString> = if self.serve_api {
            vec!["--serve-api".into()]
        } else {
            Vec::new()
        };
        forwarded.extend([
            "--stall-warn-seconds".into(),
            t.stall_warn.as_secs().to_string().into(),
            "--stall-kill-seconds".into(),
            t.stall_kill.as_secs().to_string().into(),
            "--cancel-grace-seconds".into(),
            t.cancel_grace.as_secs().to_string().into(),
            "--home".into(),
            self.home.clone().into(),
            "--var".into(),
            self.var.clone().into(),
            "--core".into(),
            self.core.clone().into(),
            "--manager".into(),
            self.manager.clone().into(),
        ]);
        forwarded
    }

    fn socket(&self) -> PathBuf {
        ctl::socket_path(&self.var)
    }
}

fn say(message: &str) {
    eprintln!("sdsyncd: {message}");
}

fn epoch() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

// ---------------------------------------------------------------- identity and layout

/// I-8: never root, and the uid that owns `home`.
pub(crate) fn check_identity(home: &Path) -> Result<u32, String> {
    let uid = sys::geteuid();
    if uid == 0 {
        return Err("refusing to run as root".into());
    }
    resolve_root(home, uid)?;
    Ok(uid)
}

/// A package root (`home`, `var`), judged by what it points at.
///
/// On DSM `/var/packages/<pkg>/home` and `/var` are framework symlinks to
/// `@apphome/<pkg>` and `@appdata/<pkg>`, and every script and profile names
/// the alias, so the root is followed, one link only. The target must be a
/// real directory owned by this uid that no group or other user may write.
/// Below the roots nothing may be a link (`ensure_layout`). Every refusal
/// names the alias, the target and the cause. Returns the resolved directory.
pub(crate) fn resolve_root(root: &Path, uid: u32) -> Result<PathBuf, String> {
    let link =
        fs::symlink_metadata(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let resolved = if link.file_type().is_symlink() {
        let target = fs::read_link(root)
            .map_err(|error| format!("{}: the link cannot be read: {error}", root.display()))?;
        let target = if target.is_absolute() {
            target
        } else {
            root.parent().unwrap_or(Path::new("/")).join(target)
        };
        let target_link = fs::symlink_metadata(&target).map_err(|error| {
            format!(
                "{} -> {}: the target is unavailable: {error}",
                root.display(),
                target.display()
            )
        })?;
        if target_link.file_type().is_symlink() {
            return Err(format!(
                "{} -> {}: the target is itself a link (one level only)",
                root.display(),
                target.display()
            ));
        }
        target
    } else {
        root.to_path_buf()
    };
    let metadata = fs::symlink_metadata(&resolved)
        .map_err(|error| format!("{}: {error}", resolved.display()))?;
    let named = || {
        if resolved == root {
            root.display().to_string()
        } else {
            format!("{} -> {}", root.display(), resolved.display())
        }
    };
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", named()));
    }
    if metadata.uid() != uid {
        return Err(format!(
            "{} is owned by uid {}, not by this process (uid {uid})",
            named(),
            metadata.uid()
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is writable by group or others (mode {:o})",
            named(),
            metadata.mode() & 0o7777
        ));
    }
    Ok(resolved)
}

/// `ensure_layout`: the daemon's private directories exist, are directories
/// (never symlinks), are ours and are `0700`.
pub(crate) fn ensure_layout(var: &Path, uid: u32) -> Result<(), String> {
    for relative in [
        "",
        "run",
        "state",
        "state/routines",
        "state/profiles",
        "log",
    ] {
        let dir = if relative.is_empty() {
            var.to_path_buf()
        } else {
            var.join(relative)
        };
        // The var root may be DSM's alias symlink (to `@appdata/<pkg>`): its
        // target is judged, and left at the mode DSM gave it.
        let root = relative.is_empty();
        if root {
            match resolve_root(&dir, uid) {
                Ok(_) => continue,
                Err(cause) if fs::symlink_metadata(&dir).is_ok() => return Err(cause),
                Err(_) => {}
            }
        }
        match fs::symlink_metadata(&dir) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(format!("{} is not a directory", dir.display()));
                }
                if metadata.uid() != uid {
                    return Err(format!(
                        "{} is owned by uid {}",
                        dir.display(),
                        metadata.uid()
                    ));
                }
                if !root && metadata.mode() & 0o077 != 0 {
                    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                        .map_err(|error| format!("{}: {error}", dir.display()))?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&dir)
                    .map_err(|error| format!("{}: {error}", dir.display()))?;
            }
            Err(error) => return Err(format!("{}: {error}", dir.display())),
        }
    }
    Ok(())
}

/// The singleton: `flock` on `var/run/daemon.lock`, released by the kernel when
/// the process dies, so there is no stale-lock recovery. `Ok(None)` when another
/// daemon holds it.
pub(crate) fn acquire_singleton(var: &Path) -> Result<Option<File>, String> {
    acquire_lock_file(var, "daemon.lock")
}

/// `flock(LOCK_EX|LOCK_NB)` on `var/run/<name>` (created `0600`, never through
/// a symlink). `Ok(None)` when another process holds it.
fn acquire_lock_file(var: &Path, name: &str) -> Result<Option<File>, String> {
    let path = var.join("run").join(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    match sys::try_lock_exclusive(&file) {
        Ok(true) => Ok(Some(file)),
        Ok(false) => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// How long a starting daemon rides out a held `daemon.lock` before it calls
/// itself a second daemon (F4): the shell's `flock -n daemon.lock true` probe
/// holds the lock for the life of one `true`.
pub(crate) const SINGLETON_WAIT: Duration = Duration::from_secs(2);

/// [`acquire_singleton`], retried every 100 ms for at most `wait`.
pub(crate) fn acquire_singleton_within(var: &Path, wait: Duration) -> Result<Option<File>, String> {
    let deadline = Instant::now() + wait;
    loop {
        match acquire_singleton(var)? {
            Some(lock) => return Ok(Some(lock)),
            None if Instant::now() >= deadline => return Ok(None),
            None => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// The identity triple of process `pid` (pid, `/proc` start tick, boot id),
/// one per line: the format of `daemon.pid`, `daemon.ready`, `supervisor.pid`
/// and `run.lock/pid` (scratchpad fix5 contract §0).
pub(crate) fn identity_record(pid: u32) -> Result<String, String> {
    let start = super::super::linux_files::process_start(pid)
        .map_err(|_| format!("the /proc start tick of PID {pid} cannot be read"))?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("/proc/sys/kernel/random/boot_id: {error}"))?;
    Ok(format!(
        "{pid}
{start}
{}
",
        boot.trim()
    ))
}

/// Remove `path` only while it still holds exactly `record` (our own).
fn remove_own_record(path: &Path, record: &str) -> Result<(), String> {
    match fs::read_to_string(path) {
        Ok(text) if text == record => {
            fs::remove_file(path).map_err(|error| format!("{}: {error}", path.display()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Write one controller.log event; when the log itself fails, stderr (which
/// the supervisor keeps in `daemon-stderr.log`) gets the event and why (S3-1).
pub(crate) fn log_or_stderr(
    sink: &dyn EventSink,
    level: Level,
    event: &'static str,
    detail: String,
) {
    let record = LogEvent {
        level,
        category: "controller",
        event,
        detail,
    };
    if let Err(cause) = sink.log(epoch().unwrap_or(0), &record) {
        say(&format!(
            "controller log event {event} could not be written ({cause}): level={} {}",
            level.as_str(),
            record.detail
        ));
    }
}

/// An Activity row; when it cannot be written, stderr says so.
fn activity_or_stderr(sink: &dyn EventSink, row: &ActivityRow) {
    if let Err(cause) = sink.activity(epoch().unwrap_or(0), row) {
        say(&format!(
            "activity row {} could not be written ({cause}): {}",
            row.code, row.message
        ));
    }
}

// ---------------------------------------------------------------- production seams

struct ProcessLauncher;

struct ProcessRun(executor::RunHandle);

impl RunControl for ProcessRun {
    fn stop(&self, cause: StopCause) {
        self.0.stop(cause);
    }
}

impl Launcher for ProcessLauncher {
    fn launch(
        &self,
        spec: RunSpec,
        sink: Box<dyn Fn(ExecEvent) + Send>,
    ) -> io::Result<Box<dyn RunControl>> {
        let handle = executor::start_run(spec, sink)?;
        Ok(Box::new(ProcessRun(handle)))
    }
}

/// Runs a mutation through the shell manager (`sdsync-dsm OP ARGS…`) in its own
/// process group, bounded by the deadline (design §2.5: `consumer_timeout`).
struct ManagerMutations {
    manager: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl MutationRunner for ManagerMutations {
    fn run(&self, mutation: &Mutation, deadline: Duration) -> MutationOutcome {
        let mut command = Command::new(&self.manager);
        command
            .arg(&mutation.op)
            .args(&mutation.args)
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
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return MutationOutcome {
                    exit: 69,
                    detail: format!("{}: {error}", self.manager.display()),
                };
            }
        };
        let pid = child.id();
        // H3 residue: the manager's stderr is read on a thread of its own and
        // collected within a bound, so a grandchild that keeps the pipe open
        // cannot hold this mutation after the manager has exited.
        let stderr = sys::Tail::start(child.stderr.take(), 64 * 1024);
        let started = Instant::now();
        let mut killed = None;
        // How the wait ended when it was not an exit: named in the outcome.
        let mut unwaited: Option<String> = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(error) => {
                    unwaited = Some(format!(
                        "{} (pid {pid}) cannot be waited for: {error}",
                        mutation.op
                    ));
                    break None;
                }
            }
            let elapsed = started.elapsed();
            match killed {
                None if elapsed >= deadline => {
                    signal_or_name(pid, libc::SIGTERM, &mutation.op);
                    killed = Some(Instant::now());
                }
                Some(at) if at.elapsed() >= MUTATION_KILL_AFTER => {
                    signal_or_name(pid, libc::SIGKILL, &mutation.op);
                    // H3 residue: the reap is bounded. A manager that left its
                    // group missed the group KILL; it is our unreaped child, so
                    // its pid cannot have been reused, and it is KILLed by pid.
                    if let Err(error) = sys::kill_process(pid, libc::SIGKILL)
                        && error.raw_os_error() != Some(libc::ESRCH)
                    {
                        say(&format!(
                            "mutation {} (pid {pid}): KILL by pid failed: {error}",
                            mutation.op
                        ));
                    }
                    if sys::reap_within(&mut child, MUTATION_REAP_BOUND) {
                        break None;
                    }
                    let handed = sys::abandon_to_reaper(child);
                    unwaited = Some(format!(
                        "{} (pid {pid}) was still running {} s after KILL (handed={handed})",
                        mutation.op,
                        MUTATION_REAP_BOUND.as_secs()
                    ));
                    break None;
                }
                _ => {}
            }
            thread::sleep(Duration::from_millis(25));
        };
        let last = stderr
            .lines(1, Duration::from_millis(500))
            .pop()
            .unwrap_or_default();
        if killed.is_some() {
            return MutationOutcome {
                exit: 124,
                detail: format!(
                    "consumer_timeout: {} did not finish within {} s and was stopped; its outcome is unknown{}",
                    mutation.op,
                    deadline.as_secs(),
                    unwaited
                        .map(|cause| format!(" ({cause})"))
                        .unwrap_or_default()
                ),
            };
        }
        if let Some(cause) = unwaited {
            // Started and not seen to end: its outcome is unknown, never a guess.
            return MutationOutcome {
                exit: 70,
                detail: format!(
                    "consumer_unwaitable: {cause}; its outcome is unknown{}",
                    if last.is_empty() {
                        String::new()
                    } else {
                        format!(" (it said: {last})")
                    }
                ),
            };
        }
        let exit = status
            .map(|s| {
                s.code()
                    .or_else(|| s.signal().map(|n| 128 + n))
                    .unwrap_or(-1)
            })
            .unwrap_or(-1);
        MutationOutcome { exit, detail: last }
    }
}

/// TERM to KILL for a mutation past its deadline.
const MUTATION_KILL_AFTER: Duration = Duration::from_secs(3);
/// After KILL, how long a mutation may take to be reaped before it is handed
/// to a reaper and named.
const MUTATION_REAP_BOUND: Duration = Duration::from_secs(5);

/// Signal a mutation's process group; a failure other than "already gone" is
/// said on stderr (the supervisor keeps it), never dropped.
fn signal_or_name(pid: u32, signal: libc::c_int, op: &str) {
    if let Err(error) = sys::kill_group(pid, signal)
        && error.raw_os_error() != Some(libc::ESRCH)
    {
        say(&format!(
            "mutation {op} (pid {pid}): signal {signal} to its group failed: {error}"
        ));
    }
}

// ---------------------------------------------------------------- --daemon

/// Remove one of the daemon's records (`ctl.sock`, `daemon.ready`,
/// `daemon.pid`). A directory there is not ours and is left. `Ok(true)` when
/// something was removed.
fn remove_record(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Err(format!(
            "{}: a directory, not a record of the daemon; left in place",
            path.display()
        )),
        Ok(_) => match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("{}: {error}", path.display())),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Remove the daemon's records, saying (stderr) what could not be removed.
/// Returns the paths removed and the failures.
fn remove_records(paths: &[&Path]) -> (Vec<String>, Vec<String>) {
    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for path in paths {
        match remove_record(path) {
            Ok(true) => removed.push(path.display().to_string()),
            Ok(false) => {}
            Err(cause) => {
                say(&format!("cannot remove a daemon record: {cause}"));
                failed.push(cause);
            }
        }
    }
    (removed, failed)
}

/// The daemon's records under `run/`, in the order a clean stop removes them.
fn daemon_records(var: &Path) -> [PathBuf; 3] {
    let run = var.join("run");
    [
        ctl::socket_path(var),
        run.join("daemon.ready"),
        run.join("daemon.pid"),
    ]
}

/// Wait at most `within` for `handle`; a thread that does not end is named
/// (stderr) and abandoned: the process is exiting.
fn join_within(handle: thread::JoinHandle<()>, name: &str, within: Duration) {
    let deadline = Instant::now() + within;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        if handle.join().is_err() {
            say(&format!("the {name} thread panicked"));
        }
    } else {
        say(&format!(
            "the {name} thread did not end within {} s; abandoned",
            within.as_secs()
        ));
    }
}

/// `--daemon`: run until stopped. Exit 0 on a clean stop; 75 when another
/// daemon holds the lock; 78 when identity or layout is refused; 1 otherwise.
pub(crate) fn run_daemon(options: &Options) -> ExitCode {
    // Signals are blocked before any thread exists, so every thread inherits
    // the mask and only the sigwait thread sees them.
    if let Err(error) = sys::block_daemon_signals() {
        say(&format!("cannot block signals: {error}"));
        return ExitCode::from(1);
    }
    sys::set_umask(0o077);
    let uid = match check_identity(&options.home) {
        Ok(uid) => uid,
        Err(cause) => {
            say(&format!("refused: {cause}"));
            return ExitCode::from(78);
        }
    };
    if let Err(cause) = ensure_layout(&options.var, uid) {
        say(&format!("refused: {cause}"));
        return ExitCode::from(78);
    }
    let _lock = match acquire_singleton_within(&options.var, SINGLETON_WAIT) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            say(&format!(
                "already running (daemon.lock is held after {} s)",
                SINGLETON_WAIT.as_secs()
            ));
            return ExitCode::from(75);
        }
        Err(cause) => {
            say(&format!("cannot take the daemon lock: {cause}"));
            return ExitCode::from(1);
        }
    };
    let run_dir = options.var.join("run");
    let pid = std::process::id();
    let pid_file = run_dir.join("daemon.pid");
    let ready_file = run_dir.join("daemon.ready");
    let socket = options.socket();
    let records = [socket.as_path(), ready_file.as_path(), pid_file.as_path()];
    let log_root = options.var.join("log");
    let sink = FileSink::new(&log_root);
    // A start that fails once this process holds daemon.lock: named in
    // controller.log and Activity, then every record of this daemon is removed
    // (S1-c2), so nothing names a daemon that is not there.
    let start_failed = |step: &str, path: &Path, cause: &str| {
        say(&format!(
            "the daemon did not start: step={step} path={} cause={cause}",
            path.display()
        ));
        log_or_stderr(
            &sink,
            Level::Error,
            "service_start_failed",
            format!(
                "step={step} path={} cause={}",
                path.display(),
                sanitize_detail(cause, 240)
            ),
        );
        activity_or_stderr(
            &sink,
            &ActivityRow {
                code: "service.start_failed",
                profile: "none".into(),
                state: "failed",
                category: "controller",
                level: Level::Error,
                message: format!("The sync service did not start ({step}: {cause})"),
            },
        );
    };
    // This process holds daemon.lock: any readiness record is a dead daemon's.
    if let Err(cause) = remove_record(&ready_file) {
        start_failed("daemon.ready", &ready_file, &cause);
        remove_records(&records);
        return ExitCode::from(1);
    }
    // daemon.pid carries the whole identity (S1-c2): a reader never mistakes
    // a reused PID for this daemon.
    let published = identity_record(pid).and_then(|identity| {
        super::state::atomic_write(&pid_file, identity.as_bytes()).map_err(|error| error.cause)
    });
    if let Err(cause) = published {
        start_failed("daemon.pid", &pid_file, &cause);
        remove_records(&records);
        return ExitCode::from(1);
    }
    let zone = sys::zone_name(epoch().unwrap_or(0) as i64).unwrap_or_else(|| "unknown".into());
    log_or_stderr(
        &sink,
        Level::Info,
        "service_clock",
        format!("zone={zone} tz={}", std::env::var("TZ").unwrap_or_default()),
    );
    // Which engine the package is set to, said once at start: the daemon runs
    // whatever the file says (it was started), but a disagreement is named.
    let engine_path = options.var.join("state/engine");
    let (level, detail) = match fs::read(&engine_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (Level::Info, "engine=daemon file=absent".to_owned())
        }
        Err(error) => (
            Level::Warn,
            format!(
                "engine=daemon file=unreadable path={} cause={error}",
                engine_path.display()
            ),
        ),
        Ok(bytes) => match super::engine_word(&bytes) {
            Ok("shell") => (
                Level::Warn,
                "engine=daemon file=shell cause=the daemon was started although the package is set to the shell engine".to_owned(),
            ),
            Ok(word) => (Level::Info, format!("engine=daemon file={word}")),
            Err(cause) => (
                Level::Warn,
                format!("engine=daemon file=invalid path={} cause={cause}", engine_path.display()),
            ),
        },
    };
    log_or_stderr(&sink, level, "service_engine", detail);
    // The roots as named and as they resolve (DSM's aliases), once.
    let resolved = |root: &Path| {
        resolve_root(root, uid)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|cause| format!("unresolved ({cause})"))
    };
    log_or_stderr(
        &sink,
        Level::Info,
        "service_layout",
        format!(
            "home={} resolved={} var={} resolved={}",
            options.home.display(),
            resolved(&options.home),
            options.var.display(),
            resolved(&options.var)
        ),
    );
    let child_env = executor::allowlisted_env(std::env::vars_os());
    let api_env = child_env.clone();
    let settings = Settings {
        child_env: child_env.clone(),
        timings: options.timings,
        // `$cache_root/pairs`: one `<profile>.redb` per profile; the core
        // creates the directory 0700 and claims it with its marker.
        pair_index_dir: Some(options.var.join("state/cache/pairs")),
        ..Settings::default()
    };
    let mutation_deadline = settings.mutation_deadline;
    // The bridge's audit outbox lives at the package paths only: a daemon
    // pointed at another tree (tests) runs unaudited, and says so.
    let audited = options.var == Path::new(super::super::PACKAGE_VAR);
    let audit: Arc<dyn super::audit::RunAudit> = if audited {
        Arc::new(super::audit::BridgeAudit {
            bridge: std::env::current_exe().unwrap_or_else(|_| {
                PathBuf::from(super::super::PACKAGE_ROOT).join("bin/sdsync-dsm-api")
            }),
            env: api_env.clone(),
            uid,
        })
    } else {
        Arc::new(super::audit::NoAudit)
    };
    log_or_stderr(
        &sink,
        if audited { Level::Info } else { Level::Warn },
        "service_audit",
        if audited {
            "runs=audited".to_owned()
        } else {
            format!(
                "runs=unaudited cause={} is not the package's var ({})",
                options.var.display(),
                super::super::PACKAGE_VAR
            )
        },
    );
    // Alerts go through the package's own sdsync-common (its alert policy,
    // ledgers and notifiers), which lives at the package paths only.
    let alert_sink = FileSink::new(&log_root);
    let alert_log: super::alerts::AlertLog = Arc::new(move |level, event, detail| {
        if let Err(cause) = alert_sink.log(
            epoch().unwrap_or(0),
            &LogEvent {
                level,
                category: "scheduler",
                event,
                detail: detail.clone(),
            },
        ) {
            say(&format!(
                "scheduler log event {event} could not be written ({cause}): level={} {detail}",
                level.as_str()
            ));
        }
    });
    let alerts: Arc<dyn super::alerts::Alerts> = if audited {
        Arc::new(super::alerts::ShellAlerts {
            common: PathBuf::from(super::super::PACKAGE_ROOT).join("libexec/sdsync-common"),
            env: api_env.clone(),
            log: alert_log,
        })
    } else {
        Arc::new(super::alerts::NoAlerts {
            cause: format!(
                "{} is not the package's var, whose alert policy and notifiers the alerts use",
                options.var.display()
            ),
            log: alert_log,
        })
    };
    let parts = Parts {
        paths: Paths::layout(&options.home, &options.var, options.core.clone()),
        settings,
        launcher: Box::new(ProcessLauncher),
        audit,
        alerts,
        mutations: Arc::new(ManagerMutations {
            manager: options.manager.clone(),
            env: child_env,
        }),
        // Native inotify per realtime routine, polling only as the fallback.
        changes: Some(Box::new(|sink| {
            Box::new(super::watch_source::WatchSource::new(
                sink,
                super::watch_source::WatchSettings::default(),
            )) as Box<dyn super::changes::ChangeSource>
        })),
        sink: Box::new(FileSink::new(&log_root)),
        clock: Box::new(sys::LibcLocalClock),
        wall: Box::new(epoch),
        pid,
    };
    let (tx, control) = match control::spawn(parts) {
        Ok(spawned) => spawned,
        Err(error) => {
            start_failed("control_thread", &pid_file, &error.to_string());
            remove_records(&records);
            return ExitCode::from(1);
        }
    };
    let signals = tx.clone();
    let signal_thread = thread::Builder::new()
        .name("sdsyncd-signals".into())
        .spawn(move || {
            loop {
                match sys::wait_signal() {
                    Ok(signal) => {
                        if signals.send(Msg::Signal(signal)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        say(&format!("sigwait failed: {error}"));
                        return;
                    }
                }
            }
        });
    // Every early exit below stops what was started, in reverse order, and
    // removes this daemon's records.
    let stop_control = |tx: &std::sync::mpsc::Sender<Msg>| {
        let reply = control::ask(tx, control::Request::Stop, Duration::from_secs(60));
        if reply["ok"] == false {
            say(&format!(
                "the control thread did not confirm the stop: {reply}"
            ));
        }
    };
    if let Err(error) = signal_thread {
        start_failed("signal_thread", &pid_file, &error.to_string());
        stop_control(&tx);
        join_within(control, "control", Duration::from_secs(10));
        remove_records(&records);
        return ExitCode::from(1);
    }
    // The dashboard's server first: a daemon whose dashboard cannot answer is
    // not ready, and `--launch-daemon` (waiting for ctl.sock) says why.
    let api = if options.serve_api {
        let api_sink = FileSink::new(&log_root);
        let log: super::api_server::KeeperLog = Arc::new(move |level, event, detail| {
            log_or_stderr(&api_sink, level, event, detail);
        });
        let started = std::env::current_exe()
            .map_err(|error| format!("cannot find this executable: {error}"))
            .and_then(|exe| {
                super::api_server::ApiKeeper::start(
                    exe,
                    PathBuf::from(super::super::API_SOCKET_PATH),
                    run_dir.join("api.pid"),
                    api_env.clone(),
                    log,
                    Duration::from_secs(15),
                )
            });
        match started {
            Ok(keeper) => Some(keeper),
            Err(cause) => {
                start_failed(
                    "api_server",
                    Path::new(super::super::API_SOCKET_PATH),
                    &cause,
                );
                stop_control(&tx);
                join_within(control, "control", Duration::from_secs(10));
                remove_records(&records);
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };
    // The dashboard's queue, beside any sync (lanes, deadlines): the shell
    // controller's consumer without its single slot. With the dashboard only.
    let queue = if api.is_some() {
        let queue_sink = FileSink::new(&log_root);
        let log: super::queue::QueueLog = Arc::new(move |level, event, detail| {
            log_or_stderr(&queue_sink, level, event, detail);
        });
        let activity_sink = FileSink::new(&log_root);
        let activity: super::queue::QueueActivity = Arc::new(move |code, level, message| {
            activity_or_stderr(
                &activity_sink,
                &ActivityRow {
                    code,
                    profile: "none".into(),
                    state: "failed",
                    category: "bridge",
                    level,
                    message,
                },
            );
        });
        // H2: the audit's health reaches controller.state through the control
        // thread, its only writer.
        let health_tx = tx.clone();
        let health: super::queue::QueueHealth = Arc::new(move |degraded| {
            if health_tx.send(Msg::AuditHealth(degraded)).is_err() {
                say(
                    "the control thread is gone; the audit's health cannot be published in controller.state",
                );
            }
        });
        match std::env::current_exe().and_then(|exe| {
            super::queue::QueueConsumer::start(
                exe,
                options.var.clone(),
                api_env.clone(),
                super::queue::QueueSettings::default(),
                log,
                activity,
                health,
            )
        }) {
            Ok(queue) => Some(queue),
            Err(error) => {
                start_failed("queue", &options.var.join("control"), &error.to_string());
                stop_control(&tx);
                join_within(control, "control", Duration::from_secs(10));
                if let Some(api) = api {
                    api.stop(Duration::from_secs(5));
                }
                remove_records(&records);
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };
    // `bounded`: a failed start waits a bounded time for the control thread;
    // a clean stop waits for it (it ends once stopped, within the grace).
    let stop_all = |control: thread::JoinHandle<()>,
                    queue: Option<super::queue::QueueConsumer>,
                    api: Option<super::api_server::ApiKeeper>,
                    bounded: bool| {
        if bounded {
            join_within(control, "control", Duration::from_secs(10));
        } else if control.join().is_err() {
            say("the control thread panicked");
        }
        if let Some(queue) = queue {
            queue.stop(Duration::from_secs(15));
        }
        if let Some(api) = api {
            api.stop(Duration::from_secs(5));
        }
    };
    let ctl_sink = FileSink::new(&log_root);
    let ctl_log: ctl::CtlLog = Arc::new(move |level, event, detail| {
        log_or_stderr(&ctl_sink, level, event, detail);
    });
    if let Err(error) = ctl::listen(&socket, uid, tx.clone(), mutation_deadline, ctl_log) {
        start_failed("ctl_socket", &socket, &error.to_string());
        stop_control(&tx);
        stop_all(control, queue, api, true);
        remove_records(&records);
        return ExitCode::from(1);
    }
    // Ready: the control thread runs and the socket answers. A daemon without
    // its readiness record is one no stop can prove, so a failed write is
    // fatal (S1-c2): the start fails by name and the records go.
    let published = identity_record(pid).and_then(|ready| {
        super::state::atomic_write(&ready_file, ready.as_bytes()).map_err(|error| error.cause)
    });
    if let Err(cause) = published {
        start_failed("daemon.ready", &ready_file, &cause);
        stop_control(&tx);
        stop_all(control, queue, api, true);
        remove_records(&records);
        return ExitCode::from(1);
    }
    drop(tx);
    stop_all(control, queue, api, false);
    // Clean stop: nothing an older pre-hook checks for is left behind (§3.4).
    let (_, failed) = remove_records(&records);
    if !failed.is_empty() {
        log_or_stderr(
            &sink,
            Level::Error,
            "service_records_kept",
            format!("cause={}", failed.join("; ")),
        );
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------- --ctl

/// `--ctl VERB [key=value…]`: print the reply; exit 0 when it says ok, 75 for a
/// busy refusal, 69 when the daemon cannot be reached, 1 otherwise.
pub(crate) fn run_ctl(options: &Options, rest: &[String]) -> ExitCode {
    let Some((verb, pairs)) = rest.split_first() else {
        say("--ctl needs a verb");
        return ExitCode::from(64);
    };
    let (request, deadline) = match ctl::request_from_args(verb, pairs) {
        Ok(built) => built,
        Err(cause) => {
            say(&cause);
            return ExitCode::from(64);
        }
    };
    match ctl::request(&options.socket(), &request, deadline) {
        Ok(reply) => {
            println!("{reply}");
            if reply["ok"] == true {
                ExitCode::SUCCESS
            } else if reply["exit"] == 75 || reply["code"] == "profile_running" {
                ExitCode::from(75)
            } else {
                ExitCode::from(1)
            }
        }
        Err(cause) => {
            println!(
                "{}",
                json!({"ok": false, "code": "daemon_unreachable", "detail": cause})
            );
            ExitCode::from(69)
        }
    }
}

// ---------------------------------------------------------------- --launch-daemon / --supervise

/// Where the supervisor keeps the daemon's (and its own) stderr.
fn stderr_log(var: &Path) -> PathBuf {
    var.join("log").join("daemon-stderr.log")
}

/// `daemon-stderr.log` is kept to this size: past it, it becomes `.1` (one
/// generation) before the next daemon starts.
const STDERR_LOG_LIMIT: u64 = 1024 * 1024;

/// Open `daemon-stderr.log` for appending, rotating it first when it is over
/// its limit. Every failure is returned by name.
fn open_stderr_log(var: &Path) -> Result<File, String> {
    let path = stderr_log(var);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!("{}: not a regular file", path.display()));
        }
        Ok(metadata) if metadata.len() > STDERR_LOG_LIMIT => {
            let old = var.join("log").join("daemon-stderr.log.1");
            fs::rename(&path, &old)
                .map_err(|error| format!("{} -> {}: {error}", path.display(), old.display()))?;
        }
        _ => {}
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// The length of `daemon-stderr.log` now: what was written after it is new.
fn stderr_mark(var: &Path) -> u64 {
    fs::metadata(stderr_log(var)).map_or(0, |m| m.len())
}

/// The last non-empty line written to `daemon-stderr.log` after `mark` (at
/// most its last 8 KiB), sanitized; empty when there is none.
fn stderr_since(var: &Path, mark: u64) -> String {
    use std::io::{Seek, SeekFrom};
    let Ok(mut file) = File::open(stderr_log(var)) else {
        return String::new();
    };
    let length = file.metadata().map_or(0, |m| m.len());
    // A rotation since the mark: the file is new, read it from its start.
    let from = if length < mark { 0 } else { mark };
    let from = from.max(length.saturating_sub(8 * 1024));
    if file.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.take(8 * 1024).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| sanitize_detail(line, 240))
        .unwrap_or_default()
}

/// `--launch-daemon`: start the supervisor detached (`setsid`) and wait for
/// `ctl ping` (`--ready-within-seconds`, 30 s by default). Exit 0 once it
/// answers; 1 with a named cause otherwise, after stopping the supervisor it
/// started (F3), with the daemon's own last words.
pub(crate) fn run_launch(options: &Options) -> ExitCode {
    if ctl::request(
        &options.socket(),
        &json!({"verb": "ping"}),
        Duration::from_secs(2),
    )
    .is_ok_and(|reply| reply["ok"] == true)
    {
        // Start is idempotent.
        return ExitCode::SUCCESS;
    }
    // The supervisor writes under var/run and var/log: the same identity and
    // layout checks as the daemon's, refused by name before anything starts.
    if let Err(cause) =
        check_identity(&options.home).and_then(|uid| ensure_layout(&options.var, uid))
    {
        say(&format!("refused: {cause}"));
        return ExitCode::from(1);
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            say(&format!("cannot find this executable: {error}"));
            return ExitCode::from(1);
        }
    };
    // The supervisor's own words (a refused start, a record it cannot
    // write) go where the daemon's do, so they can be relayed.
    let mark = stderr_mark(&options.var);
    let supervisor_stderr = match open_stderr_log(&options.var) {
        Ok(file) => Stdio::from(file),
        Err(cause) => {
            say(&format!(
                "warning: the supervisor's stderr is discarded: {cause}"
            ));
            Stdio::null()
        }
    };
    let mut command = Command::new(exe);
    command
        .arg("--supervise")
        .args(options.forward())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(supervisor_stderr);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            say(&format!("cannot start the supervisor: {error}"));
            return ExitCode::from(1);
        }
    };
    let last_words = |var: &Path| {
        let last = stderr_since(var, mark);
        if last.is_empty() {
            String::new()
        } else {
            format!("; the daemon said: {last}")
        }
    };
    let deadline = Instant::now() + options.ready_within;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                say(&format!(
                    "the supervisor exited before the daemon was ready ({status}){}",
                    last_words(&options.var)
                ));
                return ExitCode::from(1);
            }
            Ok(None) => {}
            Err(error) => {
                say(&format!("the supervisor cannot be waited for: {error}"));
                break;
            }
        }
        if ctl::request(
            &options.socket(),
            &json!({"verb": "ping"}),
            Duration::from_secs(1),
        )
        .is_ok_and(|reply| reply["ok"] == true)
        {
            return ExitCode::SUCCESS;
        }
        thread::sleep(Duration::from_millis(200));
    }
    // Not ready in time: the supervisor (and through it, its daemon) is
    // stopped, so a failed start leaves nothing running that start-stop-status
    // cannot see (F3). TERM first (it forwards to the daemon), KILL after 5 s
    // (the daemon dies with it: parent-death signal).
    let stopped = stop_child(&mut child, Duration::from_secs(5));
    say(&format!(
        "the daemon did not answer ctl ping within {} s; the supervisor (PID {}) was {}{}",
        options.ready_within.as_secs(),
        child.id(),
        stopped,
        last_words(&options.var)
    ));
    ExitCode::from(1)
}

/// TERM `child`, KILL it after `grace`, and reap it (bounded). Says how it
/// ended.
fn stop_child(child: &mut std::process::Child, grace: Duration) -> &'static str {
    let pid = child.id();
    let _ = sys::kill_process(pid, libc::SIGTERM);
    let reaped = |child: &mut std::process::Child, within: Duration| {
        let deadline = Instant::now() + within;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => return false,
            }
        }
    };
    if reaped(child, grace) {
        return "stopped";
    }
    let _ = sys::kill_process(pid, libc::SIGKILL);
    if reaped(child, Duration::from_secs(5)) {
        "killed"
    } else {
        "signalled but could not be reaped"
    }
}

/// The restart budget of the respawning parent (R5).
pub(crate) struct RestartBudget {
    window: Duration,
    limit: usize,
    restarts: VecDeque<Instant>,
}

impl RestartBudget {
    pub(crate) fn new(limit: usize, window: Duration) -> Self {
        Self {
            window,
            limit,
            restarts: VecDeque::new(),
        }
    }

    /// Record a crash at `now`; false when the budget is spent.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        while self
            .restarts
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            self.restarts.pop_front();
        }
        if self.restarts.len() >= self.limit {
            return false;
        }
        self.restarts.push_back(now);
        true
    }
}

/// The supervisor's end: its own record goes (only while it names this
/// process), and a failure to remove it is named.
fn supervisor_exit(sink: &FileSink, record_path: &Path, record: &str, code: u8) -> ExitCode {
    if let Err(cause) = remove_own_record(record_path, record) {
        log_or_stderr(
            sink,
            Level::Error,
            "service_records_kept",
            format!("path={} cause={cause}", record_path.display()),
        );
    }
    ExitCode::from(code)
}

/// Remove the records of a daemon that is gone (the supervisor gave up after
/// it died): only while `daemon.lock` is free, which proves no daemon runs,
/// and under that lock so none can start meanwhile (S1-c2).
fn remove_dead_daemon_records(sink: &FileSink, var: &Path) {
    match acquire_singleton(var) {
        Ok(Some(lock)) => {
            let paths = daemon_records(var);
            let (removed, failed) =
                remove_records(&paths.iter().map(PathBuf::as_path).collect::<Vec<_>>());
            drop(lock);
            if failed.is_empty() {
                log_or_stderr(
                    sink,
                    Level::Warn,
                    "service_records_removed",
                    format!(
                        "paths={} cause=the daemon is gone and was not restarted",
                        if removed.is_empty() {
                            "none".to_owned()
                        } else {
                            removed.join(",")
                        }
                    ),
                );
            } else {
                log_or_stderr(
                    sink,
                    Level::Error,
                    "service_records_kept",
                    format!("cause={}", failed.join("; ")),
                );
            }
        }
        Ok(None) => log_or_stderr(
            sink,
            Level::Error,
            "service_records_kept",
            format!(
                "path={} cause=daemon.lock is held, so a daemon still runs",
                var.join("run/daemon.lock").display()
            ),
        ),
        Err(cause) => log_or_stderr(
            sink,
            Level::Error,
            "service_records_kept",
            format!("cause=daemon.lock cannot be probed: {cause}"),
        ),
    }
}

/// `--supervise`: run `--daemon` and restart it after a crash (not after a
/// clean stop), at most 5 times in 10 minutes. TERM/INT are forwarded.
/// Publishes `supervisor.pid` (pid, start tick, boot id) for its life; when it
/// gives up it removes the dead daemon's records too.
pub(crate) fn run_supervise(options: &Options) -> ExitCode {
    if let Err(error) = sys::block_daemon_signals() {
        say(&format!("cannot block signals: {error}"));
        return ExitCode::from(1);
    }
    sys::set_umask(0o077);
    if let Err(cause) =
        check_identity(&options.home).and_then(|uid| ensure_layout(&options.var, uid))
    {
        say(&format!("refused: {cause}"));
        return ExitCode::from(78);
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            say(&format!("cannot find this executable: {error}"));
            return ExitCode::from(1);
        }
    };
    let sink = FileSink::new(&options.var.join("log"));
    // One supervisor per package: a second one (two starts at once) leaves,
    // named, before it writes anything.
    let _single = match acquire_lock_file(&options.var, "supervisor.lock") {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            log_or_stderr(
                &sink,
                Level::Info,
                "service_superseded",
                "cause=supervisor.lock is held by another supervisor; this one leaves".into(),
            );
            return ExitCode::SUCCESS;
        }
        Err(cause) => {
            say(&format!("cannot take the supervisor lock: {cause}"));
            return ExitCode::from(1);
        }
    };
    let record_path = options.var.join("run").join("supervisor.pid");
    let record = match identity_record(std::process::id()).and_then(|record| {
        super::state::atomic_write(&record_path, record.as_bytes())
            .map(|()| record)
            .map_err(|error| error.to_string())
    }) {
        Ok(record) => record,
        Err(cause) => {
            say(&format!("cannot publish supervisor.pid: {cause}"));
            return ExitCode::from(1);
        }
    };
    let child_pid = Arc::new(AtomicU32::new(0));
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let child_pid = Arc::clone(&child_pid);
        let stopping = Arc::clone(&stopping);
        let spawned = thread::Builder::new()
            .name("sdsyncd-supervise-signals".into())
            .spawn(move || {
                while let Ok(signal) = sys::wait_signal() {
                    if matches!(signal, libc::SIGTERM | libc::SIGINT) {
                        stopping.store(true, Ordering::SeqCst);
                    }
                    let pid = child_pid.load(Ordering::SeqCst);
                    if pid > 1 {
                        let _ = sys::kill_process(pid, signal);
                    }
                }
            });
        if let Err(error) = spawned {
            say(&format!("cannot start the signal thread: {error}"));
            return supervisor_exit(&sink, &record_path, &record, 1);
        }
    }
    let mut budget = RestartBudget::new(5, Duration::from_secs(600));
    loop {
        // A stop that arrived between two daemons is not a reason to start
        // another (F3).
        if stopping.load(Ordering::SeqCst) {
            return supervisor_exit(&sink, &record_path, &record, 0);
        }
        let mark = stderr_mark(&options.var);
        let stderr = match open_stderr_log(&options.var) {
            Ok(file) => Stdio::from(file),
            Err(cause) => {
                log_or_stderr(
                    &sink,
                    Level::Error,
                    "daemon_stderr_unavailable",
                    format!("cause={cause} the daemon's stderr is discarded"),
                );
                Stdio::null()
            }
        };
        let mut command = Command::new(&exe);
        command
            .arg("--daemon")
            .args(options.forward())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        let parent = sys::getpid();
        // SAFETY: child_setup is async-signal-safe and allocates nothing.
        unsafe {
            command.pre_exec(move || sys::child_setup(parent, None));
        }
        let status = match command.spawn() {
            Ok(mut child) => {
                child_pid.store(child.id(), Ordering::SeqCst);
                // A TERM that landed before the PID was published was not
                // forwarded: forward it now.
                if stopping.load(Ordering::SeqCst) {
                    let _ = sys::kill_process(child.id(), libc::SIGTERM);
                }
                let status = child.wait();
                child_pid.store(0, Ordering::SeqCst);
                status
            }
            Err(error) => Err(error),
        };
        let code = match &status {
            Ok(status) => status
                .code()
                .or_else(|| status.signal().map(|s| 128 + s))
                .unwrap_or(-1),
            Err(_) => -1,
        };
        // A clean stop, or a stop asked of the supervisor: done.
        if code == 0 || stopping.load(Ordering::SeqCst) {
            return supervisor_exit(&sink, &record_path, &record, 0);
        }
        let last = stderr_since(&options.var, mark);
        let said = if last.is_empty() {
            String::new()
        } else {
            format!(" last_stderr={last}")
        };
        // 75: daemon.lock was held for the whole of the daemon's wait. Another
        // daemon serving is not a failure; anything else holding it is (F4).
        if code == 75 {
            let serving = ctl::request(
                &options.socket(),
                &json!({"verb": "ping"}),
                Duration::from_secs(2),
            )
            .is_ok_and(|reply| reply["ok"] == true);
            if serving {
                log_or_stderr(
                    &sink,
                    Level::Info,
                    "service_superseded",
                    "cause=daemon.lock is held by a serving daemon; this supervisor leaves".into(),
                );
                return supervisor_exit(&sink, &record_path, &record, 0);
            }
        }
        let cause = match &status {
            Ok(_) if code == 75 => format!(
                "exit=75 cause=daemon_lock_held: daemon.lock is held but no daemon answers{said}"
            ),
            Ok(_) => format!("exit={code}{said}"),
            Err(error) => format!("spawn_failed cause={error}"),
        };
        if code == 78 || !budget.allow(Instant::now()) {
            log_or_stderr(
                &sink,
                Level::Error,
                "service_start_failed",
                format!("{cause} restarts_exhausted={}", code != 78),
            );
            activity_or_stderr(
                &sink,
                &ActivityRow {
                    code: "service.start_failed",
                    profile: "none".into(),
                    state: "failed",
                    category: "controller",
                    level: Level::Error,
                    message: format!("The sync service stopped and was not restarted ({cause})"),
                },
            );
            remove_dead_daemon_records(&sink, &options.var);
            return supervisor_exit(&sink, &record_path, &record, 1);
        }
        log_or_stderr(&sink, Level::Warn, "service_restarted", cause.clone());
        activity_or_stderr(
            &sink,
            &ActivityRow {
                code: "service.restarted",
                profile: "none".into(),
                state: "running",
                category: "controller",
                level: Level::Warn,
                message: format!(
                    "The sync service stopped unexpectedly ({cause}) and was restarted"
                ),
            },
        );
        // One second between daemons, cut short by a stop.
        let pause = Instant::now() + Duration::from_secs(1);
        while Instant::now() < pause && !stopping.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager script for [`ManagerMutations`], in a private directory.
    fn manager(label: &str, body: &str) -> (PathBuf, ManagerMutations) {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("sdsyncd-mutation-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("sdsync-dsm");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let runner = ManagerMutations {
            manager: script,
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        };
        (dir, runner)
    }

    fn mutation() -> Mutation {
        Mutation {
            op: "configure-profile".into(),
            profile: Some("alpha".into()),
            args: Vec::new(),
        }
    }

    /// H3 residue: a manager that leaves its process group and ignores TERM
    /// was waited for without a bound after the group KILL missed it
    /// (`child.wait()`). The wait is bounded and the ending named.
    #[test]
    fn a_mutation_that_outlives_its_kill_is_bounded() {
        let (dir, runner) = manager(
            "escapes",
            "exec perl -e 'setpgrp(0, getpgrp(getppid())); $SIG{TERM} = \"IGNORE\"; sleep 30'",
        );
        let began = Instant::now();
        let outcome = runner.run(&mutation(), Duration::from_millis(300));
        let took = began.elapsed();
        eprintln!("after {took:?}: {outcome:?}");
        assert!(took < Duration::from_secs(12), "bounded: {took:?}");
        assert_eq!(outcome.exit, 124, "{outcome:?}");
        assert!(outcome.detail.contains("consumer_timeout"), "{outcome:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// H3 residue: a grandchild that keeps the manager's stderr open held the
    /// collector (`collector.join()`) after the manager itself had exited.
    #[test]
    fn a_grandchild_holding_stderr_does_not_hold_the_mutation() {
        let (dir, runner) = manager("grandchild", "echo 'saved alpha' >&2\nsleep 30 &\nexit 0");
        let began = Instant::now();
        let outcome = runner.run(&mutation(), Duration::from_secs(10));
        let took = began.elapsed();
        eprintln!("after {took:?}: {outcome:?}");
        assert!(took < Duration::from_secs(5), "bounded: {took:?}");
        assert_eq!(outcome.exit, 0, "{outcome:?}");
        assert_eq!(outcome.detail, "saved alpha", "{outcome:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_restart_budget_allows_five_in_ten_minutes() {
        let mut budget = RestartBudget::new(5, Duration::from_secs(600));
        let t0 = Instant::now();
        for n in 0..5 {
            assert!(budget.allow(t0 + Duration::from_secs(n)));
        }
        assert!(!budget.allow(t0 + Duration::from_secs(10)));
        assert!(budget.allow(t0 + Duration::from_secs(601)));
    }

    /// I-8: the layout is private and symlinks are refused.
    #[test]
    fn root_refused_and_layout_is_private() {
        let dir = std::env::temp_dir().join(format!("sdsyncd-layout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        let uid = check_identity(&dir.join("home")).unwrap();
        assert_ne!(uid, 0, "the test suite never runs as root");
        let var = dir.join("var");
        ensure_layout(&var, uid).unwrap();
        for sub in ["run", "state/routines", "log"] {
            let mode = fs::metadata(var.join(sub)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{sub}");
        }
        fs::remove_dir(var.join("log")).unwrap();
        std::os::unix::fs::symlink("/tmp", var.join("log")).unwrap();
        assert!(
            ensure_layout(&var, uid)
                .unwrap_err()
                .contains("not a directory")
        );
        assert!(check_identity(&var.join("log")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// DSM's alias roots: `/var/packages/<pkg>/home` and `/var` are symlinks
    /// to `@apphome`/`@appdata`, and every script and profile names the alias.
    /// The roots are judged by their targets (and the var target keeps the
    /// mode DSM gave it); below them nothing may be a link.
    #[test]
    fn alias_roots_are_refused_by_name_when_their_target_is_not_ours() {
        let dir = std::env::temp_dir().join(format!("sdsyncd-alias-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let uid = sys::geteuid();
        // Another owner's directory (root's /tmp, also world-writable).
        std::os::unix::fs::symlink("/tmp", dir.join("foreign")).unwrap();
        let error = resolve_root(&dir.join("foreign"), uid).unwrap_err();
        assert!(
            error.contains("-> /tmp") && error.contains("owned by uid 0"),
            "{error}"
        );
        // A dangling root.
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("dangling")).unwrap();
        let error = resolve_root(&dir.join("dangling"), uid).unwrap_err();
        assert!(
            error.contains("dangling -> ") && error.contains("unavailable"),
            "{error}"
        );
        // A writable target.
        fs::create_dir_all(dir.join("open")).unwrap();
        fs::set_permissions(dir.join("open"), fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::symlink(dir.join("open"), dir.join("var")).unwrap();
        let error = ensure_layout(&dir.join("var"), uid).unwrap_err();
        assert!(error.contains("writable by group or others"), "{error}");
        // A link to a link.
        std::os::unix::fs::symlink(dir.join("var"), dir.join("twice")).unwrap();
        let error = resolve_root(&dir.join("twice"), uid).unwrap_err();
        assert!(error.contains("one level only"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dsm_alias_roots_are_followed_and_nothing_below_them() {
        let dir = std::env::temp_dir().join(format!("sdsyncd-alias-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("@apphome")).unwrap();
        fs::create_dir_all(dir.join("@appdata")).unwrap();
        fs::set_permissions(dir.join("@appdata"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(dir.join("@apphome"), dir.join("home")).unwrap();
        std::os::unix::fs::symlink(dir.join("@appdata"), dir.join("var")).unwrap();
        let uid = check_identity(&dir.join("home")).unwrap();
        ensure_layout(&dir.join("var"), uid).unwrap();
        assert!(dir.join("@appdata/run").is_dir() && dir.join("@appdata/state/routines").is_dir());
        // A link below the root is still refused.
        fs::remove_dir(dir.join("@appdata/log")).unwrap();
        std::os::unix::fs::symlink(dir.join("@apphome"), dir.join("@appdata/log")).unwrap();
        assert!(
            ensure_layout(&dir.join("var"), uid)
                .unwrap_err()
                .contains("not a directory")
        );
        fs::remove_file(dir.join("@appdata/log")).unwrap();
        assert_eq!(
            fs::metadata(dir.join("@appdata"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "DSM's directory is left as DSM made it"
        );
        // An alias to a file is not a home.
        fs::write(dir.join("file"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("file"), dir.join("not-home")).unwrap();
        assert!(
            check_identity(&dir.join("not-home"))
                .unwrap_err()
                .contains("not a directory")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_singleton_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("sdsyncd-single-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("run")).unwrap();
        let first = acquire_singleton(&dir).unwrap();
        assert!(first.is_some());
        assert!(acquire_singleton(&dir).unwrap().is_none());
        drop(first);
        assert!(acquire_singleton(&dir).unwrap().is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_parse_and_refuse_a_missing_path() {
        let (options, rest) =
            Options::parse(&["--var".into(), "/x".into(), "status".into()]).unwrap();
        assert_eq!(options.var, PathBuf::from("/x"));
        assert_eq!(rest, ["status"]);
        assert!(Options::parse(&["--home".into()]).is_err());
    }
}
