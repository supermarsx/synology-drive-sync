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

use super::state::{IndexFacts, ResultFacts, RunOutcome, RunStatus, sanitize_detail};

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
    /// The run's mandatory audit (I-7), opened by the supervisor before the
    /// core starts and completed after it ends; `None` runs unaudited (tests).
    pub(crate) audit: Option<AuditStep>,
    /// The runner's pre-core checks (daemon/preflight.rs), run by the
    /// supervisor first; a refusal never starts the core.
    pub(crate) preflight: Option<PreflightStep>,
    /// The DSM session channel (daemon/session.rs): the core's standard
    /// input, settled before the run is reported finished. `None`: stdin is
    /// `/dev/null` and the core logs in and out itself.
    pub(crate) session: Option<super::session::Lease>,
}

/// A run's pre-core check: `Err((reason, detail))` refuses it.
#[derive(Clone)]
pub(crate) struct PreflightStep(
    pub(crate) std::sync::Arc<dyn Fn() -> Result<(), (&'static str, String)> + Send + Sync>,
);

impl std::fmt::Debug for PreflightStep {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PreflightStep")
    }
}

/// What a run's audit records.
#[derive(Clone)]
pub(crate) struct AuditStep {
    pub(crate) audit: std::sync::Arc<dyn super::audit::RunAudit>,
    /// `run` or `plan`.
    pub(crate) operation: &'static str,
    pub(crate) scope: String,
    pub(crate) origin: super::audit::AuditOrigin,
}

impl std::fmt::Debug for AuditStep {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "AuditStep({} {} {:?})",
            self.operation, self.scope, self.origin
        )
    }
}

impl AuditStep {
    /// Reconcile, begin and execute: the transaction id, or why the run must
    /// not start.
    pub(crate) fn open(&self) -> Result<String, String> {
        self.audit
            .reconcile()
            .map_err(|cause| format!("the audit outbox could not be reconciled: {cause}"))?;
        let transaction = self
            .audit
            .begin(self.operation, &self.scope, self.origin)
            .map_err(|cause| format!("the run's audit could not be opened: {cause}"))?;
        if let Err(cause) = self.audit.execute(&transaction) {
            // S3-7: a completion that fails too is part of the cause.
            let completed = match self.audit.complete(&transaction, "failed") {
                Ok(()) => String::new(),
                Err(also) => format!("; completing it as failed failed too: {also}"),
            };
            return Err(format!(
                "the run's audit could not record its start: {cause}{completed}"
            ));
        }
        Ok(transaction)
    }

    /// Complete with the run's final state; why not, when it could not be.
    pub(crate) fn close(&self, transaction: &str, report: &RunReport) -> Option<String> {
        let state = match report.ending {
            // Abandoned: whether it changed anything more is not known.
            Ending::Unkillable { .. } => "outcome_unknown",
            _ if classify(report).status == RunStatus::Succeeded => "succeeded",
            _ => "failed",
        };
        self.audit
            .complete(transaction, state)
            .err()
            .map(|cause| format!("the run's audit could not be completed ({state}): {cause}"))
    }
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
    /// It was not started, for a named reason (`audit_unavailable`).
    Refused { reason: &'static str, cause: String },
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
    /// For an exit 137 the daemon did not cause: the memory a run could use
    /// when it was killed (the core's own budget probe), named in the detail.
    pub(crate) memory_note: Option<String>,
    /// The run's audit could not be completed (named; reconciliation follows).
    pub(crate) audit_error: Option<String>,
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
                index: IndexFacts::from_value(&result["pair_index"]),
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
        Ending::Refused { reason, cause } => (RunOutcome::Failed, reason, cause.clone(), false),
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
        let core_ran = !matches!(
            report.ending,
            Ending::SpawnFailed { .. } | Ending::Refused { .. }
        );
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
            // Named as a kill, never as an ordinary core failure (the runner's
            // reasons): `core_oom_killed` when this cgroup's OOM counter moved,
            // `core_killed` when nothing proves who sent the SIGKILL.
            let proven = report.oom_proven == Some(true);
            let mut named = if cause.starts_with("error:") || cause.starts_with("Error:") {
                cause
            } else if proven {
                "the sync core was killed by the out-of-memory killer (exit 137); the source may hold more entries than this NAS has memory for".into()
            } else {
                "the sync core was killed (SIGKILL, exit 137) without reporting an error; out of memory? check the DSM system log for the OOM killer".into()
            };
            if let Some(note) = &report.memory_note {
                named = format!("{named}; {note}");
            }
            verdict(
                RunStatus::Failed,
                RunOutcome::Failed,
                if proven {
                    "core_oom_killed"
                } else {
                    "core_killed"
                },
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

    /// What a run could use when it was killed: the core's own budget probe
    /// (the tighter of the cgroup's headroom and `MemAvailable`), read now.
    fn memory_note() -> String {
        match synology_drive_sync::memory_budget::probe(None) {
            Ok(probe) => format!(
                "a run could use about {} MiB ({})",
                probe.available / (1024 * 1024),
                match probe.source {
                    synology_drive_sync::memory_budget::BudgetSource::Operator => "operator budget",
                    synology_drive_sync::memory_budget::BudgetSource::Kernel => {
                        "cgroup headroom and MemAvailable"
                    }
                }
            ),
            Err(cause) => format!("the memory a run could use is unknown ({cause})"),
        }
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
        let refused = |reason: &'static str, cause: String| {
            sink(ExecEvent::Finished(Box::new(RunReport {
                exit: -1,
                ending: Ending::Refused { reason, cause },
                stderr_tail: Vec::new(),
                records: Vec::new(),
                record_error: None,
                oom_proven: None,
                memory_note: None,
                audit_error: None,
                last_phase: None,
            })));
        };
        // H8: a cancel or stop that arrived before the core is spawned is
        // honoured then: the core never starts.
        let stopped_before_start = |requests: &mpsc::Receiver<StopCause>| match requests.try_recv()
        {
            Ok(cause) => Some(cause),
            Err(mpsc::TryRecvError::Disconnected) => Some(StopCause::Shutdown),
            Err(mpsc::TryRecvError::Empty) => None,
        };
        let never_started = |cause: StopCause| {
            sink(ExecEvent::Finished(Box::new(RunReport {
                exit: 130,
                ending: Ending::Stopped {
                    cause,
                    forced: false,
                },
                stderr_tail: Vec::new(),
                records: Vec::new(),
                record_error: None,
                oom_proven: None,
                memory_note: None,
                audit_error: None,
                last_phase: None,
            })));
        };
        if let Some(PreflightStep(check)) = &spec.preflight
            && let Err((reason, cause)) = check()
        {
            refused(reason, cause);
            return;
        }
        if let Some(cause) = stopped_before_start(&requests) {
            never_started(cause);
            return;
        }
        // I-7: the audit is opened before the core starts; a run whose audit
        // cannot be opened does not start.
        let transaction = match &spec.audit {
            Some(step) => match step.open() {
                Ok(transaction) => Some(transaction),
                Err(cause) => {
                    refused("audit_unavailable", cause);
                    return;
                }
            },
            None => None,
        };
        let finished = |exit, ending, tail: Vec<u8>, records, record_error, oom, phase| {
            // The session comes back (or is ended) before anyone hears the run
            // finished, so the next run of the profile can borrow it.
            if let Some(lease) = &spec.session {
                lease.settle();
            }
            let mut report = RunReport {
                exit,
                ending,
                stderr_tail: tail,
                records,
                record_error,
                oom_proven: oom,
                memory_note: (exit == 137).then(memory_note),
                audit_error: None,
                last_phase: phase,
            };
            if let (Some(step), Some(transaction)) = (&spec.audit, &transaction) {
                report.audit_error = step.close(transaction, &report);
            }
            // The progress record is the run's own; it goes with it.
            if let Some(path) = &spec.progress_record {
                let _ = fs::remove_file(path);
            }
            sink(ExecEvent::Finished(Box::new(report)));
        };
        // H8: the audit is open; a stop that came meanwhile still ends the run
        // before the core starts (the audit is completed as failed).
        if let Some(cause) = stopped_before_start(&requests) {
            finished(
                130,
                Ending::Stopped {
                    cause,
                    forced: false,
                },
                Vec::new(),
                Vec::new(),
                None,
                None,
                None,
            );
            return;
        }
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(match &spec.session {
                Some(lease) => lease.stdin(),
                None => Stdio::null(),
            })
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let parent = sys::getpid();
        let nice = spec.nice;
        // SAFETY: child_setup is async-signal-safe and allocates nothing.
        unsafe {
            command.pre_exec(move || sys::child_setup(parent, nice));
        }
        let oom_before = oom_kill_count();
        let spawned = command.spawn();
        // The command keeps the child's end of every pipe it was given; the
        // session channel must see the child's own close, not wait for ours.
        drop(command);
        let mut child = match spawned {
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
                audit: None,
                preflight: None,
                session: None,
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

        /// Records the audit steps; fails the one named in `fail`.
        struct Recording {
            calls: Mutex<Vec<String>>,
            fail: &'static str,
        }

        impl crate::dsm_api::daemon::audit::RunAudit for Recording {
            fn reconcile(&self) -> Result<(), String> {
                self.step("reconcile".into())
            }
            fn begin(
                &self,
                operation: &str,
                scope: &str,
                origin: crate::dsm_api::daemon::audit::AuditOrigin,
            ) -> Result<String, String> {
                self.step(format!("begin {operation} {scope} {}", origin.origin()))
                    .map(|()| "tx1".into())
            }
            fn execute(&self, transaction: &str) -> Result<(), String> {
                self.step(format!("execute {transaction}"))
            }
            fn complete(&self, transaction: &str, state: &str) -> Result<(), String> {
                self.step(format!("complete {transaction} {state}"))
            }
        }

        impl Recording {
            fn step(&self, call: String) -> Result<(), String> {
                let failing = call.starts_with(self.fail);
                self.calls.lock().unwrap().push(call);
                if failing {
                    Err("the outbox is unwritable".into())
                } else {
                    Ok(())
                }
            }
        }

        fn audited(script: &str, fail: &'static str) -> (RunSpec, Arc<Recording>) {
            let audit = Arc::new(Recording {
                calls: Mutex::new(Vec::new()),
                fail,
            });
            let mut spec = sh(script, fast());
            spec.audit = Some(AuditStep {
                audit: audit.clone(),
                operation: "run",
                scope: "photos".into(),
                origin: crate::dsm_api::daemon::audit::AuditOrigin::Scheduler,
            });
            (spec, audit)
        }

        /// H8: a cancel or stop that arrives while the preflight (or the audit)
        /// runs is honoured before the core is spawned: the core never starts,
        /// and the run ends as stopped at once rather than as an unkillable
        /// core the shutdown waited for.
        #[test]
        fn a_stop_during_preflight_never_spawns_the_core() {
            let dir = scratch("stop-preflight");
            let marker = dir.join("ran");
            let mut spec = sh(&format!("touch {}", marker.display()), fast());
            spec.preflight = Some(PreflightStep(Arc::new(|| {
                thread::sleep(Duration::from_millis(400));
                Ok(())
            })));
            let (handle, events) = run(spec);
            thread::sleep(Duration::from_millis(100));
            handle.stop(StopCause::Cancel);
            let (report, seen) = finish(&events, Duration::from_secs(10));
            assert!(
                matches!(
                    report.ending,
                    Ending::Stopped {
                        cause: StopCause::Cancel,
                        forced: false
                    }
                ),
                "{:?}",
                report.ending
            );
            assert!(
                !seen.iter().any(|e| matches!(e, ExecEvent::Started { .. })),
                "{seen:?}"
            );
            thread::sleep(Duration::from_millis(200));
            assert!(!marker.exists(), "the core was spawned after the stop");
            let _ = fs::remove_dir_all(&dir);
        }

        /// I-7: the audit is opened before the core starts and completed with
        /// the run's state; a run whose audit cannot be opened never starts.
        #[test]
        fn a_run_is_audited_and_never_starts_unaudited() {
            let dir = scratch("audit");
            let marker = dir.join("ran");
            let script = format!("touch {}", marker.display());
            let (spec, audit) = audited(&script, "none");
            let (_handle, events) = run(spec);
            let (report, _) = finish(&events, Duration::from_secs(10));
            assert_eq!(report.exit, 0);
            assert!(marker.exists());
            assert_eq!(
                *audit.calls.lock().unwrap(),
                [
                    "reconcile",
                    "begin run photos scheduler",
                    "execute tx1",
                    "complete tx1 succeeded"
                ]
            );
            fs::remove_file(&marker).unwrap();
            let (spec, audit) = audited(&script, "begin");
            let (_handle, events) = run(spec);
            let (report, _) = finish(&events, Duration::from_secs(10));
            let verdict = classify(&report);
            assert_eq!(verdict.reason, "audit_unavailable");
            assert!(
                verdict.detail.contains("could not be opened"),
                "{}",
                verdict.detail
            );
            assert!(!verdict.core_ran);
            assert!(!marker.exists(), "an unaudited run started");
            assert!(
                !audit
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| c.starts_with("execute"))
            );
            // A failed execute step completes the begun record as failed.
            let (spec, audit) = audited(&script, "execute");
            let (_handle, events) = run(spec);
            let (report, _) = finish(&events, Duration::from_secs(10));
            assert_eq!(classify(&report).reason, "audit_unavailable");
            assert!(!marker.exists());
            assert_eq!(
                audit.calls.lock().unwrap().last().map(String::as_str),
                Some("complete tx1 failed")
            );
            // A failed completion is reported, not swallowed.
            let (spec, _) = audited("exit 1", "complete");
            let (_handle, events) = run(spec);
            let (report, _) = finish(&events, Duration::from_secs(10));
            assert!(
                report
                    .audit_error
                    .as_deref()
                    .is_some_and(|e| e.contains("(failed)")),
                "{:?}",
                report.audit_error
            );
            let _ = fs::remove_dir_all(&dir);
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
            // The child writes every 30 ms for about 7 s, longer than the kill window, so a stall
            // check that failed to count its I/O would kill it. Each turn of its loop starts
            // `head` and `sleep`, programs a loaded runner has to read from a busy disk: 400 ms,
            // then 1.5 s, was spent waiting for one of them in a full run now and then (the
            // child waiting, not the check miscounting), so the window is 6 s.
            let mut timings = fast();
            timings.stall_warn = Duration::from_secs(3);
            timings.stall_kill = Duration::from_secs(6);
            let (_handle, events) = run(sh(
                "i=0; while [ $i -lt 230 ]; do head -c 4096 /dev/zero > /dev/null; sleep 0.03; i=$((i+1)); done",
                timings,
            ));
            let (report, seen) = finish(&events, Duration::from_secs(40));
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
            // No OOM counter moved in a test container: a plain kill, named.
            assert_eq!(verdict.reason, "core_killed");
            assert!(verdict.detail.contains("exit 137"), "{}", verdict.detail);
            assert!(
                report.memory_note.is_some(),
                "a 137 names the memory a run had"
            );
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
            memory_note: None,
            audit_error: None,
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
                "core_killed",
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
        r.memory_note = Some("a run could use about 412 MiB (kernel)".into());
        let v = classify(&r);
        assert_eq!(v.reason, "core_oom_killed");
        assert!(v.detail.contains("out-of-memory killer"), "{}", v.detail);
        assert!(
            v.detail
                .ends_with("; a run could use about 412 MiB (kernel)"),
            "{}",
            v.detail
        );
        // Unproven: a plain kill, named as one, never `core_failed`.
        r.oom_proven = None;
        assert_eq!(classify(&r).reason, "core_killed");
        // The core's own error line wins over the guess for the detail.
        let r = report(137, Ending::Exited, "error: disk full\n");
        let v = classify(&r);
        assert_eq!(
            (v.reason.as_str(), v.detail.as_str()),
            ("core_killed", "error: disk full")
        );
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

    /// The index report of a run reaches the daemon: entries, hits, misses, the
    /// canary, and the most serious issue named (a corrupt index set aside).
    #[test]
    fn the_pair_index_report_is_read_with_its_worst_issue() {
        let records = parse_result_record(
            br#"{"schema":"sdsync.result-record.v1","profile":"photos","status":"succeeded","error":null,"result":{"pair_index":{"state":"cold","served":0,"verified_live":60,"racy":0,"expired":0,"canary_checked":0,"canary_inconclusive":0,"canary_mismatches":0,"oldest_evidence_epoch":null,"remote_change_signal":"ctime","stored":60,"removed":0,"entries":60,"issues":[{"code":"pair-index-retired","severity":"info","path":"/x","cause":"bound elsewhere"},{"code":"pair-index-corrupt","severity":"warning","path":"/var/state/cache/pairs/photos.redb","cause":"integrity check failed; set aside as photos.redb.corrupt-1"}]}}}
{"schema":"sdsync.result-record.v1","profile":"docs","status":"succeeded","error":null,"result":{}}
"#,
        )
        .unwrap();
        let index = records[0].facts.index.clone().unwrap();
        assert_eq!(index.state, "cold");
        assert_eq!((index.entries, index.hits, index.misses), (Some(60), 0, 60));
        assert_eq!(index.issue.as_deref(), Some("pair-index-corrupt"));
        assert!(
            index
                .issue_detail
                .as_deref()
                .is_some_and(|d| d.starts_with("/var/state/cache/pairs/photos.redb: integrity")),
            "{index:?}"
        );
        assert_eq!(records[1].facts.index, None, "no index, no line");
        // A retired index alone is named too (info): it explains a slow run.
        let records = parse_result_record(
            br#"{"schema":"sdsync.result-record.v1","profile":"photos","status":"succeeded","error":null,"result":{"pair_index":{"state":"cold","served":0,"verified_live":5,"canary_checked":0,"canary_mismatches":0,"stored":5,"removed":0,"entries":5,"issues":[{"code":"pair-index-not-applicable","severity":"info","path":"/x","cause":"n/a"},{"code":"pair-index-retired","severity":"info","path":"/p/photos.redb","cause":"bound to remote \"/a\", this run uses \"/b\"; retired"}]}}}
"#,
        )
        .unwrap();
        let index = records[0].facts.index.clone().unwrap();
        assert_eq!(index.issue.as_deref(), Some("pair-index-retired"));
        assert!(index.issue_detail.unwrap().contains("bound to remote"));
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
