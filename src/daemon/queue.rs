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

use std::cell::RefCell;
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

/// Where the consumer's Activity rows go (what the dashboard shows): code,
/// level, message. Profile `none`, category `bridge`, state `failed`.
pub(crate) type QueueActivity = Arc<dyn Fn(&'static str, Level, String) + Send + Sync>;

/// Where the consumer says the audit reconcile has failed past its bound
/// (`Some`) or recovered (`None`), on each change: the daemon publishes it in
/// controller.state (H2).
pub(crate) type QueueHealth = Arc<dyn Fn(Option<super::state::ControllerDegraded>) + Send + Sync>;

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
    /// How long the audit reconcile may keep failing before waiting jobs are
    /// answered `audit_unavailable` (they never ran).
    pub(crate) audit_unavailable_after: Duration,
    /// The longest wait between two reconcile attempts while it fails.
    pub(crate) reconcile_backoff_max: Duration,
    /// Past `audit_unavailable_after`, a job is refused only on a reconcile
    /// attempt made after it came: one is made once the last attempt is older
    /// than this, and the job is held until then (N-D1). A recovered audit is
    /// not refused for the rest of a long backoff.
    pub(crate) reconcile_recheck: Duration,
    /// After KILL, how long a consumer may take to be reaped before its slot
    /// is freed and it is handed to a reaper.
    pub(crate) reap_bound: Duration,
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
            audit_unavailable_after: Duration::from_secs(30),
            reconcile_backoff_max: Duration::from_secs(60),
            reconcile_recheck: Duration::from_secs(2),
            reap_bound: Duration::from_secs(10),
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
        activity: QueueActivity,
        health: QueueHealth,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let consumer = Consumer {
            bridge,
            control: var.join("control"),
            env,
            settings,
            log,
            activity,
            health,
            health_published: None,
            unreadable: RefCell::new(BTreeMap::new()),
            stop: Arc::clone(&stop),
            running: BTreeMap::new(),
            last_reconcile: None,
            reconcile: ReconcileEpisode::default(),
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

/// What the audit gate does with a waiting job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// The audit is reconciled: the job runs.
    Admit,
    /// Held, tried again later.
    Hold,
    /// Past the bound, and a fresh attempt made since the job came failed:
    /// answered `audit_unavailable`.
    Refuse,
}

/// A failing audit reconcile: since when, how many tries, the next one.
#[derive(Default)]
struct ReconcileEpisode {
    failing_since: Option<Instant>,
    /// The same moment on the wall clock, for controller.state.
    failing_since_epoch: u64,
    attempts: u32,
    next_try: Option<Instant>,
    cause: String,
}

struct Running {
    id: String,
    lane: Lane,
    operation: String,
    /// `None` once handed to a reaper (it outlived KILL).
    child: Option<Child>,
    pid: u32,
    /// The consumer's stderr, which the shell kept in controller.log (S2-1).
    stderr: Option<sys::Tail>,
    started: Instant,
    deadline: Duration,
    stopping: Option<Instant>,
    /// When KILL was first sent.
    killed: Option<Instant>,
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
    activity: QueueActivity,
    health: QueueHealth,
    /// The degradation last published and not yet cleared.
    health_published: Option<super::state::ControllerDegraded>,
    /// Control directories that could not be read, each with when that began and
    /// why: named once per episode, and published as a degradation while any is.
    unreadable: RefCell<BTreeMap<PathBuf, (u64, String)>>,
    stop: Arc<AtomicBool>,
    running: BTreeMap<String, Running>,
    last_reconcile: Option<Instant>,
    /// The audit reconcile's failure episode (H2).
    reconcile: ReconcileEpisode,
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
    /// A non-zero exit carries the helper's last stderr line (S2-1).
    fn helper(&self, arguments: &[OsString]) -> Result<(i32, String), String> {
        let mut child = self
            .command(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{}: {error}", self.bridge.display()))?;
        let stderr = sys::Tail::start(child.stderr.take(), 4096);
        let started = Instant::now();
        let said = |stderr: sys::Tail| {
            stderr
                .lines(1, Duration::from_millis(200))
                .pop()
                .map(|line| format!(" stderr={line}"))
                .unwrap_or_default()
        };
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
                    if code != 0 {
                        // The exit and the helper's own words, for the caller's log line.
                        return Ok((code, format!("{out}{}", said(stderr))));
                    }
                    return Ok((code, out));
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = sys::kill_group(child.id(), libc::SIGKILL);
                    if !sys::reap_within(&mut child, Duration::from_secs(5)) {
                        sys::abandon_to_reaper(child);
                    }
                    return Err(format!("wait failed: {error}"));
                }
            }
            if started.elapsed() >= self.settings.helper_deadline {
                let _ = sys::kill_group(child.id(), libc::SIGKILL);
                // H3: never an unbounded wait after the KILL.
                let reaped = sys::reap_within(&mut child, Duration::from_secs(5));
                let pid = child.id();
                let handed = if reaped {
                    "reaped"
                } else {
                    sys::abandon_to_reaper(child)
                };
                return Err(format!(
                    "no answer within {} s (pid={pid} {handed}){}",
                    self.settings.helper_deadline.as_secs(),
                    said(stderr)
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Reconcile the audit outbox. A failure opens an episode: named once
    /// (controller.log and an Activity row the dashboard shows), tried again
    /// after a backoff that doubles to `reconcile_backoff_max`, and its
    /// recovery is named too (H2). `false` while it fails or waits to retry;
    /// `force` tries now, inside the backoff (N-D1).
    fn reconcile_audit(&mut self, force: bool) -> bool {
        let now = Instant::now();
        if !force && self.reconcile.next_try.is_some_and(|at| now < at) {
            return false;
        }
        self.last_reconcile = Some(now);
        let failure = match self.helper(&["--audit-transaction".into(), "reconcile".into()]) {
            Ok((0, _)) => None,
            Ok((code, said)) => Some(format!("exit={code}{}", said.trim_end())),
            Err(cause) => Some(format!("cause={cause}")),
        };
        let Some(cause) = failure else {
            if let Some(since) = self.reconcile.failing_since.take() {
                self.log(
                    Level::Info,
                    "audit_reconcile_recovered",
                    format!(
                        "phase=queue_turn attempts={} failing_seconds={}",
                        self.reconcile.attempts,
                        since.elapsed().as_secs()
                    ),
                );
            }
            self.reconcile = ReconcileEpisode::default();
            return true;
        };
        let cause = cause.replace(['\n', '\r'], " ");
        self.reconcile.attempts += 1;
        let backoff = Duration::from_secs(1)
            .saturating_mul(1 << self.reconcile.attempts.saturating_sub(1).min(16))
            .min(self.settings.reconcile_backoff_max);
        self.reconcile.next_try = Some(now + backoff);
        if self.reconcile.failing_since.is_none() {
            self.reconcile.failing_since = Some(now);
            self.reconcile.failing_since_epoch = epoch_now();
            self.log(
                Level::Error,
                "audit_reconcile_failed",
                format!(
                    "phase=queue_turn {cause} retry_seconds={} unavailable_after_seconds={}",
                    backoff.as_secs(),
                    self.settings.audit_unavailable_after.as_secs()
                ),
            );
            (self.activity)(
                "bridge.failed",
                Level::Error,
                format!(
                    "The package audit log could not be reconciled ({cause}); dashboard operations \
                     wait for it and are refused after {} s if it does not recover. Review Logs.",
                    self.settings.audit_unavailable_after.as_secs()
                ),
            );
        }
        self.reconcile.cause = cause;
        false
    }

    /// N-D1: the audit gate in front of a waiting job. Without the recheck a
    /// recovered audit was refused for the rest of the backoff (up to 60 s).
    fn audit_gate(&mut self) -> Gate {
        let before = self.last_reconcile;
        if self.reconcile_audit(false) {
            return Gate::Admit;
        }
        if !self.audit_unavailable() {
            // Within the bound: held, retried after the backoff.
            return Gate::Hold;
        }
        if self.last_reconcile != before {
            // An attempt was just made, after the job arrived, and failed.
            return Gate::Refuse;
        }
        if self
            .last_reconcile
            .is_some_and(|at| at.elapsed() < self.settings.reconcile_recheck)
        {
            // The last attempt is too recent to retry: held until it is old
            // enough, so no job is refused on an attempt made before it came.
            return Gate::Hold;
        }
        // Past the bound and inside the backoff: one fresh try before refusing.
        if self.reconcile_audit(true) {
            Gate::Admit
        } else {
            Gate::Refuse
        }
    }

    /// Whether the reconcile has failed for longer than the bound: waiting
    /// jobs are then answered rather than held.
    fn audit_unavailable(&self) -> bool {
        self.reconcile
            .failing_since
            .is_some_and(|since| since.elapsed() >= self.settings.audit_unavailable_after)
    }

    /// H2: tell the daemon when the audit reconcile has failed past the
    /// bound (jobs are refused `audit_unavailable` from then on) and when it
    /// recovers, once per change.
    fn publish_health(&mut self) {
        let wanted = if self.audit_unavailable() {
            Some(super::state::ControllerDegraded {
                reason: "audit_unavailable",
                since: self.reconcile.failing_since_epoch,
                cause: format!(
                    "the audit log could not be reconciled ({}); dashboard operations are refused until it recovers",
                    self.reconcile.cause
                ),
            })
        } else {
            self.unreadable.borrow().iter().next().map(|(path, (since, cause))| {
                super::state::ControllerDegraded {
                    reason: "control_dir_unreadable",
                    since: *since,
                    cause: format!(
                        "{} cannot be read ({cause}); dashboard operations cannot be seen until it can",
                        path.display()
                    ),
                }
            })
        };
        if wanted == self.health_published {
            return;
        }
        self.health_published = wanted.clone();
        (self.health)(wanted);
    }

    /// `read_dir` of a control directory. A failure used to read as an empty
    /// directory and nothing said so: it is named once per episode with its path
    /// and cause (`control_dir_unreadable`), kept in `unreadable` for the
    /// degradation `publish_health` raises, and its end is named too
    /// (`control_dir_recovered`).
    fn read_control_dir(&self, dir: &str) -> Option<fs::ReadDir> {
        let path = self.control.join(dir);
        match fs::read_dir(&path) {
            Ok(entries) => {
                if self.unreadable.borrow_mut().remove(&path).is_some() {
                    self.log(
                        Level::Info,
                        "control_dir_recovered",
                        format!("path={}", path.display()),
                    );
                }
                Some(entries)
            }
            Err(error) => {
                let cause = error.to_string();
                // The episode keeps its start; a cause that moves while the
                // directory stays unreadable (denied, then not a directory) is
                // named again and published, so neither the log nor
                // controller.state keeps the first failure's words.
                let was = self
                    .unreadable
                    .borrow()
                    .get(&path)
                    .map(|(since, cause)| (*since, cause.clone()));
                match was {
                    Some((_, ref earlier)) if *earlier == cause => {}
                    Some((since, earlier)) => {
                        self.log(
                            Level::Error,
                            "control_dir_unreadable",
                            format!("path={} cause={cause} was={earlier}", path.display()),
                        );
                        self.unreadable.borrow_mut().insert(path, (since, cause));
                    }
                    None => {
                        self.log(
                            Level::Error,
                            "control_dir_unreadable",
                            format!("path={} cause={cause}", path.display()),
                        );
                        self.unreadable
                            .borrow_mut()
                            .insert(path, (epoch_now(), cause));
                    }
                }
                None
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
            // An open episode is retried on its backoff even with nothing
            // waiting, so a recovery is seen (and the amber cleared) without
            // a job to drive it.
            if self.reconcile.failing_since.is_some()
                && self
                    .reconcile
                    .next_try
                    .is_none_or(|at| Instant::now() >= at)
            {
                self.reconcile_audit(false);
            }
            self.publish_health();
            thread::sleep(self.settings.tick);
        }
        self.shutdown();
    }

    /// Requests waiting, oldest (lexical: the bridge's ids are monotonic) first.
    fn pending(&self) -> Vec<String> {
        let Some(entries) = self.read_control_dir("requests") else {
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

    /// Remove one of a job's control files. Absent is done; any other failure
    /// is named with the job, the kind of file, its path and the cause (N-D9:
    /// a `secret` sidecar may hold a credential, so one left behind must be
    /// findable). `true` once the file is gone.
    fn remove_control_file(&self, id: &str, kind: &str, path: &Path) -> bool {
        match fs::remove_file(path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                self.log(
                    Level::Error,
                    "control_file_unremovable",
                    format!("id={id} kind={kind} path={} cause={error}", path.display()),
                );
                false
            }
        }
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
        // The audit gate, decided once per pass (N-D1): every job of the pass
        // is admitted, or every one refused, on the same attempt.
        let mut gate: Option<Gate> = None;
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
            let decided = match gate {
                Some(decided) => decided,
                None => *gate.insert(self.audit_gate()),
            };
            match decided {
                // Held, not dropped: retried after the backoff.
                Gate::Hold => return,
                Gate::Admit => {}
                Gate::Refuse => {
                    // H2: past the bound, a job is answered by name. It never
                    // ran, so the answer is exact.
                    self.log(
                        Level::Error,
                        "audit_unavailable",
                        format!(
                            "id={id} op={} cause={} failing_seconds={}",
                            or_unknown(&operation),
                            self.reconcile.cause,
                            self.reconcile
                                .failing_since
                                .map_or(0, |since| since.elapsed().as_secs())
                        ),
                    );
                    self.classified.remove(&id);
                    self.claim(
                        &id,
                        lane,
                        operation,
                        Some(terminal.unwrap_or(("audit_unavailable", 69))),
                    );
                    continue;
                }
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
            self.remove_control_file(id, "processing", &processing);
            self.remove_control_file(id, "secret", &queued_secret);
            self.remove_control_file(id, "secret", &secret);
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
            self.remove_control_file(id, "secret", &queued_secret);
            self.remove_control_file(id, "secret", &secret);
            self.remove_control_file(id, "processing", &processing);
            return;
        }
        let child = self
            .command(&[
                "--consume-job".into(),
                processing.clone().into(),
                response.clone().into(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        match child {
            Ok(mut child) => {
                let stderr = Some(sys::Tail::start(child.stderr.take(), 8192));
                let pid = child.id();
                self.running.insert(
                    id.to_owned(),
                    Running {
                        id: id.to_owned(),
                        lane,
                        operation,
                        child: Some(child),
                        pid,
                        stderr,
                        started: Instant::now(),
                        deadline: lane.deadline(&self.settings),
                        stopping: None,
                        killed: None,
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
                self.remove_control_file(id, "secret", &secret);
                self.remove_control_file(id, "processing", &processing);
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

    /// Collect finished consumers; stop the ones past their deadline; give up
    /// on one that outlives KILL by `reap_bound` (H3: its slot is freed, it is
    /// named with its pid and handed to a reaper, and its job is answered as
    /// any stopped consumer's is).
    fn reap(&mut self) {
        let now = Instant::now();
        let mut done = Vec::new();
        let mut unkillable = Vec::new();
        let mut unwaitable = Vec::new();
        for (id, job) in &mut self.running {
            let Some(child) = job.child.as_mut() else {
                unkillable.push(id.clone());
                continue;
            };
            match child.try_wait() {
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
                        let _ = sys::kill_group(job.pid, libc::SIGTERM);
                    } else if job
                        .stopping
                        .is_some_and(|at| now.duration_since(at) >= self.settings.kill_grace)
                    {
                        if job.killed.is_none() {
                            job.killed = Some(now);
                        }
                        if job
                            .killed
                            .is_some_and(|at| now.duration_since(at) >= self.settings.reap_bound)
                        {
                            unkillable.push(id.clone());
                        } else {
                            let _ = sys::kill_group(job.pid, libc::SIGKILL);
                        }
                    }
                }
                Err(error) => {
                    // Cannot be waited for: stopped as an unkillable one is.
                    job.cause.get_or_insert("deadline");
                    let _ = sys::kill_group(job.pid, libc::SIGKILL);
                    unwaitable.push(format!("id={id} cause={error}"));
                    unkillable.push(id.clone());
                }
            }
        }
        for detail in unwaitable {
            self.log(Level::Error, "consumer_unwaitable", detail);
        }
        for (id, code) in done {
            if let Some(job) = self.running.remove(&id) {
                self.finished(job, code);
            }
        }
        for id in unkillable {
            if let Some(mut job) = self.running.remove(&id) {
                self.give_up(&mut job);
                self.finished(job, -1);
            }
        }
    }

    /// The consumer outlived KILL: name it, free its slot, hand it to a
    /// reaper. Its child handle is replaced by nothing the caller waits on.
    fn give_up(&self, job: &mut Running) {
        let pid = job.pid;
        let killed_for = job.killed.map_or(0, |at| at.elapsed().as_millis());
        self.log(
            Level::Error,
            "consumer_unkillable",
            format!(
                "id={} op={} pid={pid} lane={} path={} cause=still running {killed_for} ms after KILL; its slot is freed",
                job.id,
                or_unknown(&job.operation),
                job.lane.as_str(),
                job.processing.display()
            ),
        );
        job.cause = Some("unkillable");
        if let Some(child) = job.child.take() {
            // Our own unreaped child (its pid cannot be reused): KILL it by
            // pid too, in case it left its process group.
            let _ = sys::kill_process(pid, libc::SIGKILL);
            let handed = sys::abandon_to_reaper(child);
            if handed != "reaper" {
                self.log(
                    Level::Error,
                    "consumer_unkillable",
                    format!("id={} pid={pid} cause=handed={handed}", job.id),
                );
            }
        }
    }

    /// The shell's post-consumer rules, word for word in effect.
    fn finished(&mut self, mut job: Running, code: i32) {
        // S2-1: what the consumer said reaches controller.log, with its job.
        if let Some(stderr) = job.stderr.take() {
            let level = if code == 0 { Level::Warn } else { Level::Error };
            for line in stderr.lines(8, Duration::from_millis(500)) {
                self.log(
                    level,
                    "consumer_stderr",
                    format!(
                        "id={} op={} exit={code} path={} line={line}",
                        job.id,
                        or_unknown(&job.operation),
                        job.processing.display()
                    ),
                );
            }
        }
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
        self.remove_control_file(&job.id, "processing", &job.processing);
        match fs::symlink_metadata(&job.secret) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // Not the queue's own file: left in place, and named.
                self.log(
                    Level::Error,
                    "control_file_unremovable",
                    format!(
                        "id={} kind=secret path={} cause=a symbolic link, not the queue's file; left in place",
                        job.id,
                        job.secret.display()
                    ),
                );
            }
            Ok(_) => {
                self.remove_control_file(&job.id, "secret", &job.secret);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => self.log(
                Level::Error,
                "control_file_unremovable",
                format!(
                    "id={} kind=secret path={} cause={error}",
                    job.id,
                    job.secret.display()
                ),
            ),
        }
        if self
            .last_reconcile
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
        {
            // A failure opens (or continues) the episode, named there.
            self.reconcile_audit(false);
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
            let Some(entries) = self.read_control_dir(dir) else {
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
                    && self.remove_control_file(id, kind, &path)
                {
                    // Only a file that is gone has expired; one that stayed
                    // was named by remove_control_file.
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
        let Some(entries) = self.read_control_dir("responses") else {
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
            let id = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.remove_control_file(&id, "response", &path);
        }
    }

    /// The daemon is stopping: stop every consumer and answer what can be.
    fn shutdown(&mut self) {
        let now = Instant::now();
        for job in self.running.values_mut() {
            job.cause = Some("service_stopping");
            job.stopping = Some(now);
            let _ = sys::kill_group(job.pid, libc::SIGTERM);
        }
        let deadline = now + self.settings.kill_grace;
        while !self.running.is_empty() && Instant::now() < deadline {
            self.reap();
            thread::sleep(Duration::from_millis(20));
        }
        for job in self.running.values() {
            let _ = sys::kill_group(job.pid, libc::SIGKILL);
        }
        let end = Instant::now() + Duration::from_secs(2);
        while !self.running.is_empty() && Instant::now() < end {
            self.reap();
            thread::sleep(Duration::from_millis(20));
        }
        let left: Vec<String> = self.running.keys().cloned().collect();
        for id in left {
            if let Some(mut job) = self.running.remove(&id) {
                self.log(
                    Level::Error,
                    "control_consumer_unkillable",
                    format!("id={} pid={}", job.id, job.pid),
                );
                self.give_up(&mut job);
            }
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
  --audit-transaction)
    # `audit-fails` makes the reconcile fail: with the exit it holds, or 73 when empty.
    if [ -e "$(dirname "$0")/audit-fails" ]; then
      code=$(cat "$(dirname "$0")/audit-fails")
      exit "${code:-73}"
    fi
    exit 0 ;;
  --classify-queued-job)
    read class sleep answer < "$(dirname "$0")/var/control/requests/$2.json"
    echo "$class op-$class"; exit 0 ;;
  --consume-job)
    read class sleep answer < "$2"
    # Out of reach of the queue's signals: as a consumer in D state is.
    [ "$answer" = stubborn ] && exec perl -e 'setpgrp(0, getpgrp(getppid())); $SIG{TERM} = "IGNORE"; open(my $f, ">", $ARGV[0]); print $f "$$\n"; close($f); sleep 30' "$(dirname "$0")/stubborn.pid"
    [ "$answer" = noisy ] && { echo "boom: /volume1/share is gone" >&2; exit 1; }
    # A secret sidecar that cannot be removed (a directory in its place).
    [ "$answer" = plantdir ] && { rm -f "${2%.json}.secret"; mkdir "${2%.json}.secret"; }
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
        super::super::testkit::settle_for_exec(&bridge);
        bridge
    }

    struct Fixture {
        dir: PathBuf,
        consumer: Option<QueueConsumer>,
        events: Arc<Mutex<Vec<String>>>,
        rows: Arc<Mutex<Vec<String>>>,
        /// What the consumer published for controller.state, in order.
        health: Arc<Mutex<Vec<Option<super::super::state::ControllerDegraded>>>>,
    }

    impl Fixture {
        fn new(label: &str, settings: QueueSettings) -> Self {
            Self::prepared(label, settings, |_| {})
        }

        /// [`Fixture::new`] with `setup` run on the tree before the consumer
        /// starts.
        fn prepared(label: &str, settings: QueueSettings, setup: impl FnOnce(&Path)) -> Self {
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
            setup(&dir);
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let log: QueueLog = Arc::new(move |_, event, detail| {
                sink.lock().unwrap().push(format!("{event} {detail}"));
            });
            let rows = Arc::new(Mutex::new(Vec::new()));
            let row_sink = Arc::clone(&rows);
            let activity: QueueActivity = Arc::new(move |code, _, message| {
                row_sink.lock().unwrap().push(format!("{code} {message}"));
            });
            let health = Arc::new(Mutex::new(Vec::new()));
            let health_sink = Arc::clone(&health);
            let published: QueueHealth = Arc::new(move |degraded| {
                health_sink.lock().unwrap().push(degraded);
            });
            let consumer = QueueConsumer::start(
                bridge,
                dir.join("var"),
                vec![("PATH".into(), "/usr/bin:/bin".into())],
                settings,
                log,
                activity,
                published,
            )
            .unwrap();
            Self {
                dir,
                consumer: Some(consumer),
                events,
                rows,
                health,
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

    /// H2 (review-int hang probe): an audit reconcile that keeps failing is
    /// named once per episode and tried with a growing backoff, not four
    /// times a second; past the bound, waiting jobs are answered by name
    /// (`audit_unavailable`, they never ran) instead of never; once it
    /// recovers, jobs run again.
    #[test]
    fn a_failing_audit_reconcile_backs_off_and_answers_waiting_jobs() {
        let settings = QueueSettings {
            audit_unavailable_after: Duration::from_millis(800),
            ..fast()
        };
        let f = Fixture::new("audit-fails", settings);
        fs::write(f.dir.join("audit-fails"), b"").unwrap();
        f.enqueue(A, "concurrent 0 answer\n");
        f.wait("the job answered by name", Duration::from_secs(10), |f| {
            f.response(A)
                .is_some_and(|r| r.contains("audit_unavailable"))
        });
        thread::sleep(Duration::from_secs(2));
        let calls = fs::read_to_string(f.dir.join("calls")).unwrap();
        let reconciles = calls
            .lines()
            .filter(|l| l.starts_with("--audit-transaction"))
            .count();
        assert!(reconciles <= 8, "{reconciles} reconciles in about 3 s");
        {
            let events = f.events.lock().unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e.starts_with("audit_reconcile_failed"))
                    .count(),
                1,
                "{events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|e| e.starts_with("audit_unavailable") && e.contains(A)),
                "{events:?}"
            );
        }
        assert!(
            f.rows
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.starts_with("bridge.failed")),
            "the dashboard is told: {:?}",
            f.rows.lock().unwrap()
        );
        fs::remove_file(f.dir.join("audit-fails")).unwrap();
        f.enqueue(B, "concurrent 0 answer\n");
        f.wait("the next job run", Duration::from_secs(70), |f| {
            f.response(B).is_some_and(|r| r.trim() == "{}")
        });
        assert!(
            f.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("audit_reconcile_recovered")),
            "{:?}",
            f.events.lock().unwrap()
        );
    }

    /// N-D1: once the bound has passed, a recovered audit is not refused for
    /// the rest of a long backoff. A job is refused only on an attempt made
    /// after it arrived; one that arrives within two seconds of the last
    /// attempt is held until a fresh one can be made: the reviewer saw
    /// `{"failed":"audit_unavailable"}` 0.4 s after the fix, with the next
    /// scheduled attempt up to 60 s away.
    #[test]
    fn a_recovered_audit_is_not_refused_for_the_rest_of_the_backoff() {
        let settings = QueueSettings {
            audit_unavailable_after: Duration::from_millis(800),
            ..fast()
        };
        let f = Fixture::new("audit-recovers", settings);
        fs::write(f.dir.join("audit-fails"), b"").unwrap();
        let calls = || {
            fs::read_to_string(f.dir.join("calls"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.starts_with("--audit-transaction"))
                .count()
        };
        // Keep a job waiting until the episode has backed off to 8 s: the
        // attempts come at 0, 1, 3 and 7 s; the next is due at 15 s.
        let mut n = 0_u64;
        while calls() < 4 {
            n += 1;
            let id = format!("{n:048x}");
            f.enqueue(&id, "concurrent 0 answer\n");
            f.wait(&format!("job {n} answered"), Duration::from_secs(20), |f| {
                f.response(&id).is_some()
            });
            assert!(
                f.response(&id).unwrap().contains("audit_unavailable"),
                "job {n} is refused while the audit fails"
            );
            thread::sleep(Duration::from_millis(200));
        }
        let before = calls();
        // The audit recovers; the last attempt is now older than the recheck.
        thread::sleep(Duration::from_millis(2500));
        fs::remove_file(f.dir.join("audit-fails")).unwrap();
        const D: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4";
        let asked = Instant::now();
        f.enqueue(D, "concurrent 0 answer\n");
        f.wait("the job after recovery", Duration::from_secs(20), |f| {
            f.response(D).is_some()
        });
        let answer = f.response(D).unwrap();
        eprintln!(
            "answer after {:?}: {answer:?}; reconciles before={before} after={}; events: {:#?}",
            asked.elapsed(),
            calls(),
            f.events.lock().unwrap()
        );
        assert_eq!(answer.trim(), "{}", "the job ran, it was not refused");
        assert!(
            asked.elapsed() < Duration::from_secs(3),
            "answered within the 2 s recheck, not after the backoff: {:?}",
            asked.elapsed()
        );
        assert!(
            f.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("audit_reconcile_recovered")),
            "{:?}",
            f.events.lock().unwrap()
        );
    }

    /// H2 amber: once the reconcile has failed past the bound, the consumer
    /// publishes the degradation (reason, since, cause) once; when it
    /// recovers, with no job waiting to drive it, it publishes the clearing.
    #[test]
    fn a_failing_audit_is_published_and_cleared_for_controller_state() {
        let settings = QueueSettings {
            audit_unavailable_after: Duration::from_millis(800),
            ..fast()
        };
        let f = Fixture::new("audit-amber", settings);
        fs::write(f.dir.join("audit-fails"), b"").unwrap();
        let before = epoch_now();
        f.enqueue(A, "concurrent 0 answer\n");
        f.wait("the degradation published", Duration::from_secs(10), |f| {
            !f.health.lock().unwrap().is_empty()
        });
        let published = f.health.lock().unwrap().clone();
        eprintln!("{published:#?}");
        let degraded = published[0]
            .clone()
            .expect("the first change is a degradation");
        assert_eq!(degraded.reason, "audit_unavailable");
        assert!(
            degraded.since >= before && degraded.since <= epoch_now(),
            "{degraded:?}"
        );
        assert!(degraded.cause.contains("exit=73"), "{degraded:?}");
        // Nothing waits now: the episode is retried on its own and clears.
        fs::remove_file(f.dir.join("audit-fails")).unwrap();
        f.wait("the clearing published", Duration::from_secs(20), |f| {
            f.health.lock().unwrap().len() >= 2
        });
        thread::sleep(Duration::from_millis(300));
        let published = f.health.lock().unwrap().clone();
        eprintln!("{published:#?}");
        assert_eq!(published.len(), 2, "once per change: {published:#?}");
        assert_eq!(published[1], None);
    }

    /// S3-2: the degradation's cause follows the reconcile while the episode
    /// lasts. It was published when the degradation began and read exit=73
    /// for as long as it stood, though the reconcile had moved to exit=69.
    #[test]
    fn the_published_cause_follows_the_reconcile_while_it_fails() {
        let settings = QueueSettings {
            audit_unavailable_after: Duration::from_millis(500),
            reconcile_backoff_max: Duration::from_millis(200),
            ..fast()
        };
        let f = Fixture::new("audit-cause-moves", settings);
        fs::write(f.dir.join("audit-fails"), b"73").unwrap();
        f.enqueue(A, "concurrent 0 answer\n");
        f.wait("the degradation published", Duration::from_secs(10), |f| {
            !f.health.lock().unwrap().is_empty()
        });
        let first = f.health.lock().unwrap()[0]
            .clone()
            .expect("the first change is a degradation");
        assert!(first.cause.contains("exit=73"), "{first:?}");
        // The failure changes its mind; nothing else changes.
        fs::write(f.dir.join("audit-fails"), b"69").unwrap();
        f.wait("the new cause published", Duration::from_secs(20), |f| {
            f.health
                .lock()
                .unwrap()
                .iter()
                .any(|h| h.as_ref().is_some_and(|d| d.cause.contains("exit=69")))
        });
        let published = f.health.lock().unwrap().clone();
        eprintln!("{published:#?}");
        let latest = published
            .last()
            .and_then(Option::as_ref)
            .expect("still degraded");
        assert!(latest.cause.contains("exit=69"), "{latest:?}");
        assert_eq!(latest.since, first.since, "one episode, one start");
    }

    /// A control directory the daemon cannot read used to read as empty: no job
    /// was ever seen and nothing said so. It is named once per episode, with its
    /// path and cause, published to controller.state as a degradation, and its
    /// recovery is named too.
    #[test]
    fn an_unreadable_control_directory_is_named_once_and_published() {
        let requests = |dir: &Path| dir.join("var/control/requests");
        let f = Fixture::prepared("unreadable-dir", fast(), |dir| {
            fs::set_permissions(requests(dir), fs::Permissions::from_mode(0o000)).unwrap();
        });
        if super::super::testkit::permissions_do_not_bind(
            "an_unreadable_control_directory_is_named_once_and_published",
            &requests(&f.dir),
        ) {
            return;
        }
        f.wait("the dir named", Duration::from_secs(10), |f| {
            f.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("control_dir_unreadable"))
        });
        f.wait("the degradation published", Duration::from_secs(10), |f| {
            !f.health.lock().unwrap().is_empty()
        });
        // Many ticks later: still one line, not one per tick.
        thread::sleep(Duration::from_millis(400));
        let named: Vec<String> = f
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.starts_with("control_dir_unreadable"))
            .cloned()
            .collect();
        assert_eq!(named.len(), 1, "{named:#?}");
        let path = requests(&f.dir).display().to_string();
        assert!(named[0].contains(&format!("path={path}")), "{named:#?}");
        assert!(named[0].contains("cause="), "{named:#?}");
        let degraded = f.health.lock().unwrap()[0]
            .clone()
            .expect("the first change is a degradation");
        assert_eq!(degraded.reason, "control_dir_unreadable");
        assert!(degraded.cause.contains(&path), "{degraded:?}");
        // Readable again: the recovery is named and the degradation clears.
        fs::set_permissions(requests(&f.dir), fs::Permissions::from_mode(0o700)).unwrap();
        f.wait("the clearing published", Duration::from_secs(10), |f| {
            f.health.lock().unwrap().len() >= 2
        });
        assert_eq!(f.health.lock().unwrap()[1], None);
        assert!(
            f.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("control_dir_recovered") && e.contains(&path)),
            "{:?}",
            f.events.lock().unwrap()
        );
    }

    /// S3-2: an unreadable control directory whose cause changes while it stays
    /// unreadable (permission denied, then the directory replaced by a plain
    /// file) is published with the new cause and the episode's own start. The
    /// cause of the first failure used to stand in controller.state, and in the
    /// log, until the directory was readable again.
    #[test]
    fn an_unreadable_control_directorys_cause_follows_the_failure() {
        let requests = |dir: &Path| dir.join("var/control/requests");
        let f = Fixture::prepared("unreadable-dir-cause", fast(), |dir| {
            fs::set_permissions(requests(dir), fs::Permissions::from_mode(0o000)).unwrap();
        });
        if super::super::testkit::permissions_do_not_bind(
            "an_unreadable_control_directorys_cause_follows_the_failure",
            &requests(&f.dir),
        ) {
            return;
        }
        f.wait("the first cause published", Duration::from_secs(10), |f| {
            !f.health.lock().unwrap().is_empty()
        });
        let first = f.health.lock().unwrap()[0]
            .clone()
            .expect("the first change is a degradation");
        assert!(first.cause.contains("ermission denied"), "{first:?}");
        // Still unreadable, for another reason.
        fs::remove_dir(requests(&f.dir)).unwrap();
        fs::write(requests(&f.dir), b"not a directory").unwrap();
        f.wait("the new cause published", Duration::from_secs(10), |f| {
            f.health.lock().unwrap().iter().any(|h| {
                h.as_ref()
                    .is_some_and(|d| !d.cause.contains("ermission denied"))
            })
        });
        let published = f.health.lock().unwrap().clone();
        let events = f.events.lock().unwrap().clone();
        eprintln!("{published:#?}\n{events:#?}");
        let latest = published
            .last()
            .and_then(Option::as_ref)
            .expect("still degraded");
        assert_eq!(latest.reason, "control_dir_unreadable");
        assert_eq!(latest.since, first.since, "one episode keeps its start");
        assert!(
            events
                .iter()
                .any(|e| e.starts_with("control_dir_unreadable")
                    && e.contains("was=Permission denied")
                    && !e.contains("cause=Permission denied")),
            "the changed cause is named, with what it was: {events:#?}"
        );
    }

    /// H3: a consumer that outlives KILL (D state; here: out of the queue's
    /// reach) frees its slot after a bound, named with its pid; its job is
    /// answered and the lane runs the next job.
    #[test]
    fn an_unkillable_consumer_frees_its_slot_by_name() {
        let settings = QueueSettings {
            serialized_deadline: Duration::from_millis(500),
            reap_bound: Duration::from_millis(500),
            ..fast()
        };
        let f = Fixture::new("unkillable", settings);
        f.enqueue(A, "serialized 0 stubborn\n");
        f.enqueue(B, "serialized 0 answer\n");
        f.wait("the slot freed", Duration::from_secs(10), |f| {
            f.response(B).is_some()
        });
        let events = f.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| e.starts_with("consumer_unkillable")
                && e.contains(A)
                && e.contains("pid=")),
            "{events:?}"
        );
        kill_recorded(&f.dir.join("stubborn.pid"));
    }

    /// N-D10: KILL the process a test recorded, and only a real one: a pid of
    /// 0 is this process group and -1 is every process the user owns, so an
    /// unparseable record is a test failure, never a signal.
    fn kill_recorded(path: &Path) {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            // The process never got as far as recording itself.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("{}: {error}", path.display()),
        };
        let pid = recorded_pid(&text)
            .unwrap_or_else(|| panic!("{} holds no usable pid: {text:?}", path.display()));
        if let Err(error) = sys::kill_process(pid, libc::SIGKILL) {
            eprintln!("KILL {pid} from {}: {error}", path.display());
        }
    }

    fn recorded_pid(text: &str) -> Option<u32> {
        text.trim().parse::<u32>().ok().filter(|pid| *pid > 1)
    }

    #[test]
    fn a_recorded_pid_is_never_a_group_or_init() {
        assert_eq!(recorded_pid("4242\n"), Some(4242));
        for bad in ["", "0", "1", "-1", "garbage", "12 13"] {
            assert_eq!(recorded_pid(bad), None, "{bad:?}");
        }
    }

    /// N-D9: a secret sidecar the queue cannot remove is named with its path
    /// and cause (it may hold a credential); it was dropped without a word.
    #[test]
    fn an_unremovable_secret_is_named() {
        let f = Fixture::new("secret-stays", fast());
        let secret = f
            .dir
            .join("var/control/requests")
            .join(format!("{A}.secret"));
        fs::write(&secret, b"hunter2").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        f.enqueue(A, "concurrent 0 plantdir\n");
        f.wait("the job finished", Duration::from_secs(10), |f| {
            f.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("control_file_unremovable"))
        });
        let events = f.events.lock().unwrap().clone();
        eprintln!("{events:#?}");
        let named = events
            .iter()
            .find(|e| e.starts_with("control_file_unremovable"))
            .unwrap();
        assert!(
            named.contains(&format!("id={A}"))
                && named.contains("kind=secret")
                && named.contains(&format!("processing/{A}.secret"))
                && named.contains("cause="),
            "{named}"
        );
    }

    /// N-D9: an expired control file that cannot be removed is named as
    /// such; it was logged `control_file_expired` as if it had gone.
    #[test]
    fn an_expired_file_that_stays_is_not_called_expired() {
        let f = Fixture::prepared("prune-stays", fast(), |dir| {
            let responses = dir.join("var/control/responses");
            let old = responses.join(format!("{A}.json"));
            fs::write(&old, b"{}").unwrap();
            fs::set_permissions(&old, fs::Permissions::from_mode(0o600)).unwrap();
            fs::File::options()
                .write(true)
                .open(&old)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
                .unwrap();
            fs::set_permissions(&responses, fs::Permissions::from_mode(0o500)).unwrap();
        });
        let responses = f.dir.join("var/control/responses");
        if super::super::testkit::write_permissions_do_not_bind(
            "an_expired_file_that_stays_is_not_called_expired",
            &responses,
        ) {
            fs::set_permissions(&responses, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        f.wait("the prune named", Duration::from_secs(10), |f| {
            f.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.contains(A) && e.starts_with("control_file_"))
        });
        thread::sleep(Duration::from_millis(200));
        fs::set_permissions(&responses, fs::Permissions::from_mode(0o700)).unwrap();
        let events = f.events.lock().unwrap().clone();
        eprintln!("{events:#?}");
        assert!(
            !events.iter().any(|e| e.starts_with("control_file_expired")),
            "a file that is still there did not expire: {events:#?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.starts_with("control_file_unremovable")
                    && e.contains("kind=response")
                    && e.contains("ermission denied")),
            "{events:#?}"
        );
    }

    /// S2-1: what a consumer says on stderr reaches controller.log, named
    /// with its job, as it did under the shell controller.
    #[test]
    fn a_consumers_stderr_is_logged_with_its_job() {
        let f = Fixture::new("stderr", fast());
        f.enqueue(A, "concurrent 0 noisy\n");
        f.wait("the stderr logged", Duration::from_secs(10), |f| {
            f.events.lock().unwrap().iter().any(|e| {
                e.starts_with("consumer_stderr")
                    && e.contains(A)
                    && e.contains("boom: /volume1/share is gone")
            })
        });
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
