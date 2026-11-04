//! The daemon's state files: one atomic `key=value` writer and the renderers for
//! `run.state`, `profiles/<p>.state`, `controller.state` and `schedule.state`.
//!
//! Every document is written with the same keys, in the same order, with the
//! same value rules as the shell writers (`write_run_state`,
//! `write_profile_outcome`, `write_controller_state`, `save_schedule_deadline`),
//! so the bridge's snapshot and outcome readers and a rolled-back shell read
//! them unchanged (design §3.1, §3.4). The golden tests at the bottom prove it
//! against the bridge's own readers.
//!
//! A write is temp + rename in the target's directory, `0600`, never through a
//! symlink, and every failure names its path, the step and the system's cause.

#![forbid(unsafe_code)]

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::routines::{KvDocument, valid_name, valid_token};

/// A state write that did not land. The old document is still in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WriteError {
    pub(crate) path: PathBuf,
    /// `refuse_symlink`, `create`, `write`, `sync`, `chmod`, `rename`, `timeout`.
    pub(crate) step: &'static str,
    pub(crate) cause: String,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "path={} step={} cause={}",
            self.path.display(),
            self.step,
            self.cause
        )
    }
}

fn io_cause(error: &io::Error) -> String {
    // The system's words, one line.
    error.to_string().replace(['\n', '\r'], " ")
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically: a private temporary in the same
/// directory, flushed, then renamed over the target. A symlink at the target
/// is refused rather than followed or replaced.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    let fail = |step: &'static str, error: &io::Error| WriteError {
        path: path.to_path_buf(),
        step,
        cause: io_cause(error),
    };
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(WriteError {
                path: path.to_path_buf(),
                step: "refuse_symlink",
                cause: "the target is a symbolic link".into(),
            });
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(WriteError {
                path: path.to_path_buf(),
                step: "refuse_symlink",
                cause: "the target is not a regular file".into(),
            });
        }
        _ => {}
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    // create_new is O_CREAT|O_EXCL: it never follows a symlink planted at the
    // temporary name.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| fail("create", &error))?;
    let result = (|| {
        file.write_all(bytes)
            .map_err(|error| fail("write", &error))?;
        file.sync_data().map_err(|error| fail("sync", &error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                .map_err(|error| fail("chmod", &error))?;
        }
        drop(file);
        fs::rename(&temporary, path).map_err(|error| fail("rename", &error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// The default bound on one state write (design §2.5, layer 4).
pub(crate) const STATE_WRITE_DEADLINE: Duration = Duration::from_secs(10);

type WriteJob = (PathBuf, Vec<u8>, mpsc::Sender<Result<(), WriteError>>);

/// Writes state files on a thread of its own, so the control thread waits at
/// most a deadline for a volume that stopped answering. A write that times out
/// is still in flight; the caller holds the record and writes it again later.
pub(crate) struct StateWriter {
    jobs: mpsc::Sender<WriteJob>,
}

impl StateWriter {
    pub(crate) fn spawn() -> io::Result<Self> {
        let (jobs, receiver) = mpsc::channel::<WriteJob>();
        thread::Builder::new()
            .name("sdsyncd-state".into())
            .spawn(move || {
                for (path, bytes, reply) in receiver {
                    let _ = reply.send(atomic_write(&path, &bytes));
                }
            })?;
        Ok(Self { jobs })
    }

    pub(crate) fn write(
        &self,
        path: &Path,
        bytes: Vec<u8>,
        deadline: Duration,
    ) -> Result<(), WriteError> {
        let (reply, answer) = mpsc::channel();
        let gone = |cause: &str| WriteError {
            path: path.to_path_buf(),
            step: "timeout",
            cause: cause.into(),
        };
        self.jobs
            .send((path.to_path_buf(), bytes, reply))
            .map_err(|_| gone("the state writer thread is gone"))?;
        match answer.recv_timeout(deadline) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(gone(&format!("no answer within {} s", deadline.as_secs())))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(gone("the state writer thread is gone"))
            }
        }
    }
}

// ---------------------------------------------------------------- value rules

/// `run_state_token`: a reason is a token, never prose.
pub(crate) fn token_or_none(value: &str) -> &str {
    if valid_token(value) { value } else { "none" }
}

/// `runner_sanitize_detail`: the first line, every byte outside printable
/// ASCII turned into `?`, `|` into `/`, at most `max` bytes.
pub(crate) fn sanitize_detail(text: &str, max: usize) -> String {
    let line = text.split('\n').next().unwrap_or_default();
    let mut out: String = line
        .bytes()
        .map(|b| match b {
            b'|' => '/',
            0x20..=0x7e => char::from(b),
            _ => '?',
        })
        .collect();
    out.truncate(max);
    out
}

fn printable_line(value: &str) -> bool {
    value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

// ---------------------------------------------------------------- run.state

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

impl RunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// `run_state_outcome`: `succeeded|partial|failed|busy|cancelled`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunOutcome {
    Succeeded,
    Partial,
    Failed,
    Busy,
    Cancelled,
}

impl RunOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Busy => "busy",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The result facts (contract v1.5) a run or profile record may carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResultFacts {
    pub(crate) incomplete_listings: Option<u64>,
    pub(crate) incomplete_listing_folders: Option<String>,
    pub(crate) mtime_not_kept: Option<u64>,
    pub(crate) reauthentication_failures: Option<u64>,
    pub(crate) server_tasks_not_stopped: Option<u64>,
    pub(crate) keepalive_failures: Option<u64>,
    pub(crate) link_guard_unchecked: Option<u64>,
    /// What the verified-pair index did (`--pair-index`), when the run used it.
    pub(crate) index: Option<IndexFacts>,
}

/// The verified-pair index line of a profile record (`index_*` keys): what the
/// dashboard and `ctl status` show as `index:`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct IndexFacts {
    /// `off`, `cold`, `warm`, `refreshed`, `unusable` or `local-only`.
    pub(crate) state: String,
    /// Pairs the index holds after the run (unknown when it was never opened).
    pub(crate) entries: Option<u64>,
    /// Pairs served from the index / verified live.
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) canary_checked: u64,
    pub(crate) canary_mismatches: u64,
    /// The most serious issue the run named (`pair-index-corrupt`, ...), with
    /// its path and cause on one line. Info-level notes are not issues here.
    pub(crate) issue: Option<String>,
    pub(crate) issue_detail: Option<String>,
}

impl IndexFacts {
    /// From a result record's `pair_index` member; `None` when absent.
    pub(crate) fn from_value(value: &serde_json::Value) -> Option<Self> {
        let state = value.get("state")?.as_str()?;
        if !valid_index_state(state) {
            return None;
        }
        let number = |key: &str| value.get(key).and_then(serde_json::Value::as_u64);
        let rank = |severity: &str| match severity {
            "error" => 3,
            "warning" => 2,
            _ => 0,
        };
        let worst = value
            .get("issues")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|issue| {
                let code = issue.get("code")?.as_str()?;
                let severity = rank(issue.get("severity")?.as_str()?);
                (severity > 0 && index_issue_token(code)).then(|| {
                    let text = |key: &str| {
                        issue
                            .get(key)
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                    };
                    let detail =
                        sanitize_detail(&format!("{}: {}", text("path"), text("cause")), 160);
                    (severity, code.to_owned(), detail)
                })
            })
            .max_by_key(|(severity, _, _)| *severity);
        Some(Self {
            state: state.to_owned(),
            entries: number("entries"),
            hits: number("served").unwrap_or(0),
            misses: number("verified_live").unwrap_or(0),
            canary_checked: number("canary_checked").unwrap_or(0),
            canary_mismatches: number("canary_mismatches").unwrap_or(0),
            issue: worst.as_ref().map(|(_, code, _)| code.clone()),
            issue_detail: worst.map(|(_, _, detail)| detail),
        })
    }

    fn render(&self, out: &mut String) {
        out.push_str(&format!("index_state={}\n", self.state));
        if let Some(entries) = self.entries {
            out.push_str(&format!("index_entries={entries}\n"));
        }
        out.push_str(&format!(
            "index_hits={}\nindex_misses={}\nindex_canary_checked={}\nindex_canary_mismatches={}\n",
            self.hits, self.misses, self.canary_checked, self.canary_mismatches
        ));
        if let Some(issue) = &self.issue {
            out.push_str(&format!("index_issue={issue}\n"));
        }
        if let Some(detail) = self.issue_detail.as_deref().filter(|d| !d.is_empty()) {
            out.push_str(&format!("index_issue_detail={detail}\n"));
        }
    }
}

/// `ctl status`'s `index:` entry of one profile, from its record: `null` when the
/// last run did not use the index, the counts and the worst issue otherwise. A
/// malformed line is named (`error`), never read as "no index".
pub(crate) fn index_status_value(record: &[u8]) -> serde_json::Value {
    use serde_json::{Value, json};
    let text = String::from_utf8_lossy(record);
    let mut keys: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for line in text.lines().take(256) {
        if let Some((key, value)) = line.split_once('=')
            && key.starts_with("index_")
            && !value.is_empty()
        {
            keys.entry(key).or_insert(value);
        }
    }
    let Some(state) = keys.get("index_state").copied() else {
        return Value::Null;
    };
    if !valid_index_state(state) {
        return json!({"error": format!("index_state={state} is not an index state")});
    }
    let number = |key: &str| keys.get(key).and_then(|value| value.parse::<u64>().ok());
    json!({
        "state": state,
        "entries": number("index_entries"),
        "hits": number("index_hits"),
        "misses": number("index_misses"),
        "canary_checked": number("index_canary_checked"),
        "canary_mismatches": number("index_canary_mismatches"),
        "refreshed": state == "refreshed",
        "issue": keys.get("index_issue").filter(|code| index_issue_token(code)),
        "issue_detail": keys.get("index_issue_detail"),
    })
}

/// The index states the core names.
pub(crate) fn valid_index_state(state: &str) -> bool {
    matches!(
        state,
        "off" | "cold" | "warm" | "refreshed" | "unusable" | "local-only"
    )
}

/// An index issue code: `pair-index-` and lowercase words.
pub(crate) fn index_issue_token(code: &str) -> bool {
    code.len() <= 64
        && code.starts_with("pair-index-")
        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
}

impl ResultFacts {
    /// `write_result_facts`: each written only when the result carried it.
    fn render(&self, out: &mut String) {
        let folders = self
            .incomplete_listing_folders
            .as_deref()
            .filter(|folders| !folders.is_empty() && printable_line(folders));
        let lines: [(&str, Option<String>); 7] = [
            (
                "incomplete_listings",
                self.incomplete_listings.map(|v| v.to_string()),
            ),
            ("incomplete_listing_folders", folders.map(str::to_owned)),
            ("mtime_not_kept", self.mtime_not_kept.map(|v| v.to_string())),
            (
                "recovered_reauthentication_failures",
                self.reauthentication_failures.map(|v| v.to_string()),
            ),
            (
                "recovered_server_tasks_not_stopped",
                self.server_tasks_not_stopped.map(|v| v.to_string()),
            ),
            (
                "recovered_keepalive_failures",
                self.keepalive_failures.map(|v| v.to_string()),
            ),
            (
                "link_guard_unchecked",
                self.link_guard_unchecked.map(|v| v.to_string()),
            ),
        ];
        for (key, value) in lines {
            if let Some(value) = value {
                out.push_str(&format!("{key}={value}\n"));
            }
        }
        if let Some(index) = &self.index {
            index.render(out);
        }
    }
}

/// One `run.state` document (`write_run_state` plus `write_run_state_extras`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunRecord {
    pub(crate) status: RunStatus,
    pub(crate) started: u64,
    pub(crate) finished: u64,
    /// `-1` while running or when the core never finished.
    pub(crate) exit: i64,
    /// A profile name or `all`.
    pub(crate) scope: String,
    /// `sync` or `plan`.
    pub(crate) operation: &'static str,
    pub(crate) reason: String,
    pub(crate) detail: String,
    pub(crate) outcome: Option<RunOutcome>,
    pub(crate) deferred: Option<u64>,
    pub(crate) skipped: Option<u64>,
    pub(crate) counts_unavailable: bool,
    /// Record sinks that failed for this run (`activity`, `alert`, `profile_state`, …).
    pub(crate) degraded: Vec<&'static str>,
    pub(crate) heartbeat: Option<u64>,
    /// `io` or `liveness`.
    pub(crate) heartbeat_source: Option<&'static str>,
    pub(crate) facts: ResultFacts,
}

impl RunRecord {
    pub(crate) fn running(scope: &str, operation: &'static str, started: u64) -> Self {
        Self {
            status: RunStatus::Running,
            started,
            finished: 0,
            exit: -1,
            scope: scope.to_owned(),
            operation,
            reason: "none".into(),
            detail: String::new(),
            outcome: None,
            deferred: None,
            skipped: None,
            counts_unavailable: false,
            degraded: Vec::new(),
            heartbeat: None,
            heartbeat_source: None,
            facts: ResultFacts::default(),
        }
    }
}

/// `load_run_state_aggregate`: the last `all` sync's own copy, carried forward
/// by every other writer and re-validated rather than trusted.
fn carried_aggregate(previous: Option<&[u8]>) -> (String, u64, i64, String) {
    let doc = previous.map_or_else(KvDocument::empty, |bytes| KvDocument::parse(bytes, 64));
    let state = match doc.get("aggregate_state") {
        Some(value @ ("never" | "running" | "succeeded" | "failed")) => value.to_owned(),
        _ => "never".into(),
    };
    let finished = doc
        .get("aggregate_finished_epoch")
        .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let exit = match doc.get("aggregate_exit_code") {
        Some("-1") | None => -1,
        Some(value) if value.bytes().all(|b| b.is_ascii_digit()) => value.parse().unwrap_or(-1),
        Some(_) => -1,
    };
    let reason = doc
        .get("aggregate_reason")
        .filter(|value| valid_token(value))
        .unwrap_or("none")
        .to_owned();
    (state, finished, exit, reason)
}

/// Render `run.state`. `previous` is the current document, whose aggregate
/// fields are carried forward unless this is itself an aggregate sync.
pub(crate) fn render_run_state(record: &RunRecord, previous: Option<&[u8]>) -> Vec<u8> {
    let reason = token_or_none(&record.reason);
    let detail = if printable_line(&record.detail)
        && !record.detail.contains('|')
        && record.detail.len() <= 160
    {
        record.detail.as_str()
    } else {
        ""
    };
    let scope = if record.scope == "all" || valid_name(&record.scope) {
        record.scope.as_str()
    } else {
        "none"
    };
    let (agg_state, agg_finished, agg_exit, agg_reason) =
        if record.scope == "all" && record.operation != "plan" {
            (
                record.status.as_str().to_owned(),
                record.finished,
                record.exit.max(-1),
                reason.to_owned(),
            )
        } else {
            carried_aggregate(previous)
        };
    let mut out = format!(
        "state={}\nstarted_epoch={}\nfinished_epoch={}\nexit_code={}\nscope={scope}\n\
         operation={}\nreason={reason}\ndetail={detail}\naggregate_state={agg_state}\n\
         aggregate_finished_epoch={agg_finished}\naggregate_exit_code={agg_exit}\n\
         aggregate_reason={agg_reason}\n",
        record.status.as_str(),
        record.started,
        record.finished,
        record.exit.max(-1),
        record.operation,
    );
    if let Some(outcome) = record.outcome {
        out.push_str(&format!("outcome={}\n", outcome.as_str()));
    }
    if let Some(deferred) = record.deferred {
        out.push_str(&format!("deferred={deferred}\n"));
    }
    if let Some(skipped) = record.skipped {
        out.push_str(&format!("skipped={skipped}\n"));
    }
    if record.counts_unavailable {
        out.push_str("counts=unavailable\n");
    }
    let degraded = record
        .degraded
        .iter()
        .copied()
        .filter(|sink| {
            !sink.is_empty() && sink.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        })
        .collect::<Vec<_>>();
    if !degraded.is_empty() {
        out.push_str(&format!(
            "degraded={}\ndegraded_reason={}_write_failed\n",
            degraded.join(","),
            degraded[0]
        ));
    }
    if record.status == RunStatus::Running {
        if let Some(heartbeat) = record.heartbeat {
            out.push_str(&format!("heartbeat_epoch={heartbeat}\n"));
        }
        if let Some(source) = record.heartbeat_source {
            out.push_str(&format!("heartbeat_source={source}\n"));
        }
    }
    record.facts.render(&mut out);
    out.into_bytes()
}

// ---------------------------------------------------------------- profiles/<p>.state

/// What one run did for one profile (`write_profile_outcome`'s OUTCOME).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProfileOutcome {
    Succeeded,
    Partial,
    Failed,
    /// The run never reached this profile: keep its previous completion.
    NotRun,
    /// An aggregate whose per-profile result could not be read.
    Unknown,
}

impl ProfileOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::NotRun => "not_run",
            Self::Unknown => "unknown",
        }
    }
}

/// One profile's record of the run that just ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProfileRecord {
    pub(crate) outcome: ProfileOutcome,
    pub(crate) exit: i64,
    pub(crate) reason: String,
    pub(crate) finished: u64,
    /// The run was an aggregate (`scope=all`) rather than this profile alone.
    pub(crate) aggregate: bool,
    /// Counts from the core's result, when it finished and they could be read.
    pub(crate) deferred: Option<u64>,
    pub(crate) skipped: Option<u64>,
    /// The core ran (so missing counts are `counts=unavailable`).
    pub(crate) core_ran: bool,
    pub(crate) facts: ResultFacts,
    /// Failed or unknown only: the cause (≤ 240 bytes) and the path it concerns.
    pub(crate) detail: Option<String>,
    pub(crate) failed_path: Option<String>,
}

/// `read_profile_completion_state`.
struct PreviousCompletion {
    state: String,
    finished: u64,
    exit: i64,
    action: String,
    failures: u64,
    success: u64,
}

fn previous_completion(previous: Option<&[u8]>) -> PreviousCompletion {
    let doc = previous.map_or_else(KvDocument::empty, |bytes| KvDocument::parse(bytes, 64));
    let number = |key: &str| {
        doc.get(key)
            .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    PreviousCompletion {
        state: match doc.get("state") {
            Some(value @ ("never" | "succeeded" | "failed")) => value.to_owned(),
            _ => "never".into(),
        },
        finished: number("finished_epoch"),
        exit: match doc.get("exit_code") {
            Some(value) if value.bytes().all(|b| b.is_ascii_digit()) && !value.is_empty() => {
                value.parse().unwrap_or(-1)
            }
            _ => -1,
        },
        action: match doc.get("action") {
            Some(value @ ("sync" | "plan" | "none")) => value.to_owned(),
            _ => "none".into(),
        },
        failures: number("consecutive_failures").min(999_999),
        success: number("last_success_epoch"),
    }
}

/// Render `profiles/<p>.state` for a sync. `None` for a plan: a plan changes
/// nothing on the NAS and must not rewrite the profile's last sync.
pub(crate) fn render_profile_state(
    record: &ProfileRecord,
    operation: &str,
    previous: Option<&[u8]>,
) -> Option<Vec<u8>> {
    if operation != "sync" {
        return None;
    }
    let previous = previous_completion(previous);
    let reason = token_or_none(&record.reason);
    let mut failures = previous.failures;
    let mut success = previous.success;
    let (state, finished, exit, action) = match record.outcome {
        ProfileOutcome::Succeeded | ProfileOutcome::Partial => {
            failures = 0;
            success = record.finished;
            (
                "succeeded".to_owned(),
                record.finished,
                record.exit,
                "sync".to_owned(),
            )
        }
        ProfileOutcome::Failed => {
            failures = previous.failures + 1;
            (
                "failed".to_owned(),
                record.finished,
                record.exit,
                "sync".to_owned(),
            )
        }
        ProfileOutcome::Unknown if record.exit != 0 => {
            failures = previous.failures + 1;
            (
                "failed".to_owned(),
                record.finished,
                record.exit,
                "sync".to_owned(),
            )
        }
        ProfileOutcome::NotRun | ProfileOutcome::Unknown => (
            previous.state,
            previous.finished,
            previous.exit,
            previous.action,
        ),
    };
    let scope = if record.aggregate { "all" } else { "profile" };
    let mut out = format!(
        "state={state}\nfinished_epoch={finished}\nexit_code={}\naction={action}\n\
         outcome={}\nreason={reason}\nscope={scope}\noutcome_epoch={}\n\
         consecutive_failures={failures}\nlast_success_epoch={success}\n",
        exit.max(-1),
        record.outcome.as_str(),
        record.finished,
    );
    match (record.deferred, record.skipped) {
        (Some(deferred), Some(skipped)) => {
            out.push_str(&format!("deferred={deferred}\nskipped={skipped}\n"));
        }
        _ if record.core_ran
            && matches!(
                record.outcome,
                ProfileOutcome::Succeeded | ProfileOutcome::Partial | ProfileOutcome::Failed
            ) =>
        {
            out.push_str("counts=unavailable\n");
        }
        _ => {}
    }
    record.facts.render(&mut out);
    if matches!(
        record.outcome,
        ProfileOutcome::Failed | ProfileOutcome::Unknown
    ) {
        if let Some(detail) = &record.detail {
            let detail = sanitize_detail(detail, 240);
            if !detail.is_empty() {
                out.push_str(&format!("detail={detail}\n"));
            }
        }
        if let Some(path) = &record.failed_path {
            let path = sanitize_detail(path, 240);
            if !path.is_empty() {
                out.push_str(&format!("failed_path={path}\n"));
            }
        }
    }
    Some(out.into_bytes())
}

// ---------------------------------------------------------------- controller / schedule

/// `write_controller_state STATUS PID NEXT ACTIVE`.
pub(crate) fn render_controller_state(
    running: bool,
    pid: u32,
    next_run: u64,
    active_pid: u32,
    updated: u64,
) -> Vec<u8> {
    format!(
        "state={}\npid={pid}\nnext_run_epoch={next_run}\nactive_pid={active_pid}\nupdated_epoch={updated}\n",
        if running { "running" } else { "stopped" }
    )
    .into_bytes()
}

/// `save_schedule_deadline`.
pub(crate) fn render_schedule_state(next_run: u64) -> Vec<u8> {
    format!("next_run_epoch={next_run}\n").into_bytes()
}

/// Publish `run.lock` for the run the daemon is about to start, in the shell's
/// private-lock format (a 0700 directory whose only entry is a 0600 `pid` file
/// of three lines: pid, start tick, boot id), naming the daemon itself.
///
/// The bridge reads `run.state=running` beside a lock with no live owner as a
/// dead runner (`runner_died`), and the shell manager reads a live one as "a
/// run is in flight". The daemon holds `daemon.lock`, so a lock left by an
/// earlier daemon that died is stale by construction and is replaced.
#[cfg(target_os = "linux")]
pub(crate) fn publish_run_lock(run_dir: &Path) -> Result<(), WriteError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let lock = run_dir.join("run.lock");
    let fail = |step: &'static str, cause: String| WriteError {
        path: lock.clone(),
        step,
        cause,
    };
    let pid = std::process::id();
    let start = super::super::linux_files::process_start(pid).map_err(|_| {
        fail(
            "identity",
            "the daemon's start tick could not be read".into(),
        )
    })?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| fail("identity", io_cause(&error)))?;
    let boot = boot.trim();
    remove_run_lock(run_dir)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&lock)
        .map_err(|error| fail("create", io_cause(&error)))?;
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o700))
        .map_err(|error| fail("chmod", io_cause(&error)))?;
    atomic_write(
        &lock.join("pid"),
        format!("{pid}\n{start}\n{boot}\n").as_bytes(),
    )?;
    // atomic_write leaves its temporary beside the target only on failure; the
    // lock directory must hold exactly `pid`.
    Ok(())
}

/// Remove `run.lock` (after the final `run.state` is written).
#[cfg(target_os = "linux")]
pub(crate) fn remove_run_lock(run_dir: &Path) -> Result<(), WriteError> {
    let lock = run_dir.join("run.lock");
    match fs::symlink_metadata(&lock) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WriteError {
            path: lock,
            step: "inspect",
            cause: io_cause(&error),
        }),
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::remove_dir_all(&lock).map_err(|error| WriteError {
                path: lock,
                step: "remove",
                cause: io_cause(&error),
            })
        }
        Ok(_) => fs::remove_file(&lock).map_err(|error| WriteError {
            path: lock,
            step: "remove",
            cause: io_cause(&error),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sdsyncd-state-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temporary() {
        let dir = scratch("atomic");
        let path = dir.join("run.state");
        atomic_write(&path, b"state=running\n").unwrap();
        atomic_write(&path, b"state=succeeded\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"state=succeeded\n");
        let names = fs::read_dir(&dir).unwrap().count();
        assert_eq!(names, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_write_names_its_path_and_cause() {
        let dir = scratch("missing");
        let path = dir.join("absent-dir").join("run.state");
        let error = atomic_write(&path, b"x").unwrap_err();
        assert_eq!(error.step, "create");
        assert!(error.to_string().contains("run.state"), "{error}");
        assert!(!error.cause.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_refuses_a_symlinked_target() {
        let dir = scratch("symlink");
        let elsewhere = dir.join("elsewhere");
        fs::write(&elsewhere, b"keep").unwrap();
        let path = dir.join("run.state");
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        let error = atomic_write(&path, b"state=failed\n").unwrap_err();
        assert_eq!(error.step, "refuse_symlink");
        assert_eq!(fs::read(&elsewhere).unwrap(), b"keep");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_writer_thread_answers_within_its_deadline() {
        let dir = scratch("writer");
        let writer = StateWriter::spawn().unwrap();
        let path = dir.join("controller.state");
        writer
            .write(
                &path,
                render_controller_state(true, 7, 0, 0, 1),
                STATE_WRITE_DEADLINE,
            )
            .unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("state=running\npid=7\n")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn details_are_reduced_like_the_runner_does() {
        assert_eq!(sanitize_detail("a|b\tc\u{e9}\nsecond", 160), "a/b?c??");
        assert_eq!(sanitize_detail(&"x".repeat(300), 160).len(), 160);
        assert_eq!(token_or_none("Bad"), "none");
        assert_eq!(token_or_none("run_stalled"), "run_stalled");
    }

    #[test]
    fn run_state_has_the_shells_key_order_and_carries_the_aggregate() {
        let previous = b"state=failed\nstarted_epoch=1\nfinished_epoch=2\nexit_code=1\nscope=all\n\
            operation=sync\nreason=core_failed\ndetail=\naggregate_state=failed\n\
            aggregate_finished_epoch=2\naggregate_exit_code=1\naggregate_reason=core_failed\n";
        let mut record = RunRecord::running("photos", "sync", 100);
        record.heartbeat = Some(100);
        record.heartbeat_source = Some("io");
        assert_eq!(
            String::from_utf8(render_run_state(&record, Some(previous))).unwrap(),
            "state=running\nstarted_epoch=100\nfinished_epoch=0\nexit_code=-1\nscope=photos\n\
             operation=sync\nreason=none\ndetail=\naggregate_state=failed\n\
             aggregate_finished_epoch=2\naggregate_exit_code=1\naggregate_reason=core_failed\n\
             heartbeat_epoch=100\nheartbeat_source=io\n"
        );
        let mut done = RunRecord::running("all", "sync", 100);
        done.status = RunStatus::Failed;
        done.finished = 160;
        done.exit = 137;
        done.reason = "run_stalled".into();
        done.detail = "no progress for 45 min in phase scan".into();
        done.outcome = Some(RunOutcome::Failed);
        done.degraded = vec!["activity"];
        let text = String::from_utf8(render_run_state(&done, Some(previous))).unwrap();
        assert!(text.contains("aggregate_state=failed\naggregate_finished_epoch=160\naggregate_exit_code=137\naggregate_reason=run_stalled\n"), "{text}");
        assert!(text.contains(
            "outcome=failed\ndegraded=activity\ndegraded_reason=activity_write_failed\n"
        ));
        assert!(
            !text.contains("heartbeat"),
            "only a running document carries it"
        );
        // A detail the readers would refuse is dropped, never the record.
        done.detail = "a|b".into();
        let text = String::from_utf8(render_run_state(&done, None)).unwrap();
        assert!(text.contains("\ndetail=\n"));
    }

    /// The index line is written into the profile record, every key named.
    #[test]
    fn a_profile_record_carries_the_index_line() {
        let record = ProfileRecord {
            outcome: ProfileOutcome::Succeeded,
            exit: 0,
            reason: "ok".into(),
            finished: 500,
            aggregate: false,
            deferred: Some(0),
            skipped: Some(0),
            core_ran: true,
            facts: ResultFacts {
                index: Some(IndexFacts {
                    state: "warm".into(),
                    entries: Some(1000),
                    hits: 975,
                    misses: 25,
                    canary_checked: 25,
                    canary_mismatches: 0,
                    issue: None,
                    issue_detail: None,
                }),
                ..ResultFacts::default()
            },
            detail: None,
            failed_path: None,
        };
        let text = String::from_utf8(render_profile_state(&record, "sync", None).unwrap()).unwrap();
        assert!(
            text.contains(
                "index_state=warm\nindex_entries=1000\nindex_hits=975\nindex_misses=25\n\
                 index_canary_checked=25\nindex_canary_mismatches=0\n"
            ),
            "{text}"
        );
        assert!(!text.contains("index_issue"), "{text}");
    }

    #[test]
    fn profile_state_keeps_completion_semantics() {
        let record = ProfileRecord {
            outcome: ProfileOutcome::Failed,
            exit: 137,
            reason: "run_stalled".into(),
            finished: 500,
            aggregate: false,
            deferred: None,
            skipped: None,
            core_ran: true,
            facts: ResultFacts::default(),
            detail: Some("no progress for 45 min".into()),
            failed_path: None,
        };
        let previous = b"state=succeeded\nfinished_epoch=100\nexit_code=0\naction=sync\nconsecutive_failures=0\nlast_success_epoch=100\n";
        let text =
            String::from_utf8(render_profile_state(&record, "sync", Some(previous)).unwrap())
                .unwrap();
        assert_eq!(
            text,
            "state=failed\nfinished_epoch=500\nexit_code=137\naction=sync\noutcome=failed\n\
             reason=run_stalled\nscope=profile\noutcome_epoch=500\nconsecutive_failures=1\n\
             last_success_epoch=100\ncounts=unavailable\ndetail=no progress for 45 min\n"
        );
        assert!(render_profile_state(&record, "plan", Some(previous)).is_none());
        let not_run = ProfileRecord {
            outcome: ProfileOutcome::NotRun,
            core_ran: false,
            detail: None,
            ..record
        };
        let text =
            String::from_utf8(render_profile_state(&not_run, "sync", Some(previous)).unwrap())
                .unwrap();
        assert!(
            text.starts_with(
                "state=succeeded\nfinished_epoch=100\nexit_code=0\naction=sync\noutcome=not_run\n"
            ),
            "{text}"
        );
    }

    /// Golden tests: the daemon's bytes through the bridge's own readers.
    #[cfg(target_os = "linux")]
    mod golden {
        use super::super::super::super::{
            PackagePaths, SecurityPolicyArgs, outcomes_value, snapshot_document,
        };
        use super::super::super::routines::{
            Backend, BackendReason, DeferReason, RoutineRunState, RoutineState,
        };
        use super::*;
        use serde_json::{Value, json};
        use std::os::unix::fs::PermissionsExt;

        /// A private package tree, as the bridge's own fixture builds it.
        struct Tree {
            root: PathBuf,
            dirs: Vec<PathBuf>,
        }

        impl Tree {
            fn new(label: &str) -> Self {
                let root = scratch(&format!("golden-{label}"));
                let names = [
                    "home",
                    "var",
                    "var/log",
                    "var/control",
                    "home/config",
                    "home/config/profiles.d",
                    "home/config/routines.d",
                    "var/state",
                    "var/state/health",
                    "var/state/routines",
                    "var/state/profiles",
                    "home/secrets",
                    "var/state/cache/status",
                    "var/run",
                ];
                let mut dirs = Vec::new();
                for name in names {
                    let dir = root.join(name);
                    fs::create_dir_all(&dir).unwrap();
                    dirs.push(dir);
                }
                for dir in std::iter::once(&root).chain(dirs.iter()) {
                    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
                }
                fs::set_permissions(
                    root.join("var/state/cache"),
                    fs::Permissions::from_mode(0o700),
                )
                .unwrap();
                Self { root, dirs }
            }

            fn p(&self, rel: &str) -> PathBuf {
                self.root.join(rel)
            }

            /// The test process owns every file it creates, so it is the package uid.
            fn uid(&self) -> u32 {
                use std::os::unix::fs::MetadataExt;
                fs::metadata(&self.root).unwrap().uid()
            }

            fn with_paths<T>(&self, f: impl FnOnce(&PackagePaths<'_>) -> T) -> T {
                let manager = self.p("sdsync-dsm");
                let controller = self.p("sdsync-controller");
                let home = self.p("home");
                let var = self.p("var");
                let config = self.p("home/config");
                let logs = self.p("var/log");
                let profiles = self.p("home/config/profiles.d");
                let routines = self.p("home/config/routines.d");
                let state = self.p("var/state");
                let health = self.p("var/state/health");
                let routine_state = self.p("var/state/routines");
                let secrets = self.p("home/secrets");
                let cache = self.p("var/state/cache/status");
                let run = self.p("var/run");
                let read_lane = self.p("var/control/read-lane");
                let paths = PackagePaths {
                    manager: &manager,
                    controller: &controller,
                    package_home: &home,
                    package_var: &var,
                    config_dir: &config,
                    log_root: &logs,
                    profiles_dir: &profiles,
                    routines_dir: &routines,
                    state_dir: &state,
                    health_dir: &health,
                    routine_state_dir: &routine_state,
                    secrets_dir: &secrets,
                    status_cache: &cache,
                    run_dir: &run,
                    read_lane: &read_lane,
                };
                f(&paths)
            }

            fn snapshot(&self) -> Value {
                let bytes = self
                    .with_paths(|paths| {
                        snapshot_document(paths, self.uid(), &SecurityPolicyArgs::default())
                    })
                    .unwrap_or_else(|error| {
                        panic!("the snapshot reader refused the daemon's files: {error:?}")
                    });
                serde_json::from_slice(&bytes).unwrap()
            }

            fn outcomes(&self) -> Value {
                self.with_paths(|paths| outcomes_value(paths, self.uid()))
            }

            fn private(&self, rel: &str, bytes: &[u8]) {
                atomic_write(&self.p(rel), bytes).unwrap();
            }
        }

        impl Drop for Tree {
            fn drop(&mut self) {
                let _ = &self.dirs;
                let _ = fs::remove_dir_all(&self.root);
            }
        }

        fn every_routine_state() -> Vec<RoutineState> {
            let mut states = Vec::new();
            for (state, backend, reason, defer) in [
                (
                    RoutineRunState::Never,
                    Backend::None,
                    BackendReason::Ok,
                    DeferReason::None,
                ),
                (
                    RoutineRunState::Scheduled,
                    Backend::Interval,
                    BackendReason::Ok,
                    DeferReason::None,
                ),
                (
                    RoutineRunState::Running,
                    Backend::Daily,
                    BackendReason::Ok,
                    DeferReason::None,
                ),
                (
                    RoutineRunState::Succeeded,
                    Backend::Inotify,
                    BackendReason::Ok,
                    DeferReason::None,
                ),
                (
                    RoutineRunState::Failed,
                    Backend::Polling,
                    BackendReason::WatchLimit,
                    DeferReason::None,
                ),
                (
                    RoutineRunState::Deferred,
                    Backend::None,
                    BackendReason::SourceMissing,
                    DeferReason::Source,
                ),
                (
                    RoutineRunState::Deferred,
                    Backend::Daily,
                    BackendReason::Ok,
                    DeferReason::Window,
                ),
                (
                    RoutineRunState::Deferred,
                    Backend::None,
                    BackendReason::Ok,
                    DeferReason::DeleteBound,
                ),
                (
                    RoutineRunState::Deferred,
                    Backend::Interval,
                    BackendReason::Ok,
                    DeferReason::Busy,
                ),
            ] {
                states.push(RoutineState {
                    state,
                    backend,
                    backend_reason: reason,
                    defer_reason: defer,
                    next_run: 1_800_000_000,
                    last_success: 1_700_000_000,
                    last_exit: 137,
                    last_reason: "run_stalled".into(),
                    ..RoutineState::default()
                });
            }
            for reason in BackendReason::ALL {
                states.push(RoutineState {
                    backend: Backend::Polling,
                    backend_reason: reason,
                    ..RoutineState::default()
                });
            }
            states
        }

        /// Every routine state the daemon can write reads back through the
        /// snapshot reader with the same values, and through the outcome reader
        /// with no error.
        #[test]
        fn routine_state_bytes_are_read_by_the_bridge_snapshot() {
            let tree = Tree::new("routines");
            let secrets = tree.p("home/secrets").display().to_string();
            tree.private(
                "home/config/profiles.d/photos.toml",
                format!(
                    "source = \"/volume1/photos\"\nurl = \"https://nas.example.com:5001\"\n\
                     username = \"operator\"\nremote = \"/home/Drive/photos\"\n\
                     password-file = \"{secrets}/photos.password\"\n\
                     totp-secret-file = \"{secrets}/photos.totp\"\nexcludes = []\n"
                )
                .as_bytes(),
            );
            // The dashboard writes every key once; the bridge requires that.
            tree.private(
                "home/config/routines.d/photos.conf",
                b"enabled=true\naction=sync\nmode=realtime\ninterval_seconds=3600\n\
                  weekdays=1,2,3,4,5,6,7\ntime_window_start=00:00\ntime_window_end=23:59\n\
                  debounce_seconds=45\nretry_count=5\nretry_backoff_seconds=60\n\
                  retry_exponential=true\npoll_seconds=30\nallow_delete=false\n\
                  max_total_delete=100\ndepends_on=\n",
            );
            for state in every_routine_state() {
                tree.private("var/state/routines/photos.state", &state.render());
                let snapshot = tree.snapshot();
                let routine = &snapshot["routines"][0];
                assert_eq!(routine["state"], state.state.as_str(), "{state:?}");
                assert_eq!(routine["backend"], state.backend.as_str());
                assert_eq!(routine["backend_reason"], state.backend_reason.as_str());
                assert_eq!(routine["next_run_epoch"], json!(state.next_run));
                assert_eq!(routine["last_success_epoch"], json!(state.last_success));
                let outcomes = tree.outcomes();
                assert_eq!(outcomes["errors"], json!([]), "{state:?}");
                let row = &outcomes["routines"][0];
                let exit = (state.last_exit >= 0).then_some(state.last_exit);
                assert_eq!(row["last_exit_code"], json!(exit));
                let reason = (state.last_reason != "none").then_some(state.last_reason.as_str());
                assert_eq!(row["last_reason"], json!(reason));
                let defer = (state.defer_reason != DeferReason::None)
                    .then_some(state.defer_reason.as_str());
                assert_eq!(row["defer_reason"], json!(defer));
            }
        }

        #[test]
        fn controller_and_run_state_bytes_are_read_by_the_bridge_snapshot() {
            let tree = Tree::new("run");
            tree.private(
                "var/state/controller.state",
                &render_controller_state(true, 4242, 1_800_000_000, 4343, 1_700_000_000),
            );
            tree.private(
                "var/state/schedule.state",
                &render_schedule_state(1_800_000_000),
            );
            let mut record = RunRecord::running("all", "sync", 1_700_000_000);
            record.status = RunStatus::Failed;
            record.finished = 1_700_000_100;
            record.exit = 137;
            record.reason = "run_stalled".into();
            record.detail = "no progress for 45 min in phase upload".into();
            record.outcome = Some(RunOutcome::Failed);
            tree.private("var/state/run.state", &render_run_state(&record, None));
            let snapshot = tree.snapshot();
            assert_eq!(snapshot["controller"]["state"], "running");
            assert_eq!(snapshot["controller"]["pid"], json!(4242));
            assert_eq!(snapshot["controller"]["active_pid"], json!(4343));
            assert_eq!(snapshot["run"]["state"], "failed");
            assert_eq!(snapshot["run"]["exit_code"], json!(137));
            let outcomes = tree.outcomes();
            assert_eq!(outcomes["errors"], json!([]));
            assert_eq!(outcomes["run"]["outcome"], "failed");
            assert_eq!(outcomes["run"]["reason"], "run_stalled");
            assert_eq!(
                outcomes["run"]["detail"],
                "no progress for 45 min in phase upload"
            );
            assert_eq!(outcomes["run"]["aggregate"]["outcome"], "failed");
        }

        /// A daemon run in flight reads as running, not as a dead runner, and
        /// the lock goes with it.
        #[test]
        fn a_running_daemon_run_with_its_lock_reads_running_not_stale() {
            let tree = Tree::new("lock");
            let mut record = RunRecord::running("all", "sync", 1_700_000_000);
            record.heartbeat_source = Some("io");
            tree.private("var/state/run.state", &render_run_state(&record, None));
            let run_dir = tree.p("var/run");
            // Without the lock the bridge calls it a dead runner.
            assert_eq!(tree.outcomes()["run"]["outcome"], "stale");
            publish_run_lock(&run_dir).unwrap();
            let outcomes = tree.outcomes();
            assert_eq!(outcomes["run"]["outcome"], "running", "{outcomes}");
            assert_eq!(outcomes["run"]["stale"]["lock"], "live");
            assert_eq!(outcomes["errors"], json!([]));
            // Publishing again (a stale lock from an earlier daemon) replaces it.
            publish_run_lock(&run_dir).unwrap();
            let names: Vec<_> = fs::read_dir(run_dir.join("run.lock"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["pid"]);
            remove_run_lock(&run_dir).unwrap();
            assert!(!run_dir.join("run.lock").exists());
        }

        /// Each completed outcome the executor names reads back as itself.

        /// The index line reads back through the bridge as `index`, a corrupt
        /// index named with its path and cause, and a profile without one as null.
        #[test]
        fn the_index_line_reads_back_through_the_bridge() {
            let tree = Tree::new("index");
            tree.private("home/config/profiles.d/photos.toml", b"");
            let mut profile = ProfileRecord {
                outcome: ProfileOutcome::Succeeded,
                exit: 0,
                reason: "ok".into(),
                finished: 1_700_000_100,
                aggregate: false,
                deferred: Some(0),
                skipped: Some(0),
                core_ran: true,
                facts: ResultFacts {
                    index: Some(IndexFacts {
                        state: "cold".into(),
                        entries: Some(60),
                        hits: 0,
                        misses: 60,
                        canary_checked: 0,
                        canary_mismatches: 0,
                        issue: Some("pair-index-corrupt".into()),
                        issue_detail: Some(
                            "/var/state/cache/pairs/photos.redb: integrity check failed".into(),
                        ),
                    }),
                    ..ResultFacts::default()
                },
                detail: None,
                failed_path: None,
            };
            tree.private(
                "var/state/profiles/photos.state",
                &render_profile_state(&profile, "sync", None).unwrap(),
            );
            let outcomes = tree.outcomes();
            assert_eq!(outcomes["errors"], json!([]), "{outcomes}");
            let index = &outcomes["profiles"][0]["index"];
            assert_eq!(index["state"], "cold", "{outcomes}");
            assert_eq!(index["entries"], 60);
            assert_eq!(index["misses"], 60);
            assert_eq!(index["refreshed"], false);
            assert_eq!(index["issue"], "pair-index-corrupt");
            assert_eq!(
                index["issue_detail"],
                "/var/state/cache/pairs/photos.redb: integrity check failed"
            );
            profile.facts.index = None;
            tree.private(
                "var/state/profiles/photos.state",
                &render_profile_state(&profile, "sync", None).unwrap(),
            );
            assert_eq!(tree.outcomes()["profiles"][0]["index"], Value::Null);
        }

        #[test]
        fn every_run_outcome_reads_back_as_itself() {
            let tree = Tree::new("outcomes");
            tree.private("home/config/profiles.d/photos.toml", b"");
            for (status, exit, outcome, reason, expected_reason) in [
                (
                    RunStatus::Succeeded,
                    0,
                    RunOutcome::Succeeded,
                    "ok",
                    Value::Null,
                ),
                (
                    RunStatus::Succeeded,
                    3,
                    RunOutcome::Partial,
                    "deferred_paths",
                    json!("deferred_paths"),
                ),
                (
                    RunStatus::Failed,
                    1,
                    RunOutcome::Failed,
                    "core_failed",
                    json!("core_failed"),
                ),
                (
                    RunStatus::Failed,
                    75,
                    RunOutcome::Busy,
                    "busy",
                    json!("busy"),
                ),
                (
                    RunStatus::Failed,
                    130,
                    RunOutcome::Cancelled,
                    "cancelled",
                    json!("cancelled"),
                ),
                (
                    RunStatus::Failed,
                    137,
                    RunOutcome::Cancelled,
                    "cancelled",
                    json!("cancelled"),
                ),
                (
                    RunStatus::Failed,
                    137,
                    RunOutcome::Failed,
                    "run_stalled",
                    json!("run_stalled"),
                ),
                (
                    RunStatus::Failed,
                    137,
                    RunOutcome::Failed,
                    "core_failed",
                    json!("core_failed"),
                ),
                (
                    RunStatus::Failed,
                    -1,
                    RunOutcome::Failed,
                    "launch_failed",
                    json!("launch_failed"),
                ),
            ] {
                let mut record = RunRecord::running("photos", "sync", 1_700_000_000);
                record.status = status;
                record.finished = 1_700_000_100;
                record.exit = exit;
                record.reason = reason.into();
                record.outcome = Some(outcome);
                record.deferred = Some(0);
                record.skipped = Some(0);
                tree.private("var/state/run.state", &render_run_state(&record, None));
                let profile = ProfileRecord {
                    outcome: match outcome {
                        RunOutcome::Succeeded => ProfileOutcome::Succeeded,
                        RunOutcome::Partial => ProfileOutcome::Partial,
                        _ => ProfileOutcome::Failed,
                    },
                    exit,
                    reason: reason.into(),
                    finished: 1_700_000_100,
                    aggregate: false,
                    deferred: Some(0),
                    skipped: Some(0),
                    core_ran: true,
                    facts: ResultFacts::default(),
                    detail: Some("cause".into()),
                    failed_path: None,
                };
                tree.private(
                    "var/state/profiles/photos.state",
                    &render_profile_state(&profile, "sync", None).unwrap(),
                );
                let outcomes = tree.outcomes();
                let label = format!("{exit} {reason}");
                assert_eq!(outcomes["errors"], json!([]), "{label}");
                assert_eq!(outcomes["run"]["outcome"], outcome.as_str(), "{label}");
                assert_eq!(outcomes["run"]["reason"], expected_reason, "{label}");
                assert_eq!(
                    outcomes["profiles"][0]["reason"], expected_reason,
                    "{label}"
                );
            }
        }
    }
}
