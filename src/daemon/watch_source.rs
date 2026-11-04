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
}

impl Default for WatchSettings {
    fn default() -> Self {
        Self {
            setup_deadline: synology_drive_sync::watch::DEFAULT_SETUP_DEADLINE,
            walk_deadline: synology_drive_sync::watch::DEFAULT_SETUP_DEADLINE,
            rearm_first: Duration::from_secs(600),
            rearm_max: Duration::from_secs(6 * 3600),
            source_recheck: Duration::from_secs(30),
            slice: Duration::from_millis(500),
            native: cfg!(target_os = "linux"),
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
        while !self.stopped() {
            let failure = if self.settings.native {
                match self.native(&mut source_missing, &mut caught_up) {
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
                    match self.poll_for(
                        reason,
                        failure.to_string(),
                        rearm_wait,
                        &mut source_missing,
                    ) {
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
    fn native(&self, source_missing: &mut bool, caught_up: &mut bool) -> Attempt {
        use synology_drive_sync::watch::{Change, EventSet, LimitPolicy, TreeWatch, WatchOptions};
        let filter = match self.filter() {
            Ok(filter) => filter,
            Err(error) => return Attempt::Failed(error),
        };
        let mut events = EventSet::all();
        // CLOSE_WRITE marks the end of a write; MODIFY would be one event per
        // buffer of every copy.
        events.modify = false;
        let mut tree = match TreeWatch::arm(
            &self.spec.source,
            WatchOptions {
                filter,
                events,
                limit: LimitPolicy::System,
                setup_deadline: self.settings.setup_deadline,
            },
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
        loop {
            if self.stopped() {
                return Attempt::Stopped;
            }
            match tree.wait(self.settings.slice) {
                Ok(changes) => {
                    for change in changes {
                        let event = match change {
                            Change::Path { dir, .. } => ChangeEvent::Changed {
                                routine: self.spec.routine.clone(),
                                dir,
                            },
                            Change::Overflow => ChangeEvent::Overflow {
                                routine: Some(self.spec.routine.clone()),
                            },
                        };
                        if !self.emit(event) {
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

    #[cfg(not(target_os = "linux"))]
    fn native(&self, _source_missing: &mut bool, _caught_up: &mut bool) -> Attempt {
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
        let Some(since) = self.spec.last_success_epoch else {
            return self.emit(ChangeEvent::Notice {
                routine: self.spec.routine.clone(),
                warn: false,
                detail: "catch_up=whole_tree cause=no_successful_run_yet".to_owned(),
            }) && self.emit(ChangeEvent::Overflow { routine });
        };
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
        match newest_change(
            self.spec.source.clone(),
            filter,
            self.settings.walk_deadline,
        ) {
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

/// The newest modification or status change (epoch seconds) of anything in
/// the tree the filter keeps, read on a walker thread bounded by `deadline`.
/// `Ok(None)` for an empty tree.
fn newest_change(
    root: PathBuf,
    filter: Arc<Filter>,
    deadline: Duration,
) -> Result<Option<i64>, String> {
    let current = Arc::new(std::sync::Mutex::new(String::new()));
    let (tx, rx) = mpsc::channel();
    let walker_current = Arc::clone(&current);
    thread::Builder::new()
        .name("sdsyncd-catch-up".into())
        .spawn(move || {
            let _ = tx.send(walk_newest(&root, &filter, &walker_current));
        })
        .map_err(|error| format!("walker_unavailable: {error}"))?;
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => {
            let at = current.lock().map(|path| path.clone()).unwrap_or_default();
            Err(format!(
                "walk_deadline deadline={}s path={at}",
                deadline.as_secs()
            ))
        }
    }
}

fn walk_newest(
    root: &Path,
    filter: &Filter,
    current: &std::sync::Mutex<String>,
) -> Result<Option<i64>, String> {
    let mut newest: Option<i64> = None;
    let mut pending = vec![String::new()];
    while let Some(relative) = pending.pop() {
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
            Err(_) => continue,
        };
        note_time(&mut newest, std::fs::symlink_metadata(&dir).ok().as_ref());
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let is_dir = metadata.is_dir();
            if filter.excludes(&child, is_dir) {
                continue;
            }
            note_time(&mut newest, Some(&metadata));
            if is_dir {
                pending.push(child);
            }
        }
    }
    Ok(newest)
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
            rearm_first: Duration::from_millis(400),
            rearm_max: Duration::from_secs(2),
            source_recheck: Duration::from_millis(200),
            slice: Duration::from_millis(100),
            native,
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
        )
        .unwrap()
        .unwrap();
        assert!((newest - now_epoch()).abs() < 120, "{newest}");
        // An excluded name does not count.
        let excluded = temp_tree("catchup-excluded");
        std::fs::create_dir_all(excluded.join("source/@eaDir")).unwrap();
        let only_dsm = Arc::new(Filter::build(&excluded.join("source"), Vec::new()).unwrap());
        assert!(newest_change(excluded.join("source"), only_dsm, Duration::from_secs(10)).is_ok());
        // A missing root is named.
        let missing =
            newest_change(root.join("nope"), filter, Duration::from_secs(10)).unwrap_err();
        assert!(missing.starts_with("root_unreadable"), "{missing}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&excluded);
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
