//! The control thread (design §2.2): one thread owns all mutable state, never
//! touches a share and never blocks on anything without a deadline. Everything
//! else talks to it through one channel:
//!
//! - signals (from the `sigwait` thread): TERM/INT stop, HUP reload, USR2 wake;
//! - `ctl` requests, each answered from memory (status) or immediately
//!   (run, cancel), or once the operation completes (stop, mutate);
//! - executor events (started, stall warning, finished);
//! - mutation completions from the mutation lane;
//! - change events from a realtime [`ChangeSource`].
//!
//! Invariants enforced here:
//! - I-1: one executor slot, package-wide: routine, aggregate and requested
//!   runs share it and never overlap.
//! - I-3: a realtime change dispatched to a run that does not succeed is merged
//!   back, and changes during a run stay pending for the next one.
//! - I-11/I-12: stop cancels the queue and the run within the grace, and no new
//!   run starts once shutdown is latched.
//! - R6: mutations run beside a sync; only removing (or re-defaulting) the
//!   running profile is refused, named `profile_running`.
//!
//! The module is portable: process control comes in through [`Launcher`] and
//! [`MutationRunner`], so the scheduling is tested with fakes everywhere.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::changes::{
    ArmSpec, ChangeEvent, ChangeSink, ChangeSource, ChangeSourceFactory, DirtySet,
};
use super::events::{ActivityRow, EventSink, LogEvent};
use super::executor::{
    ExecEvent, RunReport, RunSpec, StopCause, Timings, Verdict as RunVerdict, allowlisted_env,
    classify,
};
use super::routines::{
    self, Action, Backend, BackendReason, Effect, Inputs, Level, Mode, ProfileFacts, RealtimeView,
    Routine, RoutineRunState, RoutineState, RunEnd, Schedule, SkipField, SourceStatus, Verdict,
    valid_name,
};
use super::state::{
    ProfileOutcome, ProfileRecord, RunOutcome, RunRecord, RunStatus, STATE_WRITE_DEADLINE,
    StateWriter, WriteError, render_controller_state, render_profile_state, render_run_state,
    render_schedule_state, sanitize_detail,
};
use super::window::{LocalClock, window_state};

// ---------------------------------------------------------------- seams

/// The control thread's handle on one in-flight child.
pub(crate) trait RunControl: Send {
    fn stop(&self, cause: StopCause);
}

/// Starts runs. Production: the executor's process supervisor (Linux).
pub(crate) trait Launcher: Send {
    fn launch(
        &self,
        spec: RunSpec,
        sink: Box<dyn Fn(ExecEvent) + Send>,
    ) -> io::Result<Box<dyn RunControl>>;
}

/// A dashboard or CLI mutation (profile save, schedule change, …).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mutation {
    /// The manager command, e.g. `configure-profile`, `remove-profile`.
    pub(crate) op: String,
    /// The profile it concerns, if any.
    pub(crate) profile: Option<String>,
    /// Further manager arguments.
    pub(crate) args: Vec<String>,
}

impl Mutation {
    /// R6: these must not act on the profile a run is using.
    pub(crate) fn conflicts_with_a_run(&self) -> bool {
        matches!(
            self.op.as_str(),
            "remove-profile" | "remove-routine" | "set-default"
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MutationOutcome {
    pub(crate) exit: i32,
    pub(crate) detail: String,
}

/// Executes mutations on a worker thread, bounded by `deadline`.
pub(crate) trait MutationRunner: Send + Sync {
    fn run(&self, mutation: &Mutation, deadline: Duration) -> MutationOutcome;
}

/// The wall clock in epoch seconds; `None` when it cannot be read (fail closed).
pub(crate) type WallClock = Box<dyn Fn() -> Option<u64> + Send>;

// ---------------------------------------------------------------- messages

/// A `ctl` request, already parsed and uid-checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Ping,
    Status,
    Run {
        scope: String,
        action: Action,
        allow_delete: bool,
        max_total_delete: Option<u64>,
    },
    Cancel {
        id: Option<u64>,
    },
    Reload,
    Wake,
    Stop,
    RoutineNow {
        profile: String,
    },
    Mutate(Mutation),
}

pub(crate) enum Msg {
    Signal(i32),
    Ctl(Request, Sender<Value>),
    Exec(u64, ExecEvent),
    MutationDone(u64, MutationOutcome),
    /// An event from a change source, tagged with the arming it belongs to.
    Armed(u64, ChangeEvent),
    /// An untagged change event (tests drive the control thread with these).
    #[cfg(test)]
    Change(ChangeEvent),
}

/// What signals mean, independent of their numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignalKind {
    Stop,
    Reload,
    Wake,
}

// ---------------------------------------------------------------- configuration

/// Package paths the control thread reads and writes.
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    pub(crate) config_file: PathBuf,
    pub(crate) config_dir: PathBuf,
    pub(crate) profiles_dir: PathBuf,
    pub(crate) routines_dir: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) routine_state_dir: PathBuf,
    pub(crate) profile_state_dir: PathBuf,
    pub(crate) run_dir: PathBuf,
    pub(crate) log_root: PathBuf,
    pub(crate) core_binary: PathBuf,
}

impl Paths {
    /// The package layout under `home` and `var`.
    pub(crate) fn layout(home: &Path, var: &Path, core_binary: PathBuf) -> Self {
        let config_dir = home.join("config");
        let state_dir = var.join("state");
        Self {
            config_file: config_dir.join("config.toml"),
            profiles_dir: config_dir.join("profiles.d"),
            routines_dir: config_dir.join("routines.d"),
            config_dir,
            routine_state_dir: state_dir.join("routines"),
            profile_state_dir: state_dir.join("profiles"),
            state_dir,
            run_dir: var.join("run"),
            log_root: var.join("log"),
            core_binary,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Settings {
    pub(crate) timings: Timings,
    /// The longest the control thread sleeps between turns.
    pub(crate) wake_cap: Duration,
    /// How long a mutation may run before its process group is killed.
    pub(crate) mutation_deadline: Duration,
    /// The performance level's nice value for the core.
    pub(crate) core_nice: Option<i32>,
    /// Phase 3 hook (perf design §2.2): `var/state/cache/pairs`, passed to the
    /// core as `--pair-cache DIR` once the core accepts it. `None` keeps today's
    /// argv. The daemon never opens the index itself: only the sync child does,
    /// briefly, and runs stay child processes (redb can abort a process on some
    /// damaged files).
    pub(crate) pair_cache_dir: Option<PathBuf>,
    /// Publish `run.lock` for the bridge and shell readers (Linux).
    pub(crate) publish_run_lock: bool,
    /// The child's base environment (already allowlisted).
    pub(crate) child_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            timings: Timings::default(),
            wake_cap: Duration::from_secs(30),
            mutation_deadline: Duration::from_secs(120),
            core_nice: None,
            pair_cache_dir: None,
            publish_run_lock: cfg!(target_os = "linux"),
            child_env: allowlisted_env(std::env::vars_os()),
        }
    }
}

// ---------------------------------------------------------------- state

#[derive(Clone, Debug, PartialEq, Eq)]
enum Origin {
    Routine {
        name: String,
        window_day: Option<String>,
        dirty: Option<DirtySet>,
    },
    Aggregate,
    Requested,
}

#[derive(Clone, Debug)]
struct RunRequest {
    id: u64,
    origin: Origin,
    action: Action,
    /// A profile name or `all`.
    scope: String,
    allow_delete: bool,
    max_total_delete: Option<u64>,
    /// A realtime routine's lowest common ancestor (`--scope`).
    subscope: Option<String>,
}

struct Active {
    request: RunRequest,
    control: Option<Box<dyn RunControl>>,
    started_epoch: u64,
    started: Instant,
    pid: Option<u32>,
    stop: Option<StopCause>,
    stall: Option<String>,
    heartbeat_source: &'static str,
    /// Every profile the run covers (for R6 and the per-profile records).
    profiles: Vec<String>,
    degraded: Vec<&'static str>,
}

struct Entry {
    routine: Result<Option<Routine>, SkipField>,
    facts: Result<ProfileFacts, SkipField>,
    state: Result<RoutineState, &'static str>,
    dirty: DirtySet,
    source: SourceStatus,
    backend: Option<(Backend, BackendReason)>,
    /// What the change source is watching for this routine, if anything.
    armed: Option<ArmSpec>,
}

struct InFlightMutation {
    mutation: Mutation,
    started: Instant,
    reply: Sender<Value>,
}

/// The control thread's state.
pub(crate) struct Control {
    paths: Paths,
    settings: Settings,
    launcher: Box<dyn Launcher>,
    mutations: Arc<dyn MutationRunner>,
    changes: Option<Box<dyn ChangeSource>>,
    next_generation: u64,
    sink: Box<dyn EventSink>,
    writer: StateWriter,
    clock: Box<dyn LocalClock + Send>,
    wall: WallClock,
    tx: Sender<Msg>,
    pid: u32,

    routines: BTreeMap<String, Entry>,
    schedule: Schedule,
    schedule_deadline: u64,
    queue: VecDeque<RunRequest>,
    active: Option<Active>,
    next_id: u64,

    shutdown: bool,
    shutdown_deadline: Option<Instant>,
    stop_waiters: Vec<Sender<Value>>,
    held: Option<(String, RoutineState)>,
    skips: HashSet<(String, SkipField)>,
    clock_episodes: HashSet<String>,
    write_failures: HashSet<String>,
    events: VecDeque<Value>,
    in_flight: BTreeMap<u64, InFlightMutation>,
    last_controller_write: Option<Instant>,
    wall_unavailable: bool,
    turn_now: bool,
    started: Instant,
}

const EVENT_RING: usize = 256;

pub(crate) struct Parts {
    pub(crate) paths: Paths,
    pub(crate) settings: Settings,
    pub(crate) launcher: Box<dyn Launcher>,
    pub(crate) mutations: Arc<dyn MutationRunner>,
    pub(crate) changes: Option<ChangeSourceFactory>,
    pub(crate) sink: Box<dyn EventSink>,
    pub(crate) clock: Box<dyn LocalClock + Send>,
    pub(crate) wall: WallClock,
    pub(crate) pid: u32,
}

impl Control {
    pub(crate) fn new(parts: Parts, tx: Sender<Msg>) -> io::Result<Self> {
        Ok(Self {
            paths: parts.paths,
            settings: parts.settings,
            launcher: parts.launcher,
            mutations: parts.mutations,
            changes: parts.changes.map(|factory| {
                let events = tx.clone();
                let sink: ChangeSink = Arc::new(move |generation, event| {
                    events.send(Msg::Armed(generation, event)).is_ok()
                });
                factory(sink)
            }),
            next_generation: 1,
            sink: parts.sink,
            writer: StateWriter::spawn()?,
            clock: parts.clock,
            wall: parts.wall,
            tx,
            pid: parts.pid,
            routines: BTreeMap::new(),
            schedule: routines::parse_schedule(None),
            schedule_deadline: 0,
            queue: VecDeque::new(),
            active: None,
            next_id: 1,
            shutdown: false,
            shutdown_deadline: None,
            stop_waiters: Vec::new(),
            held: None,
            skips: HashSet::new(),
            clock_episodes: HashSet::new(),
            write_failures: HashSet::new(),
            events: VecDeque::new(),
            in_flight: BTreeMap::new(),
            last_controller_write: None,
            wall_unavailable: false,
            turn_now: true,
            started: Instant::now(),
        })
    }

    // ------------------------------------------------------------ the loop

    /// Run until stopped. Returns once every child is gone and the state says
    /// `stopped`.
    pub(crate) fn run(mut self, rx: Receiver<Msg>) {
        self.startup();
        loop {
            let timeout = if self.turn_now {
                Duration::ZERO
            } else {
                self.next_wake()
            };
            match rx.recv_timeout(timeout) {
                Ok(message) => self.handle(message),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.begin_shutdown("channel_closed"),
            }
            while let Ok(message) = rx.try_recv() {
                self.handle(message);
            }
            self.turn_now = false;
            if self.shutdown {
                if self.shutdown_complete() {
                    return;
                }
                continue;
            }
            self.turn();
        }
    }

    fn now(&mut self) -> Option<u64> {
        let now = (self.wall)();
        match (now, self.wall_unavailable) {
            (None, false) => {
                self.wall_unavailable = true;
                self.log(
                    Level::Error,
                    "controller",
                    "controller_clock_unavailable",
                    "cause=the system clock could not be read; nothing is scheduled until it can"
                        .into(),
                );
            }
            (Some(_), true) => {
                self.wall_unavailable = false;
                self.log(
                    Level::Warn,
                    "controller",
                    "controller_clock_recovered",
                    String::new(),
                );
            }
            _ => {}
        }
        now
    }

    /// A `running` run.state left by a daemon that died is a dead run: say so
    /// rather than leave it reading Running (or stale) forever.
    fn reconcile_dead_run(&mut self, now: u64) {
        let path = self.paths.state_dir.join("run.state");
        let Some(previous) = read_small(&path) else {
            return;
        };
        let doc = routines::KvDocument::parse(&previous, 64);
        if doc.get("state") != Some("running") {
            return;
        }
        let scope = doc
            .get("scope")
            .filter(|s| *s == "all" || valid_name(s))
            .unwrap_or("none")
            .to_owned();
        let action = if doc.get("operation") == Some("plan") {
            Action::Plan
        } else {
            Action::Sync
        };
        let started = doc
            .get("started_epoch")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut record = RunRecord::running(&scope, static_operation(action), started);
        record.status = RunStatus::Failed;
        record.finished = now.max(started);
        record.reason = "runner_died".into();
        record.detail = "the service stopped or died while this run was in flight".into();
        record.outcome = Some(RunOutcome::Failed);
        let _ = self.write_state(&path, render_run_state(&record, Some(&previous)));
        #[cfg(target_os = "linux")]
        if self.settings.publish_run_lock {
            let _ = super::state::remove_run_lock(&self.paths.run_dir);
        }
        self.log(
            Level::Warn,
            "controller",
            "run_reconciled",
            format!("scope={scope} started={started} reason=runner_died"),
        );
    }

    fn startup(&mut self) {
        self.reload("start");
        if let Some(now) = (self.wall)() {
            self.reconcile_dead_run(now);
        }
        let now = self.now().unwrap_or(0);
        self.schedule_deadline = routines::schedule_initial_deadline(
            now,
            self.schedule.interval,
            routines::parse_schedule_deadline(
                read_small(&self.paths.state_dir.join("schedule.state")).as_deref(),
            ),
        );
        if self.schedule.enabled {
            self.save_schedule();
        }
        self.write_controller_state(true);
        self.activity(
            "service.started",
            "none",
            "running",
            "controller",
            Level::Info,
            "The sync service started".into(),
        );
        self.log(
            Level::Info,
            "controller",
            "service_started",
            format!("pid={} routines={}", self.pid, self.routines.len()),
        );
    }

    fn next_wake(&self) -> Duration {
        let mut wake = self.settings.wake_cap;
        let instant_now = Instant::now();
        if let Some(deadline) = self.shutdown_deadline {
            wake = wake.min(deadline.saturating_duration_since(instant_now));
        }
        let now = (self.wall)().unwrap_or(0);
        let until_epoch = |epoch: u64| Duration::from_secs(epoch.saturating_sub(now));
        if self.schedule.enabled && self.schedule_deadline > 0 {
            wake = wake.min(until_epoch(self.schedule_deadline));
        }
        for entry in self.routines.values() {
            if let (Ok(Some(routine)), Ok(state)) = (&entry.routine, &entry.state) {
                if state.next_run > 0 {
                    wake = wake.min(until_epoch(state.next_run));
                }
                if let Mode::Realtime { debounce, .. } = routine.mode
                    && let Some(at) = entry.dirty.due_at(Duration::from_secs(debounce))
                {
                    wake = wake.min(at.saturating_duration_since(instant_now));
                }
            }
        }
        wake.max(Duration::from_millis(10))
    }

    fn handle(&mut self, message: Msg) {
        match message {
            Msg::Signal(signal) => match signal_kind(signal) {
                Some(SignalKind::Stop) => self.begin_shutdown("signal"),
                Some(SignalKind::Reload) => {
                    self.reload("signal");
                    self.turn_now = true;
                }
                Some(SignalKind::Wake) | None => self.turn_now = true,
            },
            Msg::Ctl(request, reply) => self.request(request, reply),
            Msg::Exec(id, event) => self.exec_event(id, event),
            Msg::MutationDone(id, outcome) => self.mutation_done(id, outcome),
            Msg::Armed(generation, event) => {
                // An event of an earlier arming (the routine was re-armed or
                // removed since) describes a watch that no longer exists.
                let current = self
                    .routines
                    .get(change_routine(&event))
                    .and_then(|entry| entry.armed.as_ref())
                    .is_some_and(|spec| spec.generation == generation);
                if current {
                    self.change(event);
                }
            }
            #[cfg(test)]
            Msg::Change(event) => self.change(event),
        }
    }

    // ------------------------------------------------------------ records

    fn ring(&mut self, value: Value) {
        if self.events.len() == EVENT_RING {
            self.events.pop_front();
        }
        self.events.push_back(value);
    }

    fn log(&mut self, level: Level, category: &'static str, event: &'static str, detail: String) {
        let epoch = (self.wall)().unwrap_or(0);
        let record = LogEvent {
            level,
            category,
            event,
            detail,
        };
        self.ring(json!({
            "epoch": epoch, "level": level.as_str(), "event": event, "detail": record.detail,
        }));
        if let Err(cause) = self.sink.log(epoch, &record) {
            // The sink is the log; the only place left is stderr, which the
            // lifecycle points at the daemon's own log (I-16).
            eprintln!(
                "sdsyncd: controller log event {event} could not be written ({cause}): level={} {}",
                level.as_str(),
                record.detail
            );
        }
    }

    /// Returns false when the row could not be written (the caller degrades).
    fn activity(
        &mut self,
        code: &'static str,
        profile: &str,
        state: &'static str,
        category: &'static str,
        level: Level,
        message: String,
    ) -> bool {
        let epoch = (self.wall)().unwrap_or(0);
        let row = ActivityRow {
            code,
            profile: profile.to_owned(),
            state,
            category,
            level,
            message,
        };
        match self.sink.activity(epoch, &row) {
            Ok(()) => true,
            Err(cause) => {
                self.log(
                    Level::Error,
                    "controller",
                    "activity_append_failed",
                    format!("code={code} cause={cause}"),
                );
                false
            }
        }
    }

    /// Write a state file; a failure is logged once per episode per file, and a
    /// recovery is logged too.
    fn write_state(&mut self, path: &Path, bytes: Vec<u8>) -> Result<(), WriteError> {
        let key = path.display().to_string();
        let result = self.writer.write(path, bytes, STATE_WRITE_DEADLINE);
        match &result {
            Ok(()) => {
                if self.write_failures.remove(&key) {
                    self.log(
                        Level::Warn,
                        "controller",
                        "state_write_recovered",
                        format!("path={key}"),
                    );
                }
            }
            Err(error) => {
                if self.write_failures.insert(key) {
                    let event = if error.step == "timeout" {
                        "state_write_timeout"
                    } else {
                        "state_write_failed"
                    };
                    self.log(Level::Error, "controller", event, error.to_string());
                }
            }
        }
        result
    }

    fn write_controller_state(&mut self, running: bool) {
        let next = if self.schedule.enabled {
            self.schedule_deadline
        } else {
            0
        };
        let active = self.active.as_ref().and_then(|a| a.pid).unwrap_or(0);
        let updated = (self.wall)().unwrap_or(0);
        let path = self.paths.state_dir.join("controller.state");
        let _ = self.write_state(
            &path,
            render_controller_state(running, self.pid, next, active, updated),
        );
        self.last_controller_write = Some(Instant::now());
    }

    fn save_schedule(&mut self) {
        let path = self.paths.state_dir.join("schedule.state");
        if self
            .write_state(&path, render_schedule_state(self.schedule_deadline))
            .is_err()
        {
            // write_state named it; the deadline lives on in memory.
        }
    }

    fn routine_state_path(&self, name: &str) -> PathBuf {
        self.paths.routine_state_dir.join(format!("{name}.state"))
    }

    /// Persist a routine's state. `false` when it did not land.
    fn save_routine(&mut self, name: &str) -> bool {
        let Some(Ok(state)) = self.routines.get(name).map(|e| e.state.clone()) else {
            return false;
        };
        let path = self.routine_state_path(name);
        self.write_state(&path, state.render()).is_ok()
    }

    fn apply_effects(&mut self, name: &str, effects: Vec<Effect>) -> bool {
        let mut saved = true;
        for effect in effects {
            match effect {
                Effect::Save => saved &= self.save_routine(name),
                Effect::Log {
                    level,
                    event,
                    detail,
                } => self.log(level, "scheduler", event, detail),
                Effect::Activity {
                    code,
                    state,
                    message,
                    level,
                } => {
                    self.activity(code, name, state, "routines", level, message);
                }
                Effect::Escalate { exit } => {
                    // The notifier and system-log channels are not ported to the
                    // daemon yet (cut-over item); never silent.
                    self.log(
                        Level::Error,
                        "scheduler",
                        "alert_unavailable",
                        format!(
                            "profile={name} exit={exit} channel=desktop,system_log cause=the daemon does not deliver escalations yet"
                        ),
                    );
                }
            }
        }
        saved
    }

    // ------------------------------------------------------------ configuration

    fn reload(&mut self, why: &str) {
        let schedule = routines::parse_schedule(
            read_small(&self.paths.config_dir.join("schedule.conf")).as_deref(),
        );
        if schedule.invalid_max_total && !self.schedule.invalid_max_total {
            self.log(
                Level::Warn,
                "scheduler",
                "schedule_invalid",
                "field=max_total_delete".into(),
            );
        }
        if schedule.enabled && !self.schedule.enabled {
            let now = (self.wall)().unwrap_or(0);
            self.schedule_deadline =
                routines::schedule_initial_deadline(now, schedule.interval, self.schedule_deadline);
        } else if schedule.enabled
            && schedule.interval != self.schedule.interval
            && self.schedule_deadline > 0
        {
            let now = (self.wall)().unwrap_or(0);
            self.schedule_deadline = self.schedule_deadline.min(now + schedule.interval);
        }
        self.schedule = schedule;

        let names = conf_names(&self.paths.routines_dir);
        let present: HashSet<String> = names.iter().cloned().collect();
        let gone: Vec<String> = self
            .routines
            .keys()
            .filter(|name| !present.contains(*name))
            .cloned()
            .collect();
        for name in gone {
            if let Some(entry) = self.routines.remove(&name)
                && entry.armed.is_some()
                && let Some(changes) = self.changes.as_mut()
            {
                changes.disarm(&name);
            }
        }
        for name in names {
            let routine = if self
                .paths
                .profiles_dir
                .join(format!("{name}.toml"))
                .is_file()
            {
                routines::parse_routine(
                    read_small(&self.paths.routines_dir.join(format!("{name}.conf"))).as_deref(),
                )
            } else {
                Err(SkipField::ProfileMissing)
            };
            let facts = routines::parse_profile_facts(
                read_small(&self.paths.profiles_dir.join(format!("{name}.toml"))).as_deref(),
            );
            let existing = self.routines.remove(&name);
            let state = match &existing {
                Some(entry) if entry.state.is_ok() => entry.state.clone(),
                _ => {
                    let path = self.routine_state_path(&name);
                    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                        Err("symlink")
                    } else {
                        RoutineState::parse(read_small(&path).as_deref())
                    }
                }
            };
            let mut entry = existing.unwrap_or(Entry {
                routine: Ok(None),
                facts: Err(SkipField::ProfileMaxDelete),
                state: Ok(RoutineState::default()),
                dirty: DirtySet::default(),
                source: SourceStatus::Ok,
                backend: None,
                armed: None,
            });
            entry.state = state;
            let realtime =
                matches!(&routine, Ok(Some(r)) if matches!(r.mode, Mode::Realtime { .. }));
            let source = facts.as_ref().ok().and_then(|f| f.source.clone());
            entry.routine = routine;
            entry.facts = facts;
            let wanted = match (&entry.routine, &source) {
                (Ok(Some(routine)), Some(source)) => match routine.mode {
                    Mode::Realtime { poll, .. } => {
                        let fragment = self.paths.profiles_dir.join(format!("{name}.toml"));
                        Some(ArmSpec {
                            routine: name.clone(),
                            source: PathBuf::from(source),
                            fragment_stamp: file_stamp(&fragment),
                            fragment,
                            poll: Duration::from_secs(poll.max(1)),
                            last_success_epoch: entry
                                .state
                                .as_ref()
                                .ok()
                                .map(|state| state.last_success)
                                .filter(|epoch| *epoch > 0),
                            generation: 0,
                        })
                    }
                    _ => None,
                },
                _ => None,
            };
            debug_assert_eq!(wanted.is_some(), realtime && source.is_some());
            if self.changes.is_some() {
                match (wanted, entry.armed.clone()) {
                    (Some(wanted), Some(armed)) if wanted.same_target(&armed) => {}
                    (Some(mut wanted), previous) => {
                        wanted.generation = self.next_generation;
                        self.next_generation += 1;
                        if previous.is_some() {
                            // A new source or exclude list is a new watch: the
                            // old one would keep reporting the old tree.
                            self.log(
                                Level::Info,
                                "scheduler",
                                "routine_rearmed",
                                format!(
                                    "profile={name} source={} generation={}",
                                    wanted.source.display(),
                                    wanted.generation
                                ),
                            );
                        }
                        if let Some(changes) = self.changes.as_mut() {
                            if previous.is_some() {
                                changes.disarm(&name);
                            }
                            changes.arm(wanted.clone());
                        }
                        entry.armed = Some(wanted);
                    }
                    (None, Some(_)) => {
                        if let Some(changes) = self.changes.as_mut() {
                            changes.disarm(&name);
                        }
                        entry.armed = None;
                        entry.dirty = DirtySet::default();
                    }
                    (None, None) => {}
                }
            }
            self.routines.insert(name, entry);
        }
        if why != "turn" {
            // A reload starts the once-per-field reporting again, so a fix that
            // does not take is reported again.
            self.skips.clear();
            self.log(
                Level::Info,
                "controller",
                "configuration_loaded",
                format!("trigger={why} routines={}", self.routines.len()),
            );
        }
    }

    // ------------------------------------------------------------ the turn

    fn turn(&mut self) {
        // Configuration is cheap to read and the shell re-read it every turn;
        // a save whose HUP never arrived still takes effect.
        self.reload("turn");
        let Some(now) = self.now() else {
            return;
        };
        if !self.flush_held() {
            return;
        }
        self.schedule_aggregate(now);
        self.start_next(now);
        self.evaluate_routines(now);
        if self
            .last_controller_write
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(30))
        {
            self.write_controller_state(true);
        }
    }

    fn flush_held(&mut self) -> bool {
        let Some((name, state)) = self.held.take() else {
            return true;
        };
        let path = self.routine_state_path(&name);
        if self.write_state(&path, state.render()).is_ok() {
            if let Some(entry) = self.routines.get_mut(&name) {
                entry.state = Ok(state);
            }
            true
        } else {
            // Until it lands nothing is evaluated: each would fail the same way.
            self.held = Some((name, state));
            false
        }
    }

    fn schedule_aggregate(&mut self, now: u64) {
        if !self.schedule.enabled {
            if self.schedule_deadline != 0 {
                self.schedule_deadline = 0;
                self.save_schedule();
            }
            return;
        }
        if self.schedule_deadline == 0 {
            self.schedule_deadline = now + self.schedule.interval;
            self.save_schedule();
            return;
        }
        if let Some(clamped) = super::window::clamp_clock_step(
            self.schedule_deadline as i64,
            now as i64,
            self.schedule.interval as i64,
        ) {
            self.log(
                Level::Warn,
                "scheduler",
                "clock_step_detected",
                format!(
                    "scope=schedule field=next_run_epoch ahead_seconds={}",
                    self.schedule_deadline - now
                ),
            );
            self.schedule_deadline = clamped as u64 + 60;
            self.save_schedule();
            return;
        }
        let pending = self.queue.iter().any(|r| r.origin == Origin::Aggregate)
            || self
                .active
                .as_ref()
                .is_some_and(|a| a.request.origin == Origin::Aggregate);
        if now >= self.schedule_deadline && !pending {
            let id = self.take_id();
            self.queue.push_back(RunRequest {
                id,
                origin: Origin::Aggregate,
                action: Action::Sync,
                scope: "all".into(),
                allow_delete: self.schedule.allow_delete,
                max_total_delete: Some(self.schedule.max_total_delete),
                subscope: None,
            });
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.paths.profiles_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name.strip_suffix(".toml")
                            .filter(|n| valid_name(n))
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn evaluate_routines(&mut self, now: u64) {
        let names: Vec<String> = self.routines.keys().cloned().collect();
        for name in names {
            if self.shutdown {
                return;
            }
            let running_here = self.active.as_ref().is_some_and(
                |a| matches!(&a.request.origin, Origin::Routine { name: n, .. } if *n == name),
            );
            if running_here {
                continue;
            }
            let slot_free = self.active.is_none() && self.queue.is_empty();
            let Some(entry) = self.routines.get(&name) else {
                continue;
            };
            let routine = match &entry.routine {
                Ok(Some(routine)) => routine.clone(),
                Ok(None) => continue,
                Err(field) => {
                    let field = *field;
                    self.skip(&name, field);
                    continue;
                }
            };
            let facts = match &entry.facts {
                Ok(facts) => facts.clone(),
                Err(field) => {
                    let field = *field;
                    self.skip(&name, field);
                    continue;
                }
            };
            let mut state = match &entry.state {
                Ok(state) => state.clone(),
                Err(field) => {
                    if self.skips.insert((name.clone(), SkipField::Number)) {
                        self.log(
                            Level::Warn,
                            "scheduler",
                            "routine_state_invalid",
                            format!("profile={name} field={field}"),
                        );
                    }
                    continue;
                }
            };
            let window = match &routine.mode {
                Mode::Daily {
                    weekdays,
                    start,
                    end,
                } => Some(window_state(
                    weekdays,
                    start,
                    end,
                    now as i64,
                    &state.last_day,
                    self.clock.as_ref(),
                )),
                _ => None,
            };
            let realtime = match routine.mode {
                Mode::Realtime { debounce, .. } => {
                    let (backend, reason) = match (&self.changes, entry.backend) {
                        (_, Some(reported)) => reported,
                        (Some(_), None) => (Backend::Polling, BackendReason::WatchSetupFailed),
                        (None, None) => (Backend::Polling, BackendReason::WatchUnavailable),
                    };
                    Some(RealtimeView {
                        backend,
                        reason,
                        due: entry
                            .dirty
                            .due(Instant::now(), Duration::from_secs(debounce)),
                    })
                }
                _ => None,
            };
            let source = entry.source;
            let profile_state_dir = self.paths.profile_state_dir.clone();
            let deps = move |list: &str| {
                routines::dependencies_allow(list, &|dep: &str| {
                    let path = profile_state_dir.join(format!("{dep}.state"));
                    if fs::symlink_metadata(&path).ok()?.file_type().is_symlink() {
                        return None;
                    }
                    read_small(&path).map(|bytes| routines::profile_completion_state(&bytes))
                })
            };
            let inputs = Inputs {
                name: &name,
                now,
                profile_max_delete: facts.max_delete.unwrap_or(0),
                window: window.clone(),
                realtime,
                source,
                dependencies_ok: &deps,
                shutdown: self.shutdown,
            };
            let mut effects = Vec::new();
            let verdict = routines::evaluate(&routine, &mut state, &inputs, &mut effects);
            match (&window, self.clock_episodes.contains(&name)) {
                (Some(Ok(_)), true) => {
                    self.clock_episodes.remove(&name);
                    self.log(
                        Level::Info,
                        "scheduler",
                        "routine_clock_recovered",
                        format!("profile={name}"),
                    );
                }
                (Some(Err(error)), false) if verdict == Verdict::ClockUnavailable => {
                    self.clock_episodes.insert(name.clone());
                    self.log(
                        Level::Error,
                        "scheduler",
                        "routine_clock_unavailable",
                        format!(
                            "profile={name} cause=the local date and time could not be read (status {})",
                            error.status()
                        ),
                    );
                }
                _ => {}
            }
            if matches!(verdict, Verdict::Dispatch(_)) && !slot_free {
                // Not this turn: the slot is taken. Nothing is committed, so the
                // routine is evaluated again once it frees (I-1).
                continue;
            }
            if let Some(entry) = self.routines.get_mut(&name) {
                entry.state = Ok(state);
            }
            let saved = self.apply_effects(&name, effects);
            match verdict {
                Verdict::Skip(field) => self.skip(&name, field),
                Verdict::Dispatch(dispatch) => {
                    if !saved {
                        // The record of this run cannot be written; started anyway,
                        // nothing on disk would say it ran (shell: `break`).
                        return;
                    }
                    let (dirty, subscope) = match routine.mode {
                        Mode::Realtime { .. } => {
                            let dirty = self
                                .routines
                                .get_mut(&name)
                                .map(|e| std::mem::take(&mut e.dirty))
                                .unwrap_or_default();
                            let scope = dirty.scope();
                            (Some(dirty), scope)
                        }
                        _ => (None, None),
                    };
                    let id = self.take_id();
                    let request = RunRequest {
                        id,
                        origin: Origin::Routine {
                            name: name.clone(),
                            window_day: dispatch.window_day,
                            dirty,
                        },
                        action: routine.action,
                        scope: name.clone(),
                        allow_delete: routine.allow_delete,
                        max_total_delete: None,
                        subscope,
                    };
                    self.start(request, now);
                }
                Verdict::Idle | Verdict::ClockUnavailable => {}
            }
        }
    }

    fn skip(&mut self, name: &str, field: SkipField) {
        if !self.skips.insert((name.to_owned(), field)) {
            return;
        }
        if field == SkipField::ProfileMissing {
            self.log(
                Level::Warn,
                "controller",
                "routine_profile_missing",
                format!("profile={name}"),
            );
        } else {
            self.log(
                Level::Warn,
                "controller",
                "routine_invalid",
                format!("profile={name} field={}", field.as_str()),
            );
        }
        let profile = if valid_name(name) { name } else { "none" };
        self.activity(
            "routine.deferred",
            profile,
            "deferred",
            "routines",
            Level::Warn,
            format!(
                "Routine skipped: its {} setting is invalid or its profile is missing",
                field.as_str()
            ),
        );
    }

    // ------------------------------------------------------------ runs

    fn start_next(&mut self, now: u64) {
        if self.active.is_some() || self.shutdown {
            return;
        }
        if let Some(request) = self.queue.pop_front() {
            self.start(request, now);
        }
    }

    fn core_args(&self, request: &RunRequest, result_record: &Path) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec![
            "--config".into(),
            self.paths.config_file.clone().into(),
            "--result-record".into(),
            result_record.into(),
            "--output".into(),
            "ndjson".into(),
            request.action.as_str().into(),
        ];
        if request.scope == "all" {
            args.push("--all-profiles".into());
            args.push("--max-total-delete".into());
            args.push(request.max_total_delete.unwrap_or(0).to_string().into());
        } else {
            args.push("--profile".into());
            args.push(request.scope.clone().into());
        }
        if !request.allow_delete {
            args.push("--no-delete".into());
        }
        if let Some(scope) = &request.subscope {
            args.push("--scope".into());
            args.push(scope.into());
        }
        if let Some(dir) = &self.settings.pair_cache_dir {
            args.push("--pair-cache".into());
            args.push(dir.into());
        }
        args
    }

    /// Start a run in the (free) slot. Every way it cannot start is recorded.
    fn start(&mut self, request: RunRequest, now: u64) {
        debug_assert!(self.active.is_none(), "I-1: one run at a time");
        if self.active.is_some() {
            self.queue.push_front(request);
            return;
        }
        let profiles = if request.scope == "all" {
            self.profile_names()
        } else {
            vec![request.scope.clone()]
        };
        let operation = request.action.as_str();
        let run_state_path = self.paths.state_dir.join("run.state");
        let previous = read_small(&run_state_path);
        let mut record = RunRecord::running(&request.scope, static_operation(request.action), now);
        record.heartbeat = Some(now);
        record.heartbeat_source = Some("io");
        let mut failure = self
            .write_state(
                &run_state_path,
                render_run_state(&record, previous.as_deref()),
            )
            .err()
            .map(|error| error.to_string());
        #[cfg(target_os = "linux")]
        if failure.is_none()
            && self.settings.publish_run_lock
            && let Err(error) = super::state::publish_run_lock(&self.paths.run_dir)
        {
            self.log(
                Level::Error,
                "controller",
                "run_lock_unpublished",
                error.to_string(),
            );
            failure = Some(error.to_string());
        }
        if let Some(cause) = failure {
            self.refuse_start(request, now, &cause);
            return;
        }
        let result_record = self
            .paths
            .run_dir
            .join(format!("core-result.{}", request.id));
        let _ = fs::remove_file(&result_record);
        let spec = RunSpec {
            program: self.paths.core_binary.clone(),
            args: self.core_args(&request, &result_record),
            env: self.settings.child_env.clone(),
            nice: self.settings.core_nice.or_else(|| {
                Some(performance_nice(
                    read_small(&self.paths.config_dir.join("performance.conf")).as_deref(),
                ))
            }),
            result_record: Some(result_record),
            progress_record: None,
            timings: self.settings.timings,
        };
        self.activity(
            "run.started",
            &request.scope,
            "running",
            "sync",
            Level::Info,
            format!("Package action started ({operation})"),
        );
        self.log(
            Level::Info,
            "scheduler",
            "run_started",
            format!(
                "id={} scope={} action={operation} delete={} origin={}{}",
                request.id,
                request.scope,
                request.allow_delete,
                origin_word(&request.origin),
                request
                    .subscope
                    .as_deref()
                    .map(|s| format!(" subscope={s}"))
                    .unwrap_or_default()
            ),
        );
        let id = request.id;
        let tx = self.tx.clone();
        let sink = Box::new(move |event: ExecEvent| {
            let _ = tx.send(Msg::Exec(id, event));
        });
        let control = match self.launcher.launch(spec, sink) {
            Ok(control) => Some(control),
            Err(error) => {
                // The supervisor thread itself could not start; the run never ran.
                let _ = self.tx.send(Msg::Exec(
                    id,
                    ExecEvent::Finished(Box::new(RunReport {
                        exit: -1,
                        ending: super::executor::Ending::SpawnFailed {
                            cause: format!("the run supervisor could not start: {error}"),
                        },
                        stderr_tail: Vec::new(),
                        records: Vec::new(),
                        record_error: None,
                        oom_proven: None,
                        last_phase: None,
                    })),
                ));
                None
            }
        };
        self.active = Some(Active {
            request,
            control,
            started_epoch: now,
            started: Instant::now(),
            pid: None,
            stop: None,
            stall: None,
            heartbeat_source: "io",
            profiles,
            degraded: Vec::new(),
        });
        self.write_controller_state(true);
    }

    /// The run's record could not be written before the core started: nothing
    /// ran, which is said, and the run is tried again after the floor.
    fn refuse_start(&mut self, request: RunRequest, now: u64, cause: &str) {
        self.log(
            Level::Error,
            "scheduler",
            "run_refused",
            format!(
                "id={} scope={} reason=state_unwritable cause={cause}",
                request.id, request.scope
            ),
        );
        self.activity(
            "run.failed",
            &request.scope,
            "failed",
            "sync",
            Level::Error,
            format!(
                "Package action did not start: its run record could not be written ({})",
                sanitize_detail(cause, 120)
            ),
        );
        match request.origin {
            Origin::Routine { name, dirty, .. } => {
                if let Some(entry) = self.routines.get_mut(&name) {
                    if let Some(dirty) = dirty {
                        entry.dirty.merge(dirty);
                    }
                    if let Ok(state) = entry.state.as_mut() {
                        state.state = RoutineRunState::Failed;
                        state.last_reason = "state_unwritable".into();
                        state.last_exit = -1;
                        state.next_run = now + routines::RETRY_FLOOR_SECONDS;
                    }
                }
                self.save_routine(&name);
            }
            Origin::Aggregate => {
                self.schedule_deadline = now + routines::RETRY_FLOOR_SECONDS;
                self.save_schedule();
            }
            Origin::Requested => {}
        }
    }

    fn exec_event(&mut self, id: u64, event: ExecEvent) {
        let Some(active) = self.active.as_mut().filter(|a| a.request.id == id) else {
            return;
        };
        match event {
            ExecEvent::Started { pid } => {
                active.pid = Some(pid);
                self.write_controller_state(true);
            }
            ExecEvent::Unobservable => {
                active.heartbeat_source = "liveness";
                let scope = active.request.scope.clone();
                self.log(
                    Level::Warn,
                    "scheduler",
                    "stall_unobservable",
                    format!("id={id} scope={scope} cause=/proc/<pid>/io is unreadable, so a hang cannot be detected"),
                );
                self.rewrite_running(None);
            }
            ExecEvent::StallWarning { idle, detail } => {
                active.stall = Some(detail.clone());
                let scope = active.request.scope.clone();
                self.log(
                    Level::Warn,
                    "scheduler",
                    "run_stalled",
                    format!(
                        "id={id} scope={scope} idle_seconds={} {detail}",
                        idle.as_secs()
                    ),
                );
            }
            ExecEvent::Finished(report) => self.finish(*report),
        }
    }

    /// Rewrite the running run.state (heartbeat source changes).
    fn rewrite_running(&mut self, heartbeat: Option<u64>) {
        let Some(active) = &self.active else { return };
        let mut record = RunRecord::running(
            &active.request.scope,
            static_operation(active.request.action),
            active.started_epoch,
        );
        record.heartbeat = heartbeat;
        record.heartbeat_source = Some(active.heartbeat_source);
        let path = self.paths.state_dir.join("run.state");
        let previous = read_small(&path);
        let _ = self.write_state(&path, render_run_state(&record, previous.as_deref()));
    }

    fn finish(&mut self, report: RunReport) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        let verdict = classify(&report);
        let finished = (self.wall)()
            .unwrap_or(active.started_epoch)
            .max(active.started_epoch);
        let request = active.request.clone();
        let sync = request.action == Action::Sync;

        // Per-profile records first; run.state last, so a reader that waits for
        // it sees every other record in place.
        if sync && !verdict.cancelled {
            for profile in active.profiles.clone() {
                let record = self.profile_record(&request, &report, &verdict, &profile, finished);
                let path = self
                    .paths
                    .profile_state_dir
                    .join(format!("{profile}.state"));
                let previous = read_small(&path);
                if let Some(bytes) = render_profile_state(&record, "sync", previous.as_deref())
                    && self.write_state(&path, bytes).is_err()
                {
                    push_degraded(&mut active.degraded, "profile_state");
                }
            }
        }
        let (code, state) = match verdict.outcome {
            RunOutcome::Succeeded | RunOutcome::Partial => ("run.succeeded", "succeeded"),
            RunOutcome::Busy => ("run.deferred", "deferred"),
            RunOutcome::Cancelled => ("run.cancelled", "cancelled"),
            RunOutcome::Failed => ("run.failed", "failed"),
        };
        let level = match verdict.outcome {
            RunOutcome::Succeeded | RunOutcome::Busy | RunOutcome::Cancelled => Level::Info,
            RunOutcome::Partial => Level::Warn,
            RunOutcome::Failed => Level::Error,
        };
        let message = if verdict.detail.is_empty() {
            format!("Package action {}", verdict.outcome.as_str())
        } else {
            format!(
                "Package action {}: {}",
                verdict.outcome.as_str(),
                sanitize_detail(&verdict.detail, 160)
            )
        };
        if !self.activity(code, &request.scope, state, "sync", level, message) {
            push_degraded(&mut active.degraded, "activity");
        }
        if sync && verdict.outcome == RunOutcome::Failed {
            // The failure alert (counter, desktop notification, system log) is
            // not ported to the daemon yet: say so, and mark the run's record.
            self.log(
                Level::Error,
                "scheduler",
                "alert_unavailable",
                format!(
                    "scope={} reason={} channel=desktop,system_log cause=the daemon does not deliver failure alerts yet",
                    request.scope, verdict.reason
                ),
            );
            push_degraded(&mut active.degraded, "alert");
        }

        // The core's stderr tail, kept where the dashboard's run-stderr source
        // reads it, under the header that ties it to this run (RB-2).
        if !report.stderr_tail.is_empty() {
            let mut document = format!(
                "# epoch={finished} scope={} operation={} exit={}\n",
                request.scope,
                request.action.as_str(),
                report.exit.max(0)
            )
            .into_bytes();
            document.extend_from_slice(&report.stderr_tail);
            if !document.ends_with(b"\n") {
                document.push(b'\n');
            }
            let path = self.paths.log_root.join("run-stderr.log");
            let rotated = rotate_generations(&path, 3);
            if let Err(cause) = rotated {
                self.log(
                    Level::Error,
                    "scheduler",
                    "run_stderr_unavailable",
                    format!("path={} step=rotate cause={cause}", path.display()),
                );
                push_degraded(&mut active.degraded, "run_stderr");
            } else if self.write_state(&path, document).is_err() {
                push_degraded(&mut active.degraded, "run_stderr");
            }
        }
        let (deferred, skipped) = sum_counts(&report);
        let mut record = RunRecord::running(
            &request.scope,
            static_operation(request.action),
            active.started_epoch,
        );
        record.status = verdict.status;
        record.finished = finished;
        record.exit = i64::from(report.exit);
        record.reason = verdict.reason.clone();
        record.detail = sanitize_detail(&verdict.detail, 160);
        record.outcome = Some(verdict.outcome);
        record.deferred = deferred;
        record.skipped = skipped;
        record.counts_unavailable = verdict.core_ran
            && !verdict.cancelled
            && deferred.is_none()
            && matches!(
                verdict.outcome,
                RunOutcome::Succeeded | RunOutcome::Partial | RunOutcome::Failed
            );
        record.degraded = active.degraded.clone();
        if let [single] = report.records.as_slice() {
            record.facts = single.facts.clone();
        }
        if let Some(error) = &report.record_error {
            self.log(
                Level::Warn,
                "scheduler",
                "result_record_unreadable",
                format!("id={} {error}", request.id),
            );
        }
        let path = self.paths.state_dir.join("run.state");
        let previous = read_small(&path);
        let _ = self.write_state(&path, render_run_state(&record, previous.as_deref()));
        #[cfg(target_os = "linux")]
        if self.settings.publish_run_lock
            && let Err(error) = super::state::remove_run_lock(&self.paths.run_dir)
        {
            self.log(
                Level::Error,
                "controller",
                "run_lock_unremovable",
                error.to_string(),
            );
        }

        let elapsed = active.started.elapsed().as_secs();
        self.log(
            match verdict.outcome {
                RunOutcome::Failed => Level::Error,
                RunOutcome::Partial => Level::Warn,
                _ => Level::Info,
            },
            "scheduler",
            "run_finished",
            format!(
                "id={} scope={} action={} exit={} outcome={} reason={} elapsed_seconds={elapsed}{}",
                request.id,
                request.scope,
                request.action.as_str(),
                report.exit,
                verdict.outcome.as_str(),
                verdict.reason,
                if verdict.detail.is_empty() {
                    String::new()
                } else {
                    format!(" detail={}", verdict.detail)
                }
            ),
        );
        self.ring(json!({
            "event": "run_finished", "id": request.id, "scope": request.scope,
            "exit": report.exit, "outcome": verdict.outcome.as_str(), "reason": verdict.reason,
            "detail": verdict.detail,
        }));

        match &request.origin {
            Origin::Routine {
                name,
                window_day,
                dirty,
            } => {
                self.finish_routine(
                    name,
                    window_day.as_deref(),
                    dirty.clone(),
                    &report,
                    &verdict,
                    finished,
                );
            }
            Origin::Aggregate => {
                let (next, level, reason) = routines::schedule_after_run(
                    finished,
                    self.schedule.interval,
                    report.exit,
                    verdict.cancelled,
                );
                self.schedule_deadline = next;
                self.save_schedule();
                let reason = if reason == "failed" {
                    verdict.reason.as_str()
                } else {
                    reason
                };
                self.log(
                    level,
                    "scheduler",
                    "scheduled_run",
                    format!("exit={} next={next} reason={reason}", report.exit),
                );
            }
            Origin::Requested => {}
        }
        self.write_controller_state(!self.shutdown);
        self.turn_now = true;
    }

    fn finish_routine(
        &mut self,
        name: &str,
        window_day: Option<&str>,
        dirty: Option<DirtySet>,
        report: &RunReport,
        verdict: &RunVerdict,
        finished: u64,
    ) {
        let today = self
            .clock
            .fields(finished as i64)
            .map(|f| f.day)
            .unwrap_or_else(|| "none".into());
        let Some(entry) = self.routines.get_mut(name) else {
            self.log(
                Level::Warn,
                "scheduler",
                "routine_outcome_orphaned",
                format!("profile={name} cause=the routine was removed while it ran"),
            );
            return;
        };
        // I-3: only a clean success consumes the change it was dispatched for.
        if let Some(dirty) = dirty
            && (report.exit != 0
                || verdict.cancelled
                || !matches!(report.ending, super::executor::Ending::Exited))
        {
            entry.dirty.merge(dirty);
        }
        let (Ok(Some(routine)), Ok(state)) = (&entry.routine, entry.state.as_mut()) else {
            return;
        };
        let routine = routine.clone();
        if let super::executor::Ending::SpawnFailed { .. } = report.ending {
            // Nothing ran: recorded, consumes no retry, tried again after the
            // floor (the shell's admission_failed).
            state.state = RoutineRunState::Failed;
            state.last_reason = "launch_failed".into();
            state.last_exit = -1;
            state.next_run = finished + routines::RETRY_FLOOR_SECONDS;
            let snapshot = state.clone();
            let path = self.routine_state_path(name);
            if self.write_state(&path, snapshot.render()).is_err() {
                self.held = Some((name.to_owned(), snapshot));
            }
            return;
        }
        let mut effects = Vec::new();
        let end = RunEnd {
            exit: if matches!(report.ending, super::executor::Ending::Exited) {
                report.exit
            } else if verdict.cancelled {
                143
            } else {
                report.exit.max(1)
            },
            cancelled: verdict.cancelled,
            reason: verdict.reason.clone(),
            admission_refused: false,
        };
        routines::apply_outcome(
            &routine,
            state,
            name,
            &end,
            finished,
            window_day.unwrap_or(&today),
            &mut effects,
        );
        let snapshot = state.clone();
        effects.retain(|e| *e != Effect::Save);
        let path = self.routine_state_path(name);
        if self.write_state(&path, snapshot.render()).is_err() {
            self.held = Some((name.to_owned(), snapshot));
        }
        self.apply_effects(name, effects);
    }

    fn profile_record(
        &self,
        request: &RunRequest,
        report: &RunReport,
        verdict: &RunVerdict,
        profile: &str,
        finished: u64,
    ) -> ProfileRecord {
        let aggregate = request.scope == "all";
        let job = report.records.iter().find(|r| r.profile == profile);
        let (outcome, detail, path, deferred, skipped, facts) = match job {
            Some(job) => {
                let failed = job.status == "failed" || job.error_code.is_some();
                let outcome = if failed {
                    ProfileOutcome::Failed
                } else if job.outcome.as_deref() == Some("partial") {
                    ProfileOutcome::Partial
                } else {
                    ProfileOutcome::Succeeded
                };
                (
                    outcome,
                    job.error_detail.clone(),
                    job.error_path.clone(),
                    job.deferred,
                    job.skipped,
                    job.facts.clone(),
                )
            }
            None if !aggregate => (
                match verdict.outcome {
                    RunOutcome::Succeeded => ProfileOutcome::Succeeded,
                    RunOutcome::Partial => ProfileOutcome::Partial,
                    _ => ProfileOutcome::Failed,
                },
                Some(verdict.detail.clone()).filter(|d| !d.is_empty()),
                None,
                None,
                None,
                Default::default(),
            ),
            // An aggregate whose per-profile result could not be read.
            None if report.records.is_empty() => (
                ProfileOutcome::Unknown,
                Some(verdict.detail.clone()),
                None,
                None,
                None,
                Default::default(),
            ),
            None => (
                ProfileOutcome::NotRun,
                None,
                None,
                None,
                None,
                Default::default(),
            ),
        };
        let reason = match outcome {
            ProfileOutcome::Succeeded => "ok".to_owned(),
            ProfileOutcome::Partial => "deferred_paths".to_owned(),
            _ => job
                .and_then(|j| j.error_code.clone())
                .filter(|c| routines::valid_token(c))
                .unwrap_or_else(|| verdict.reason.clone()),
        };
        ProfileRecord {
            outcome,
            exit: i64::from(report.exit),
            reason,
            finished,
            aggregate,
            deferred,
            skipped,
            core_ran: verdict.core_ran,
            facts,
            detail,
            failed_path: path,
        }
    }

    // ------------------------------------------------------------ changes

    fn change(&mut self, event: ChangeEvent) {
        let now = Instant::now();
        match event {
            ChangeEvent::Changed { routine, dir } => {
                if let Some(entry) = self.routines.get_mut(&routine) {
                    entry.dirty.mark(&dir, now);
                }
            }
            ChangeEvent::Overflow { routine } => {
                for (name, entry) in &mut self.routines {
                    if routine.as_ref().is_none_or(|r| r == name) && entry.armed.is_some() {
                        entry.dirty.mark_whole_tree(now);
                    }
                }
                self.log(
                    Level::Info,
                    "scheduler",
                    "routine_watch_overflow",
                    format!("profile={}", routine.as_deref().unwrap_or("all")),
                );
            }
            ChangeEvent::Backend {
                routine,
                backend,
                reason,
                detail,
            } => {
                let changed = self
                    .routines
                    .get_mut(&routine)
                    .map(|entry| {
                        entry.backend.replace((backend, reason)) != Some((backend, reason))
                    })
                    .unwrap_or(false);
                if changed {
                    let level = if reason == BackendReason::Ok {
                        Level::Info
                    } else {
                        Level::Warn
                    };
                    self.log(
                        level,
                        "scheduler",
                        "routine_backend",
                        format!(
                            "profile={routine} backend={} reason={} {detail}",
                            backend.as_str(),
                            reason.as_str()
                        ),
                    );
                }
            }
            ChangeEvent::Source { routine, status } => {
                if let Some(entry) = self.routines.get_mut(&routine) {
                    entry.source = status;
                }
            }
            ChangeEvent::Notice {
                routine,
                warn,
                detail,
            } => {
                self.log(
                    if warn { Level::Warn } else { Level::Info },
                    "scheduler",
                    "routine_watch_notice",
                    format!("profile={routine} {detail}"),
                );
            }
        }
        self.turn_now = true;
    }

    // ------------------------------------------------------------ requests

    fn request(&mut self, request: Request, reply: Sender<Value>) {
        match request {
            Request::Ping => {
                let _ =
                    reply.send(json!({"ok": true, "pid": self.pid, "state": self.state_word()}));
            }
            Request::Status => {
                let _ = reply.send(self.status());
            }
            Request::Reload => {
                self.reload("ctl");
                self.turn_now = true;
                let _ = reply.send(json!({"ok": true}));
            }
            Request::Wake => {
                self.turn_now = true;
                let _ = reply.send(json!({"ok": true}));
            }
            Request::Stop => {
                self.stop_waiters.push(reply);
                self.begin_shutdown("ctl");
            }
            Request::Run {
                scope,
                action,
                allow_delete,
                max_total_delete,
            } => {
                let _ = reply.send(self.enqueue(scope, action, allow_delete, max_total_delete));
            }
            Request::RoutineNow { profile } => {
                let routine = self
                    .routines
                    .get(&profile)
                    .and_then(|e| e.routine.clone().ok().flatten());
                let answer = match routine {
                    Some(routine) => self.enqueue(
                        profile,
                        routine.action,
                        routine.allow_delete,
                        Some(routine.max_total_delete),
                    ),
                    None => refusal("routine_unknown", "no enabled, valid routine has that name"),
                };
                let _ = reply.send(answer);
            }
            Request::Cancel { id } => {
                let _ = reply.send(self.cancel(id));
            }
            Request::Mutate(mutation) => self.mutate(mutation, reply),
        }
    }

    fn state_word(&self) -> &'static str {
        if self.shutdown { "stopping" } else { "running" }
    }

    fn enqueue(
        &mut self,
        scope: String,
        action: Action,
        allow_delete: bool,
        max_total_delete: Option<u64>,
    ) -> Value {
        if self.shutdown {
            return refusal("stopping", "the service is stopping and takes no new work");
        }
        if scope != "all"
            && !(valid_name(&scope)
                && self
                    .paths
                    .profiles_dir
                    .join(format!("{scope}.toml"))
                    .is_file())
        {
            return refusal("scope_not_found", "no profile has that name");
        }
        let duplicate = self.queue.iter().find(|r| {
            r.origin == Origin::Requested
                && r.scope == scope
                && r.action == action
                && r.allow_delete == allow_delete
        });
        if let Some(existing) = duplicate {
            return json!({"ok": true, "id": existing.id, "queued": true, "duplicate": true});
        }
        let id = self.take_id();
        self.queue.push_back(RunRequest {
            id,
            origin: Origin::Requested,
            action,
            scope,
            allow_delete,
            max_total_delete: max_total_delete.or(Some(self.schedule.max_total_delete)),
            subscope: None,
        });
        self.turn_now = true;
        json!({
            "ok": true, "id": id,
            "position": self.queue.len() + usize::from(self.active.is_some()),
        })
    }

    fn cancel(&mut self, id: Option<u64>) -> Value {
        if let Some(active) = self.active.as_mut()
            && id.is_none_or(|id| id == active.request.id)
        {
            let run = active.request.id;
            if active.stop.is_none() {
                active.stop = Some(StopCause::Cancel);
                if let Some(control) = &active.control {
                    control.stop(StopCause::Cancel);
                }
            }
            let scope = active.request.scope.clone();
            self.log(
                Level::Info,
                "scheduler",
                "run_cancel_requested",
                format!("id={run} scope={scope}"),
            );
            return json!({"ok": true, "id": run, "state": "stopping"});
        }
        if let Some(id) = id
            && let Some(index) = self.queue.iter().position(|r| r.id == id)
        {
            if let Some(request) = self.queue.remove(index) {
                self.activity(
                    "run.cancelled",
                    &request.scope,
                    "cancelled",
                    "sync",
                    Level::Info,
                    "Package action cancelled before it started".into(),
                );
                if let Origin::Routine {
                    name,
                    dirty: Some(dirty),
                    ..
                } = request.origin
                    && let Some(entry) = self.routines.get_mut(&name)
                {
                    entry.dirty.merge(dirty);
                }
            }
            return json!({"ok": true, "id": id, "state": "removed"});
        }
        refusal("nothing_running", "no run or queued request has that id")
    }

    fn mutate(&mut self, mutation: Mutation, reply: Sender<Value>) {
        if self.shutdown {
            let _ = reply.send(refusal(
                "stopping",
                "the service is stopping and takes no new work",
            ));
            return;
        }
        if mutation.conflicts_with_a_run()
            && let (Some(active), Some(profile)) = (&self.active, &mutation.profile)
            && active.profiles.iter().any(|p| p == profile)
        {
            let mut answer = refusal(
                "profile_running",
                &format!(
                    "profile {profile} is being synced; {} is refused until the run ends",
                    mutation.op
                ),
            );
            answer["exit"] = json!(75);
            self.log(
                Level::Info,
                "controller",
                "mutation_refused",
                format!(
                    "op={} profile={profile} reason=profile_running",
                    mutation.op
                ),
            );
            let _ = reply.send(answer);
            return;
        }
        let id = self.take_id();
        let runner = Arc::clone(&self.mutations);
        let tx = self.tx.clone();
        let deadline = self.settings.mutation_deadline;
        let worker = mutation.clone();
        let spawned = thread::Builder::new()
            .name("sdsyncd-mutation".into())
            .spawn(move || {
                let outcome = runner.run(&worker, deadline);
                let _ = tx.send(Msg::MutationDone(id, outcome));
            });
        match spawned {
            Ok(_) => {
                self.in_flight.insert(
                    id,
                    InFlightMutation {
                        mutation,
                        started: Instant::now(),
                        reply,
                    },
                );
            }
            Err(error) => {
                let _ = reply.send(refusal(
                    "mutation_unavailable",
                    &format!("the mutation worker could not start: {error}"),
                ));
            }
        }
    }

    fn mutation_done(&mut self, id: u64, outcome: MutationOutcome) {
        let Some(done) = self.in_flight.remove(&id) else {
            return;
        };
        let elapsed = done.started.elapsed();
        self.log(
            if outcome.exit == 0 {
                Level::Info
            } else {
                Level::Warn
            },
            "controller",
            "mutation_finished",
            format!(
                "op={} exit={} elapsed_ms={}{}",
                done.mutation.op,
                outcome.exit,
                elapsed.as_millis(),
                if outcome.detail.is_empty() {
                    String::new()
                } else {
                    format!(" detail={}", sanitize_detail(&outcome.detail, 160))
                }
            ),
        );
        let _ = done.reply.send(json!({
            "ok": outcome.exit == 0, "exit": outcome.exit, "detail": outcome.detail,
            "elapsed_ms": elapsed.as_millis() as u64,
        }));
        // A save applies to the next run: read it now.
        self.reload("mutation");
        self.turn_now = true;
    }

    fn status(&self) -> Value {
        let now = (self.wall)().unwrap_or(0);
        let active = self.active.as_ref().map(|a| {
            json!({
                "id": a.request.id, "scope": a.request.scope, "action": a.request.action.as_str(),
                "origin": origin_word(&a.request.origin), "pid": a.pid,
                "started_epoch": a.started_epoch, "running_seconds": a.started.elapsed().as_secs(),
                "subscope": a.request.subscope, "stopping": a.stop.map(|c| format!("{c:?}").to_lowercase()),
                "stall": a.stall, "heartbeat_source": a.heartbeat_source,
            })
        });
        let queue: Vec<Value> = self
            .queue
            .iter()
            .map(|r| json!({"id": r.id, "scope": r.scope, "action": r.action.as_str(), "origin": origin_word(&r.origin)}))
            .collect();
        let routines: Vec<Value> = self
            .routines
            .iter()
            .map(|(name, e)| {
                let state = e.state.as_ref().ok();
                json!({
                    "profile": name,
                    "valid": e.routine.is_ok() && e.facts.is_ok() && e.state.is_ok(),
                    "skip": e.routine.as_ref().err().or(e.facts.as_ref().err()).map(|f| f.as_str()),
                    "enabled": matches!(e.routine, Ok(Some(_))),
                    "state": state.map(|s| s.state.as_str()),
                    "backend": state.map(|s| s.backend.as_str()),
                    "backend_reason": state.map(|s| s.backend_reason.as_str()),
                    "next_run_epoch": state.map(|s| s.next_run),
                    "pending_change": !e.dirty.is_empty(),
                    "mode": e.routine.as_ref().ok().and_then(|r| r.as_ref()).map(|r| r.mode.as_str()),
                })
            })
            .collect();
        let mutations: Vec<Value> = self
            .in_flight
            .values()
            .map(|m| json!({"op": m.mutation.op, "profile": m.mutation.profile, "running_ms": m.started.elapsed().as_millis() as u64}))
            .collect();
        json!({
            "ok": true,
            "schema": "sdsyncd.status.v1",
            "state": self.state_word(),
            "pid": self.pid,
            "uptime_seconds": self.started.elapsed().as_secs(),
            "now_epoch": now,
            "active": active,
            "queue": queue,
            "schedule": {"enabled": self.schedule.enabled, "next_run_epoch": self.schedule_deadline},
            // Profiles a run is using now: a status read reports `index: "busy"`
            // for these rather than waiting on the pair index (perf design §2.7).
            "busy_profiles": self.active.as_ref().map(|a| a.profiles.clone()).unwrap_or_default(),
            "routines": routines,
            "mutations": mutations,
            "held_outcome": self.held.as_ref().map(|(name, _)| name.clone()),
            "events": self.events.iter().rev().take(32).cloned().collect::<Vec<_>>(),
        })
    }

    // ------------------------------------------------------------ shutdown

    fn begin_shutdown(&mut self, why: &str) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;
        let budget = self.settings.timings.cancel_grace
            + self.settings.timings.kill_wait
            + Duration::from_secs(5);
        self.shutdown_deadline = Some(Instant::now() + budget);
        self.log(
            Level::Info,
            "controller",
            "service_stopping",
            format!(
                "trigger={why} queued={} active={}",
                self.queue.len(),
                self.active.is_some()
            ),
        );
        // Queued work is dropped; a dispatched realtime change goes back.
        for request in std::mem::take(&mut self.queue) {
            if request.origin == Origin::Requested {
                self.activity(
                    "run.cancelled",
                    &request.scope,
                    "cancelled",
                    "sync",
                    Level::Info,
                    "Package action cancelled before it started: the service stopped".into(),
                );
            }
            if let Origin::Routine {
                name,
                dirty: Some(dirty),
                ..
            } = request.origin
                && let Some(entry) = self.routines.get_mut(&name)
            {
                entry.dirty.merge(dirty);
            }
        }
        if let Some(active) = self.active.as_mut() {
            if active.stop.is_none() {
                active.stop = Some(StopCause::Shutdown);
            }
            if let Some(control) = &active.control {
                control.stop(StopCause::Shutdown);
            }
        }
        if let Some(changes) = self.changes.as_mut() {
            for (name, entry) in &mut self.routines {
                if entry.armed.take().is_some() {
                    changes.disarm(name);
                }
            }
        }
    }

    /// True once everything has stopped and been recorded.
    fn shutdown_complete(&mut self) -> bool {
        let overdue = self.shutdown_deadline.is_some_and(|d| Instant::now() >= d);
        if self.active.is_some() && !overdue {
            return false;
        }
        if let Some(active) = self.active.take() {
            // The supervisor did not report within its own bound: record the run
            // as the cancellation it was, naming that it was abandoned.
            let scope = active.request.scope.clone();
            self.log(
                Level::Error,
                "controller",
                "run_unkillable",
                format!(
                    "id={} scope={scope} cause=no outcome within the stop budget",
                    active.request.id
                ),
            );
            let mut record = RunRecord::running(
                &scope,
                static_operation(active.request.action),
                active.started_epoch,
            );
            record.status = RunStatus::Failed;
            record.finished = (self.wall)().unwrap_or(active.started_epoch);
            record.exit = 137;
            record.reason = "cancelled".into();
            record.detail = "run_unkillable: the core did not exit within the stop budget".into();
            record.outcome = Some(RunOutcome::Cancelled);
            let path = self.paths.state_dir.join("run.state");
            let previous = read_small(&path);
            let _ = self.write_state(&path, render_run_state(&record, previous.as_deref()));
        }
        self.write_controller_state(false);
        self.activity(
            "service.stopped",
            "none",
            "succeeded",
            "controller",
            Level::Info,
            "The sync service stopped".into(),
        );
        self.log(
            Level::Info,
            "controller",
            "service_stopped",
            format!("pid={}", self.pid),
        );
        for waiter in std::mem::take(&mut self.stop_waiters) {
            let _ = waiter.send(json!({"ok": true, "state": "stopped"}));
        }
        for (_, mutation) in std::mem::take(&mut self.in_flight) {
            let _ = mutation.reply.send(refusal(
                "outcome_unknown",
                "the service stopped while this change ran; check the result before retrying",
            ));
        }
        true
    }
}

// ---------------------------------------------------------------- helpers

pub(crate) fn signal_kind(signal: i32) -> Option<SignalKind> {
    // POSIX numbers, fixed on every Linux architecture the package ships for.
    match signal {
        15 | 2 => Some(SignalKind::Stop),
        1 => Some(SignalKind::Reload),
        10 | 12 => Some(SignalKind::Wake),
        _ => None,
    }
}

/// The performance level's nice value for the core (`load_performance_policy_runtime`):
/// full 0, balanced 10, gentle 19; unknown, malformed or absent is balanced.
pub(crate) fn performance_nice(bytes: Option<&[u8]>) -> i32 {
    let doc = bytes.map_or_else(routines::KvDocument::empty, |b| {
        routines::KvDocument::parse(b, 256)
    });
    match doc.get("level") {
        Some("full") => 0,
        Some("gentle") => 19,
        _ => 10,
    }
}

fn refusal(code: &str, detail: &str) -> Value {
    json!({"ok": false, "code": code, "detail": detail})
}

fn origin_word(origin: &Origin) -> &'static str {
    match origin {
        Origin::Routine { .. } => "routine",
        Origin::Aggregate => "schedule",
        Origin::Requested => "request",
    }
}

fn static_operation(action: Action) -> &'static str {
    action.as_str()
}

fn push_degraded(sinks: &mut Vec<&'static str>, sink: &'static str) {
    if !sinks.contains(&sink) {
        sinks.push(sink);
    }
}

fn sum_counts(report: &RunReport) -> (Option<u64>, Option<u64>) {
    if report.records.is_empty() {
        return (None, None);
    }
    let sum = |f: &dyn Fn(&super::executor::JobRecord) -> Option<u64>| {
        report
            .records
            .iter()
            .map(f)
            .try_fold(0u64, |acc, v| v.map(|v| acc + v))
    };
    (sum(&|r| r.deferred), sum(&|r| r.skipped))
}

/// `base.(keep-1)` -> `base.keep`, …, `base` -> `base.1`.
fn rotate_generations(base: &Path, keep: usize) -> io::Result<()> {
    let generation = |n: usize| PathBuf::from(format!("{}.{n}", base.display()));
    for n in (1..keep).rev() {
        if generation(n).exists() {
            fs::rename(generation(n), generation(n + 1))?;
        }
    }
    match fs::rename(base, generation(1)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// A small package document (config or state), or `None` when absent or unreadable.
fn read_small(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Routine names from `routines.d/*.conf`, sorted, names validated.
fn conf_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".conf")
                        .filter(|n| valid_name(n))
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Spawn the control thread and return its sender.
/// The routine a change event concerns (`""` for an all-routine overflow).
fn change_routine(event: &ChangeEvent) -> &str {
    match event {
        ChangeEvent::Changed { routine, .. }
        | ChangeEvent::Backend { routine, .. }
        | ChangeEvent::Source { routine, .. }
        | ChangeEvent::Notice { routine, .. } => routine,
        ChangeEvent::Overflow { routine } => routine.as_deref().unwrap_or(""),
    }
}

/// A file's length and modification time (nanoseconds since the epoch), or
/// `None` when it cannot be read: an unreadable fragment still arms, and the
/// watcher names the failure when it reads the exclude rules.
fn file_stamp(path: &Path) -> Option<(u64, u128)> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((metadata.len(), modified))
}

pub(crate) fn spawn(parts: Parts) -> io::Result<(Sender<Msg>, thread::JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel();
    let control = Control::new(parts, tx.clone())?;
    let handle = thread::Builder::new()
        .name("sdsyncd-control".into())
        .spawn(move || control.run(rx))?;
    Ok((tx, handle))
}

/// Ask the control thread and wait at most `deadline` (design §2.5: a named
/// `daemon_busy` rather than a hang; it should never fire).
pub(crate) fn ask(tx: &Sender<Msg>, request: Request, deadline: Duration) -> Value {
    let (reply, answer) = mpsc::channel();
    if tx.send(Msg::Ctl(request, reply)).is_err() {
        return refusal("daemon_stopped", "the control thread is gone");
    }
    match answer.recv_timeout(deadline) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => refusal(
            "daemon_busy",
            &format!("no answer within {} ms", deadline.as_millis()),
        ),
        Err(RecvTimeoutError::Disconnected) => refusal(
            "daemon_stopped",
            "the control thread stopped before answering",
        ),
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
