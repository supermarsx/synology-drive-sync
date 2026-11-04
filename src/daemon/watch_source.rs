//! The realtime change source: native inotify ([`synology_drive_sync::watch`]),
//! with polling only as the fallback (design §2.3).
//!
//! One worker thread per armed routine. Phase 1 keeps one inotify instance per
//! source tree on purpose (two nested sources must not share watch
//! descriptors), and a tree that is slow to walk must not hold up another.
//! Nothing here blocks the control thread: arming, walking and every failure
//! happen on the worker and come back as [`ChangeEvent`]s tagged with the
//! arming's generation, so a re-armed or removed routine never sees a stale
//! report.
//!
//! Every way the native watch cannot run is named with the existing
//! vocabulary, never a new token (the bridge validates `backend_reason`):
//! `watch_limit`, `watch_unavailable` and `watch_setup_failed` switch the
//! routine to polling with the watcher's own one-line cause; `source_missing`
//! defers it until the source is back. Polling re-tries the native watch on a
//! backoff, so a transient setup failure does not leave a routine polling for
//! good.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use synology_drive_sync::watch::{Filter, WatchError, profile_excludes};

use super::changes::{ArmSpec, ChangeEvent, ChangeSink, ChangeSource};
use super::poll::{PollConfig, spawn_poller};
use super::routines::{Backend, BackendReason, SourceStatus};

/// Timing of the change source. Tests shorten it.
#[derive(Clone, Debug)]
pub(crate) struct WatchSettings {
    /// Bound on every walk of the native watcher (arming, new subtrees).
    pub(crate) setup_deadline: Duration,
    /// Bound on one polling reading and on the start-up catch-up walk.
    pub(crate) walk_deadline: Duration,
    /// How far the polling walk's deadline, and the native watch's setup
    /// deadline, may grow on a tree too large to walk in time (H4).
    pub(crate) walk_deadline_max: Duration,
    /// First wait before polling tries the native watch again; doubles up to
    /// `rearm_max` (a tree over the watch limit stays over it).
    pub(crate) rearm_first: Duration,
    pub(crate) rearm_max: Duration,
    /// How often a missing source is looked for.
    pub(crate) source_recheck: Duration,
    /// How long one native wait lasts before `stop` is checked again.
    pub(crate) slice: Duration,
    /// `false` forces the polling fallback (tests; non-Linux always polls).
    pub(crate) native: bool,
    /// The longest the safety-net floor waits under a healthy native watch.
    pub(crate) floor_max: Duration,
}

impl Default for WatchSettings {
    fn default() -> Self {
        Self {
            setup_deadline: synology_drive_sync::watch::DEFAULT_SETUP_DEADLINE,
            walk_deadline: synology_drive_sync::watch::DEFAULT_SETUP_DEADLINE,
            walk_deadline_max: Duration::from_secs(1800),
            rearm_first: Duration::from_secs(600),
            rearm_max: Duration::from_secs(6 * 3600),
            source_recheck: Duration::from_secs(30),
            slice: Duration::from_millis(500),
            native: cfg!(target_os = "linux"),
            floor_max: Duration::from_secs(3600),
        }
    }
}

/// The production [`ChangeSource`].
pub(crate) struct WatchSource {
    sink: ChangeSink,
    settings: WatchSettings,
    workers: HashMap<String, Arc<AtomicBool>>,
}

impl WatchSource {
    pub(crate) fn new(sink: ChangeSink, settings: WatchSettings) -> Self {
        Self {
            sink,
            settings,
            workers: HashMap::new(),
        }
    }
}

impl ChangeSource for WatchSource {
    fn arm(&mut self, spec: ArmSpec) {
        self.disarm(&spec.routine);
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            spec: spec.clone(),
            stop: Arc::clone(&stop),
            sink: Arc::clone(&self.sink),
            settings: self.settings.clone(),
        };
        let spawned = thread::Builder::new()
            .name(format!("sdsyncd-watch-{}", spec.routine))
            .spawn(move || worker.run());
        match spawned {
            Ok(_) => {
                self.workers.insert(spec.routine, stop);
            }
            // No thread, no watch and no poller: say so, named, rather than
            // leave the routine reading "armed".
            Err(error) => {
                (self.sink)(
                    spec.generation,
                    ChangeEvent::Backend {
                        routine: spec.routine.clone(),
                        backend: Backend::Polling,
                        reason: BackendReason::WatchSetupFailed,
                        detail: format!("the watch thread could not start: {error}"),
                    },
                );
            }
        }
    }

    fn disarm(&mut self, routine: &str) {
        if let Some(stop) = self.workers.remove(routine) {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for WatchSource {
    fn drop(&mut self) {
        for stop in self.workers.values() {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

struct Worker {
    spec: ArmSpec,
    stop: Arc<AtomicBool>,
    sink: ChangeSink,
    settings: WatchSettings,
}

/// How one native attempt ended.
enum Attempt {
    Stopped,
    Failed(WatchError),
}

impl Worker {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Deliver one event; `false` when the control thread is gone.
    fn emit(&self, event: ChangeEvent) -> bool {
        (self.sink)(self.spec.generation, event)
    }

    fn backend(&self, backend: Backend, reason: BackendReason, detail: String) -> bool {
        self.emit(ChangeEvent::Backend {
            routine: self.spec.routine.clone(),
            backend,
            reason,
            detail,
        })
    }

    /// Sleep `total` in slices; `true` if told to stop meanwhile.
    fn pause(&self, total: Duration) -> bool {
        let deadline = std::time::Instant::now() + total;
        loop {
            if self.stopped() {
                return true;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            thread::sleep(left.min(Duration::from_millis(250)));
        }
    }

    /// The watch filter: the DSM-managed names, the source's `.sdsyncignore`
    /// and the profile's `excludes`.
    fn filter(&self) -> Result<Filter, WatchError> {
        let excludes = profile_excludes(&self.spec.fragment)?;
        Filter::build(&self.spec.source, excludes)
    }

    fn run(self) {
        let mut source_missing = false;
        let mut caught_up = false;
        let mut rearm_wait = self.settings.rearm_first;
        // H4: a tree too large to arm within the deadline is given longer on
        // each retry, up to the cap, and the growth is named.
        let mut setup_deadline = self.settings.setup_deadline;
        while !self.stopped() {
            let failure = if self.settings.native {
                match self.native(&mut source_missing, &mut caught_up, setup_deadline) {
                    Attempt::Stopped => return,
                    Attempt::Failed(error) => error,
                }
            } else {
                WatchError::Unavailable("inotify is available on Linux only".to_owned())
            };
            match failure.reason() {
                "source_missing" => {
                    if !source_missing {
                        source_missing = true;
                        if !self.emit(ChangeEvent::Source {
                            routine: self.spec.routine.clone(),
                            status: SourceStatus::Missing,
                        }) {
                            return;
                        }
                    }
                    if self.pause(self.settings.source_recheck) {
                        return;
                    }
                }
                reason => {
                    let reason = match reason {
                        "watch_limit" => BackendReason::WatchLimit,
                        "watch_unavailable" => BackendReason::WatchUnavailable,
                        _ => BackendReason::WatchSetupFailed,
                    };
                    let mut detail = failure.to_string();
                    if matches!(failure, WatchError::Deadline { .. }) {
                        setup_deadline = setup_deadline
                            .saturating_mul(2)
                            .min(self.settings.walk_deadline_max)
                            .max(setup_deadline);
                        detail.push_str(&format!(
                            "; next_setup_deadline_seconds={}",
                            setup_deadline.as_secs()
                        ));
                    }
                    match self.poll_for(reason, detail, rearm_wait, &mut source_missing) {
                        PollEnd::Stopped => return,
                        PollEnd::SourceMissing => {}
                        PollEnd::Retry => {
                            rearm_wait = (rearm_wait * 2).min(self.settings.rearm_max);
                        }
                    }
                    if !self.settings.native && self.stopped() {
                        return;
                    }
                }
            }
        }
    }

    /// Arm the native watch and serve it until it fails or is stopped.
    #[cfg(target_os = "linux")]
    fn native(
        &self,
        source_missing: &mut bool,
        caught_up: &mut bool,
        setup_deadline: Duration,
    ) -> Attempt {
        use synology_drive_sync::watch::{EventSet, LimitPolicy, TreeWatch, WatchOptions};
        let filter = match self.filter() {
            Ok(filter) => filter,
            Err(error) => return Attempt::Failed(error),
        };
        let mut events = EventSet::all();
        // CLOSE_WRITE marks the end of a write; MODIFY would be one event per
        // buffer of every copy.
        events.modify = false;
        // H9: a disarm ends the arming walk (and every later one) at once.
        let mut tree = match TreeWatch::arm_until(
            &self.spec.source,
            WatchOptions {
                filter,
                events,
                limit: LimitPolicy::System,
                setup_deadline,
            },
            Arc::clone(&self.stop),
        ) {
            Ok(tree) => tree,
            Err(error) => return Attempt::Failed(error),
        };
        if std::mem::take(source_missing)
            && !self.emit(ChangeEvent::Source {
                routine: self.spec.routine.clone(),
                status: SourceStatus::Ok,
            })
        {
            return Attempt::Stopped;
        }
        let mut detail = format!("{} directories watched", tree.watch_count());
        if let Some(note) = tree.limit_note() {
            detail.push_str("; ");
            detail.push_str(note);
        }
        if !self.backend(Backend::Inotify, BackendReason::Ok, detail) {
            return Attempt::Stopped;
        }
        if !std::mem::replace(caught_up, true) && !self.catch_up() {
            return Attempt::Stopped;
        }
        let mut floor = Floor::start(self);
        loop {
            if self.stopped() {
                return Attempt::Stopped;
            }
            match tree.wait(self.settings.slice) {
                Ok(changes) => {
                    if !self.forward(changes, &mut floor) {
                        return Attempt::Stopped;
                    }
                    if floor.due() {
                        // Changes made while the floor walked are drained
                        // first, so they are not taken for ones inotify missed.
                        let reading = floor.read(self);
                        if let Ok(late) = tree.wait(Duration::ZERO)
                            && !self.forward(late, &mut floor)
                        {
                            return Attempt::Stopped;
                        }
                        if !floor.compare(self, reading) {
                            return Attempt::Stopped;
                        }
                    }
                }
                Err(error) => {
                    // The tree is unusable from here; whatever happened since
                    // its last event is unknown, so the whole tree is due.
                    if !self.emit(ChangeEvent::Overflow {
                        routine: Some(self.spec.routine.clone()),
                    }) {
                        return Attempt::Stopped;
                    }
                    return Attempt::Failed(error);
                }
            }
        }
    }

    /// Deliver the watcher's changes, noting them for the floor.
    #[cfg(target_os = "linux")]
    fn forward(&self, changes: Vec<synology_drive_sync::watch::Change>, floor: &mut Floor) -> bool {
        use synology_drive_sync::watch::Change;
        for change in changes {
            let event = match change {
                Change::Path { dir, .. } => {
                    floor.reported(&dir);
                    ChangeEvent::Changed {
                        routine: self.spec.routine.clone(),
                        dir,
                    }
                }
                Change::Overflow => {
                    floor.overflowed();
                    ChangeEvent::Overflow {
                        routine: Some(self.spec.routine.clone()),
                    }
                }
            };
            if !self.emit(event) {
                return false;
            }
        }
        true
    }

    /// How often the floor walks a healthy native watch: the routine's poll
    /// interval, at least a minute and at most an hour.
    fn floor(&self) -> Duration {
        self.spec
            .poll
            .clamp(Duration::from_secs(60), Duration::from_secs(3600))
            .min(self.settings.floor_max)
    }

    #[cfg(not(target_os = "linux"))]
    fn native(
        &self,
        _source_missing: &mut bool,
        _caught_up: &mut bool,
        _setup_deadline: Duration,
    ) -> Attempt {
        Attempt::Failed(WatchError::Unavailable(
            "inotify is available on Linux only".to_owned(),
        ))
    }

    /// The start-up safety net (design §2.3, R2): nothing watched the tree
    /// while the daemon was down, so a tree with any entry changed after the
    /// routine's last success is synced whole. A walk that cannot finish is
    /// named, and the tree is synced whole too: unknown is not unchanged.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn catch_up(&self) -> bool {
        let routine = Some(self.spec.routine.clone());
        let filter = match self.filter() {
            Ok(filter) => Arc::new(filter),
            Err(error) => {
                return self.emit(ChangeEvent::Notice {
                    routine: self.spec.routine.clone(),
                    warn: true,
                    detail: format!("catch_up=whole_tree cause={error}"),
                }) && self.emit(ChangeEvent::Overflow { routine });
            }
        };
        let facts = newest_change(
            self.spec.source.clone(),
            filter,
            self.settings.walk_deadline,
            Some(&self.stop),
        );
        if self.stopped() {
            return false;
        }
        // S3-7: what the walk could not read is named, not passed over.
        if let Ok(facts) = &facts
            && facts.skipped > 0
            && !self.emit(ChangeEvent::Notice {
                routine: self.spec.routine.clone(),
                warn: true,
                detail: format!(
                    "catch_up_skipped entries={} first={}",
                    facts.skipped,
                    facts.skip_sample.join(",")
                ),
            })
        {
            return false;
        }
        if let Ok(facts) = &facts
            && !facts.mounts.is_empty()
            && !self.emit(ChangeEvent::Notice {
                routine: self.spec.routine.clone(),
                warn: false,
                detail: format!(
                    "nested_mounts={} first={} watched_by=poll_floor every_seconds={}",
                    facts.mounts.len(),
                    facts
                        .mounts
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(","),
                    self.floor().as_secs()
                ),
            })
        {
            return false;
        }
        let Some(since) = self.spec.last_success_epoch else {
            return self.emit(ChangeEvent::Notice {
                routine: self.spec.routine.clone(),
                warn: false,
                detail: "catch_up=whole_tree cause=no_successful_run_yet".to_owned(),
            }) && self.emit(ChangeEvent::Overflow { routine });
        };
        match facts.map(|facts| facts.newest) {
            Ok(Some(newest)) if newest + 2 < since as i64 => true,
            Ok(None) => true,
            Ok(Some(newest)) => {
                self.emit(ChangeEvent::Notice {
                    routine: self.spec.routine.clone(),
                    warn: false,
                    detail: format!(
                        "catch_up=whole_tree cause=changed_while_unwatched newest={newest} \
                         last_success={since}"
                    ),
                }) && self.emit(ChangeEvent::Overflow { routine })
            }
            Err(cause) => {
                self.emit(ChangeEvent::Notice {
                    routine: self.spec.routine.clone(),
                    warn: true,
                    detail: format!("catch_up=whole_tree cause={cause}"),
                }) && self.emit(ChangeEvent::Overflow { routine })
            }
        }
    }

    /// Poll for `wait`, then return to try the native watch again.
    fn poll_for(
        &self,
        reason: BackendReason,
        detail: String,
        wait: Duration,
        source_missing: &mut bool,
    ) -> PollEnd {
        let filter = match self.filter() {
            Ok(filter) => Arc::new(filter),
            Err(error) if error.reason() == "source_missing" => {
                if !std::mem::replace(source_missing, true)
                    && !self.emit(ChangeEvent::Source {
                        routine: self.spec.routine.clone(),
                        status: SourceStatus::Missing,
                    })
                {
                    return PollEnd::Stopped;
                }
                return if self.pause(self.settings.source_recheck) {
                    PollEnd::Stopped
                } else {
                    PollEnd::SourceMissing
                };
            }
            // Unreadable exclude rules: poll by the DSM-managed names and the
            // source's own rules alone, and say so. Reporting a change the
            // sync then excludes costs a no-op run; missing one loses data.
            Err(error) => match Filter::build(&self.spec.source, Vec::new()) {
                Ok(filter) => {
                    if !self.emit(ChangeEvent::Notice {
                        routine: self.spec.routine.clone(),
                        warn: true,
                        detail: format!("poll_filter=without_profile_excludes cause={error}"),
                    }) {
                        return PollEnd::Stopped;
                    }
                    Arc::new(filter)
                }
                Err(error) => {
                    if !self.backend(
                        Backend::Polling,
                        BackendReason::PollUnavailable,
                        error.to_string(),
                    ) {
                        return PollEnd::Stopped;
                    }
                    return if self.pause(self.settings.source_recheck) {
                        PollEnd::Stopped
                    } else {
                        PollEnd::Retry
                    };
                }
            },
        };
        if std::mem::take(source_missing)
            && !self.emit(ChangeEvent::Source {
                routine: self.spec.routine.clone(),
                status: SourceStatus::Ok,
            })
        {
            return PollEnd::Stopped;
        }
        let poll_stop = Arc::new(AtomicBool::new(false));
        let sink = Arc::clone(&self.sink);
        let generation = self.spec.generation;
        let spawned = spawn_poller(
            self.spec.routine.clone(),
            self.spec.source.clone(),
            filter,
            PollConfig {
                interval: self.spec.poll,
                walk_deadline: self.settings.walk_deadline,
                walk_deadline_max: self.settings.walk_deadline_max,
                degraded_every: self.floor(),
                reason,
                detail,
            },
            Arc::clone(&poll_stop),
            Box::new(move |event| sink(generation, event)),
        );
        let poller = match spawned {
            Ok(handle) => handle,
            Err(error) => {
                if !self.backend(
                    Backend::Polling,
                    BackendReason::PollUnavailable,
                    format!("the polling thread could not start: {error}"),
                ) {
                    return PollEnd::Stopped;
                }
                return if self.pause(self.settings.source_recheck) {
                    PollEnd::Stopped
                } else {
                    PollEnd::Retry
                };
            }
        };
        // Native retries only where there is a native watch to retry.
        let stopped = if self.settings.native {
            self.pause(wait)
        } else {
            while !self.pause(Duration::from_secs(3600)) {}
            true
        };
        poll_stop.store(true, Ordering::SeqCst);
        // The poller honours its flag within a slice; it is not joined, so a
        // walk stuck on a dying disk cannot hold this thread.
        drop(poller);
        if stopped {
            PollEnd::Stopped
        } else {
            PollEnd::Retry
        }
    }
}

enum PollEnd {
    Stopped,
    SourceMissing,
    Retry,
}

/// The safety net under a healthy native watch. inotify does not see writes
/// that do not pass through this kernel's VFS (a CIFS/NFS folder mounted in a
/// share, some bind and USB mounts) nor mmap writes that never close. A
/// stat-only reading of the tree every floor period finds those; a directory
/// whose contents moved without an event is synced and named
/// (`watch_missed_change`, warn), which shows where inotify is blind.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Floor {
    filter: Option<Arc<Filter>>,
    every: Duration,
    next: std::time::Instant,
    baseline: Option<super::poll::Reading>,
    reported: std::collections::BTreeSet<String>,
    overflowed: bool,
    failing: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Floor {
    fn start(worker: &Worker) -> Self {
        let every = worker.floor();
        // S3-7: a floor that cannot build its filter or its first reading is
        // named (once per episode, as compare() does), never silently blind.
        let (filter, failure) = match worker.filter() {
            Ok(filter) => (Some(Arc::new(filter)), None),
            Err(error) => (None, Some(format!("filter: {error}"))),
        };
        let (baseline, failure) = match &filter {
            Some(filter) => match super::poll::read_with_deadline(
                worker.spec.source.clone(),
                Arc::clone(filter),
                worker.settings.walk_deadline,
            ) {
                Ok(reading) => (Some(reading), failure),
                Err(error) => (None, Some(error.to_string())),
            },
            None => (None, failure),
        };
        if let Some(cause) = &failure {
            // A gone control thread is seen by the next emit.
            let _ = worker.emit(ChangeEvent::Notice {
                routine: worker.spec.routine.clone(),
                warn: true,
                detail: format!("poll_floor_failed cause={cause}"),
            });
        }
        Self {
            filter,
            every,
            next: std::time::Instant::now() + every,
            baseline,
            reported: std::collections::BTreeSet::new(),
            overflowed: false,
            failing: failure.is_some(),
        }
    }

    fn reported(&mut self, dir: &str) {
        self.reported.insert(dir.to_owned());
    }

    fn overflowed(&mut self) {
        self.overflowed = true;
    }

    fn due(&self) -> bool {
        std::time::Instant::now() >= self.next
    }

    fn read(&mut self, worker: &Worker) -> Option<Result<super::poll::Reading, String>> {
        self.next = std::time::Instant::now() + self.every;
        let filter = self.filter.as_ref()?;
        Some(
            super::poll::read_with_deadline(
                worker.spec.source.clone(),
                Arc::clone(filter),
                worker.settings.walk_deadline,
            )
            .map_err(|error| error.to_string()),
        )
    }

    /// Whether inotify reported `dir`'s change: the directory itself, the
    /// parent of a new one, or a directory below a removed one.
    fn covered(&self, dir: &str) -> bool {
        self.reported.iter().any(|reported| {
            reported == dir || reported.is_empty() || dir.starts_with(&format!("{reported}/"))
        })
    }

    /// Compare a fresh reading with the last; `false` when the control
    /// thread is gone.
    fn compare(
        &mut self,
        worker: &Worker,
        reading: Option<Result<super::poll::Reading, String>>,
    ) -> bool {
        let reading = match reading {
            None => return true,
            Some(Ok(reading)) => reading,
            Some(Err(cause)) => {
                // Named once per episode; the native watch goes on.
                if !std::mem::replace(&mut self.failing, true) {
                    return worker.emit(ChangeEvent::Notice {
                        routine: worker.spec.routine.clone(),
                        warn: true,
                        detail: format!("poll_floor_failed cause={cause}"),
                    });
                }
                return true;
            }
        };
        self.failing = false;
        let mut alive = true;
        if let Some(old) = &self.baseline
            && !self.overflowed
        {
            let missed: Vec<String> = super::poll::changed_dirs(old, &reading)
                .into_iter()
                .filter(|dir| !self.covered(dir))
                .collect();
            if !missed.is_empty() {
                alive = worker.emit(ChangeEvent::Notice {
                    routine: worker.spec.routine.clone(),
                    warn: true,
                    detail: format!(
                        "watch_missed_change dirs={} first={} floor_seconds={}",
                        missed.len(),
                        missed.iter().take(5).cloned().collect::<Vec<_>>().join(","),
                        self.every.as_secs()
                    ),
                });
                for dir in missed {
                    alive = alive
                        && worker.emit(ChangeEvent::Changed {
                            routine: worker.spec.routine.clone(),
                            dir,
                        });
                }
            }
        }
        self.baseline = Some(reading);
        self.reported.clear();
        self.overflowed = false;
        alive
    }
}

/// The newest modification or status change (epoch seconds) of anything in
/// the tree the filter keeps, read on a walker thread bounded by `deadline`.
/// `Ok(None)` for an empty tree.
/// What the arm-time walk learned.
#[derive(Debug, Default)]
struct TreeFacts {
    /// The newest modification or status change (epoch seconds).
    newest: Option<i64>,
    /// Directories on another filesystem than the root (USB, bind, CIFS/NFS
    /// mounts): inotify sees only this kernel's own writes there.
    mounts: Vec<String>,
    /// Entries the walk could not read (S3-7), and the first few of them.
    skipped: usize,
    skip_sample: Vec<String>,
}

/// The arm-time walk, on a thread of its own under the process's walk cap,
/// bounded by `deadline` and ended by `stop` (H9): an abandoned walk is told
/// to cancel and does so at its next directory.
fn newest_change(
    root: PathBuf,
    filter: Arc<Filter>,
    deadline: Duration,
    stop: Option<&AtomicBool>,
) -> Result<TreeFacts, String> {
    let slot = synology_drive_sync::watch::walk_slot().map_err(|live| {
        format!("walker_unavailable: {live} earlier directory walks are still blocked")
    })?;
    let current = Arc::new(std::sync::Mutex::new(String::new()));
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let walker_current = Arc::clone(&current);
    let walker_cancel = Arc::clone(&cancel);
    thread::Builder::new()
        .name("sdsyncd-catch-up".into())
        .spawn(move || {
            let _slot = slot;
            let _ = tx.send(walk_newest(&root, &filter, &walker_current, &walker_cancel));
        })
        .map_err(|error| format!("walker_unavailable: {error}"))?;
    let give_up_at = std::time::Instant::now() + deadline;
    loop {
        if stop.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
            cancel.store(true, Ordering::SeqCst);
            return Err("walk_stopped: the routine was disarmed".to_owned());
        }
        let left = give_up_at.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            cancel.store(true, Ordering::SeqCst);
            let at = current.lock().map(|path| path.clone()).unwrap_or_default();
            return Err(format!(
                "walk_deadline deadline={}s path={at}",
                deadline.as_secs()
            ));
        }
        match rx.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("walker_unavailable: the walk ended without a result".to_owned());
            }
        }
    }
}

fn walk_newest(
    root: &Path,
    filter: &Filter,
    current: &std::sync::Mutex<String>,
    cancel: &AtomicBool,
) -> Result<TreeFacts, String> {
    let mut newest: Option<i64> = None;
    let mut mounts = Vec::new();
    let mut skipped = 0_usize;
    let mut skip_sample = Vec::new();
    let mut skip = |what: String| {
        skipped += 1;
        if skip_sample.len() < 3 {
            skip_sample.push(what);
        }
    };
    #[cfg(unix)]
    let root_device = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(root).ok().map(|metadata| metadata.dev())
    };
    let mut pending = vec![String::new()];
    while let Some(relative) = pending.pop() {
        if cancel.load(Ordering::SeqCst) {
            return Err("walk_cancelled".to_owned());
        }
        let dir = if relative.is_empty() {
            root.to_owned()
        } else {
            root.join(&relative)
        };
        if let Ok(mut path) = current.lock() {
            path.clone_from(&relative);
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // The root is the only directory whose failure decides anything:
            // an unreadable subdirectory is the sync's to report.
            Err(error) if relative.is_empty() => {
                return Err(format!("root_unreadable path={}: {error}", dir.display()));
            }
            Err(error) => {
                skip(format!("{relative} ({:?})", error.kind()));
                continue;
            }
        };
        note_time(&mut newest, std::fs::symlink_metadata(&dir).ok().as_ref());
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                // Gone since it was listed: nothing to compare.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    skip(format!("{child} ({:?})", error.kind()));
                    continue;
                }
            };
            let is_dir = metadata.is_dir();
            if filter.excludes(&child, is_dir) {
                continue;
            }
            note_time(&mut newest, Some(&metadata));
            if is_dir {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if root_device.is_some_and(|device| device != metadata.dev()) {
                        mounts.push(child.clone());
                    }
                }
                pending.push(child);
            }
        }
    }
    mounts.sort();
    Ok(TreeFacts {
        newest,
        mounts,
        skipped,
        skip_sample,
    })
}

fn note_time(newest: &mut Option<i64>, metadata: Option<&std::fs::Metadata>) {
    let Some(metadata) = metadata else { return };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|age| age.as_secs() as i64);
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.ctime())
    };
    #[cfg(not(unix))]
    let changed: Option<i64> = None;
    for time in [modified, changed].into_iter().flatten() {
        *newest = Some(newest.map_or(time, |current| current.max(time)));
    }
}

/// Seconds since the epoch now, for tests.
#[cfg(test)]
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|age| age.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_tree(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sdsyncd-watch-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("source/docs")).unwrap();
        std::fs::write(root.join("profile.toml"), "source = \"x\"\n").unwrap();
        root
    }

    struct Recorder {
        events: Arc<Mutex<Vec<(u64, ChangeEvent)>>>,
    }

    impl Recorder {
        fn new() -> (Self, ChangeSink) {
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink_events = Arc::clone(&events);
            let sink: ChangeSink = Arc::new(move |generation, event| {
                sink_events.lock().unwrap().push((generation, event));
                true
            });
            (Self { events }, sink)
        }

        fn wait(&self, what: &str, within: Duration, done: impl Fn(&[(u64, ChangeEvent)]) -> bool) {
            let deadline = Instant::now() + within;
            loop {
                if done(&self.events.lock().unwrap()) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}: {:?}",
                    self.events.lock().unwrap()
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn settings(native: bool) -> WatchSettings {
        WatchSettings {
            setup_deadline: Duration::from_secs(10),
            walk_deadline: Duration::from_secs(10),
            walk_deadline_max: Duration::from_secs(10),
            rearm_first: Duration::from_millis(400),
            rearm_max: Duration::from_secs(2),
            source_recheck: Duration::from_millis(200),
            slice: Duration::from_millis(100),
            native,
            floor_max: Duration::from_secs(3600),
        }
    }

    fn spec(root: &Path, generation: u64, last_success: Option<u64>) -> ArmSpec {
        ArmSpec {
            routine: "docs".into(),
            source: root.join("source"),
            fragment: root.join("profile.toml"),
            fragment_stamp: None,
            poll: Duration::from_millis(150),
            last_success_epoch: last_success,
            generation,
        }
    }

    fn changed(events: &[(u64, ChangeEvent)], dir: &str) -> bool {
        events
            .iter()
            .any(|(_, e)| matches!(e, ChangeEvent::Changed { dir: d, .. } if d == dir))
    }

    /// The fallback alone (non-Linux, or a refused native watch): named as
    /// polling, and a change after the baseline is reported from one reading.
    #[test]
    fn the_polling_fallback_reports_a_change_and_names_itself() {
        let root = temp_tree("poll");
        let (recorder, sink) = Recorder::new();
        let mut source = WatchSource::new(sink, settings(false));
        source.arm(spec(&root, 7, Some(now_epoch() as u64 + 60)));
        recorder.wait("the polling backend", Duration::from_secs(5), |events| {
            events.iter().any(|(generation, e)| {
                *generation == 7
                    && matches!(
                        e,
                        ChangeEvent::Backend {
                            backend: Backend::Polling,
                            reason: BackendReason::WatchUnavailable,
                            detail,
                            ..
                        } if detail.contains("Linux only")
                    )
            })
        });
        thread::sleep(Duration::from_millis(400));
        std::fs::write(root.join("source/docs/new.txt"), "x").unwrap();
        recorder.wait("the change", Duration::from_secs(5), |events| {
            changed(events, "docs")
        });
        source.disarm("docs");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A missing source is reported once and its return is noticed.
    #[test]
    fn a_missing_source_is_named_once_and_its_return_noticed() {
        let root = temp_tree("missing");
        std::fs::remove_dir_all(root.join("source")).unwrap();
        let (recorder, sink) = Recorder::new();
        let mut source = WatchSource::new(sink, settings(cfg!(target_os = "linux")));
        source.arm(spec(&root, 3, Some(1)));
        recorder.wait("missing", Duration::from_secs(5), |events| {
            events.iter().any(|(_, e)| {
                matches!(
                    e,
                    ChangeEvent::Source {
                        status: SourceStatus::Missing,
                        ..
                    }
                )
            })
        });
        thread::sleep(Duration::from_millis(700));
        let missing = recorder
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e,
                    ChangeEvent::Source {
                        status: SourceStatus::Missing,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(missing, 1, "{:?}", recorder.events.lock().unwrap());
        std::fs::create_dir_all(root.join("source/docs")).unwrap();
        recorder.wait("back", Duration::from_secs(5), |events| {
            events.iter().any(|(_, e)| {
                matches!(
                    e,
                    ChangeEvent::Source {
                        status: SourceStatus::Ok,
                        ..
                    }
                )
            })
        });
        source.disarm("docs");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The catch-up walk: a tree changed after the last success, or one with
    /// no success yet, is synced whole; an untouched one is not.
    #[test]
    fn the_catch_up_walk_compares_the_newest_change_with_the_last_success() {
        let root = temp_tree("catchup");
        std::fs::write(root.join("source/docs/a.txt"), "a").unwrap();
        let filter = Arc::new(Filter::build(&root.join("source"), Vec::new()).unwrap());
        let newest = newest_change(
            root.join("source"),
            Arc::clone(&filter),
            Duration::from_secs(10),
            None,
        )
        .unwrap()
        .newest
        .unwrap();
        assert!((newest - now_epoch()).abs() < 120, "{newest}");
        // An excluded name does not count.
        let excluded = temp_tree("catchup-excluded");
        std::fs::create_dir_all(excluded.join("source/@eaDir")).unwrap();
        let only_dsm = Arc::new(Filter::build(&excluded.join("source"), Vec::new()).unwrap());
        assert!(
            newest_change(
                excluded.join("source"),
                only_dsm,
                Duration::from_secs(10),
                None
            )
            .is_ok()
        );
        // A missing root is named.
        let missing =
            newest_change(root.join("nope"), filter, Duration::from_secs(10), None).unwrap_err();
        assert!(missing.starts_with("root_unreadable"), "{missing}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&excluded);
    }

    /// S3-7: the catch-up walk names what it could not read instead of
    /// passing over it; H9: a stop ends the walk at once.
    #[cfg(unix)]
    #[test]
    fn the_catch_up_walk_names_its_skips_and_honours_a_stop() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_tree("catchup-skips");
        std::fs::create_dir_all(root.join("source/locked/inner")).unwrap();
        std::fs::set_permissions(
            root.join("source/locked"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let filter = Arc::new(Filter::build(&root.join("source"), Vec::new()).unwrap());
        if std::fs::read_dir(root.join("source/locked")).is_err() {
            let facts = newest_change(
                root.join("source"),
                Arc::clone(&filter),
                Duration::from_secs(10),
                None,
            )
            .unwrap();
            assert_eq!(facts.skipped, 1, "{facts:?}");
            assert!(facts.skip_sample[0].starts_with("locked"), "{facts:?}");
        }
        std::fs::set_permissions(
            root.join("source/locked"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let stop = AtomicBool::new(true);
        let stopped = newest_change(
            root.join("source"),
            filter,
            Duration::from_secs(10),
            Some(&stop),
        );
        // Stopped before or after a (fast) walk finished: never a hang.
        if let Err(cause) = stopped {
            assert!(cause.starts_with("walk_stopped"), "{cause}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The floor names and syncs what inotify did not report, and only that.
    #[test]
    fn the_floor_syncs_and_names_a_change_inotify_did_not_report() {
        use super::super::poll::Reading;
        let root = temp_tree("floor");
        let (recorder, sink) = Recorder::new();
        let worker = Worker {
            spec: spec(&root, 9, Some(1)),
            stop: Arc::new(AtomicBool::new(false)),
            sink,
            settings: settings(false),
        };
        let reading = |pairs: &[(&str, u64)]| Reading {
            dirs: pairs.iter().map(|(d, h)| ((*d).to_owned(), *h)).collect(),
            skipped: 0,
            skip_sample: Vec::new(),
        };
        let mut floor = Floor {
            filter: None,
            every: Duration::from_secs(60),
            next: std::time::Instant::now(),
            baseline: Some(reading(&[("", 1), ("docs", 1), ("mnt", 1)])),
            reported: std::collections::BTreeSet::new(),
            overflowed: false,
            failing: false,
        };
        // inotify saw docs; mnt (a CIFS mount, say) changed unseen.
        floor.reported("docs");
        assert!(floor.compare(
            &worker,
            Some(Ok(reading(&[("", 1), ("docs", 2), ("mnt", 2)])))
        ));
        let events = recorder.events.lock().unwrap().clone();
        assert!(changed(&events, "mnt"), "{events:?}");
        assert!(
            !changed(&events, "docs"),
            "reported changes are not repeated"
        );
        assert!(
            events.iter().any(
                |(_, e)| matches!(e, ChangeEvent::Notice { warn: true, detail, .. }
                if detail.starts_with("watch_missed_change dirs=1 first=mnt"))
            ),
            "{events:?}"
        );
        // A reading that fails is named once, not every period.
        assert!(floor.compare(&worker, Some(Err("walk_deadline path=mnt".into()))));
        assert!(floor.compare(&worker, Some(Err("walk_deadline path=mnt".into()))));
        let failures = recorder
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| matches!(e, ChangeEvent::Notice { detail, .. } if detail.starts_with("poll_floor_failed")))
            .count();
        assert_eq!(failures, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Nested mounts are found at arm time (Linux: /dev holds devpts, shm).
    #[cfg(target_os = "linux")]
    #[test]
    fn nested_mounts_are_found_by_device() {
        let filter = Arc::new(Filter::build(Path::new("/dev"), Vec::new()).unwrap());
        let facts =
            newest_change(PathBuf::from("/dev"), filter, Duration::from_secs(10), None).unwrap();
        assert!(
            facts
                .mounts
                .iter()
                .any(|m| m == "pts" || m == "shm" || m == "mqueue"),
            "{:?}",
            facts.mounts
        );
    }

    /// Linux: the native watch is named `inotify`/`ok`, the catch-up runs for a
    /// tree changed after the last success, and a write is reported by
    /// directory.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_native_watch_reports_writes_by_directory() {
        let root = temp_tree("native");
        let (recorder, sink) = Recorder::new();
        let mut source = WatchSource::new(sink, settings(true));
        source.arm(spec(&root, 11, Some(1)));
        recorder.wait("inotify", Duration::from_secs(10), |events| {
            events.iter().any(|(_, e)| {
                matches!(
                    e,
                    ChangeEvent::Backend {
                        backend: Backend::Inotify,
                        reason: BackendReason::Ok,
                        ..
                    }
                )
            })
        });
        recorder.wait("the catch-up", Duration::from_secs(10), |events| {
            events.iter().any(|(_, e)| {
                matches!(e, ChangeEvent::Notice { detail, .. }
                    if detail.contains("changed_while_unwatched"))
            }) && events
                .iter()
                .any(|(_, e)| matches!(e, ChangeEvent::Overflow { .. }))
        });
        std::fs::write(root.join("source/docs/written.txt"), "x").unwrap();
        recorder.wait("the write", Duration::from_secs(10), |events| {
            changed(events, "docs")
        });
        assert!(
            recorder
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|(generation, _)| *generation == 11)
        );
        source.disarm("docs");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A disarmed worker stops emitting.
    #[test]
    fn a_disarmed_routine_goes_quiet() {
        let root = temp_tree("quiet");
        let (recorder, sink) = Recorder::new();
        let mut source = WatchSource::new(sink, settings(false));
        source.arm(spec(&root, 5, Some(now_epoch() as u64 + 60)));
        recorder.wait("armed", Duration::from_secs(5), |events| !events.is_empty());
        thread::sleep(Duration::from_millis(400));
        source.disarm("docs");
        thread::sleep(Duration::from_millis(700));
        let before = recorder.events.lock().unwrap().len();
        std::fs::write(root.join("source/docs/after.txt"), "x").unwrap();
        thread::sleep(Duration::from_millis(800));
        assert_eq!(recorder.events.lock().unwrap().len(), before);
        let _ = std::fs::remove_dir_all(&root);
    }
}
