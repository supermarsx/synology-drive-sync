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
    let metadata =
        fs::symlink_metadata(home).map_err(|error| format!("{}: {error}", home.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{} is not a directory", home.display()));
    }
    if metadata.uid() != uid {
        return Err(format!(
            "{} is owned by uid {}, not by this process (uid {uid})",
            home.display(),
            metadata.uid()
        ));
    }
    Ok(uid)
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
                if metadata.mode() & 0o077 != 0 {
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
    let path = var.join("run").join("daemon.lock");
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
        let mut stderr = child.stderr.take();
        let collector = thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(pipe) = stderr.as_mut() {
                let _ = pipe.take(64 * 1024).read_to_end(&mut text);
            }
            text
        });
        let started = Instant::now();
        let mut killed = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(_) => break None,
            }
            let elapsed = started.elapsed();
            match killed {
                None if elapsed >= deadline => {
                    let _ = sys::kill_group(pid, libc::SIGTERM);
                    killed = Some(Instant::now());
                }
                Some(at) if at.elapsed() >= Duration::from_secs(3) => {
                    let _ = sys::kill_group(pid, libc::SIGKILL);
                    // Reaped below; KILL cannot be ignored.
                    break child.wait().ok();
                }
                _ => {}
            }
            thread::sleep(Duration::from_millis(25));
        };
        let text = collector.join().unwrap_or_default();
        let last = String::from_utf8_lossy(&text)
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| sanitize_detail(line, 240))
            .unwrap_or_default();
        if killed.is_some() {
            return MutationOutcome {
                exit: 124,
                detail: format!(
                    "consumer_timeout: {} did not finish within {} s and was stopped; its outcome is unknown",
                    mutation.op,
                    deadline.as_secs()
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

// ---------------------------------------------------------------- --daemon

fn remove_if_ours(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.is_dir()
    {
        let _ = fs::remove_file(path);
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
    let _lock = match acquire_singleton(&options.var) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            say("already running (daemon.lock is held)");
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
    remove_if_ours(&ready_file);
    if let Err(error) = super::state::atomic_write(&pid_file, format!("{pid}\n").as_bytes()) {
        say(&format!("cannot publish daemon.pid: {error}"));
        return ExitCode::from(1);
    }
    let log_root = options.var.join("log");
    let sink = FileSink::new(&log_root);
    let zone = sys::zone_name(epoch().unwrap_or(0) as i64).unwrap_or_else(|| "unknown".into());
    let _ = sink.log(
        epoch().unwrap_or(0),
        &LogEvent {
            level: Level::Info,
            category: "controller",
            event: "service_clock",
            detail: format!("zone={zone} tz={}", std::env::var("TZ").unwrap_or_default()),
        },
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
    let _ = sink.log(
        epoch().unwrap_or(0),
        &LogEvent {
            level,
            category: "controller",
            event: "service_engine",
            detail,
        },
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
    let _ = sink.log(
        epoch().unwrap_or(0),
        &LogEvent {
            level: if audited { Level::Info } else { Level::Warn },
            category: "controller",
            event: "service_audit",
            detail: if audited {
                "runs=audited".to_owned()
            } else {
                format!(
                    "runs=unaudited cause={} is not the package's var ({})",
                    options.var.display(),
                    super::super::PACKAGE_VAR
                )
            },
        },
    );
    // Alerts go through the package's own sdsync-common (its alert policy,
    // ledgers and notifiers), which lives at the package paths only.
    let alert_sink = FileSink::new(&log_root);
    let alert_log: super::alerts::AlertLog = Arc::new(move |level, event, detail| {
        let _ = alert_sink.log(
            epoch().unwrap_or(0),
            &LogEvent {
                level,
                category: "scheduler",
                event,
                detail,
            },
        );
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
        sink: Box::new(sink),
        clock: Box::new(sys::LibcLocalClock),
        wall: Box::new(epoch),
        pid,
    };
    let (tx, control) = match control::spawn(parts) {
        Ok(spawned) => spawned,
        Err(error) => {
            say(&format!("cannot start the control thread: {error}"));
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
    if let Err(error) = signal_thread {
        say(&format!("cannot start the signal thread: {error}"));
        let _ = control::ask(&tx, control::Request::Stop, Duration::from_secs(60));
        let _ = control.join();
        return ExitCode::from(1);
    }
    // The dashboard's server first: a daemon whose dashboard cannot answer is
    // not ready, and `--launch-daemon` (waiting for ctl.sock) says why.
    let api = if options.serve_api {
        let api_sink = FileSink::new(&log_root);
        let log: super::api_server::KeeperLog = Arc::new(move |level, event, detail| {
            let _ = api_sink.log(
                epoch().unwrap_or(0),
                &LogEvent {
                    level,
                    category: "controller",
                    event,
                    detail,
                },
            );
        });
        let started = std::env::current_exe()
            .map_err(|error| format!("cannot find this executable: {error}"))
            .and_then(|exe| {
                super::api_server::ApiKeeper::start(
                    exe,
                    PathBuf::from(super::super::API_SOCKET_PATH),
                    api_env.clone(),
                    log,
                    Duration::from_secs(15),
                )
            });
        match started {
            Ok(keeper) => Some(keeper),
            Err(cause) => {
                say(&format!("the dashboard API server did not start: {cause}"));
                let _ = control::ask(&tx, control::Request::Stop, Duration::from_secs(60));
                let _ = control.join();
                remove_if_ours(&pid_file);
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
            let _ = queue_sink.log(
                epoch().unwrap_or(0),
                &LogEvent {
                    level,
                    category: "controller",
                    event,
                    detail,
                },
            );
        });
        match std::env::current_exe().and_then(|exe| {
            super::queue::QueueConsumer::start(
                exe,
                options.var.clone(),
                api_env.clone(),
                super::queue::QueueSettings::default(),
                log,
            )
        }) {
            Ok(queue) => Some(queue),
            Err(error) => {
                say(&format!("the dashboard queue did not start: {error}"));
                let _ = control::ask(&tx, control::Request::Stop, Duration::from_secs(60));
                let _ = control.join();
                if let Some(api) = api {
                    api.stop(Duration::from_secs(5));
                }
                remove_if_ours(&pid_file);
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };
    let socket = options.socket();
    if let Err(error) = ctl::listen(&socket, uid, tx.clone(), mutation_deadline) {
        say(&format!("cannot serve {}: {error}", socket.display()));
        let _ = control::ask(&tx, control::Request::Stop, Duration::from_secs(60));
        let _ = control.join();
        if let Some(queue) = queue {
            queue.stop(Duration::from_secs(15));
        }
        if let Some(api) = api {
            api.stop(Duration::from_secs(5));
        }
        remove_if_ours(&pid_file);
        return ExitCode::from(1);
    }
    // Ready: the control thread runs and the socket answers.
    let start = super::super::linux_files::process_start(pid).unwrap_or(0);
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    if let Err(error) = super::state::atomic_write(
        &ready_file,
        format!("{pid}\n{start}\n{}\n", boot.trim()).as_bytes(),
    ) {
        say(&format!("cannot publish daemon.ready: {error}"));
    }
    drop(tx);
    let _ = control.join();
    if let Some(queue) = queue {
        queue.stop(Duration::from_secs(15));
    }
    if let Some(api) = api {
        api.stop(Duration::from_secs(5));
    }
    // Clean stop: nothing an older pre-hook checks for is left behind (§3.4).
    for path in [&socket, &ready_file, &pid_file] {
        remove_if_ours(path);
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

/// `--launch-daemon`: start the supervisor detached (`setsid`) and wait for
/// `ctl ping` (≤ 30 s). Exit 0 once it answers; 1 with a named cause otherwise.
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
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            say(&format!("cannot find this executable: {error}"));
            return ExitCode::from(1);
        }
    };
    let mut command = Command::new(exe);
    command
        .arg("--supervise")
        .args(options.forward())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            say(&format!(
                "the supervisor exited before the daemon was ready ({status})"
            ));
            return ExitCode::from(1);
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
    say("the daemon did not answer ctl ping within 30 s");
    ExitCode::from(1)
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

/// `--supervise`: run `--daemon` and restart it after a crash (not after a
/// clean stop), at most 5 times in 10 minutes. TERM/INT are forwarded.
pub(crate) fn run_supervise(options: &Options) -> ExitCode {
    if let Err(error) = sys::block_daemon_signals() {
        say(&format!("cannot block signals: {error}"));
        return ExitCode::from(1);
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            say(&format!("cannot find this executable: {error}"));
            return ExitCode::from(1);
        }
    };
    let child_pid = Arc::new(AtomicU32::new(0));
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let child_pid = Arc::clone(&child_pid);
        let stopping = Arc::clone(&stopping);
        let _ = thread::Builder::new()
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
    }
    let sink = FileSink::new(&options.var.join("log"));
    let mut budget = RestartBudget::new(5, Duration::from_secs(600));
    loop {
        let stderr = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(options.var.join("log").join("daemon-stderr.log"))
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
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
        // A clean stop, or another daemon already serving: done.
        if code == 0 || code == 75 || stopping.load(Ordering::SeqCst) {
            return ExitCode::SUCCESS;
        }
        let now = epoch().unwrap_or(0);
        let cause = match &status {
            Ok(_) => format!("exit={code}"),
            Err(error) => format!("spawn_failed cause={error}"),
        };
        if code == 78 || !budget.allow(Instant::now()) {
            let _ = sink.log(
                now,
                &LogEvent {
                    level: Level::Error,
                    category: "controller",
                    event: "service_start_failed",
                    detail: format!("{cause} restarts_exhausted={}", code != 78),
                },
            );
            let _ = sink.activity(
                now,
                &ActivityRow {
                    code: "service.start_failed",
                    profile: "none".into(),
                    state: "failed",
                    category: "controller",
                    level: Level::Error,
                    message: format!("The sync service stopped and was not restarted ({cause})"),
                },
            );
            return ExitCode::from(1);
        }
        let _ = sink.log(
            now,
            &LogEvent {
                level: Level::Warn,
                category: "controller",
                event: "service_restarted",
                detail: cause.clone(),
            },
        );
        let _ = sink.activity(
            now,
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
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
