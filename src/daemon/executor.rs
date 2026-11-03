//! The executor: the core sync runs as a child process in its own process
//! group, watched by a supervisor thread that bounds every way it can hang
//! (design §2.1, §2.5 layer 3, §2.6).
//!
//! - Cancel or stop: TERM to the group, KILL after `cancel_grace`, read as
//!   `cancelled` (I-11). A child still not reaped `kill_wait` after the KILL is
//!   abandoned and named `run_unkillable` (R4); the slot is released.
//! - Watchdog: progress is the child's `/proc/<pid>/io` rchar+wchar or the
//!   progress record's mtime moving. No progress for `stall_warn` is a named
//!   warning; for `stall_kill` the run is ended and named `run_stalled`. There
//!   is deliberately no total-duration cap: a first upload can take days.
//! - Exit 137 that the daemon did not cause is named, with the cgroup OOM
//!   counter as proof when it moved.
//! - The child's environment is an allowlist (I-9), its stdout is discarded
//!   (the result record carries the outcome) and its stderr is kept as a
//!   bounded 16 KiB tail.
//!
//! The classification of a finished run into the state vocabulary is pure and
//! portable; spawning and supervising are Linux-only.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use super::state::{ResultFacts, RunOutcome, RunStatus, sanitize_detail};

/// How much of the child's stderr is kept (matches `run-stderr.log`).
pub(crate) const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// The largest result record read back.
const MAX_RESULT_RECORD_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Timings {
    /// How often the supervisor looks at the child.
    pub(crate) poll: Duration,
    pub(crate) stall_warn: Duration,
    pub(crate) stall_kill: Duration,
    pub(crate) cancel_grace: Duration,
    /// After KILL, how long to wait for the reap before naming it unkillable.
    pub(crate) kill_wait: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(250),
            stall_warn: Duration::from_secs(15 * 60),
            stall_kill: Duration::from_secs(45 * 60),
            cancel_grace: Duration::from_secs(20),
            kill_wait: Duration::from_secs(2),
        }
    }
}

/// One child to run.
#[derive(Clone, Debug)]
pub(crate) struct RunSpec {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    /// The complete environment of the child; nothing else is inherited.
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) nice: Option<i32>,
    pub(crate) result_record: Option<PathBuf>,
    pub(crate) progress_record: Option<PathBuf>,
    pub(crate) timings: Timings,
}

/// The variables a child may inherit from the daemon (I-9). Everything else,
/// every `SDSYNC_*` included, is dropped: an allowlist is stronger than the
/// shell's denylist (`cmn:169`).
pub(crate) const ENV_ALLOWLIST: [&str; 7] =
    ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "HOME", "TMPDIR"];

/// The allowlisted subset of `source`.
pub(crate) fn allowlisted_env(
    source: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    source
        .into_iter()
        .filter(|(key, _)| ENV_ALLOWLIST.iter().any(|allowed| key == allowed))
        .collect()
}

/// Why the daemon stopped a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopCause {
    /// An operator cancelled the run (dashboard or `ctl cancel`).
    Cancel,
    /// The daemon itself is stopping (SIGTERM, `ctl stop`).
    Shutdown,
}

/// How the supervision ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    /// The child exited on its own.
    Exited,
    /// Stopped by the daemon; `forced` when it outlasted the grace and was KILLed.
    Stopped { cause: StopCause, forced: bool },
    /// The watchdog ended it after `idle` without progress.
    Stalled { idle: Duration, forced: bool },
    /// KILLed and still not reaped after `kill_wait`: abandoned (R4).
    Unkillable { after: Box<Ending> },
    /// It never started.
    SpawnFailed { cause: String },
}

/// One `sdsync.result-record.v1` line, as far as the daemon needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct JobRecord {
    pub(crate) profile: String,
    pub(crate) status: String,
    pub(crate) error_code: Option<String>,
    pub(crate) error_detail: Option<String>,
    pub(crate) error_path: Option<String>,
    pub(crate) outcome: Option<String>,
    pub(crate) deferred: Option<u64>,
    pub(crate) skipped: Option<u64>,
    pub(crate) facts: ResultFacts,
}

/// Everything the supervisor learned about one run.
#[derive(Clone, Debug)]
pub(crate) struct RunReport {
    /// The exit code; `128 + signal` for a signal; `-1` when it never ran.
    pub(crate) exit: i32,
    pub(crate) ending: Ending,
    pub(crate) stderr_tail: Vec<u8>,
    pub(crate) records: Vec<JobRecord>,
    /// Why the result record could not be read, when the core ran.
    pub(crate) record_error: Option<String>,
    /// For an exit 137 the daemon did not cause: did the cgroup OOM counter move?
    pub(crate) oom_proven: Option<bool>,
    pub(crate) elapsed: Duration,
    /// The progress phase last seen in the progress record, if any.
    pub(crate) last_phase: Option<String>,
}

/// Events the supervisor reports while a run is in flight.
#[derive(Clone, Debug)]
pub(crate) enum ExecEvent {
    Started {
        pid: u32,
    },
    /// No progress for `idle` (named `run_stalled` warning; the run continues).
    StallWarning {
        idle: Duration,
        detail: String,
    },
    /// `/proc/<pid>/io` is unreadable: progress cannot be observed (`stall_unobservable`).
    Unobservable,
    Finished(Box<RunReport>),
}

/// Parse a result record: one JSON object per line. Unknown fields are ignored;
/// a line that is not a result record is skipped; an unreadable record names why.
pub(crate) fn parse_result_record(bytes: &[u8]) -> Result<Vec<JobRecord>, String> {
    let mut records = Vec::new();
    for (index, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: Value = serde_json::from_slice(line)
            .map_err(|error| format!("line {} is not JSON: {error}", index + 1))?;
        if value["schema"] != "sdsync.result-record.v1" {
            continue;
        }
        let text = |v: &Value| v.as_str().map(str::to_owned);
        let count = |v: &Value| v.as_u64().or_else(|| v.as_array().map(|a| a.len() as u64));
        let result = &value["result"];
        let recovered = &result["recovered"];
        let folders = result["incomplete_listing_folders"]
            .as_array()
            .map(|folders| {
                folders
                    .iter()
                    .filter_map(Value::as_str)
                    .take(5)
                    .map(|folder| sanitize_detail(folder, 120).replace('|', "?"))
                    .collect::<Vec<_>>()
                    .join("|")
            });
        records.push(JobRecord {
            profile: text(&value["profile"]).unwrap_or_default(),
            status: text(&value["status"]).unwrap_or_default(),
            error_code: text(&value["error"]["code"]),
            error_detail: text(&value["error"]["detail"]),
            error_path: text(&value["error"]["path"]),
            outcome: text(&result["outcome"]),
            deferred: count(&result["deferred"]),
            skipped: count(&result["skipped"]),
            facts: ResultFacts {
                incomplete_listings: result["incomplete_listings"].as_u64(),
                incomplete_listing_folders: folders.filter(|f| !f.is_empty()),
                mtime_not_kept: result["mtime_not_kept"].as_u64(),
                reauthentication_failures: recovered["reauthentication_failures"].as_u64(),
                server_tasks_not_stopped: recovered["server_tasks_not_stopped"].as_u64(),
                keepalive_failures: recovered["keepalive_failures"].as_u64(),
                link_guard_unchecked: result["link_guard_unchecked"].as_u64(),
            },
        });
    }
    Ok(records)
}

/// A finished run in the state vocabulary (the runner's exit mapping,
/// `run:1206-1247`, plus the daemon's own endings).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) status: RunStatus,
    pub(crate) outcome: RunOutcome,
    /// A state token: `ok`, `deferred_paths`, `busy`, `cancelled`, `run_stalled`,
    /// `core_failed`, `supervisor_refused`, `launch_failed`.
    pub(crate) reason: String,
    /// One printable line, ≤ 240 bytes (run.state keeps the first 160).
    pub(crate) detail: String,
    /// Cancelled by an operator or the daemon's stop (consumes no retry, I-5).
    pub(crate) cancelled: bool,
    /// The core ran (so missing counts are `counts=unavailable`).
    pub(crate) core_ran: bool,
}

/// The core's own cause: its last `error:` line, else its last non-empty line.
pub(crate) fn stderr_cause(tail: &[u8]) -> String {
    let text = String::from_utf8_lossy(tail);
    let mut chosen = None;
    let mut last = None;
    for line in text.lines() {
        if line.starts_with("error: ")
            || line.starts_with("Error: ")
            || line.starts_with("sdsync-dsm-api: supervised core refused: ")
        {
            chosen = Some(line);
        }
        if !line.trim().is_empty() {
            last = Some(line);
        }
    }
    sanitize_detail(chosen.or(last).unwrap_or_default(), 240)
}

/// Whole minutes, rounded up, so a stall is never reported as "0 min".
fn minutes(duration: Duration) -> u128 {
    duration.as_millis().div_ceil(60_000)
}

fn ending_words(ending: &Ending, report: &RunReport) -> (RunOutcome, &'static str, String, bool) {
    let phase = report
        .last_phase
        .as_deref()
        .map(|phase| format!(" (last phase {phase})"))
        .unwrap_or_default();
    match ending {
        Ending::Stopped { cause, forced } => {
            let who = match cause {
                StopCause::Cancel => "the run was cancelled",
                StopCause::Shutdown => "the package stopped during the run",
            };
            let how = if *forced {
                "; the core outlasted the cancel grace and was killed"
            } else {
                ""
            };
            (
                RunOutcome::Cancelled,
                "cancelled",
                format!("{who}{how}"),
                true,
            )
        }
        Ending::Stalled { idle, forced } => (
            RunOutcome::Failed,
            "run_stalled",
            format!(
                "no progress for {} min{phase}; the run was stopped{}",
                minutes(*idle),
                if *forced { " and killed" } else { "" }
            ),
            false,
        ),
        Ending::Unkillable { after } => {
            let (outcome, reason, detail, cancelled) = ending_words(after, report);
            (
                outcome,
                reason,
                format!(
                    "run_unkillable: {detail}; the core did not exit after SIGKILL and was abandoned"
                ),
                cancelled,
            )
        }
        Ending::SpawnFailed { cause } => (
            RunOutcome::Failed,
            "launch_failed",
            format!("the core could not be started: {cause}"),
            false,
        ),
        Ending::Exited => (RunOutcome::Failed, "core_failed", String::new(), false),
    }
}

fn or_else(cause: String, fallback: &str) -> String {
    if cause.is_empty() {
        fallback.to_owned()
    } else {
        cause
    }
}

/// Classify a finished run.
pub(crate) fn classify(report: &RunReport) -> Verdict {
    let verdict = |status, outcome, reason: &str, detail: String, cancelled, core_ran| Verdict {
        status,
        outcome,
        reason: reason.to_owned(),
        detail: sanitize_detail(&detail, 240),
        cancelled,
        core_ran,
    };
    if !matches!(report.ending, Ending::Exited) {
        let (outcome, reason, detail, cancelled) = ending_words(&report.ending, report);
        let core_ran = !matches!(report.ending, Ending::SpawnFailed { .. });
        return verdict(
            RunStatus::Failed,
            outcome,
            reason,
            detail,
            cancelled,
            core_ran,
        );
    }
    let cause = stderr_cause(&report.stderr_tail);
    match report.exit {
        0 => verdict(
            RunStatus::Succeeded,
            RunOutcome::Succeeded,
            "ok",
            String::new(),
            false,
            true,
        ),
        3 => verdict(
            RunStatus::Succeeded,
            RunOutcome::Partial,
            "deferred_paths",
            String::new(),
            false,
            true,
        ),
        75 => verdict(
            RunStatus::Failed,
            RunOutcome::Busy,
            "busy",
            or_else(cause, "the core found the package busy (exit 75)"),
            false,
            true,
        ),
        // Signalled from outside the daemon (the core maps TERM to 130).
        130 | 143 => verdict(
            RunStatus::Failed,
            RunOutcome::Cancelled,
            "cancelled",
            "the core was stopped by a signal from outside the package".into(),
            true,
            true,
        ),
        78 => verdict(
            RunStatus::Failed,
            RunOutcome::Failed,
            "supervisor_refused",
            or_else(
                cause,
                "the package supervisor refused to start the core (exit 78)",
            ),
            false,
            false,
        ),
        137 => {
            let named = if cause.starts_with("error:") || cause.starts_with("Error:") {
                cause
            } else if report.oom_proven == Some(true) {
                "the sync core was killed by the out-of-memory killer (exit 137); the source may hold more entries than this NAS has memory for".into()
            } else {
                "the sync core was killed (SIGKILL, exit 137) without reporting an error; out of memory? check the DSM system log for the OOM killer".into()
            };
            verdict(
                RunStatus::Failed,
                RunOutcome::Failed,
                "core_failed",
                named,
                false,
                true,
            )
        }
        code => {
            let detail = if cause.is_empty() {
                format!("the core exited with status {code} without an error line")
            } else {
                cause
            };
            verdict(
                RunStatus::Failed,
                RunOutcome::Failed,
                "core_failed",
                detail,
                false,
                true,
            )
        }
    }
}

/// Read and remove the result record the core wrote.
pub(crate) fn take_result_record(path: &std::path::Path) -> Result<Vec<JobRecord>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|error| {
        format!(
            "the result record {} could not be opened: {error}",
            path.display()
        )
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_RESULT_RECORD_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "the result record {} could not be read: {error}",
                path.display()
            )
        })?;
    let _ = std::fs::remove_file(path);
    parse_result_record(&bytes)
}

#[cfg(target_os = "linux")]
pub(crate) use process::{RunHandle, start_run};

#[cfg(target_os = "linux")]
mod process {
    use std::collections::VecDeque;
    use std::fs;
    use std::io::Read;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Instant, SystemTime};

    use super::super::sys;
    use super::*;

    /// The control thread's handle on one in-flight run.
    pub(crate) struct RunHandle {
        stop: mpsc::Sender<StopCause>,
    }

    impl RunHandle {
        /// Ask the supervisor to stop the child (TERM, then KILL after the grace).
        /// Never blocks; a second request is ignored by the supervisor.
        pub(crate) fn stop(&self, cause: StopCause) {
            let _ = self.stop.send(cause);
        }
    }

    /// Start `spec` on a supervisor thread. Every outcome, a failed spawn
    /// included, arrives through `sink` as `ExecEvent::Finished`.
    pub(crate) fn start_run(
        spec: RunSpec,
        sink: impl Fn(ExecEvent) + Send + 'static,
    ) -> std::io::Result<RunHandle> {
        let (stop, requests) = mpsc::channel();
        thread::Builder::new()
            .name("sdsyncd-run".into())
            .spawn(move || supervise(spec, requests, &sink))?;
        Ok(RunHandle { stop })
    }

    fn exit_code(status: std::process::ExitStatus) -> i32 {
        status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(-1)
    }

    /// rchar + wchar of a process, or `None` when the kernel will not show it.
    fn io_counter(pid: u32) -> Option<u64> {
        let text = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
        let mut total = 0u64;
        let mut seen = 0;
        for line in text.lines() {
            if let Some(value) = line
                .strip_prefix("rchar: ")
                .or_else(|| line.strip_prefix("wchar: "))
            {
                total = total.saturating_add(value.trim().parse().ok()?);
                seen += 1;
            }
        }
        (seen == 2).then_some(total)
    }

    fn mtime(path: &Path) -> Option<SystemTime> {
        fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    /// The `section` the progress record last named.
    fn progress_phase(path: &Path) -> Option<String> {
        let bytes = fs::read(path).ok().filter(|b| b.len() <= 4096)?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        value["section"]
            .as_str()
            .filter(|s| s.len() <= 64 && s.bytes().all(|b| b.is_ascii_graphic()))
            .map(str::to_owned)
    }

    /// The kernel's OOM-kill counter for this process's cgroup (v2
    /// `memory.events`, else v1 `memory.oom_control`), as `runner_oom_kill_count`.
    pub(crate) fn oom_kill_count() -> Option<u64> {
        let cgroups = fs::read_to_string("/proc/self/cgroup").ok()?;
        let events = cgroups.lines().find_map(|line| {
            let mut parts = line.splitn(3, ':');
            let (id, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
            if id == "0" && controllers.is_empty() {
                Some(format!("/sys/fs/cgroup{path}/memory.events"))
            } else if controllers.split(',').any(|c| c == "memory") {
                Some(format!("/sys/fs/cgroup/memory{path}/memory.oom_control"))
            } else {
                None
            }
        })?;
        let text = fs::read_to_string(events).ok()?;
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (key == "oom_kill").then(|| value.trim().parse().ok())?
        })
    }

    fn drain_stderr(
        mut pipe: impl Read + Send + 'static,
    ) -> (Arc<Mutex<VecDeque<u8>>>, mpsc::Receiver<()>) {
        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_BYTES)));
        let (done, finished) = mpsc::channel();
        let writer = Arc::clone(&tail);
        let spawned = thread::Builder::new()
            .name("sdsyncd-stderr".into())
            .spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if let Ok(mut tail) = writer.lock() {
                                tail.extend(&buffer[..read]);
                                let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                                tail.drain(..excess);
                            }
                        }
                    }
                }
                let _ = done.send(());
            });
        if let Err(error) = spawned {
            // No drain thread: the pipe was dropped with the closure, so the
            // child sees EPIPE rather than blocking on a full pipe. Say so in
            // the tail itself rather than leave it silently empty.
            if let Ok(mut tail) = tail.lock() {
                tail.extend(
                    format!(
                        "[sdsyncd: stderr not captured: {error}]
"
                    )
                    .bytes(),
                );
            }
        }
        (tail, finished)
    }

    struct Watch {
        last_progress: Instant,
        last_io: Option<u64>,
        last_mtime: Option<SystemTime>,
        warned: bool,
        observable: bool,
    }

    fn supervise(spec: RunSpec, requests: mpsc::Receiver<StopCause>, sink: &dyn Fn(ExecEvent)) {
        let started = Instant::now();
        let finished = |exit, ending, tail: Vec<u8>, records, record_error, oom, phase| {
            sink(ExecEvent::Finished(Box::new(RunReport {
                exit,
                ending,
                stderr_tail: tail,
                records,
                record_error,
                oom_proven: oom,
                elapsed: started.elapsed(),
                last_phase: phase,
            })));
        };
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let parent = sys::getpid();
        let nice = spec.nice;
        // SAFETY: child_setup is async-signal-safe and allocates nothing.
        unsafe {
            command.pre_exec(move || sys::child_setup(parent, nice));
        }
        let oom_before = oom_kill_count();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let cause = format!("{}: {error}", spec.program.display());
                finished(
                    -1,
                    Ending::SpawnFailed { cause },
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    None,
                );
                return;
            }
        };
        let pid = child.id();
        sink(ExecEvent::Started { pid });
        let (tail, stderr_done) = match child.stderr.take() {
            Some(pipe) => drain_stderr(pipe),
            None => (Arc::new(Mutex::new(VecDeque::new())), mpsc::channel().1),
        };
        let mut watch = Watch {
            last_progress: Instant::now(),
            last_io: io_counter(pid),
            last_mtime: spec.progress_record.as_deref().and_then(mtime),
            warned: false,
            observable: true,
        };
        if watch.last_io.is_none() && spec.progress_record.is_none() {
            watch.observable = false;
            sink(ExecEvent::Unobservable);
        }
        let outcome = watch_child(&mut child, pid, &spec, &requests, &mut watch, sink);
        let (exit, ending) = match outcome {
            Watched::Exited(code, ending) => (code, ending),
            Watched::Abandoned(ending) => {
                let tail = tail
                    .lock()
                    .map(|t| t.iter().copied().collect())
                    .unwrap_or_default();
                finished(
                    137,
                    Ending::Unkillable {
                        after: Box::new(ending),
                    },
                    tail,
                    Vec::new(),
                    Some("the core was abandoned before it wrote a result".into()),
                    None,
                    spec.progress_record.as_deref().and_then(progress_phase),
                );
                // Keep this thread (the child's PDEATHSIG parent) until the
                // kernel finally reaps it.
                let _ = child.wait();
                return;
            }
        };
        // The pipe closes when the core and anything it started are gone;
        // a descendant holding it open must not hold the result.
        let _ = stderr_done.recv_timeout(Duration::from_secs(1));
        let tail: Vec<u8> = tail
            .lock()
            .map(|t| t.iter().copied().collect())
            .unwrap_or_default();
        let (records, record_error) = match &spec.result_record {
            Some(path) => match take_result_record(path) {
                Ok(records) => (records, None),
                Err(error) => (Vec::new(), Some(error)),
            },
            None => (Vec::new(), None),
        };
        let oom = (exit == 137 && matches!(ending, Ending::Exited)).then(|| {
            match (oom_before, oom_kill_count()) {
                (Some(before), Some(after)) => Some(after > before),
                _ => None,
            }
        });
        let phase = spec.progress_record.as_deref().and_then(progress_phase);
        finished(
            exit,
            ending,
            tail,
            records,
            record_error,
            oom.flatten(),
            phase,
        );
    }

    enum Watched {
        Exited(i32, Ending),
        Abandoned(Ending),
    }

    fn watch_child(
        child: &mut Child,
        pid: u32,
        spec: &RunSpec,
        requests: &mpsc::Receiver<StopCause>,
        watch: &mut Watch,
        sink: &dyn Fn(ExecEvent),
    ) -> Watched {
        let timings = spec.timings;
        // (what is ending it, when TERM went, when KILL went)
        let mut stopping: Option<(Ending, Instant)> = None;
        let mut killed_at: Option<Instant> = None;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = exit_code(status);
                    let ending = match stopping.take() {
                        Some((Ending::Stopped { cause, .. }, _)) => Ending::Stopped {
                            cause,
                            forced: killed_at.is_some(),
                        },
                        Some((Ending::Stalled { idle, .. }, _)) => Ending::Stalled {
                            idle,
                            forced: killed_at.is_some(),
                        },
                        Some((other, _)) => other,
                        None => Ending::Exited,
                    };
                    return Watched::Exited(code, ending);
                }
                Ok(None) => {}
                // ECHILD and friends: nothing left to wait for.
                Err(_) => return Watched::Exited(-1, Ending::Exited),
            }
            let request = match requests.recv_timeout(timings.poll) {
                Ok(cause) => Some(cause),
                Err(RecvTimeoutError::Timeout) => None,
                // The control thread dropped the handle: it is gone, so stop.
                Err(RecvTimeoutError::Disconnected) => Some(StopCause::Shutdown),
            };
            let now = Instant::now();
            if let (Some(cause), None) = (request, &stopping) {
                let _ = sys::kill_group(pid, libc::SIGTERM);
                stopping = Some((
                    Ending::Stopped {
                        cause,
                        forced: false,
                    },
                    now,
                ));
            }
            // Progress.
            let io = io_counter(pid);
            if io.is_some() && io != watch.last_io {
                watch.last_io = io;
                watch.last_progress = now;
                watch.warned = false;
            }
            if let Some(path) = &spec.progress_record {
                let current = mtime(path);
                if current.is_some() && current != watch.last_mtime {
                    watch.last_mtime = current;
                    watch.last_progress = now;
                    watch.warned = false;
                }
            }
            let idle = now.duration_since(watch.last_progress);
            if watch.observable && stopping.is_none() {
                if idle >= timings.stall_kill {
                    let _ = sys::kill_group(pid, libc::SIGTERM);
                    stopping = Some((
                        Ending::Stalled {
                            idle,
                            forced: false,
                        },
                        now,
                    ));
                } else if idle >= timings.stall_warn && !watch.warned {
                    watch.warned = true;
                    let phase = spec
                        .progress_record
                        .as_deref()
                        .and_then(progress_phase)
                        .map(|phase| format!(" in phase {phase}"))
                        .unwrap_or_default();
                    sink(ExecEvent::StallWarning {
                        idle,
                        detail: format!("no progress for {} min{phase}", minutes(idle)),
                    });
                }
            }
            if let Some((_, term_at)) = &stopping {
                match killed_at {
                    None if now.duration_since(*term_at) >= timings.cancel_grace => {
                        let _ = sys::kill_group(pid, libc::SIGKILL);
                        killed_at = Some(now);
                    }
                    Some(at) if now.duration_since(at) >= timings.kill_wait => {
                        let ending = match stopping.take() {
                            Some((Ending::Stopped { cause, .. }, _)) => Ending::Stopped {
                                cause,
                                forced: true,
                            },
                            Some((Ending::Stalled { idle, .. }, _)) => {
                                Ending::Stalled { idle, forced: true }
                            }
                            Some((other, _)) => other,
                            None => Ending::Exited,
                        };
                        return Watched::Abandoned(ending);
                    }
                    _ => {}
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::mpsc::Receiver;

        fn sh(script: &str, timings: Timings) -> RunSpec {
            RunSpec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                nice: None,
                result_record: None,
                progress_record: None,
                timings,
            }
        }

        fn fast() -> Timings {
            Timings {
                poll: Duration::from_millis(20),
                stall_warn: Duration::from_secs(600),
                stall_kill: Duration::from_secs(600),
                cancel_grace: Duration::from_millis(400),
                kill_wait: Duration::from_millis(500),
            }
        }

        fn run(spec: RunSpec) -> (RunHandle, Receiver<ExecEvent>) {
            let (events, received) = mpsc::channel();
            let handle = start_run(spec, move |event| {
                let _ = events.send(event);
            })
            .unwrap();
            (handle, received)
        }

        fn finish(events: &Receiver<ExecEvent>, within: Duration) -> (RunReport, Vec<ExecEvent>) {
            let deadline = Instant::now() + within;
            let mut seen = Vec::new();
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match events.recv_timeout(left) {
                    Ok(ExecEvent::Finished(report)) => return (*report, seen),
                    Ok(other) => seen.push(other),
                    Err(_) => panic!("no outcome within {within:?}; saw {seen:?}"),
                }
            }
        }

        fn scratch(label: &str) -> std::path::PathBuf {
            let dir =
                std::env::temp_dir().join(format!("sdsyncd-exec-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn a_clean_exit_reads_its_result_record_and_removes_it() {
            let dir = scratch("clean");
            let record = dir.join("result");
            let mut spec = sh(
                &format!(
                    "printf '%s\\n' '{{\"schema\":\"sdsync.result-record.v1\",\"profile\":\"photos\",\"status\":\"complete\",\"error\":null,\"result\":{{\"outcome\":\"partial\",\"deferred\":[{{\"path\":\"a\",\"reason\":\"x\"}}],\"skipped\":2,\"mtime_not_kept\":1}}}}' > {}; exit 3",
                    record.display()
                ),
                fast(),
            );
            spec.result_record = Some(record.clone());
            let (_handle, events) = run(spec);
            let (report, seen) = finish(&events, Duration::from_secs(10));
            assert!(matches!(seen[0], ExecEvent::Started { .. }));
            assert_eq!(report.exit, 3);
            assert_eq!(report.ending, Ending::Exited);
            assert_eq!(report.records.len(), 1);
            assert_eq!(report.records[0].deferred, Some(1));
            assert_eq!(report.records[0].skipped, Some(2));
            assert_eq!(report.records[0].facts.mtime_not_kept, Some(1));
            assert!(!record.exists());
            let verdict = classify(&report);
            assert_eq!(
                (verdict.status, verdict.outcome),
                (RunStatus::Succeeded, RunOutcome::Partial)
            );
            assert_eq!(verdict.reason, "deferred_paths");
            fs::remove_dir_all(&dir).unwrap();
        }

        /// Hang test: a fake core that ignores TERM is KILLed after the grace
        /// and read as `cancelled` (I-11).
        #[test]
        fn a_core_that_ignores_term_is_killed_after_grace_and_reads_cancelled() {
            let (handle, events) = run(sh("trap '' TERM; while :; do sleep 0.05; done", fast()));
            match events.recv_timeout(Duration::from_secs(5)).unwrap() {
                ExecEvent::Started { .. } => {}
                other => panic!("{other:?}"),
            }
            thread::sleep(Duration::from_millis(100));
            let asked = Instant::now();
            handle.stop(StopCause::Cancel);
            let (report, _) = finish(&events, Duration::from_secs(10));
            let took = asked.elapsed();
            assert_eq!(report.exit, 137);
            assert_eq!(
                report.ending,
                Ending::Stopped {
                    cause: StopCause::Cancel,
                    forced: true
                }
            );
            assert!(took >= Duration::from_millis(400), "{took:?}");
            assert!(took < Duration::from_millis(400 + 1_500), "{took:?}");
            let verdict = classify(&report);
            assert!(verdict.cancelled);
            assert_eq!(verdict.outcome, RunOutcome::Cancelled);
            assert_eq!(verdict.status, RunStatus::Failed, "I-5: never succeeded");
            assert_eq!(verdict.reason, "cancelled");
            assert!(verdict.detail.contains("killed"), "{}", verdict.detail);
        }

        #[test]
        fn a_core_that_honours_term_stops_inside_the_grace() {
            let (handle, events) = run(sh(
                "trap 'exit 130' TERM; while :; do sleep 0.05; done",
                fast(),
            ));
            let _ = events.recv_timeout(Duration::from_secs(5)).unwrap();
            thread::sleep(Duration::from_millis(100));
            handle.stop(StopCause::Shutdown);
            let (report, _) = finish(&events, Duration::from_secs(10));
            assert_eq!(
                report.ending,
                Ending::Stopped {
                    cause: StopCause::Shutdown,
                    forced: false
                }
            );
            assert_eq!(classify(&report).reason, "cancelled");
        }

        /// Hang test: a stuck child (no I/O at all) is warned about, then ended
        /// by the watchdog, and the outcome names it.
        #[test]
        fn a_stuck_child_is_named_by_the_watchdog() {
            let mut timings = fast();
            timings.stall_warn = Duration::from_millis(300);
            timings.stall_kill = Duration::from_millis(900);
            let (_handle, events) = run(sh("exec sleep 60", timings));
            let (report, seen) = finish(&events, Duration::from_secs(10));
            assert!(
                seen.iter()
                    .any(|e| matches!(e, ExecEvent::StallWarning { .. })),
                "{seen:?}"
            );
            assert!(
                matches!(report.ending, Ending::Stalled { forced: false, .. }),
                "{:?}",
                report.ending
            );
            let verdict = classify(&report);
            assert_eq!(verdict.reason, "run_stalled");
            assert_eq!(verdict.outcome, RunOutcome::Failed);
            assert!(!verdict.cancelled, "a stall consumes a retry");
            assert!(
                verdict.detail.starts_with("no progress for 1 min"),
                "{}",
                verdict.detail
            );
        }

        /// A child that keeps doing I/O is never called stalled.
        #[test]
        fn a_busy_child_is_not_stalled() {
            let mut timings = fast();
            timings.stall_warn = Duration::from_millis(200);
            timings.stall_kill = Duration::from_millis(400);
            let (_handle, events) = run(sh(
                "i=0; while [ $i -lt 40 ]; do head -c 4096 /dev/zero > /dev/null; sleep 0.03; i=$((i+1)); done",
                timings,
            ));
            let (report, seen) = finish(&events, Duration::from_secs(20));
            assert_eq!(report.ending, Ending::Exited, "{seen:?}");
            assert_eq!(report.exit, 0);
        }

        /// A SIGKILL the daemon did not send is named, not left as a bare 137.
        #[test]
        fn an_outside_kill_is_named() {
            let (_handle, events) = run(sh("kill -KILL $$", fast()));
            let (report, _) = finish(&events, Duration::from_secs(10));
            assert_eq!(report.exit, 137);
            let verdict = classify(&report);
            assert_eq!(verdict.reason, "core_failed");
            assert!(verdict.detail.contains("exit 137"), "{}", verdict.detail);
            assert!(!verdict.cancelled);
        }

        #[test]
        fn a_missing_core_is_a_named_launch_failure() {
            let mut spec = sh("", fast());
            spec.program = "/nonexistent/sdsync-core".into();
            let (_handle, events) = run(spec);
            let (report, _) = finish(&events, Duration::from_secs(5));
            assert_eq!(report.exit, -1);
            let verdict = classify(&report);
            assert_eq!(verdict.reason, "launch_failed");
            assert!(
                verdict.detail.contains("/nonexistent/sdsync-core"),
                "{}",
                verdict.detail
            );
            assert!(!verdict.core_ran);
        }

        /// I-9: the child sees exactly the environment it was given.
        #[test]
        fn child_env_is_allowlisted() {
            let dir = scratch("env");
            let out = dir.join("env");
            let mut spec = sh(&format!("env > {}", out.display()), fast());
            spec.env = allowlisted_env(vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("SDSYNC_DSM_AUDIT_ACTOR".into(), "forged".into()),
                ("SDSYNC_SECRET".into(), "x".into()),
                ("TZ".into(), "Europe/Lisbon".into()),
            ]);
            let (_handle, events) = run(spec);
            let (report, _) = finish(&events, Duration::from_secs(5));
            assert_eq!(report.exit, 0);
            let seen = fs::read_to_string(&out).unwrap();
            assert!(!seen.contains("SDSYNC_"), "{seen}");
            assert!(seen.contains("TZ=Europe/Lisbon"));
            fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn stderr_is_kept_as_a_bounded_tail_and_names_the_cause() {
            let (_handle, events) = run(sh(
                "i=0; while [ $i -lt 2000 ]; do echo \"noise line $i ................................\" >&2; i=$((i+1)); done; echo 'error: request to https://nas:5001/webapi/entry.cgi timed out after 30s' >&2; exit 1",
                fast(),
            ));
            let (report, _) = finish(&events, Duration::from_secs(20));
            assert_eq!(report.stderr_tail.len(), STDERR_TAIL_BYTES);
            let verdict = classify(&report);
            assert_eq!(verdict.reason, "core_failed");
            assert_eq!(
                verdict.detail,
                "error: request to https://nas:5001/webapi/entry.cgi timed out after 30s"
            );
        }

        #[test]
        fn the_child_has_its_own_process_group_and_an_empty_signal_mask() {
            let dir = scratch("pgid");
            let out = dir.join("status");
            let (_handle, events) = run(sh(
                &format!(
                    "cat /proc/self/stat > {0}; grep SigBlk /proc/self/status >> {0}",
                    out.display()
                ),
                fast(),
            ));
            let (report, seen) = finish(&events, Duration::from_secs(5));
            assert_eq!(report.exit, 0);
            let ExecEvent::Started { pid } = seen[0] else {
                panic!()
            };
            let text = fs::read_to_string(&out).unwrap();
            let after = text.rsplit_once(") ").unwrap().1;
            let fields: Vec<&str> = after.split_whitespace().collect();
            // pgrp is the third field after the command: the shell is its own leader.
            assert_eq!(fields[2], pid.to_string(), "{text}");
            assert!(text.contains("SigBlk:\t0000000000000000"), "{text}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(exit: i32, ending: Ending, stderr: &str) -> RunReport {
        RunReport {
            exit,
            ending,
            stderr_tail: stderr.as_bytes().to_vec(),
            records: Vec::new(),
            record_error: None,
            oom_proven: None,
            elapsed: Duration::ZERO,
            last_phase: None,
        }
    }

    #[test]
    fn exits_map_to_the_runners_vocabulary() {
        let table: &[(i32, RunStatus, RunOutcome, &str, bool)] = &[
            (0, RunStatus::Succeeded, RunOutcome::Succeeded, "ok", false),
            (
                3,
                RunStatus::Succeeded,
                RunOutcome::Partial,
                "deferred_paths",
                false,
            ),
            (75, RunStatus::Failed, RunOutcome::Busy, "busy", false),
            (
                130,
                RunStatus::Failed,
                RunOutcome::Cancelled,
                "cancelled",
                true,
            ),
            (
                143,
                RunStatus::Failed,
                RunOutcome::Cancelled,
                "cancelled",
                true,
            ),
            (
                78,
                RunStatus::Failed,
                RunOutcome::Failed,
                "supervisor_refused",
                false,
            ),
            (
                1,
                RunStatus::Failed,
                RunOutcome::Failed,
                "core_failed",
                false,
            ),
            (
                137,
                RunStatus::Failed,
                RunOutcome::Failed,
                "core_failed",
                false,
            ),
        ];
        for (exit, status, outcome, reason, cancelled) in table {
            let v = classify(&report(*exit, Ending::Exited, ""));
            assert_eq!(
                (v.status, v.outcome, v.reason.as_str(), v.cancelled),
                (*status, *outcome, *reason, *cancelled),
                "exit {exit}"
            );
            assert!(
                v.detail.is_empty() == matches!(exit, 0 | 3),
                "exit {exit}: {}",
                v.detail
            );
        }
    }

    #[test]
    fn an_oom_kill_is_named_when_the_counter_proves_it() {
        let mut r = report(137, Ending::Exited, "");
        r.oom_proven = Some(true);
        assert!(classify(&r).detail.contains("out-of-memory killer"));
        // The core's own error line wins over the guess.
        let r = report(137, Ending::Exited, "error: disk full\n");
        assert_eq!(classify(&r).detail, "error: disk full");
    }

    #[test]
    fn daemon_endings_are_named() {
        let mut r = report(
            137,
            Ending::Stalled {
                idle: Duration::from_secs(45 * 60),
                forced: true,
            },
            "",
        );
        r.last_phase = Some("upload".into());
        let v = classify(&r);
        assert_eq!(v.reason, "run_stalled");
        assert_eq!(
            v.detail,
            "no progress for 45 min (last phase upload); the run was stopped and killed"
        );
        let v = classify(&report(
            137,
            Ending::Unkillable {
                after: Box::new(Ending::Stopped {
                    cause: StopCause::Shutdown,
                    forced: true,
                }),
            },
            "",
        ));
        assert!(v.cancelled);
        assert!(v.detail.starts_with("run_unkillable: "), "{}", v.detail);
        let v = classify(&report(
            -1,
            Ending::SpawnFailed {
                cause: "x: not found".into(),
            },
            "",
        ));
        assert_eq!((v.reason.as_str(), v.core_ran), ("launch_failed", false));
    }

    #[test]
    fn stderr_cause_prefers_the_error_line() {
        assert_eq!(
            stderr_cause(b"info\nerror: boom\ntrailing note\n"),
            "error: boom"
        );
        assert_eq!(stderr_cause(b"one\ntwo\n\n"), "two");
        assert_eq!(stderr_cause(b""), "");
    }

    #[test]
    fn result_records_parse_and_bad_ones_are_named() {
        let records = parse_result_record(
            b"{\"schema\":\"sdsync.result-record.v1\",\"profile\":\"a\",\"status\":\"failed\",\"error\":{\"code\":\"timeout\",\"detail\":\"timed out\",\"path\":null},\"result\":null}\n\
              {\"schema\":\"other\"}\n\n",
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].error_code.as_deref(), Some("timeout"));
        assert_eq!(records[0].deferred, None);
        assert!(
            parse_result_record(b"{not json\n")
                .unwrap_err()
                .contains("line 1")
        );
    }

    #[test]
    fn the_env_allowlist_drops_everything_else() {
        let env = allowlisted_env(vec![
            ("PATH".into(), "/bin".into()),
            ("SDSYNC_X".into(), "1".into()),
            ("LD_PRELOAD".into(), "/evil.so".into()),
            ("HOME".into(), "/var/packages/x/home".into()),
        ]);
        let keys: Vec<_> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(keys, ["PATH", "HOME"]);
    }
}
