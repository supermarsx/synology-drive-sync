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

use synology_drive_sync::watch::{Filter, WalkSlot, WatchError, profile_excludes};

use super::changes::{ArmSpec, ChangeEvent, ChangeSink, ChangeSource};
use super::poll::{PollConfig, SharedCarry, new_carry, spawn_poller};
use super::routines::{Backend, BackendReason, SourceStatus};

/// What the backend detail says while the process-wide walk cap leaves a
/// routine with nothing watching or polling it. The control thread names the
/// observation cause `walk_cap` by this text.
pub(crate) const WALK_CAP_NOTE: &str =
    "nothing is watching or polling this tree until a walk is admitted";

/// The notice detail that opens a floor-failure episode, and the one that ends it.
pub(crate) const FLOOR_FAILED: &str = "poll_floor_failed";
pub(crate) const FLOOR_RECOVERED: &str = "poll_floor_recovered";

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
    /// N-D6: while no walk of a tree finishes, it is synced whole at most this
    /// often, or every floor if that is longer. A tree that cannot be walked is
    /// one that is expensive to sync: not once a minute.
    pub(crate) degraded_min: Duration,
    /// How soon an arm that was refused by the process-wide walk cap is tried
    /// again (a crowd of routines arming together, not a broken tree).
    pub(crate) walk_cap_retry: Duration,
    /// How long the start-up catch-up waits, retrying, for a place under the
    /// process-wide walk cap before it gives up and syncs the tree whole (S3-13).
    /// A crowd of routines starting together takes turns; a cap held by walks
    /// abandoned on a stalled disk never frees, and unknown is not unchanged.
    pub(crate) catch_up_cap_wait: Duration,
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
            degraded_min: Duration::from_secs(15 * 60),
            walk_cap_retry: Duration::from_secs(15),
            catch_up_cap_wait: Duration::from_secs(600),
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
            carry: new_carry(),
            armed: AtomicBool::new(false),
            slot: real_slot(),
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
    /// What the pollers of this arming hand to each other (N-D6): `poll_for`
    /// builds a new poller on every native retry.
    carry: SharedCarry,
    /// Set by a native attempt that armed its watch, however it ended: a cap
    /// episode that follows a successful arm is a new episode (S3-13).
    armed: AtomicBool,
    /// Where the arm-time walk takes its place under the process-wide cap
    /// (tests refuse it without holding the real, process-wide places).
    slot: SlotSource,
}

/// Where a walk takes its place under the process-wide walk cap.
type SlotSource = Arc<dyn Fn() -> Result<WalkSlot, usize> + Send + Sync>;

fn real_slot() -> SlotSource {
    Arc::new(synology_drive_sync::watch::walk_slot)
}

/// How one try of the catch-up ended (S3-13).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum CatchUp {
    /// Settled: nothing to sync, or the whole-tree sync was asked for.
    Done,
    /// The routine was disarmed, or the control thread is gone.
    Stopped,
    /// The process-wide walk cap had no place for the walk: ask again.
    Capped,
}

impl CatchUp {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn of(alive: bool) -> Self {
        if alive { Self::Done } else { Self::Stopped }
    }
}

/// What a catch-up turn means for the native loop.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Turn {
    Done,
    Waiting,
    Stopped,
}

/// A catch-up the walk cap keeps refusing (S3-13).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct CatchUpWait {
    since: std::time::Instant,
    next: std::time::Instant,
    asks: u32,
    /// The deferral has been named.
    deferred: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl CatchUpWait {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            since: now,
            next: now,
            asks: 0,
            deferred: false,
        }
    }

    fn due(&self) -> bool {
        std::time::Instant::now() >= self.next
    }
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
        self.run_with(|worker, source_missing, caught_up, setup_deadline| {
            if worker.settings.native {
                worker.native(source_missing, caught_up, setup_deadline)
            } else {
                Attempt::Failed(WatchError::Unavailable(
                    "inotify is available on Linux only".to_owned(),
                ))
            }
        });
    }

    /// The arming loop, with the native attempt supplied (tests supply one).
    fn run_with(self, mut attempt: impl FnMut(&Worker, &mut bool, &mut bool, Duration) -> Attempt) {
        let mut source_missing = false;
        let mut caught_up = false;
        let mut rearm_wait = self.settings.rearm_first;
        // H4: a tree too large to arm within the deadline is given longer on
        // each retry, up to the cap, and the growth is named.
        let mut setup_deadline = self.settings.setup_deadline;
        // The walk cap is named once per episode, not on every quick retry.
        let mut cap_reported = false;
        while !self.stopped() {
            let failure = match attempt(&self, &mut source_missing, &mut caught_up, setup_deadline)
            {
                Attempt::Stopped => return,
                Attempt::Failed(error) => error,
            };
            // H9: an attempt that ended because the routine was disarmed is not
            // a failure to report, and there is nothing left to poll for.
            if self.stopped() {
                return;
            }
            // S3-13: an attempt that armed its watch ended whatever came
            // before it; a cap refusal after it is a new episode.
            if self.armed.swap(false, Ordering::SeqCst) {
                cap_reported = false;
            }
            if matches!(failure, WatchError::WalkCap { .. }) {
                // The process-wide walk cap is a transient crowd, not a verdict
                // on this tree: say so once, and retry soon rather than after
                // the long polling backoff. No poller runs during the episode
                // (that is the point of retrying soon), so the routine is not
                // reported as polling (S3-13): nothing observes it until an
                // arm walk is admitted.
                if !std::mem::replace(&mut cap_reported, true)
                    && !self.backend(
                        Backend::None,
                        BackendReason::WatchSetupFailed,
                        format!(
                            "{failure}; retrying every {} s; {WALK_CAP_NOTE}",
                            self.settings.walk_cap_retry.as_secs()
                        ),
                    )
                {
                    return;
                }
                if self.pause(self.settings.walk_cap_retry) {
                    return;
                }
                continue;
            }
            cap_reported = false;
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
        // However this attempt ends, the watch armed: a cap refusal that
        // follows is a new episode, not a continuation of an earlier one.
        self.armed.store(true, Ordering::SeqCst);
        if std::mem::take(source_missing)
            && !self.emit(ChangeEvent::Source {
                routine: self.spec.routine.clone(),
                status: SourceStatus::Ok,
            })
        {
            return Attempt::Stopped;
        }
        // The native watch is the report now: a poller that follows a later
        // failure opens with its own account again.
        super::poll::native_reported(&self.carry);
        let mut detail = format!("{} directories watched", tree.watch_count());
        if let Some(note) = tree.limit_note() {
            detail.push_str("; ");
            detail.push_str(note);
        }
        if !self.backend(Backend::Inotify, BackendReason::Ok, detail) {
            return Attempt::Stopped;
        }
        // S3-13: a catch-up walk the process-wide cap refused is asked for
        // again between the watch's own turns, not given up as unwalkable.
        let mut waiting = (!*caught_up).then(CatchUpWait::new);
        if let Some(wait) = waiting.as_mut() {
            match self.catch_up_turn(wait) {
                Turn::Stopped => return Attempt::Stopped,
                Turn::Done => {
                    *caught_up = true;
                    waiting = None;
                }
                Turn::Waiting => {}
            }
        }
        let mut floor = Floor::start(self);
        loop {
            if self.stopped() {
                return Attempt::Stopped;
            }
            if let Some(wait) = waiting.as_mut()
                && wait.due()
            {
                match self.catch_up_turn(wait) {
                    Turn::Stopped => return Attempt::Stopped,
                    Turn::Done => {
                        *caught_up = true;
                        waiting = None;
                    }
                    Turn::Waiting => {}
                }
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

    /// N-D6: how often a tree no walk of which finishes is synced whole. The
    /// floor, but never under [`WatchSettings::degraded_min`]: it was the poll
    /// interval's floor (a minute at the default 30 s poll), which synced an
    /// unwalkable tree whole every minute. At most an hour, like the floor.
    fn degraded_every(&self) -> Duration {
        self.floor().max(self.settings.degraded_min)
    }

    /// How long a catch-up refused by the walk cap waits before it asks again
    /// (S3-13): the retry interval plus up to as much again, by routine name,
    /// so routines refused together (a daemon start with more than sixteen
    /// realtime routines) come back one after another, not all at once.
    fn stagger(&self) -> Duration {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.spec.routine.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let retry = self.settings.walk_cap_retry;
        retry + retry.mul_f64((hash % 1000) as f64 / 1000.0)
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
    fn catch_up(&self) -> CatchUp {
        let routine = Some(self.spec.routine.clone());
        let filter = match self.filter() {
            Ok(filter) => Arc::new(filter),
            Err(error) => {
                return CatchUp::of(
                    self.emit(ChangeEvent::Notice {
                        routine: self.spec.routine.clone(),
                        warn: true,
                        detail: format!("catch_up=whole_tree cause={error}"),
                    }) && self.emit(ChangeEvent::Overflow { routine }),
                );
            }
        };
        let facts = newest_change_in(
            self.spec.source.clone(),
            filter,
            self.settings.walk_deadline,
            Some(&self.stop),
            &self.slot,
        );
        if self.stopped() {
            return CatchUp::Stopped;
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
            return CatchUp::Stopped;
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
            return CatchUp::Stopped;
        }
        let Some(since) = self.spec.last_success_epoch else {
            return CatchUp::of(
                self.emit(ChangeEvent::Notice {
                    routine: self.spec.routine.clone(),
                    warn: false,
                    detail: "catch_up=whole_tree cause=no_successful_run_yet".to_owned(),
                }) && self.emit(ChangeEvent::Overflow { routine }),
            );
        };
        match facts.map(|facts| facts.newest) {
            Ok(Some(newest)) if newest + 2 < since as i64 => CatchUp::Done,
            Ok(None) => CatchUp::Done,
            Ok(Some(newest)) => CatchUp::of(
                self.emit(ChangeEvent::Notice {
                    routine: self.spec.routine.clone(),
                    warn: false,
                    detail: format!(
                        "catch_up=whole_tree cause=changed_while_unwatched newest={newest} \
                         last_success={since}"
                    ),
                }) && self.emit(ChangeEvent::Overflow { routine }),
            ),
            // S3-13: the process-wide cap had no place for the walk. That says
            // nothing about this tree, so it is not synced whole for it: the
            // caller asks again, staggered.
            Err(CatchUpError::WalkCap(_)) => CatchUp::Capped,
            Err(cause) => CatchUp::of(
                self.emit(ChangeEvent::Notice {
                    routine: self.spec.routine.clone(),
                    warn: true,
                    detail: format!("catch_up=whole_tree cause={cause}"),
                }) && self.emit(ChangeEvent::Overflow { routine }),
            ),
        }
    }

    /// One turn of the catch-up (S3-13). A walk the cap refused is named once
    /// as deferred, asked for again after a stagger that differs by routine (so
    /// routines refused together do not return together), and reported when it
    /// goes through. After `catch_up_cap_wait` of refusals the tree is synced
    /// whole, with the cap as the cause: a cap that never frees must not leave
    /// the tree unchecked.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn catch_up_turn(&self, wait: &mut CatchUpWait) -> Turn {
        match self.catch_up() {
            CatchUp::Stopped => Turn::Stopped,
            CatchUp::Done => {
                if wait.deferred
                    && !self.emit(ChangeEvent::Notice {
                        routine: self.spec.routine.clone(),
                        warn: false,
                        detail: format!(
                            "catch_up=resumed waited_seconds={} asks={}",
                            wait.since.elapsed().as_secs(),
                            wait.asks
                        ),
                    })
                {
                    return Turn::Stopped;
                }
                Turn::Done
            }
            CatchUp::Capped => {
                wait.asks += 1;
                let waited = wait.since.elapsed();
                if waited >= self.settings.catch_up_cap_wait {
                    let routine = Some(self.spec.routine.clone());
                    let alive = self.emit(ChangeEvent::Notice {
                        routine: self.spec.routine.clone(),
                        warn: true,
                        detail: format!(
                            "catch_up=whole_tree cause=walk_cap waited_seconds={} asks={}; the \
                             process-wide walk cap never had a place for the catch-up walk",
                            waited.as_secs(),
                            wait.asks
                        ),
                    }) && self.emit(ChangeEvent::Overflow { routine });
                    return if alive { Turn::Done } else { Turn::Stopped };
                }
                if !std::mem::replace(&mut wait.deferred, true)
                    && !self.emit(ChangeEvent::Notice {
                        routine: self.spec.routine.clone(),
                        warn: false,
                        detail: format!(
                            "catch_up=deferred cause=walk_cap retry_seconds={} \
                             give_up_after_seconds={}; the process-wide walk cap had no place \
                             for the walk, and the tree is not synced whole for that",
                            self.stagger().as_secs(),
                            self.settings.catch_up_cap_wait.as_secs()
                        ),
                    })
                {
                    return Turn::Stopped;
                }
                wait.next = std::time::Instant::now() + self.stagger();
                Turn::Waiting
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
                degraded_every: self.degraded_every(),
                reason,
                detail,
                carry: Arc::clone(&self.carry),
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
        // H9: a disarm ends this walk at once; a stopped walk is nothing to report.
        let (baseline, failure) = match &filter {
            Some(filter) => match super::poll::read_until_stopped(
                worker.spec.source.clone(),
                Arc::clone(filter),
                worker.settings.walk_deadline,
                &worker.stop,
            ) {
                Some(Ok(reading)) => (Some(reading), failure),
                Some(Err(error)) => (None, Some(error.to_string())),
                None => (None, failure),
            },
            None => (None, failure),
        };
        if let Some(cause) = &failure {
            // A gone control thread is seen by the next emit.
            let _ = worker.emit(ChangeEvent::Notice {
                routine: worker.spec.routine.clone(),
                warn: true,
                detail: format!("{FLOOR_FAILED} cause={cause}"),
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
        // H9: a disarm ends the floor's walk at once (`None`: nothing to compare).
        super::poll::read_until_stopped(
            worker.spec.source.clone(),
            Arc::clone(filter),
            worker.settings.walk_deadline,
            &worker.stop,
        )
        .map(|result| result.map_err(|error| error.to_string()))
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
                        detail: format!("{FLOOR_FAILED} cause={cause}"),
                    });
                }
                return true;
            }
        };
        // The end of a failure episode is said too, so the observation the
        // control thread keeps for the routine can close (C2).
        let mut alive = if std::mem::replace(&mut self.failing, false) {
            worker.emit(ChangeEvent::Notice {
                routine: worker.spec.routine.clone(),
                warn: false,
                detail: FLOOR_RECOVERED.to_owned(),
            })
        } else {
            true
        };
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

/// [`newest_change_in`] under the process's real walk cap.
#[cfg(test)]
fn newest_change(
    root: PathBuf,
    filter: Arc<Filter>,
    deadline: Duration,
    stop: Option<&AtomicBool>,
) -> Result<TreeFacts, CatchUpError> {
    newest_change_in(root, filter, deadline, stop, &real_slot())
}

/// Why the arm-time walk gave no answer.
#[derive(Debug, PartialEq, Eq)]
enum CatchUpError {
    /// The process-wide walk cap had no place for it (S3-13): a crowd of
    /// routines starting together, not a verdict on this tree. The walk was
    /// not started.
    WalkCap(String),
    /// The walk was started and failed, or could not be started.
    Failed(String),
}

impl std::fmt::Display for CatchUpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WalkCap(cause) | Self::Failed(cause) => formatter.write_str(cause),
        }
    }
}

/// The arm-time walk, on a thread of its own under the process's walk cap
/// (its place taken from `take_slot`), bounded by `deadline` and ended by
/// `stop` (H9): an abandoned walk is told to cancel and does so at its next
/// directory.
fn newest_change_in(
    root: PathBuf,
    filter: Arc<Filter>,
    deadline: Duration,
    stop: Option<&AtomicBool>,
    take_slot: &SlotSource,
) -> Result<TreeFacts, CatchUpError> {
    let slot = take_slot().map_err(|live| {
        CatchUpError::WalkCap(format!(
            "walk_cap: {}",
            synology_drive_sync::watch::walk_cap_refusal(live)
        ))
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
        .map_err(|error| CatchUpError::Failed(format!("walker_unavailable: {error}")))?;
    let give_up_at = std::time::Instant::now() + deadline;
    loop {
        if stop.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
            cancel.store(true, Ordering::SeqCst);
            return Err(CatchUpError::Failed(
                "walk_stopped: the routine was disarmed".to_owned(),
            ));
        }
        let left = give_up_at.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            cancel.store(true, Ordering::SeqCst);
            let at = current.lock().map(|path| path.clone()).unwrap_or_default();
            return Err(CatchUpError::Failed(format!(
                "walk_deadline deadline={}s path={at}",
                deadline.as_secs()
            )));
        }
        match rx.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(result) => return result.map_err(CatchUpError::Failed),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(CatchUpError::Failed(
                    "walker_unavailable: the walk ended without a result".to_owned(),
                ));
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
            degraded_min: Duration::from_secs(900),
            walk_cap_retry: Duration::from_millis(100),
            catch_up_cap_wait: Duration::from_secs(5),
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
        let missing = newest_change(root.join("nope"), filter, Duration::from_secs(10), None)
            .unwrap_err()
            .to_string();
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
        if !super::super::testkit::permissions_do_not_bind(
            "the_catch_up_walk_names_its_skips_and_honours_a_stop",
            &root.join("source/locked"),
        ) {
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
            assert!(cause.to_string().starts_with("walk_stopped"), "{cause}");
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
            carry: new_carry(),
            armed: AtomicBool::new(false),
            slot: real_slot(),
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

    fn worker(root: &Path, poll: Duration, sink: ChangeSink, settings: WatchSettings) -> Worker {
        let mut arm = spec(root, 1, None);
        arm.poll = poll;
        Worker {
            spec: arm,
            stop: Arc::new(AtomicBool::new(false)),
            sink,
            settings,
            carry: new_carry(),
            armed: AtomicBool::new(false),
            slot: real_slot(),
        }
    }

    /// N-D6: an unwalkable tree is synced whole at most every fifteen minutes
    /// (or every floor, if that is longer), not every minute.
    #[test]
    fn the_degraded_whole_tree_cadence_is_the_floor_but_never_under_fifteen_minutes() {
        let root = temp_tree("cadence");
        let (_, sink) = Recorder::new();
        let at = |poll_s: u64| {
            worker(
                &root,
                Duration::from_secs(poll_s),
                Arc::clone(&sink),
                WatchSettings::default(),
            )
            .degraded_every()
        };
        // The default 30 s poll: the floor is a minute, the cadence 15 minutes.
        assert_eq!(at(30), Duration::from_secs(15 * 60));
        assert_eq!(at(60), Duration::from_secs(15 * 60));
        // A longer poll interval keeps its own floor, up to the hour.
        assert_eq!(at(30 * 60), Duration::from_secs(30 * 60));
        assert_eq!(at(10 * 3600), Duration::from_secs(3600));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// H9: the floor's baseline walk ends at once when the routine is already
    /// disarmed, and says nothing: a stopped walk is not a failed one.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_floors_walk_honours_a_disarm() {
        let root = temp_tree("floor-stop");
        let (recorder, sink) = Recorder::new();
        let mut settings = settings(true);
        // A walk that can never finish in time: only the stop can end it first.
        settings.walk_deadline = Duration::ZERO;
        let worker = worker(&root, Duration::from_secs(60), sink, settings);
        worker.stop.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let mut floor = Floor::start(&worker);
        assert!(floor.read(&worker).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            recorder.events.lock().unwrap().is_empty(),
            "a disarm is not a floor failure: {:?}",
            recorder.events.lock().unwrap()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// H9: when the routine was disarmed during the arm, the failed attempt is
    /// not polled through: no poller, no report for a routine that is gone.
    #[test]
    fn a_failure_after_a_disarm_does_not_start_a_poller() {
        let root = temp_tree("stop-then-fail");
        let (recorder, sink) = Recorder::new();
        let worker = worker(&root, Duration::from_millis(150), sink, settings(true));
        let stop = Arc::clone(&worker.stop);
        let finished = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&finished);
        let handle = thread::spawn(move || {
            worker.run_with(move |worker, _, _, _| {
                // The disarm lands while the arm walk is running, which then fails.
                stop.store(true, Ordering::SeqCst);
                Attempt::Failed(WatchError::Setup {
                    path: worker.spec.source.clone(),
                    detail: "the watch was stopped during its directory walk".to_owned(),
                })
            });
            done.store(true, Ordering::SeqCst);
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !finished.load(Ordering::SeqCst) {
            assert!(Instant::now() < until, "the worker did not end");
            thread::sleep(Duration::from_millis(10));
        }
        handle.join().unwrap();
        thread::sleep(Duration::from_millis(500));
        assert!(
            recorder.events.lock().unwrap().is_empty(),
            "nothing is reported or polled after a disarm: {:?}",
            recorder.events.lock().unwrap()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// H9: the shared walk cap is named for what it is, and an arm it refused is
    /// retried soon (it is a crowd of routines arming together), not after the
    /// long polling backoff, and not by polling through it.
    #[test]
    fn the_walk_cap_is_named_honestly_and_retried_soon() {
        let root = temp_tree("walk-cap");
        let (recorder, sink) = Recorder::new();
        let worker = worker(&root, Duration::from_millis(150), sink, settings(true));
        let stop = Arc::clone(&worker.stop);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let handle = thread::spawn(move || {
            worker.run_with(move |worker, _, _, _| {
                if counted.fetch_add(1, Ordering::SeqCst) >= 4 {
                    stop.store(true, Ordering::SeqCst);
                    return Attempt::Stopped;
                }
                Attempt::Failed(WatchError::WalkCap {
                    path: worker.spec.source.clone(),
                    live: 16,
                })
            });
        });
        let started = Instant::now();
        handle.join().unwrap();
        // Four refusals at 100 ms: seconds, not the 400 ms-to-2 s polling backoff
        // times the attempts, and certainly not 600 s.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "retried after {:?}",
            started.elapsed()
        );
        let events = recorder.events.lock().unwrap();
        let named: Vec<&ChangeEvent> = events
            .iter()
            .map(|(_, e)| e)
            .filter(|e| matches!(e, ChangeEvent::Backend { .. }))
            .collect();
        assert_eq!(named.len(), 1, "named once per episode: {events:?}");
        let ChangeEvent::Backend {
            detail,
            reason,
            backend,
            ..
        } = named[0]
        else {
            unreachable!()
        };
        assert_eq!(*reason, BackendReason::WatchSetupFailed);
        // S3-13: no poller runs during the episode, so the routine is not
        // reported as polling.
        assert_eq!(
            *backend,
            Backend::None,
            "nothing watches or polls while the cap refuses the arm: {detail}"
        );
        assert!(
            detail.contains("nothing is watching or polling"),
            "the detail says so: {detail}"
        );
        assert!(
            detail.contains("process-wide cap of 16 concurrent directory walks")
                && detail.contains("retrying every"),
            "{detail}"
        );
        assert!(
            !detail.contains("still blocked"),
            "the cap is not a blocked filesystem: {detail}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// S3-13: `cap_reported` was reset only by a non-cap failure, so a second
    /// cap episode after a native watch had armed went unreported.
    #[test]
    fn a_second_cap_episode_after_a_successful_arm_is_reported() {
        let root = temp_tree("walk-cap-twice");
        let (recorder, sink) = Recorder::new();
        let worker = worker(&root, Duration::from_millis(150), sink, settings(true));
        let stop = Arc::clone(&worker.stop);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let handle = thread::spawn(move || {
            worker.run_with(move |worker, _, _, _| {
                let cap = || {
                    Attempt::Failed(WatchError::WalkCap {
                        path: worker.spec.source.clone(),
                        live: 16,
                    })
                };
                match counted.fetch_add(1, Ordering::SeqCst) {
                    // The first episode: refused twice.
                    0 | 1 => cap(),
                    // The watch armed, and a later walk of it was refused.
                    2 => {
                        worker.armed.store(true, Ordering::SeqCst);
                        cap()
                    }
                    3 => cap(),
                    _ => {
                        stop.store(true, Ordering::SeqCst);
                        Attempt::Stopped
                    }
                }
            });
        });
        handle.join().unwrap();
        let events = recorder.events.lock().unwrap();
        let reports = events
            .iter()
            .filter(|(_, e)| matches!(e, ChangeEvent::Backend { .. }))
            .count();
        assert_eq!(
            reports, 2,
            "one report per cap episode, and the arm in between ended the first: {events:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A walk-place source that refuses the first `refusals` requests (as the
    /// process-wide cap does while other routines walk) and then takes a real
    /// place. The real places are never held, so other tests are not starved.
    fn refusing_slots(refusals: usize, asked: Arc<std::sync::atomic::AtomicUsize>) -> SlotSource {
        Arc::new(move || {
            if asked.fetch_add(1, Ordering::SeqCst) < refusals {
                Err(16)
            } else {
                synology_drive_sync::watch::walk_slot()
            }
        })
    }

    fn notices(events: &[(u64, ChangeEvent)]) -> Vec<String> {
        events
            .iter()
            .filter_map(|(_, e)| match e {
                ChangeEvent::Notice { detail, .. } => Some(detail.clone()),
                _ => None,
            })
            .collect()
    }

    fn overflows(events: &[(u64, ChangeEvent)]) -> usize {
        events
            .iter()
            .filter(|(_, e)| matches!(e, ChangeEvent::Overflow { .. }))
            .count()
    }

    /// S3-13: a catch-up walk refused by the process-wide cap was taken for an
    /// unwalkable tree and synced whole, so more than sixteen routines starting
    /// together all synced their whole trees at once. It waits its turn, and a
    /// tree unchanged since its last success is not synced.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_catch_up_refused_by_the_cap_waits_instead_of_syncing_the_whole_tree() {
        let root = temp_tree("catchup-cap");
        let (recorder, sink) = Recorder::new();
        let mut worker = worker(&root, Duration::from_secs(60), sink, settings(true));
        // Nothing in the tree is newer than the last success.
        worker.spec.last_success_epoch = Some(now_epoch() as u64 + 10_000);
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        worker.slot = refusing_slots(2, Arc::clone(&asked));
        let stop = Arc::clone(&worker.stop);
        let handle = thread::spawn(move || worker.run());
        recorder.wait(
            "the catch-up to finish",
            Duration::from_secs(10),
            |events| {
                notices(events)
                    .iter()
                    .any(|n| n.starts_with("catch_up=resumed"))
            },
        );
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        let events = recorder.events.lock().unwrap();
        let notices = notices(&events);
        assert_eq!(
            overflows(&events),
            0,
            "an unchanged tree is not synced whole because the cap was busy: {notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|n| n.starts_with("catch_up=deferred") && n.contains("cause=walk_cap")),
            "the wait is named: {notices:?}"
        );
        assert!(
            asked.load(Ordering::SeqCst) >= 3,
            "the walk was asked for again after each refusal: {}",
            asked.load(Ordering::SeqCst)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// S3-13: a cap that never frees (walks abandoned on a stalled disk) does
    /// not leave the tree unsynced for good: after the wait it is synced whole,
    /// and the notice says why.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_catch_up_the_cap_never_admits_ends_in_a_named_whole_tree_sync() {
        let root = temp_tree("catchup-cap-held");
        let (recorder, sink) = Recorder::new();
        let mut settings = settings(true);
        settings.catch_up_cap_wait = Duration::from_millis(500);
        let mut worker = worker(&root, Duration::from_secs(60), sink, settings);
        worker.spec.last_success_epoch = Some(now_epoch() as u64 + 10_000);
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        worker.slot = refusing_slots(usize::MAX, Arc::clone(&asked));
        let stop = Arc::clone(&worker.stop);
        let handle = thread::spawn(move || worker.run());
        recorder.wait("the whole-tree sync", Duration::from_secs(10), |events| {
            overflows(events) > 0
        });
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        let events = recorder.events.lock().unwrap();
        let notices = notices(&events);
        assert!(
            notices.iter().any(|n| n.starts_with("catch_up=whole_tree")
                && n.contains("cause=walk_cap")
                && n.contains("waited_seconds=")),
            "the whole-tree sync names the cap: {notices:?}"
        );
        assert!(
            asked.load(Ordering::SeqCst) >= 3,
            "it tried again before giving up: {} asks",
            asked.load(Ordering::SeqCst)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Routines that were refused together do not come back together.
    #[test]
    fn refused_catch_ups_are_staggered_by_routine() {
        let root = temp_tree("stagger");
        let (_, sink) = Recorder::new();
        let mut delays = std::collections::BTreeSet::new();
        for n in 0..16 {
            let mut worker = worker(
                &root,
                Duration::from_secs(60),
                Arc::clone(&sink),
                settings(true),
            );
            worker.spec.routine = format!("routine-{n}");
            let delay = worker.stagger();
            assert!(
                delay >= worker.settings.walk_cap_retry
                    && delay < worker.settings.walk_cap_retry * 2,
                "{delay:?}"
            );
            delays.insert(delay);
        }
        assert!(
            delays.len() >= 8,
            "sixteen routines, {} delays",
            delays.len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
