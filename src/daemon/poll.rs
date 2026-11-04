//! The polling fallback of the realtime change source.
//!
//! When inotify is unusable (`watch_limit`, `watch_setup_failed`,
//! `watch_unavailable`, or a platform without it) a routine's source is read on
//! an interval and compared with the previous reading. The shell poller this
//! replaces failed in three ways, and each has a rule here:
//!
//! - It failed closed on any stage error and then reported nothing forever. A
//!   per-entry error (an entry vanished mid-walk, a subdirectory is not
//!   readable, a metadata call failed) is a named skip: counted, the first few
//!   paths kept with their error kind, and the walk goes on. Only a failure on
//!   the source root itself ends a reading, as a named [`PollError`].
//! - It waited for two identical differing readings before it reported. The
//!   first reading that differs from the previous one reports; the caller's
//!   debounce provides the settle time.
//! - Its walk could run forever. Every walk runs on a helper thread and is
//!   waited for with a deadline; on timeout the error names the directory the
//!   walk was in, the thread is abandoned (it owns only its own data) and the
//!   next interval tries again. A process-wide cap bounds abandoned walkers.
//!
//! A [`Reading`] is one 64-bit digest per directory over its sorted entries
//! (name, kind, size, mtime in ns; a subdirectory contributes only its name, so
//! a change deep in the tree names the directory that changed and not every
//! ancestor). Nothing here follows a symlink or descends into a directory the
//! sync's own [`Filter`] excludes.

#![forbid(unsafe_code)]
#![allow(
    dead_code,
    reason = "the worker is started by the change source, which plugs in at integration"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use synology_drive_sync::watch::Filter;

use super::changes::{ChangeEvent, DIRTY_CAP};
use super::routines::{Backend, BackendReason, SourceStatus};

/// How many skipped entries a reading names.
const SKIP_SAMPLE: usize = 3;
/// The longest the poller sleeps or waits without looking at `stop`.
const SLICE: Duration = Duration::from_millis(250);
/// How often a waiting poller looks at `stop` while a walk runs.
const WALK_SLICE: Duration = Duration::from_millis(50);

/// One reading of a source tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reading {
    /// Relative directory (`""` is the root, `/` separators) to its digest.
    pub(crate) dirs: BTreeMap<String, u64>,
    /// Entries (and unreadable subdirectories) the walk had to leave out.
    pub(crate) skipped: usize,
    /// The first [`SKIP_SAMPLE`] skips, `"a/b (PermissionDenied)"`.
    pub(crate) skip_sample: Vec<String>,
}

impl Reading {
    fn skip(&mut self, relative: &str, error: &io::Error) {
        self.skipped += 1;
        if self.skip_sample.len() < SKIP_SAMPLE {
            self.skip_sample
                .push(format!("{relative} ({:?})", error.kind()));
        }
    }
}

/// Why a reading failed as a whole. Every variant names the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PollError {
    /// The source root does not exist.
    SourceMissing { path: PathBuf, detail: String },
    /// The source root is a symlink, not a directory, or cannot be read.
    Unreadable { path: PathBuf, detail: String },
    /// The walk did not finish in time; `path` is the directory it was in.
    Deadline { path: PathBuf, deadline: Duration },
}

impl PollError {
    /// The `backend_reason` this error is reported under (existing vocabulary only).
    pub(crate) fn reason(&self) -> BackendReason {
        match self {
            Self::SourceMissing { .. } => BackendReason::SourceMissing,
            Self::Unreadable { .. } => BackendReason::PollUnreadable,
            Self::Deadline { .. } => BackendReason::WatchSetupFailed,
        }
    }
}

impl std::fmt::Display for PollError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let token = self.reason().as_str();
        match self {
            Self::SourceMissing { path, detail } | Self::Unreadable { path, detail } => {
                write!(formatter, "{token}: {detail}: {}", path.display())
            }
            Self::Deadline { path, deadline } => write!(
                formatter,
                "{token}: polling walk gave up after {}s in directory: {}",
                deadline.as_secs_f64(),
                path.display()
            ),
        }
    }
}

impl std::error::Error for PollError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Other,
}

/// One directory entry as the walk needs it.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i128,
}

/// A listed entry, or the name (`""` when the listing could not even say it)
/// and error of one that could not be read.
pub(crate) type Listed = Result<Entry, (String, io::Error)>;

/// The directory read the walk is made of; a test injects one that blocks.
pub(crate) trait DirSource: Send + Sync + 'static {
    fn list(&self, dir: &Path) -> io::Result<Vec<Listed>>;
}

struct FsSource;

impl DirSource for FsSource {
    fn list(&self, dir: &Path) -> io::Result<Vec<Listed>> {
        let mut listed = Vec::new();
        for item in fs::read_dir(dir)? {
            let entry = match item {
                Ok(entry) => entry,
                Err(error) => {
                    listed.push(Err((String::new(), error)));
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            listed.push(match fs::symlink_metadata(entry.path()) {
                Ok(meta) => describe(name.clone(), &meta).map_err(|error| (name, error)),
                Err(error) => Err((name, error)),
            });
        }
        Ok(listed)
    }
}

fn describe(name: String, meta: &fs::Metadata) -> io::Result<Entry> {
    let kind = if meta.file_type().is_symlink() {
        Kind::Other
    } else if meta.is_dir() {
        Kind::Dir
    } else if meta.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    let mtime_ns = match meta.modified()?.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
        Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
    };
    Ok(Entry {
        name,
        kind,
        size: meta.len(),
        mtime_ns,
    })
}

/// FNV-1a, 64 bit: stable across runs, platforms and releases.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn digest(entries: &[Entry]) -> u64 {
    let mut hash = Fnv::new();
    for entry in entries {
        hash.bytes(entry.name.as_bytes());
        // 0xff never occurs in UTF-8, so a name cannot run into the fixed fields.
        hash.bytes(&[0xff]);
        match entry.kind {
            Kind::Dir => hash.bytes(b"d"),
            Kind::File | Kind::Other => {
                hash.bytes(&[if entry.kind == Kind::File { b'f' } else { b'o' }]);
                hash.bytes(&entry.size.to_le_bytes());
                hash.bytes(&entry.mtime_ns.to_le_bytes());
            }
        }
    }
    hash.0
}

fn join(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

fn parent(directory: &str) -> &str {
    directory.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn set_current(current: &Mutex<String>, relative: &str) {
    let mut guard = current.lock().unwrap_or_else(PoisonError::into_inner);
    guard.clear();
    guard.push_str(relative);
}

fn classify_root(root: &Path, what: &str, error: &io::Error) -> PollError {
    let path = root.to_path_buf();
    let detail = format!("{what}: {error}");
    if error.kind() == io::ErrorKind::NotFound {
        PollError::SourceMissing { path, detail }
    } else {
        PollError::Unreadable { path, detail }
    }
}

fn check_root(root: &Path) -> Result<(), PollError> {
    match fs::symlink_metadata(root) {
        Err(error) => Err(classify_root(root, "cannot stat the source", &error)),
        Ok(meta) if meta.file_type().is_symlink() => Err(PollError::Unreadable {
            path: root.to_path_buf(),
            detail: "the source is a symlink".to_owned(),
        }),
        Ok(meta) if !meta.is_dir() => Err(PollError::Unreadable {
            path: root.to_path_buf(),
            detail: "the source is not a directory".to_owned(),
        }),
        Ok(_) => Ok(()),
    }
}

fn walk(
    source: &dyn DirSource,
    root: &Path,
    filter: &Filter,
    current: &Mutex<String>,
    cancel: &AtomicBool,
) -> Result<Reading, PollError> {
    set_current(current, "");
    check_root(root)?;
    let mut reading = Reading::default();
    let mut pending = vec![String::new()];
    while let Some(relative) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(PollError::Unreadable {
                path: root.to_path_buf(),
                detail: "the walk was given up".to_owned(),
            });
        }
        set_current(current, &relative);
        let path = if relative.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&relative)
        };
        let listed = match source.list(&path) {
            Ok(listed) => listed,
            Err(error) if relative.is_empty() => {
                return Err(classify_root(root, "cannot read the source", &error));
            }
            Err(error) => {
                reading.skip(&relative, &error);
                continue;
            }
        };
        let mut entries = Vec::with_capacity(listed.len());
        for item in listed {
            match item {
                Err((name, error)) => {
                    let label = if name.is_empty() { "<entry>" } else { &name };
                    reading.skip(&join(&relative, label), &error);
                }
                Ok(entry) => {
                    let child = join(&relative, &entry.name);
                    if !filter.excludes(&child, entry.kind == Kind::Dir) {
                        entries.push(entry);
                    }
                }
            }
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        for entry in entries.iter().rev().filter(|entry| entry.kind == Kind::Dir) {
            pending.push(join(&relative, &entry.name));
        }
        reading.dirs.insert(relative, digest(&entries));
    }
    Ok(reading)
}

/// Read `root` once on the calling thread. `current` follows the directory the
/// walk is in. No deadline: use [`read_with_deadline`] unless already on a
/// thread somebody else bounds.
pub(crate) fn read_tree(
    root: &Path,
    filter: &Filter,
    current: &Mutex<String>,
) -> Result<Reading, PollError> {
    walk(&FsSource, root, filter, current, &AtomicBool::new(false))
}

/// The directories whose digest changed between two readings. An added
/// directory names itself and its parent; a removed one names its parent (when
/// that still exists). Sorted, deduplicated.
pub(crate) fn changed_dirs(old: &Reading, new: &Reading) -> Vec<String> {
    let mut changed = BTreeSet::new();
    for (directory, digest) in &new.dirs {
        match old.dirs.get(directory) {
            Some(before) if before == digest => {}
            Some(_) => {
                changed.insert(directory.clone());
            }
            None => {
                changed.insert(directory.clone());
                if !directory.is_empty() {
                    changed.insert(parent(directory).to_owned());
                }
            }
        }
    }
    for directory in old.dirs.keys() {
        if !new.dirs.contains_key(directory) && !directory.is_empty() {
            let above = parent(directory);
            if new.dirs.contains_key(above) {
                changed.insert(above.to_owned());
            }
        }
    }
    changed.into_iter().collect()
}

enum Waited {
    Done(Result<Reading, PollError>),
    Stopped,
}

/// Run one walk on a helper thread and wait for it at most `deadline`, looking
/// at `stop` (when given) while it waits.
fn wait_walk(
    source: Arc<dyn DirSource>,
    root: PathBuf,
    filter: Arc<Filter>,
    deadline: Duration,
    stop: Option<&AtomicBool>,
) -> Waited {
    // The cap is the process's, shared with the native watch's walks (H9).
    let slot = match synology_drive_sync::watch::walk_slot() {
        Ok(slot) => slot,
        Err(live) => {
            return Waited::Done(Err(PollError::Unreadable {
                path: root,
                detail: synology_drive_sync::watch::walk_cap_refusal(live),
            }));
        }
    };
    let current = Arc::new(Mutex::new(String::new()));
    let cancel = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("sdsyncd-poll-walk".to_owned())
        .spawn({
            let (current, cancel, root) = (Arc::clone(&current), Arc::clone(&cancel), root.clone());
            move || {
                let _slot = slot;
                let result = walk(source.as_ref(), &root, &filter, &current, &cancel);
                // The receiver is gone once the deadline passed; nothing to report then.
                let _ = sender.send(result);
            }
        });
    if let Err(error) = spawned {
        return Waited::Done(Err(PollError::Unreadable {
            path: root,
            detail: format!("cannot start the directory walk thread: {error}"),
        }));
    }
    let give_up_at = Instant::now() + deadline;
    loop {
        // A disarm ends the wait before any result is looked at: a walk that happened to finish
        // just before the flag was read must not be reported as one the routine still wants (it
        // made `the_floors_walk_honours_a_disarm` fail about 3 runs in 100 under load).
        if stop.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
            cancel.store(true, Ordering::Relaxed);
            return Waited::Stopped;
        }
        let remaining = give_up_at.saturating_duration_since(Instant::now());
        let slice = if stop.is_some() {
            remaining.min(WALK_SLICE)
        } else {
            remaining
        };
        match receiver.recv_timeout(slice) {
            Ok(result) => return Waited::Done(result),
            Err(RecvTimeoutError::Disconnected) => {
                return Waited::Done(Err(PollError::Unreadable {
                    path: root,
                    detail: "the directory walk thread ended without a result".to_owned(),
                }));
            }
            Err(RecvTimeoutError::Timeout) => {
                if stop.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
                    cancel.store(true, Ordering::Relaxed);
                    return Waited::Stopped;
                }
                if Instant::now() >= give_up_at {
                    cancel.store(true, Ordering::Relaxed);
                    let relative = current
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    let path = if relative.is_empty() {
                        root
                    } else {
                        root.join(relative)
                    };
                    return Waited::Done(Err(PollError::Deadline { path, deadline }));
                }
            }
        }
    }
}

/// [`read_tree`] on a helper thread named `sdsyncd-poll-walk`, given up after
/// `deadline` with a [`PollError::Deadline`] naming the directory it was in.
pub(crate) fn read_with_deadline(
    root: PathBuf,
    filter: Arc<Filter>,
    deadline: Duration,
) -> Result<Reading, PollError> {
    match wait_walk(Arc::new(FsSource), root, filter, deadline, None) {
        Waited::Done(result) => result,
        Waited::Stopped => unreachable!("no stop flag was given"),
    }
}

/// [`read_with_deadline`] that also ends the moment `stop` is set (H9: a disarmed
/// routine does not wait out a walk on a disk that stopped answering): `None`
/// when it was stopped, with the walk told to cancel.
pub(crate) fn read_until_stopped(
    root: PathBuf,
    filter: Arc<Filter>,
    deadline: Duration,
    stop: &AtomicBool,
) -> Option<Result<Reading, PollError>> {
    match wait_walk(Arc::new(FsSource), root, filter, deadline, Some(stop)) {
        Waited::Done(result) => Some(result),
        Waited::Stopped => None,
    }
}

/// How the poller runs.
#[derive(Clone, Debug)]
pub(crate) struct PollConfig {
    /// Pause between the end of one reading and the start of the next.
    pub(crate) interval: Duration,
    /// The longest the first walk may take.
    pub(crate) walk_deadline: Duration,
    /// How far the walk deadline may grow when walks keep giving up (H4).
    pub(crate) walk_deadline_max: Duration,
    /// While no walk finishes, the tree is synced whole at most this often
    /// (N-D6: the routine's poll floor, but never under fifteen minutes; at
    /// most an hour).
    pub(crate) degraded_every: Duration,
    /// Why polling: `WatchLimit`, `WatchSetupFailed` or `WatchUnavailable`.
    pub(crate) reason: BackendReason,
    /// The watcher's own account of why, reported with every `Backend` event.
    pub(crate) detail: String,
    /// What outlives this poller: the change source rebuilds the poller on
    /// every native retry, and a tree it cannot walk must not start over each time.
    pub(crate) carry: SharedCarry,
}

/// What a polling worker learned that its successor must not forget (N-D6): the
/// grown walk deadline, when the tree was last synced whole because no walk
/// finished, and the last backend report (so a successor with the same account
/// does not repeat it).
#[derive(Debug, Default)]
pub(crate) struct PollCarry {
    /// The walk deadline in force (H4); it only grows.
    pub(crate) walk_deadline: Option<Duration>,
    /// When the last whole-tree run was asked for because no walk finished.
    pub(crate) degraded_overflow: Option<Instant>,
    /// The `(reason, detail)` of the last `Backend` event this routine's pollers
    /// sent; the change source clears it when the native watch reports in.
    pub(crate) announced: Option<(BackendReason, String)>,
    /// The watcher's own `(reason, detail)` the pollers last opened with.
    pub(crate) account: Option<(BackendReason, String)>,
    /// Changes during a gap are unknown and the baseline that ends it is owed an overflow. A
    /// successor starts in the gap its predecessor left (between pollers nothing watches), so
    /// this is the predecessor's unpaid debt, handed on.
    pub(crate) gap: bool,
    /// A poller has run for this routine: whoever runs next starts in a gap.
    pub(crate) earlier: bool,
}

impl PollCarry {
    /// The native watch took over: the next poller starts from a clean report.
    pub(crate) fn native_reported(&mut self) {
        self.announced = None;
        self.account = None;
    }
}

pub(crate) type SharedCarry = Arc<Mutex<PollCarry>>;

/// A fresh carry for one arming.
pub(crate) fn new_carry() -> SharedCarry {
    Arc::new(Mutex::new(PollCarry::default()))
}

fn carried(carry: &SharedCarry) -> std::sync::MutexGuard<'_, PollCarry> {
    carry.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The native watch reported in: see [`PollCarry::native_reported`].
pub(crate) fn native_reported(carry: &SharedCarry) {
    carried(carry).native_reported();
}

type Emit = Box<dyn Fn(ChangeEvent) -> bool + Send>;

#[derive(Default)]
struct State {
    baseline: Option<Reading>,
    /// The reason of the failure already reported, until a reading succeeds.
    failed: Option<BackendReason>,
    /// `Source Missing` was reported and `Source Ok` has not been yet.
    missing: bool,
    /// Changes during a gap are unknown: the next baseline comes with an overflow.
    gap: bool,
    reported_skips: (usize, Vec<String>),
    /// The walk deadline in force: it grows when walks give up (H4).
    walk_deadline: Option<Duration>,
    /// When the last whole-tree run was asked for because no walk finished.
    degraded_overflow: Option<Instant>,
}

struct Poller {
    source: Arc<dyn DirSource>,
    routine: String,
    root: PathBuf,
    filter: Arc<Filter>,
    config: PollConfig,
    stop: Arc<AtomicBool>,
    emit: Emit,
}

impl Poller {
    fn send(&self, event: ChangeEvent) -> bool {
        (self.emit)(event)
    }

    /// Report the backend. The same account as the last one this routine's
    /// pollers sent is not repeated: a poller rebuilt on a native retry would
    /// otherwise announce the same cause again every time.
    fn backend(&self, reason: BackendReason, detail: String) -> bool {
        {
            let mut carry = carried(&self.config.carry);
            if carry
                .announced
                .as_ref()
                .is_some_and(|(r, d)| *r == reason && *d == detail)
            {
                return true;
            }
            carry.announced = Some((reason, detail.clone()));
        }
        self.send(ChangeEvent::Backend {
            routine: self.routine.clone(),
            backend: Backend::Polling,
            reason,
            detail,
        })
    }

    fn source(&self, status: SourceStatus) -> bool {
        self.send(ChangeEvent::Source {
            routine: self.routine.clone(),
            status,
        })
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The watcher's detail, with the skipped entries named when there are any.
    fn detail_with(&self, skips: &(usize, Vec<String>)) -> String {
        let (count, sample) = skips;
        if *count == 0 {
            return self.config.detail.clone();
        }
        let mut named = sample.join(", ");
        if *count > sample.len() {
            named.push_str(&format!(", and {} more", count - sample.len()));
        }
        let noun = if *count == 1 { "entry" } else { "entries" };
        let skipped = format!("{count} {noun} skipped: {named}");
        if self.config.detail.is_empty() {
            skipped
        } else {
            format!("{}; {skipped}", self.config.detail)
        }
    }

    fn run(self) {
        let mut state = {
            let mut carry = carried(&self.config.carry);
            // A successor's first baseline may hold changes nobody saw: it owes an overflow,
            // as one whose predecessor already owed one and never paid it.
            let gap = carry.gap || carry.earlier;
            carry.earlier = true;
            State {
                walk_deadline: Some(carry.walk_deadline.unwrap_or(self.config.walk_deadline)),
                degraded_overflow: carry.degraded_overflow,
                gap,
                ..State::default()
            }
        };
        self.serve(&mut state);
        // N-D6: what was learned outlives this poller.
        self.persist(&state);
    }

    fn serve(&self, state: &mut State) {
        // The watcher's account of why polling is announced once per arming: a
        // successor with the same account stays quiet (N-D6).
        let account = (self.config.reason, self.config.detail.clone());
        if carried(&self.config.carry).account.as_ref() != Some(&account) {
            if !self.backend(account.0, account.1.clone()) {
                return;
            }
            carried(&self.config.carry).account = Some(account);
        }
        while !self.stopped() {
            let waited = wait_walk(
                Arc::clone(&self.source),
                self.root.clone(),
                Arc::clone(&self.filter),
                state.walk_deadline.unwrap_or(self.config.walk_deadline),
                Some(&self.stop),
            );
            let keep_going = match waited {
                Waited::Stopped => return,
                Waited::Done(Ok(reading)) => self.on_reading(state, reading),
                Waited::Done(Err(error)) => self.on_error(state, &error),
            };
            self.persist(state);
            if !keep_going || !self.pause() {
                return;
            }
        }
    }

    /// Hand what this poller has learned to its successor (N-D6).
    fn persist(&self, state: &State) {
        let mut carry = carried(&self.config.carry);
        carry.walk_deadline = state.walk_deadline;
        carry.degraded_overflow = state.degraded_overflow;
        carry.gap = state.gap;
    }

    /// Sleep one interval in slices; `false` when `stop` was set meanwhile.
    fn pause(&self) -> bool {
        let wake_at = Instant::now() + self.config.interval;
        loop {
            if self.stopped() {
                return false;
            }
            let remaining = wake_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            thread::sleep(remaining.min(SLICE));
        }
    }

    fn on_reading(&self, state: &mut State, reading: Reading) -> bool {
        if state.missing {
            state.missing = false;
            state.baseline = None;
            state.gap = true;
            if !self.source(SourceStatus::Ok) {
                return false;
            }
        }
        let skips = (reading.skipped, reading.skip_sample.clone());
        // A reading that finished clears any failure account; `backend` sends
        // only what differs from the last report, so a clean tree is silent and
        // a poller rebuilt after a failure still retracts it.
        state.failed = None;
        let detail = self.detail_with(&skips);
        state.reported_skips = skips;
        if !self.backend(self.config.reason, detail) {
            return false;
        }
        match state.baseline.replace(reading) {
            None => {
                let overflow = std::mem::take(&mut state.gap);
                !overflow
                    || self.send(ChangeEvent::Overflow {
                        routine: Some(self.routine.clone()),
                    })
            }
            Some(old) => {
                let Some(new) = state.baseline.as_ref() else {
                    return true;
                };
                let dirs = changed_dirs(&old, new);
                if dirs.len() > DIRTY_CAP {
                    return self.send(ChangeEvent::Overflow {
                        routine: Some(self.routine.clone()),
                    });
                }
                dirs.into_iter().all(|dir| {
                    self.send(ChangeEvent::Changed {
                        routine: self.routine.clone(),
                        dir,
                    })
                })
            }
        }
    }

    fn on_error(&self, state: &mut State, error: &PollError) -> bool {
        if let PollError::Deadline { path, deadline } = error {
            return self.on_deadline(state, path, *deadline);
        }
        let reason = error.reason();
        if state.failed != Some(reason) {
            state.failed = Some(reason);
            if !self.backend(reason, error.to_string()) {
                return false;
            }
        }
        if matches!(error, PollError::SourceMissing { .. }) {
            if !state.missing {
                state.missing = true;
                state.baseline = None;
                state.gap = true;
                return self.source(SourceStatus::Missing);
            }
        } else if state.missing {
            // The source exists again but cannot be read: no longer "missing".
            state.missing = false;
            return self.source(SourceStatus::Ok);
        }
        true
    }
}

impl Poller {
    /// A walk gave up (H4): the deadline grows, named, up to its cap, so a
    /// large tree is read eventually instead of never; while no walk
    /// finishes, nothing can be compared, so the tree is synced whole at most
    /// once per `degraded_every`, and the first walk that finishes is a fresh
    /// baseline that comes with an overflow (unknown is not unchanged).
    fn on_deadline(&self, state: &mut State, path: &Path, gave_up_after: Duration) -> bool {
        let reason = BackendReason::WatchSetupFailed;
        let current = state.walk_deadline.unwrap_or(self.config.walk_deadline);
        let next = (current.saturating_mul(2))
            .min(self.config.walk_deadline_max)
            .max(current);
        let grew = next != current || state.failed != Some(reason);
        state.walk_deadline = Some(next);
        state.failed = Some(reason);
        if grew {
            let capped = if next >= self.config.walk_deadline_max {
                " (the cap)"
            } else {
                ""
            };
            let detail = format!(
                "{}{}walk_deadline_exceeded: the walk gave up after {}s in {}; \
                 next_walk_deadline_seconds={}{capped}; the tree is synced whole \
                 at most every {} s until a walk finishes",
                self.config.detail,
                if self.config.detail.is_empty() {
                    ""
                } else {
                    "; "
                },
                gave_up_after.as_secs_f64(),
                path.display(),
                next.as_secs(),
                self.config.degraded_every.as_secs()
            );
            if !self.backend(reason, detail) {
                return false;
            }
        }
        if state.baseline.is_none() {
            state.gap = true;
        }
        let due = state
            .degraded_overflow
            .is_none_or(|at| at.elapsed() >= self.config.degraded_every);
        if due {
            state.degraded_overflow = Some(Instant::now());
            return self.send(ChangeEvent::Overflow {
                routine: Some(self.routine.clone()),
            });
        }
        true
    }
}

/// Start the polling worker of `routine` on `root`. It reports through `emit`
/// (which returns `false` to end it) and ends within 250 ms of `stop` being set.
pub(crate) fn spawn_poller(
    routine: String,
    root: PathBuf,
    filter: Arc<Filter>,
    config: PollConfig,
    stop: Arc<AtomicBool>,
    emit: Box<dyn Fn(ChangeEvent) -> bool + Send>,
) -> io::Result<JoinHandle<()>> {
    spawn_poller_from(
        Arc::new(FsSource),
        routine,
        root,
        filter,
        config,
        stop,
        emit,
    )
}

/// [`spawn_poller`] reading the tree through `source`.
pub(crate) fn spawn_poller_from(
    source: Arc<dyn DirSource>,
    routine: String,
    root: PathBuf,
    filter: Arc<Filter>,
    config: PollConfig,
    stop: Arc<AtomicBool>,
    emit: Box<dyn Fn(ChangeEvent) -> bool + Send>,
) -> io::Result<JoinHandle<()>> {
    let poller = Poller {
        source,
        routine,
        root,
        filter,
        config,
        stop,
        emit,
    };
    thread::Builder::new()
        .name("sdsyncd-poller".to_owned())
        .spawn(move || poller.run())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize};
    use std::sync::mpsc::Receiver;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "sdsyncd-poll-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, relative: &str, content: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            #[cfg(unix)]
            restore_modes(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn restore_modes(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    restore_modes(&entry.path());
                }
            }
        }
    }

    fn filter(root: &Path, extra: &[&str]) -> Arc<Filter> {
        let patterns = extra.iter().map(|pattern| (*pattern).to_owned()).collect();
        Arc::new(Filter::build(root, patterns).unwrap())
    }

    fn read(root: &Path, filter: &Filter) -> Reading {
        read_tree(root, filter, &Mutex::new(String::new())).unwrap()
    }

    #[test]
    fn a_created_file_names_its_directory() {
        let dir = TempDir::new("create");
        dir.write("a/one.txt", "1");
        let filter = filter(dir.path(), &[]);
        let before = read(dir.path(), &filter);
        dir.write("a/two.txt", "2");
        assert_eq!(changed_dirs(&before, &read(dir.path(), &filter)), ["a"]);
        dir.write("top.txt", "t");
        assert_eq!(changed_dirs(&before, &read(dir.path(), &filter)), ["", "a"]);
    }

    #[test]
    fn a_modified_file_by_size_or_mtime_names_its_directory() {
        let dir = TempDir::new("modify");
        dir.write("a/b/f.txt", "short");
        let filter = filter(dir.path(), &[]);
        let before = read(dir.path(), &filter);
        dir.write("a/b/f.txt", "much longer content");
        let after_size = read(dir.path(), &filter);
        assert_eq!(changed_dirs(&before, &after_size), ["a/b"]);

        let file = fs::File::options()
            .write(true)
            .open(dir.path().join("a/b/f.txt"))
            .unwrap();
        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        drop(file);
        assert_eq!(
            changed_dirs(&after_size, &read(dir.path(), &filter)),
            ["a/b"]
        );
    }

    #[test]
    fn a_deleted_file_names_its_directory() {
        let dir = TempDir::new("delete");
        dir.write("a/keep.txt", "k");
        dir.write("a/gone.txt", "g");
        let filter = filter(dir.path(), &[]);
        let before = read(dir.path(), &filter);
        fs::remove_file(dir.path().join("a/gone.txt")).unwrap();
        assert_eq!(changed_dirs(&before, &read(dir.path(), &filter)), ["a"]);
    }

    #[test]
    fn a_new_and_a_removed_subdirectory_name_themselves_and_their_parent() {
        let dir = TempDir::new("subdir");
        dir.write("a/f.txt", "f");
        let filter = filter(dir.path(), &[]);
        let before = read(dir.path(), &filter);
        fs::create_dir_all(dir.path().join("a/new/deeper")).unwrap();
        let added = read(dir.path(), &filter);
        assert_eq!(
            changed_dirs(&before, &added),
            ["a", "a/new", "a/new/deeper"]
        );
        fs::remove_dir_all(dir.path().join("a/new")).unwrap();
        let removed = read(dir.path(), &filter);
        assert_eq!(changed_dirs(&added, &removed), ["a"]);
        assert!(changed_dirs(&removed, &removed).is_empty());
    }

    #[test]
    fn a_change_in_a_child_does_not_name_its_ancestors() {
        let dir = TempDir::new("ancestors");
        dir.write("a/b/c/f.txt", "f");
        let filter = filter(dir.path(), &[]);
        let before = read(dir.path(), &filter);
        dir.write("a/b/c/f.txt", "changed size");
        assert_eq!(changed_dirs(&before, &read(dir.path(), &filter)), ["a/b/c"]);
    }

    #[test]
    fn excluded_names_never_produce_a_change() {
        let dir = TempDir::new("excluded");
        dir.write("keep.txt", "k");
        let filter = filter(dir.path(), &["*.tmp"]);
        let before = read(dir.path(), &filter);
        dir.write("@eaDir/x/meta", "m");
        dir.write("#recycle/old.txt", "o");
        dir.write("scratch.tmp", "t");
        dir.write("sub/scratch.tmp", "t");
        let after = read(dir.path(), &filter);
        assert!(!after.dirs.keys().any(|dir| dir.contains("eaDir")));
        assert!(!after.dirs.keys().any(|dir| dir.contains("recycle")));
        // `sub` is a new directory (its parent changed) but holds nothing visible.
        assert_eq!(changed_dirs(&before, &after), ["", "sub"]);
        dir.write("@eaDir/x/meta", "changed");
        dir.write("scratch.tmp", "changed again");
        let again = read(dir.path(), &filter);
        assert!(changed_dirs(&after, &again).is_empty());
    }

    #[test]
    fn digests_are_stable() {
        let entries = [Entry {
            name: "f".to_owned(),
            kind: Kind::File,
            size: 1,
            mtime_ns: 2,
        }];
        // Pinned (verified against an independent FNV-1a): a changed digest would make every poller report a change at upgrade.
        assert_eq!(digest(&entries), digest(&entries));
        assert_eq!(digest(&[]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(digest(&entries), 8761745093470897663);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_subdirectory_is_a_named_skip_and_the_rest_is_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("denied");
        dir.write("open/f.txt", "f");
        dir.write("locked/secret.txt", "s");
        let filter = filter(dir.path(), &[]);
        fs::set_permissions(dir.path().join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        if super::super::testkit::permissions_do_not_bind(
            "an_unreadable_subdirectory_is_a_named_skip_and_the_rest_is_read",
            &dir.path().join("locked"),
        ) {
            return;
        }
        let before = read(dir.path(), &filter);
        assert_eq!(before.skipped, 1);
        assert_eq!(before.skip_sample, ["locked (PermissionDenied)"]);
        assert!(before.dirs.contains_key("open"));
        assert!(!before.dirs.contains_key("locked"));
        dir.write("open/g.txt", "g");
        let after = read(dir.path(), &filter);
        assert_eq!(changed_dirs(&before, &after), ["open"]);
    }

    #[test]
    fn a_vanishing_entry_is_a_skip_not_a_failure() {
        struct Flaky;
        impl DirSource for Flaky {
            fn list(&self, dir: &Path) -> io::Result<Vec<Listed>> {
                let mut listed = FsSource.list(dir)?;
                listed.push(Err((
                    "gone.txt".to_owned(),
                    io::Error::from(io::ErrorKind::NotFound),
                )));
                listed.push(Err((String::new(), io::Error::other("odd"))));
                Ok(listed)
            }
        }
        let dir = TempDir::new("flaky");
        dir.write("a/f.txt", "f");
        let filter = filter(dir.path(), &[]);
        let reading = walk(
            &Flaky,
            dir.path(),
            &filter,
            &Mutex::new(String::new()),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(reading.skipped, 4);
        assert_eq!(reading.skip_sample.len(), SKIP_SAMPLE);
        assert_eq!(reading.skip_sample[0], "gone.txt (NotFound)");
        assert!(reading.dirs.contains_key("a"));
    }

    #[test]
    fn a_missing_root_is_source_missing_naming_the_path() {
        let dir = TempDir::new("missing");
        let root = dir.path().join("not-there");
        let filter = filter(dir.path(), &[]);
        let error = read_with_deadline(root.clone(), filter, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(&error, PollError::SourceMissing { path, .. } if *path == root));
        assert_eq!(error.reason(), BackendReason::SourceMissing);
        let text = error.to_string();
        assert!(text.starts_with("source_missing: "), "{text}");
        assert!(text.ends_with(&root.display().to_string()), "{text}");
    }

    #[test]
    fn a_root_that_is_a_file_is_unreadable_naming_the_path() {
        let dir = TempDir::new("rootfile");
        dir.write("file", "x");
        let root = dir.path().join("file");
        let filter = filter(dir.path(), &[]);
        let error = read_with_deadline(root.clone(), filter, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(&error, PollError::Unreadable { path, .. } if *path == root));
        assert_eq!(error.reason(), BackendReason::PollUnreadable);
        assert!(error.to_string().ends_with(&root.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_root_that_is_a_symlink_is_unreadable() {
        let dir = TempDir::new("rootlink");
        dir.write("real/f", "x");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("real"), &link).unwrap();
        let filter = filter(dir.path(), &[]);
        let error = read_with_deadline(link.clone(), filter, Duration::from_secs(5)).unwrap_err();
        assert!(
            matches!(&error, PollError::Unreadable { detail, .. } if detail.contains("symlink")),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed() {
        let dir = TempDir::new("nofollow");
        dir.write("real/f", "x");
        dir.write("tree/f", "x");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("tree/link")).unwrap();
        let filter = filter(dir.path().join("tree").as_path(), &[]);
        let reading = read(&dir.path().join("tree"), &filter);
        assert_eq!(
            reading.dirs.keys().cloned().collect::<Vec<_>>(),
            vec![String::new()]
        );
    }

    #[test]
    fn a_blocked_directory_read_hits_the_deadline_naming_the_directory() {
        struct Slow;
        impl DirSource for Slow {
            fn list(&self, dir: &Path) -> io::Result<Vec<Listed>> {
                if dir.ends_with("slow") {
                    thread::sleep(Duration::from_secs(2));
                }
                FsSource.list(dir)
            }
        }
        let dir = TempDir::new("deadline");
        dir.write("fast/f", "f");
        dir.write("slow/f", "f");
        let filter = filter(dir.path(), &[]);
        let started = Instant::now();
        let waited = wait_walk(
            Arc::new(Slow),
            dir.path().to_path_buf(),
            filter,
            Duration::from_millis(300),
            None,
        );
        let elapsed = started.elapsed();
        let Waited::Done(Err(error)) = waited else {
            panic!("the walk was not given up");
        };
        assert!(
            matches!(&error, PollError::Deadline { path, .. } if *path == dir.path().join("slow")),
            "{error}"
        );
        assert_eq!(error.reason(), BackendReason::WatchSetupFailed);
        let text = error.to_string();
        assert!(text.starts_with("watch_setup_failed: "), "{text}");
        assert!(text.ends_with("slow"), "{text}");
        assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(800), "{elapsed:?}");
    }

    #[test]
    fn a_zero_deadline_is_a_deadline_error_not_a_hang() {
        let dir = TempDir::new("zero");
        for index in 0..200 {
            dir.write(&format!("d{index}/f"), "f");
        }
        let filter = filter(dir.path(), &[]);
        let started = Instant::now();
        let result = read_with_deadline(dir.path().to_path_buf(), filter, Duration::ZERO);
        // Either the walk won the race or it was given up; never a hang.
        assert!(
            matches!(result, Ok(_) | Err(PollError::Deadline { .. })),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    struct Harness {
        events: Receiver<ChangeEvent>,
        stop: Arc<AtomicBool>,
        handle: JoinHandle<()>,
    }

    fn start(root: &Path, extra: &[&str], keep: Option<usize>) -> Harness {
        let (sender, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let sent = AtomicUsize::new(0);
        let handle = spawn_poller(
            "r".to_owned(),
            root.to_path_buf(),
            filter(root, extra),
            PollConfig {
                interval: Duration::from_millis(100),
                walk_deadline: Duration::from_secs(5),
                walk_deadline_max: Duration::from_secs(5),
                degraded_every: Duration::from_secs(60),
                reason: BackendReason::WatchLimit,
                detail: "limit 8".to_owned(),
                carry: new_carry(),
            },
            Arc::clone(&stop),
            Box::new(move |event| {
                let accepted = keep.is_none_or(|keep| sent.fetch_add(1, Ordering::SeqCst) < keep);
                accepted && sender.send(event).is_ok()
            }),
        )
        .unwrap();
        Harness {
            events,
            stop,
            handle,
        }
    }

    impl Harness {
        fn next(&self) -> ChangeEvent {
            self.events
                .recv_timeout(Duration::from_secs(10))
                .expect("an event within ten seconds")
        }
        fn quiet(&self, wait: Duration) {
            if let Ok(event) = self.events.recv_timeout(wait) {
                panic!("unexpected event {event:?}");
            }
        }
        fn finish(self) {
            self.stop.store(true, Ordering::SeqCst);
            let stopped_at = Instant::now();
            while !self.handle.is_finished() {
                assert!(
                    stopped_at.elapsed() < Duration::from_secs(2),
                    "the poller did not honour stop"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            self.handle.join().unwrap();
            assert!(stopped_at.elapsed() < Duration::from_millis(500));
        }
    }

    fn backend(reason: BackendReason, detail: &str) -> ChangeEvent {
        ChangeEvent::Backend {
            routine: "r".to_owned(),
            backend: Backend::Polling,
            reason,
            detail: detail.to_owned(),
        }
    }

    #[test]
    fn the_poller_reports_backend_then_the_first_difference() {
        let dir = TempDir::new("e2e");
        dir.write("a/f.txt", "f");
        let harness = start(dir.path(), &[], None);
        assert_eq!(
            harness.next(),
            backend(BackendReason::WatchLimit, "limit 8")
        );
        harness.quiet(Duration::from_millis(450));
        dir.write("a/new.txt", "n");
        assert_eq!(
            harness.next(),
            ChangeEvent::Changed {
                routine: "r".to_owned(),
                dir: "a".to_owned()
            }
        );
        harness.quiet(Duration::from_millis(450));
        harness.finish();
    }

    #[test]
    fn emit_returning_false_ends_the_poller() {
        let dir = TempDir::new("emitfalse");
        let harness = start(dir.path(), &[], Some(1));
        assert_eq!(
            harness.next(),
            backend(BackendReason::WatchLimit, "limit 8")
        );
        // The baseline first: written before it, the files would be in it and
        // nothing would ever be reported (the poller, rightly, would run on).
        harness.quiet(Duration::from_millis(250));
        // Bounded: a poller that does not end fails the test, never hangs it.
        // A new file every 300 ms, so a slow baseline cannot swallow them all.
        let waited = Instant::now();
        let mut written = 0;
        while !harness.handle.is_finished() {
            assert!(
                waited.elapsed() < Duration::from_secs(10),
                "the poller did not end after emit returned false"
            );
            if waited.elapsed() >= Duration::from_millis(300 * written) {
                dir.write(&format!("f{written}.txt"), "f");
                written += 1;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_vanished_source_is_reported_once_and_its_return_is_a_fresh_baseline_with_overflow() {
        let dir = TempDir::new("gap");
        let root = dir.path().join("src");
        fs::create_dir_all(root.join("a")).unwrap();
        let harness = start(&root, &[], None);
        assert_eq!(
            harness.next(),
            backend(BackendReason::WatchLimit, "limit 8")
        );
        harness.quiet(Duration::from_millis(250));
        // Out of the poller's sight in one step. `remove_dir_all` takes the tree
        // apart entry by entry, and a reading that lands in the middle of that
        // (slow filesystems: macOS x86_64 runners) correctly reports the
        // half-removed tree as changed (`Changed`), or an entry that vanished
        // under the walk as a named skip; neither is what this test is about.
        let gone = dir.path().join("src.gone");
        fs::rename(&root, &gone).unwrap();
        fs::remove_dir_all(&gone).unwrap();
        let failure = harness.next();
        let ChangeEvent::Backend { reason, detail, .. } = failure else {
            panic!("expected the failure, got {failure:?}");
        };
        assert_eq!(reason, BackendReason::SourceMissing);
        assert!(detail.ends_with(&root.display().to_string()), "{detail}");
        assert_eq!(
            harness.next(),
            ChangeEvent::Source {
                routine: "r".to_owned(),
                status: SourceStatus::Missing
            }
        );
        // Once per transition, not every interval.
        harness.quiet(Duration::from_millis(450));
        // The source returns whole: built beside it and renamed into place, so no
        // reading sees it half-built (that would be a legitimate `Changed` after
        // the fresh baseline, and a second event this test does not expect).
        let fresh = dir.path().join("src.new");
        fs::create_dir_all(fresh.join("b")).unwrap();
        fs::rename(&fresh, &root).unwrap();
        assert_eq!(
            harness.next(),
            ChangeEvent::Source {
                routine: "r".to_owned(),
                status: SourceStatus::Ok
            }
        );
        assert_eq!(
            harness.next(),
            backend(BackendReason::WatchLimit, "limit 8")
        );
        assert_eq!(
            harness.next(),
            ChangeEvent::Overflow {
                routine: Some("r".to_owned())
            }
        );
        harness.quiet(Duration::from_millis(450));
        harness.finish();
    }

    #[cfg(unix)]
    #[test]
    fn skipped_entries_are_named_in_the_backend_detail() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("skipdetail");
        dir.write("open/f", "f");
        dir.write("locked/s", "s");
        fs::set_permissions(dir.path().join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        if super::super::testkit::permissions_do_not_bind(
            "skipped_entries_are_named_in_the_backend_detail",
            &dir.path().join("locked"),
        ) {
            return;
        }
        let harness = start(dir.path(), &[], None);
        assert_eq!(
            harness.next(),
            backend(BackendReason::WatchLimit, "limit 8")
        );
        assert_eq!(
            harness.next(),
            backend(
                BackendReason::WatchLimit,
                "limit 8; 1 entry skipped: locked (PermissionDenied)"
            )
        );
        harness.finish();
    }

    /// H4: a tree that cannot be walked within the deadline was never read,
    /// so realtime never dispatched anything. The deadline now grows (named)
    /// up to its cap; while no walk finishes the tree is synced whole at most
    /// once per floor; and once one finishes, changes are seen again.
    #[test]
    fn a_tree_too_slow_to_walk_grows_its_deadline_and_is_synced_whole_meanwhile() {
        struct Slow;
        impl DirSource for Slow {
            fn list(&self, dir: &Path) -> io::Result<Vec<Listed>> {
                if dir.ends_with("slow") {
                    thread::sleep(Duration::from_millis(400));
                }
                FsSource.list(dir)
            }
        }
        let dir = TempDir::new("grow");
        dir.write("slow/f", "f");
        dir.write("a/f", "f");
        let (sender, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_poller_from(
            Arc::new(Slow),
            "r".to_owned(),
            dir.path().to_path_buf(),
            filter(dir.path(), &[]),
            PollConfig {
                interval: Duration::from_millis(20),
                walk_deadline: Duration::from_millis(100),
                walk_deadline_max: Duration::from_secs(2),
                degraded_every: Duration::from_secs(30),
                reason: BackendReason::WatchSetupFailed,
                detail: "arm gave up".to_owned(),
                carry: new_carry(),
            },
            Arc::clone(&stop),
            Box::new(move |event| sender.send(event).is_ok()),
        )
        .unwrap();
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        // Until a change made after the first finished walk is reported.
        let mut written = false;
        loop {
            assert!(Instant::now() < deadline, "{seen:#?}");
            let Ok(event) = events.recv_timeout(Duration::from_millis(100)) else {
                continue;
            };
            if !written
                && matches!(&event, ChangeEvent::Backend { detail, .. } if detail == "arm gave up")
                && seen.len() > 1
            {
                // The walk finished (the backend is reported clean again).
                dir.write("a/new", "n");
                written = true;
            }
            let done = matches!(&event, ChangeEvent::Changed { dir, .. } if dir == "a");
            seen.push(event);
            if done {
                break;
            }
        }
        stop.store(true, Ordering::SeqCst);
        let grown = seen
            .iter()
            .filter(|e| matches!(e, ChangeEvent::Backend { detail, .. }
                if detail.contains("walk_deadline_exceeded") && detail.contains("next_walk_deadline_seconds=")))
            .count();
        assert!(grown >= 1, "the growth is named: {seen:#?}");
        let overflows = seen
            .iter()
            .filter(|e| matches!(e, ChangeEvent::Overflow { .. }))
            .count();
        // One whole-tree run while degraded (the floor is 30 s), and one for
        // the gap when the first walk finishes: never one per walk.
        assert_eq!(
            overflows, 2,
            "whole tree once per floor while degraded: {seen:#?}"
        );
        let _ = handle.join();
    }

    /// N-D6: the change source rebuilds the poller on every native retry. A
    /// tree it cannot walk used to start over each time: the deadline back to
    /// its first value, and the tree synced whole at once. What the first poller
    /// learned (the grown deadline, when the tree was last synced whole, what it
    /// last reported) now outlives it.
    /// A change made while no poller walks (between a poller and its successor) is unknown
    /// to the successor, whose first finished walk becomes its baseline: without a gap carried
    /// over, a file created in that gap ends up in the baseline and nothing is ever reported for
    /// it. The successor owes an overflow for the first baseline of a routine that had a poller.
    #[test]
    fn a_successor_poller_owes_an_overflow_for_what_changed_between_pollers() {
        let dir = TempDir::new("gap-between");
        dir.write("f", "f");
        let carry = new_carry();
        let run_one = |source: Arc<dyn DirSource>, watch_for: Duration| {
            let (sender, events) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let handle = spawn_poller_from(
                source,
                "r".to_owned(),
                dir.path().to_path_buf(),
                filter(dir.path(), &[]),
                PollConfig {
                    // The walks here are instant; a 60 ms deadline failed whenever the machine was busy.
                    interval: Duration::from_millis(20),
                    walk_deadline: Duration::from_secs(5),
                    walk_deadline_max: Duration::from_secs(5),
                    degraded_every: Duration::from_secs(30),
                    reason: BackendReason::WatchSetupFailed,
                    detail: "arm gave up".to_owned(),
                    carry: Arc::clone(&carry),
                },
                Arc::clone(&stop),
                Box::new(move |event| sender.send(event).is_ok()),
            )
            .unwrap();
            let mut seen = Vec::new();
            let until = Instant::now() + watch_for;
            while Instant::now() < until {
                if let Ok(event) = events.recv_timeout(Duration::from_millis(20)) {
                    seen.push(event);
                }
            }
            stop.store(true, Ordering::SeqCst);
            handle.join().expect("the poller panicked");
            seen
        };
        // The first poller reads the tree (its baseline) and ends; then a file appears in the
        // gap. No blocked walks: they would hold the process-wide walk cap that other tests
        // share.
        let first = run_one(Arc::new(FsSource), Duration::from_millis(400));
        assert!(!first.is_empty(), "the first poller reported nothing");
        dir.write("created-in-the-gap", "g");
        let second = run_one(Arc::new(FsSource), Duration::from_millis(600));
        let overflows = second
            .iter()
            .filter(|event| matches!(event, ChangeEvent::Overflow { .. }))
            .count();
        assert_eq!(
            overflows, 1,
            "the successor's first baseline owes one overflow for the gap: {second:#?}"
        );
    }

    #[test]
    fn a_restarted_poller_keeps_the_growth_and_the_whole_tree_cadence() {
        struct Stuck;
        impl DirSource for Stuck {
            fn list(&self, dir: &Path) -> io::Result<Vec<Listed>> {
                thread::sleep(Duration::from_millis(400));
                FsSource.list(dir)
            }
        }
        let dir = TempDir::new("restart");
        dir.write("f", "f");
        let carry = new_carry();
        let run_one = |label: &str, watch_for: Duration| {
            let (sender, events) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let handle = spawn_poller_from(
                Arc::new(Stuck),
                "r".to_owned(),
                dir.path().to_path_buf(),
                filter(dir.path(), &[]),
                PollConfig {
                    interval: Duration::from_millis(20),
                    walk_deadline: Duration::from_millis(60),
                    walk_deadline_max: Duration::from_millis(240),
                    degraded_every: Duration::from_secs(30),
                    reason: BackendReason::WatchSetupFailed,
                    detail: "arm gave up".to_owned(),
                    carry: Arc::clone(&carry),
                },
                Arc::clone(&stop),
                Box::new(move |event| sender.send(event).is_ok()),
            )
            .unwrap();
            let mut seen = Vec::new();
            let until = Instant::now() + watch_for;
            while Instant::now() < until {
                if let Ok(event) = events.recv_timeout(Duration::from_millis(20)) {
                    seen.push(event);
                }
            }
            stop.store(true, Ordering::SeqCst);
            handle
                .join()
                .unwrap_or_else(|_| panic!("{label}: the poller panicked"));
            seen
        };
        let first = run_one("first", Duration::from_millis(700));
        let overflows = |seen: &[ChangeEvent]| {
            seen.iter()
                .filter(|e| matches!(e, ChangeEvent::Overflow { .. }))
                .count()
        };
        assert_eq!(
            overflows(&first),
            1,
            "the first poller syncs whole once: {first:#?}"
        );
        let grown = carry.lock().unwrap().walk_deadline;
        assert!(
            grown > Some(Duration::from_millis(60)),
            "the deadline grew and was handed on: {grown:?}"
        );
        // A successor with the same account: no second whole-tree run inside
        // the cadence, no repeat of the opening report, and it starts from the
        // grown deadline.
        let second = run_one("second", Duration::from_millis(700));
        assert_eq!(
            overflows(&second),
            0,
            "the restart must not sync the tree whole again: {second:#?}"
        );
        assert!(
            !second.iter().any(
                |e| matches!(e, ChangeEvent::Backend { detail, .. } if detail == "arm gave up")
            ),
            "the same account is not announced again: {second:#?}"
        );
        assert!(
            carry.lock().unwrap().walk_deadline >= grown,
            "a deadline never shrinks across restarts"
        );
    }

    /// H9: a disarmed routine's floor walk ends at once, not at its deadline.
    #[test]
    fn a_walk_waited_for_with_a_stop_flag_ends_when_it_is_set() {
        let dir = TempDir::new("untilstop");
        dir.write("f", "f");
        let stop = AtomicBool::new(true);
        let started = Instant::now();
        let outcome = read_until_stopped(
            dir.path().to_path_buf(),
            filter(dir.path(), &[]),
            Duration::ZERO,
            &stop,
        );
        assert!(outcome.is_none(), "stopped, not a deadline: {outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        let running = AtomicBool::new(false);
        let read = read_until_stopped(
            dir.path().to_path_buf(),
            filter(dir.path(), &[]),
            Duration::from_secs(5),
            &running,
        )
        .expect("not stopped")
        .expect("a small tree reads");
        assert!(read.dirs.contains_key(""));
    }

    #[test]
    fn stop_ends_the_poller_within_500_ms() {
        let dir = TempDir::new("stop");
        let harness = start(dir.path(), &[], None);
        assert_eq!(
            harness.next(),
            backend(BackendReason::WatchLimit, "limit 8")
        );
        harness.finish();
    }
}
