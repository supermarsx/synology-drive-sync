//! The dashboard queue under the daemon engine: the port of the shell
//! controller's `process_control_queue` (sdsync-controller), without its one
//! serial slot.
//!
//! The bridge (`ui/api.cgi`) enqueues a job as `var/control/requests/<id>.json`
//! (plus an optional `.secret` sidecar) and reads `responses/<id>.json`. The
//! shell consumed one job per controller turn and held every job but a
//! connection probe behind a running sync, with no deadline: the "everything
//! hangs" the user saw. Here:
//! - jobs run in lanes, beside any sync: one `serialized` job at a time (they
//!   commit configuration and must not interleave with each other), up to two
//!   `concurrent` (status walks, Doctor, actions) and two `connection` probes;
//!   order is kept within a lane, never across lanes;
//! - every child has a deadline (connection 3 min, serialized 10 min,
//!   concurrent 6 h): past it the process group is stopped and the job is
//!   answered exactly as the shell answered a dead consumer;
//! - every way a job ends without an answer is named in `controller.log`
//!   with the job id, the operation and the cause.
//!
//! The work itself is the bridge's own, in child processes exactly as the
//! shell invoked it (`--classify-queued-job`, `--consume-job`, `--fail-job`,
//! `--audit-transaction reconcile`): those entry points clear the process
//! environment and assume they are alone, which a thread of the daemon is not.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::routines::Level;
use super::sys;

/// Where the consumer's events go: level, event name, detail.
pub(crate) type QueueLog = Arc<dyn Fn(Level, &'static str, String) + Send + Sync>;

/// A job's class, as the bridge's classifier names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Lane {
    Connection,
    Concurrent,
    Serialized,
}

impl Lane {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "connection" => Some(Self::Connection),
            "concurrent" => Some(Self::Concurrent),
            "serialized" => Some(Self::Serialized),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Concurrent => "concurrent",
            Self::Serialized => "serialized",
        }
    }

    fn capacity(self) -> usize {
        match self {
            Self::Serialized => 1,
            Self::Concurrent | Self::Connection => 2,
        }
    }

    fn deadline(self, settings: &QueueSettings) -> Duration {
        match self {
            Self::Connection => settings.connection_deadline,
            Self::Serialized => settings.serialized_deadline,
            Self::Concurrent => settings.concurrent_deadline,
        }
    }
}

/// Timing and limits; tests shorten them.
#[derive(Clone, Debug)]
pub(crate) struct QueueSettings {
    pub(crate) tick: Duration,
    pub(crate) connection_deadline: Duration,
    pub(crate) serialized_deadline: Duration,
    pub(crate) concurrent_deadline: Duration,
    /// Bound on the short helper children (classify, fail, reconcile).
    pub(crate) helper_deadline: Duration,
    /// TERM to KILL.
    pub(crate) kill_grace: Duration,
    pub(crate) request_max_age: u64,
    pub(crate) claim_max_age: u64,
    pub(crate) response_max_age: u64,
    pub(crate) max_responses: usize,
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(250),
            connection_deadline: Duration::from_secs(180),
            serialized_deadline: Duration::from_secs(600),
            concurrent_deadline: Duration::from_secs(6 * 3600),
            helper_deadline: Duration::from_secs(60),
            kill_grace: Duration::from_secs(10),
            request_max_age: 86_400,
            claim_max_age: 3_600,
            response_max_age: 3_600,
            max_responses: 256,
        }
    }
}

/// The consumer thread.
pub(crate) struct QueueConsumer {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl QueueConsumer {
    /// Start consuming `var/control`. `bridge` is `sdsync-dsm-api`.
    pub(crate) fn start(
        bridge: PathBuf,
        var: PathBuf,
        env: Vec<(OsString, OsString)>,
        settings: QueueSettings,
        log: QueueLog,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let consumer = Consumer {
            bridge,
            control: var.join("control"),
            env,
            settings,
            log,
            stop: Arc::clone(&stop),
            running: BTreeMap::new(),
            last_reconcile: None,
            classified: BTreeMap::new(),
            reported: std::collections::BTreeSet::new(),
        };
        let thread = thread::Builder::new()
            .name("sdsyncd-queue".into())
            .spawn(move || consumer.run())?;
        Ok(Self { stop, thread })
    }

    /// Stop taking jobs, stop running consumers (TERM, then KILL), answer
    /// what can be answered. Bounded by `grace` plus the kill grace.
    pub(crate) fn stop(self, grace: Duration) {
        self.stop.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + grace;
        while !self.thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if self.thread.is_finished() {
            let _ = self.thread.join();
        }
    }
}

struct Running {
    id: String,
    lane: Lane,
    operation: String,
    child: Child,
    started: Instant,
    deadline: Duration,
    stopping: Option<Instant>,
    processing: PathBuf,
    secret: PathBuf,
    response: PathBuf,
    cause: Option<&'static str>,
}

struct Consumer {
    bridge: PathBuf,
    control: PathBuf,
    env: Vec<(OsString, OsString)>,
    settings: QueueSettings,
    log: QueueLog,
    stop: Arc<AtomicBool>,
    running: BTreeMap<String, Running>,
    last_reconcile: Option<Instant>,
    /// Each waiting job is classified once, not every tick.
    classified: BTreeMap<String, Classified>,
    /// Files named once as unusable (a rejected request, a collision).
    reported: std::collections::BTreeSet<String>,
}

/// A waiting job's class, its operation, and a terminal cause when it cannot
/// be run at all.
#[derive(Clone)]
struct Classified {
    lane: Lane,
    operation: String,
    terminal: Option<(&'static str, i32)>,
}

/// A job id: 48 lowercase hex characters.
fn valid_job_id(id: &str) -> bool {
    id.len() == 48 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `private_control_file`: a regular file, ours, not readable or writable by
/// group or other. Returns its mtime (epoch seconds).
fn private_control_file(path: &Path) -> Option<u64> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != sys::geteuid()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return None;
    }
    u64::try_from(metadata.mtime()).ok()
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|age| age.as_secs())
        .unwrap_or(0)
}

impl Consumer {
    fn log(&self, level: Level, event: &'static str, detail: String) {
        (self.log)(level, event, detail);
    }

    fn command(&self, arguments: &[OsString]) -> Command {
        let mut command = Command::new(&self.bridge);
        command
            .args(arguments)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null());
        let parent = sys::getpid();
        // SAFETY: child_setup is async-signal-safe and allocates nothing.
        unsafe {
            command.pre_exec(move || sys::child_setup(parent, None));
        }
        command
    }

    /// A short helper child, bounded: its exit code and stdout, or why not.
    fn helper(&self, arguments: &[OsString]) -> Result<(i32, String), String> {
        let mut child = self
            .command(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{}: {error}", self.bridge.display()))?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut out = String::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = (&mut stdout).take(4096).read_to_string(&mut out);
                    }
                    let code = status
                        .code()
                        .or_else(|| status.signal().map(|n| 128 + n))
                        .unwrap_or(-1);
                    return Ok((code, out));
                }
                Ok(None) => {}
                Err(error) => return Err(format!("wait failed: {error}")),
            }
            if started.elapsed() >= self.settings.helper_deadline {
                let _ = sys::kill_group(child.id(), libc::SIGKILL);
                let _ = child.wait();
                return Err(format!(
                    "no answer within {} s",
                    self.settings.helper_deadline.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn reconcile_audit(&mut self) -> bool {
        self.last_reconcile = Some(Instant::now());
        match self.helper(&["--audit-transaction".into(), "reconcile".into()]) {
            Ok((0, _)) => true,
            Ok((code, _)) => {
                self.log(
                    Level::Error,
                    "audit_reconcile_failed",
                    format!("phase=queue_turn exit={code}"),
                );
                false
            }
            Err(cause) => {
                self.log(
                    Level::Error,
                    "audit_reconcile_failed",
                    format!("phase=queue_turn cause={cause}"),
                );
                false
            }
        }
    }

    fn run(mut self) {
        let mut last_prune = Instant::now() - Duration::from_secs(3600);
        while !self.stop.load(Ordering::SeqCst) {
            self.reap();
            if last_prune.elapsed() >= Duration::from_secs(60) {
                last_prune = Instant::now();
                self.prune();
            }
            self.dispatch();
            thread::sleep(self.settings.tick);
        }
        self.shutdown();
    }

    /// Requests waiting, oldest (lexical: the bridge's ids are monotonic) first.
    fn pending(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.control.join("requests")) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".json").map(str::to_owned)
            })
            .filter(|id| !self.running.contains_key(id))
            .collect();
        ids.sort();
        ids
    }

    fn classify(&mut self, id: &str) -> Classified {
        if let Some(known) = self.classified.get(id) {
            return known.clone();
        }
        let classified = match self.helper(&["--classify-queued-job".into(), id.into()]) {
            Ok((0, out)) => {
                let mut words = out.split_whitespace();
                match words.next().and_then(Lane::parse) {
                    Some(lane) => Classified {
                        lane,
                        operation: words
                            .next()
                            .filter(|w| w.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
                            .unwrap_or("")
                            .to_owned(),
                        terminal: None,
                    },
                    None => {
                        self.log(
                            Level::Error,
                            "control_request_classification_failed",
                            format!("id={id} cause=unknown class {out:?}"),
                        );
                        Classified {
                            lane: Lane::Serialized,
                            operation: String::new(),
                            terminal: Some(("classification_failed", 73)),
                        }
                    }
                }
            }
            Ok((code, _)) => {
                self.log(
                    Level::Error,
                    "control_request_classification_failed",
                    format!("id={id} exit={code}"),
                );
                Classified {
                    lane: Lane::Serialized,
                    operation: String::new(),
                    terminal: Some(("classification_failed", code.clamp(1, 255))),
                }
            }
            Err(cause) => {
                self.log(
                    Level::Error,
                    "control_request_classification_failed",
                    format!("id={id} cause={cause}"),
                );
                Classified {
                    lane: Lane::Serialized,
                    operation: String::new(),
                    terminal: Some(("classification_failed", 69)),
                }
            }
        };
        self.classified.insert(id.to_owned(), classified.clone());
        classified
    }

    fn report_once(&mut self, key: String, event: &'static str, detail: String) {
        if self.reported.insert(key) {
            self.log(Level::Warn, event, detail);
        }
    }

    fn dispatch(&mut self) {
        let pending = self.pending();
        self.classified.retain(|id, _| pending.contains(id));
        self.reported
            .retain(|key| pending.iter().any(|id| key.ends_with(id.as_str())));
        if pending.is_empty() {
            return;
        }
        let mut reconciled = false;
        // Per lane, the first job still waiting; later jobs of a lane wait for
        // it (order within a lane), other lanes go on.
        let mut blocked: Vec<Lane> = Vec::new();
        for id in pending {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let request = self.control.join("requests").join(format!("{id}.json"));
            if !valid_job_id(&id) || private_control_file(&request).is_none() {
                self.report_once(
                    format!("rejected {id}"),
                    "control_request_rejected",
                    format!("file={id}.json reason=not a private control file"),
                );
                continue;
            }
            if fs::metadata(&request).map(|m| m.len()).unwrap_or(u64::MAX) > 65_536 {
                self.report_once(
                    format!("size {id}"),
                    "control_request_rejected",
                    format!("id={id} reason=size"),
                );
                continue;
            }
            let processing = self.control.join("processing").join(format!("{id}.json"));
            if fs::symlink_metadata(&processing).is_ok() {
                self.report_once(
                    format!("collision {id}"),
                    "control_request_collision",
                    format!("id={id}"),
                );
                continue;
            }
            let Classified {
                lane,
                operation,
                terminal,
            } = self.classify(&id);
            if terminal.is_none()
                && (blocked.contains(&lane)
                    || self.running.values().filter(|r| r.lane == lane).count() >= lane.capacity())
            {
                blocked.push(lane);
                continue;
            }
            // The audit outbox is reconciled before anything is admitted, once
            // per pass, as the shell did at the top of every queue turn.
            if !reconciled {
                if !self.reconcile_audit() {
                    return;
                }
                reconciled = true;
            }
            self.classified.remove(&id);
            self.claim(&id, lane, operation, terminal);
        }
    }

    fn claim(
        &mut self,
        id: &str,
        lane: Lane,
        operation: String,
        terminal: Option<(&'static str, i32)>,
    ) {
        let requests = self.control.join("requests");
        let processing_dir = self.control.join("processing");
        let request = requests.join(format!("{id}.json"));
        let processing = processing_dir.join(format!("{id}.json"));
        let response = self.control.join("responses").join(format!("{id}.json"));
        if let Err(error) = fs::rename(&request, &processing) {
            self.log(
                Level::Warn,
                "control_request_claim_failed",
                format!("id={id} cause={error}"),
            );
            return;
        }
        let secret = processing_dir.join(format!("{id}.secret"));
        let queued_secret = requests.join(format!("{id}.secret"));
        if fs::symlink_metadata(&response).is_ok() {
            self.log(
                Level::Error,
                "control_response_rejected",
                format!("id={id} cause=a response already exists"),
            );
            let _ = fs::remove_file(&processing);
            let _ = fs::remove_file(&queued_secret);
            let _ = fs::remove_file(&secret);
            return;
        }
        let mut terminal = terminal;
        if fs::symlink_metadata(&queued_secret).is_ok() {
            let claimed = private_control_file(&queued_secret).is_some()
                && fs::symlink_metadata(&secret).is_err()
                && fs::rename(&queued_secret, &secret).is_ok();
            if !claimed {
                self.log(
                    Level::Error,
                    "control_secret_rejected",
                    format!("id={id} reason=claim_failed"),
                );
                terminal = Some(("secret_claim_failed", 0));
            }
        }
        if let Some((cause, exit)) = terminal {
            self.publish_failure(id, &processing, &response, cause, exit);
            let _ = fs::remove_file(&queued_secret);
            let _ = fs::remove_file(&secret);
            let _ = fs::remove_file(&processing);
            return;
        }
        let child = self
            .command(&[
                "--consume-job".into(),
                processing.clone().into(),
                response.clone().into(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => {
                self.running.insert(
                    id.to_owned(),
                    Running {
                        id: id.to_owned(),
                        lane,
                        operation,
                        child,
                        started: Instant::now(),
                        deadline: lane.deadline(&self.settings),
                        stopping: None,
                        processing,
                        secret,
                        response,
                        cause: None,
                    },
                );
            }
            Err(error) => {
                // Never started: nothing ran, so the failure is exact.
                self.log(
                    Level::Error,
                    "control_consumer_failed",
                    format!("id={id} op={} cause=spawn: {error}", or_unknown(&operation)),
                );
                self.publish_failure(id, &processing, &response, "consumer_failed", 69);
                let _ = fs::remove_file(&secret);
                let _ = fs::remove_file(&processing);
            }
        }
    }

    fn publish_failure(
        &self,
        id: &str,
        processing: &Path,
        response: &Path,
        cause: &str,
        exit: i32,
    ) {
        match self.helper(&[
            "--fail-job".into(),
            processing.into(),
            response.into(),
            cause.into(),
            exit.clamp(0, 255).to_string().into(),
        ]) {
            Ok((0, _)) => self.log(
                Level::Info,
                "control_request_rejected_terminal",
                format!("id={id} cause={cause}"),
            ),
            Ok((code, _)) => self.log(
                Level::Error,
                "control_request_rejection_failed",
                format!("id={id} cause={cause} exit={code}"),
            ),
            Err(failure) => self.log(
                Level::Error,
                "control_request_rejection_failed",
                format!("id={id} cause={cause} failure={failure}"),
            ),
        }
    }

    /// Collect finished consumers; stop the ones past their deadline.
    fn reap(&mut self) {
        let now = Instant::now();
        let mut done = Vec::new();
        for (id, job) in &mut self.running {
            match job.child.try_wait() {
                Ok(Some(status)) => {
                    let code = status
                        .code()
                        .or_else(|| status.signal().map(|n| 128 + n))
                        .unwrap_or(-1);
                    done.push((id.clone(), code));
                }
                Ok(None) => {
                    if job.stopping.is_none() && now.duration_since(job.started) >= job.deadline {
                        job.cause = Some("deadline");
                        job.stopping = Some(now);
                        let _ = sys::kill_group(job.child.id(), libc::SIGTERM);
                    } else if job
                        .stopping
                        .is_some_and(|at| now.duration_since(at) >= self.settings.kill_grace)
                    {
                        let _ = sys::kill_group(job.child.id(), libc::SIGKILL);
                    }
                }
                Err(_) => done.push((id.clone(), -1)),
            }
        }
        for (id, code) in done {
            if let Some(job) = self.running.remove(&id) {
                self.finished(job, code);
            }
        }
    }

    /// The shell's post-consumer rules, word for word in effect.
    fn finished(&mut self, job: Running, code: i32) {
        let published = private_control_file(&job.response).is_some()
            && fs::metadata(&job.response)
                .map(|m| m.len())
                .unwrap_or(u64::MAX)
                <= 1_048_576;
        let op = or_unknown(&job.operation);
        if code != 0 || !published || job.cause.is_some() {
            self.log(
                Level::Error,
                "control_consumer_failed",
                format!(
                    "id={} op={op} exit={code} lane={}{}",
                    job.id,
                    job.lane.as_str(),
                    match job.cause {
                        Some(cause) =>
                            format!(" cause={cause} deadline_seconds={}", job.deadline.as_secs()),
                        None => String::new(),
                    }
                ),
            );
            if published {
                // The consumer's own account stands; never overwritten.
            } else if job.lane != Lane::Serialized {
                // Commits nothing a failure could misdescribe: answer it.
                let cause = if code == 0 && job.cause.is_none() {
                    "consumer_wrote_no_result"
                } else {
                    "consumer_failed"
                };
                self.publish_failure(&job.id, &job.processing, &job.response, cause, code.max(0));
            } else {
                // A serialized job may have committed part of a change: left
                // for the audit reconciliation to record as outcome_unknown.
                self.log(
                    Level::Warn,
                    "control_consumer_outcome_unknown",
                    format!("id={} op={op} exit={code} class=serialized", job.id),
                );
            }
        }
        let _ = fs::remove_file(&job.processing);
        if fs::symlink_metadata(&job.secret).is_ok_and(|m| !m.file_type().is_symlink()) {
            let _ = fs::remove_file(&job.secret);
        }
        if self
            .last_reconcile
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
        {
            let _ = self.reconcile_audit();
        }
        self.prune_responses();
    }

    fn prune(&self) {
        let now = epoch_now();
        for (dir, max_age, kind, level) in [
            (
                "requests",
                self.settings.request_max_age,
                "request",
                Level::Info,
            ),
            (
                "processing",
                self.settings.claim_max_age,
                "processing_indeterminate",
                Level::Warn,
            ),
            (
                "progress",
                self.settings.claim_max_age,
                "progress",
                Level::Info,
            ),
            (
                "responses",
                self.settings.response_max_age,
                "response",
                Level::Info,
            ),
        ] {
            let Ok(entries) = fs::read_dir(self.control.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(id) = name
                    .strip_suffix(".json")
                    .or_else(|| name.strip_suffix(".secret"))
                else {
                    continue;
                };
                let valid = if kind == "progress" {
                    id.split_once('.').is_some_and(|(client, job)| {
                        client.len() == 32
                            && client.bytes().all(|b| b.is_ascii_hexdigit())
                            && valid_job_id(job)
                    })
                } else {
                    valid_job_id(id)
                };
                if !valid || self.running.contains_key(id) {
                    continue;
                }
                let path = entry.path();
                if let Some(mtime) = private_control_file(&path)
                    && mtime <= now
                    && now - mtime > max_age
                {
                    let _ = fs::remove_file(&path);
                    self.log(
                        level,
                        "control_file_expired",
                        format!("id={id} kind={kind}"),
                    );
                }
            }
        }
    }

    fn prune_responses(&self) {
        let Ok(entries) = fs::read_dir(self.control.join("responses")) else {
            return;
        };
        let mut responses: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".json"))
                    .is_some_and(valid_job_id)
                    && private_control_file(path).is_some()
            })
            .collect();
        if responses.len() <= self.settings.max_responses {
            return;
        }
        responses.sort();
        let excess = responses.len() - self.settings.max_responses;
        for path in responses.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }

    /// The daemon is stopping: stop every consumer and answer what can be.
    fn shutdown(&mut self) {
        let now = Instant::now();
        for job in self.running.values_mut() {
            job.cause = Some("service_stopping");
            job.stopping = Some(now);
            let _ = sys::kill_group(job.child.id(), libc::SIGTERM);
        }
        let deadline = now + self.settings.kill_grace;
        while !self.running.is_empty() && Instant::now() < deadline {
            self.reap();
            thread::sleep(Duration::from_millis(20));
        }
        for job in self.running.values() {
            let _ = sys::kill_group(job.child.id(), libc::SIGKILL);
        }
        let end = Instant::now() + Duration::from_secs(2);
        while !self.running.is_empty() && Instant::now() < end {
            self.reap();
            thread::sleep(Duration::from_millis(20));
        }
        for job in self.running.values() {
            self.log(
                Level::Error,
                "control_consumer_unkillable",
                format!("id={} pid={}", job.id, job.child.id()),
            );
        }
    }
}

fn or_unknown(operation: &str) -> &str {
    if operation.is_empty() {
        "unknown"
    } else {
        operation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
    const B: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2";
    const C: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3";

    /// A fake bridge: classifies by the request's first word, consumes by
    /// sleeping as long as the request says and answering (or not).
    fn fake_bridge(dir: &Path) -> PathBuf {
        let bridge = dir.join("bridge");
        fs::write(
            &bridge,
            r#"#!/bin/sh
log="$(dirname "$0")/calls"
echo "$*" >> "$log"
case $1 in
  --audit-transaction) exit 0 ;;
  --classify-queued-job)
    read class sleep answer < "$(dirname "$0")/var/control/requests/$2.json"
    echo "$class op-$class"; exit 0 ;;
  --consume-job)
    read class sleep answer < "$2"
    sleep "$sleep"
    [ "$answer" = answer ] && { umask 077; echo '{}' > "$3"; }
    exit 0 ;;
  --fail-job) umask 077; echo "{\"failed\":\"$4\"}" > "$3"; exit 0 ;;
esac
exit 64
"#,
        )
        .unwrap();
        fs::set_permissions(&bridge, fs::Permissions::from_mode(0o755)).unwrap();
        bridge
    }

    struct Fixture {
        dir: PathBuf,
        consumer: Option<QueueConsumer>,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl Fixture {
        fn new(label: &str, settings: QueueSettings) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sdsyncd-queue-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&dir);
            for sub in ["requests", "processing", "responses", "progress"] {
                fs::create_dir_all(dir.join("var/control").join(sub)).unwrap();
            }
            let bridge = fake_bridge(&dir);
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let log: QueueLog = Arc::new(move |_, event, detail| {
                sink.lock().unwrap().push(format!("{event} {detail}"));
            });
            let consumer = QueueConsumer::start(
                bridge,
                dir.join("var"),
                vec![("PATH".into(), "/usr/bin:/bin".into())],
                settings,
                log,
            )
            .unwrap();
            Self {
                dir,
                consumer: Some(consumer),
                events,
            }
        }

        fn enqueue(&self, id: &str, body: &str) {
            let path = self
                .dir
                .join("var/control/requests")
                .join(format!("{id}.json"));
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        fn response(&self, id: &str) -> Option<String> {
            fs::read_to_string(
                self.dir
                    .join("var/control/responses")
                    .join(format!("{id}.json")),
            )
            .ok()
        }

        fn wait(&self, what: &str, within: Duration, done: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + within;
            while !done(self) {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}: {:?}",
                    self.events.lock().unwrap()
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(consumer) = self.consumer.take() {
                consumer.stop(Duration::from_secs(5));
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn fast() -> QueueSettings {
        QueueSettings {
            tick: Duration::from_millis(20),
            kill_grace: Duration::from_millis(300),
            ..QueueSettings::default()
        }
    }

    /// A save is answered while a long status walk is still running: lanes,
    /// not one serial slot.
    #[test]
    fn a_save_is_answered_beside_a_long_status_walk() {
        let f = Fixture::new("lanes", fast());
        f.enqueue(A, "concurrent 3 answer\n");
        f.wait("the walk running", Duration::from_secs(5), |f| {
            f.dir
                .join("var/control/processing")
                .join(format!("{A}.json"))
                .exists()
        });
        let asked = Instant::now();
        f.enqueue(B, "serialized 0 answer\n");
        f.wait("the save answered", Duration::from_secs(5), |f| {
            f.response(B).is_some()
        });
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "{:?}",
            asked.elapsed()
        );
        assert!(f.response(A).is_none(), "the walk is still running");
        f.wait("the walk answered", Duration::from_secs(10), |f| {
            f.response(A).is_some()
        });
    }

    /// Two saves never run at once, and keep their order.
    #[test]
    fn saves_run_one_at_a_time_in_order() {
        let f = Fixture::new("serial", fast());
        f.enqueue(A, "serialized 1 answer\n");
        f.enqueue(B, "serialized 0 answer\n");
        f.wait("both answered", Duration::from_secs(10), |f| {
            f.response(A).is_some() && f.response(B).is_some()
        });
        let calls = fs::read_to_string(f.dir.join("calls")).unwrap();
        let consumed: Vec<&str> = calls
            .lines()
            .filter(|l| l.starts_with("--consume-job"))
            .collect();
        assert_eq!(consumed.len(), 2);
        assert!(
            consumed[0].contains(A) && consumed[1].contains(B),
            "{calls}"
        );
    }

    /// A consumer past its deadline is stopped and its job answered (a
    /// concurrent job commits nothing a failure could misdescribe); a silent
    /// serialized one is left for the audit reconciliation, named.
    #[test]
    fn deadlines_stop_consumers_and_every_ending_is_named() {
        let settings = QueueSettings {
            concurrent_deadline: Duration::from_millis(500),
            serialized_deadline: Duration::from_millis(500),
            ..fast()
        };
        let f = Fixture::new("deadline", settings);
        f.enqueue(A, "concurrent 30 answer\n");
        f.wait("the stopped walk answered", Duration::from_secs(10), |f| {
            f.response(A).is_some_and(|r| r.contains("consumer_failed"))
        });
        f.enqueue(B, "serialized 30 answer\n");
        f.wait("the stopped save named", Duration::from_secs(10), |f| {
            f.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("control_consumer_outcome_unknown") && e.contains(B))
        });
        assert!(
            f.response(B).is_none(),
            "a serialized job is never given a guessed verdict"
        );
        f.enqueue(C, "concurrent 0 silent\n");
        f.wait("the silent walk answered", Duration::from_secs(10), |f| {
            f.response(C)
                .is_some_and(|r| r.contains("consumer_wrote_no_result"))
        });
        let events = f.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.contains(&format!("id={A}")) && e.contains("cause=deadline")),
            "{events:?}"
        );
    }

    /// A request that is not a private file is named and left alone.
    #[test]
    fn a_foreign_request_is_named_not_consumed() {
        let f = Fixture::new("foreign", fast());
        let path = f.dir.join("var/control/requests").join(format!("{A}.json"));
        fs::write(&path, "concurrent 0 answer\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        f.wait("the rejection", Duration::from_secs(5), |f| {
            f.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("control_request_rejected"))
        });
        assert!(f.response(A).is_none());
    }
}
