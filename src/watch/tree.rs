//! The inotify tree: one instance, one source, one watch per directory that is not excluded.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor};

use super::{
    Change, EventSet, Filter, LimitExceeded, LimitPolicy, WatchError, effective_watch_limit,
    join_relative, watch_budget,
};

/// Events gathered into one batch before it is processed. A burst larger than this is handled
/// over several calls, so a flood cannot grow the batch without bound.
const BATCH_EVENTS: usize = 16_384;

/// How to arm a [`TreeWatch`].
pub struct WatchOptions {
    pub filter: Filter,
    pub events: EventSet,
    pub limit: LimitPolicy,
    /// Bound on every walk: the initial arming, each new subtree, each re-walk.
    pub setup_deadline: Duration,
}

/// Lists the subdirectories of one directory. The walk reaches the filesystem only through this,
/// which is what lets a test stand in a directory whose read never returns.
pub(crate) trait DirReader: Send + Sync {
    fn subdirectories(&self, path: &Path) -> io::Result<Vec<OsString>>;
}

struct FsReader;

impl DirReader for FsReader {
    fn subdirectories(&self, path: &Path) -> io::Result<Vec<OsString>> {
        #[cfg(feature = "fault-inject")]
        fault::block_if_named("read_dir", path);
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            // d_type where the filesystem reports it, an lstat where it does not: a symbolic
            // link to a directory is not a directory here, so the walk never leaves the tree.
            if entry.file_type()?.is_dir() {
                names.push(entry.file_name());
            }
        }
        Ok(names)
    }
}

/// `SDSYNC_FAULT_BLOCK=read_dir:*/slow` makes listing any directory whose path ends in `/slow`
/// block forever, the way a directory on a dying disk does. Never compiled into a release.
#[cfg(feature = "fault-inject")]
mod fault {
    use std::path::Path;

    pub(super) fn block_if_named(operation: &str, path: &Path) {
        let Some(rule) = std::env::var_os("SDSYNC_FAULT_BLOCK") else {
            return;
        };
        let rule = rule.to_string_lossy();
        let Some(pattern) = rule
            .strip_prefix(operation)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            return;
        };
        let text = path.to_string_lossy();
        let matched = match pattern.strip_prefix('*') {
            Some(suffix) => text.ends_with(suffix),
            None => text == pattern,
        };
        if matched {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }
}

/// One watched directory. About a hundred bytes each: a share with 100 000 directories costs the
/// watcher roughly 12 MiB, which is why children are found by scanning a short list rather than
/// kept in a map keyed by a second copy of every name.
struct Node {
    parent: Option<WatchDescriptor>,
    name: Box<OsStr>,
    children: Vec<WatchDescriptor>,
}

/// A directory still to be walked.
struct Start {
    path: PathBuf,
    relative: String,
    parent: Option<WatchDescriptor>,
    name: OsString,
}

/// A directory the walk armed.
struct Walked {
    wd: WatchDescriptor,
    parent: Option<WatchDescriptor>,
    name: OsString,
}

/// The budget a walk works against.
#[derive(Clone, Copy, Debug)]
struct Budget {
    budget: Option<u64>,
    limit: Option<u64>,
    held: u64,
}

impl Budget {
    fn exceeded(&self, dirs: u64, refused_at: Option<PathBuf>) -> WatchError {
        WatchError::Limit(LimitExceeded {
            dirs,
            budget: self.budget,
            limit: self.limit,
            held: self.held,
            refused_at,
        })
    }
}

/// A recursively watched source tree.
pub struct TreeWatch {
    inotify: Arc<Inotify>,
    root: PathBuf,
    /// The root's own descriptor; `None` only until the first walk has armed it.
    root_wd: Option<WatchDescriptor>,
    nodes: HashMap<WatchDescriptor, Node>,
    filter: Arc<Filter>,
    events: EventSet,
    budget: Budget,
    limit_note: Option<String>,
    deadline: Duration,
    reader: Arc<dyn DirReader>,
    /// Set by the owner to end a walk in progress (the routine was disarmed).
    stop: Option<Arc<AtomicBool>>,
}

/// Every mask bit the tree asks for. `IN_ONLYDIR` and `IN_DONT_FOLLOW` make a directory replaced
/// by a file or a symbolic link between listing and watching fail instead of being watched;
/// `IN_EXCL_UNLINK` stops events for files already unlinked from a watched directory.
fn watch_mask(events: &EventSet) -> AddWatchFlags {
    let mut mask = AddWatchFlags::IN_CREATE
        | AddWatchFlags::IN_MOVED_FROM
        | AddWatchFlags::IN_MOVED_TO
        | AddWatchFlags::IN_DELETE_SELF
        | AddWatchFlags::IN_MOVE_SELF
        | AddWatchFlags::IN_ONLYDIR
        | AddWatchFlags::IN_DONT_FOLLOW
        | AddWatchFlags::from_bits_retain(libc::IN_EXCL_UNLINK);
    if events.close_write {
        mask |= AddWatchFlags::IN_CLOSE_WRITE;
    }
    if events.delete {
        mask |= AddWatchFlags::IN_DELETE;
    }
    if events.attrib {
        mask |= AddWatchFlags::IN_ATTRIB;
    }
    if events.modify {
        mask |= AddWatchFlags::IN_MODIFY;
    }
    mask
}

/// The single kind a [`Change::Path`] carries for `mask`.
fn kind_of(mask: AddWatchFlags) -> &'static str {
    if mask.contains(AddWatchFlags::IN_CLOSE_WRITE) {
        "close_write"
    } else if mask.contains(AddWatchFlags::IN_CREATE) {
        "create"
    } else if mask.contains(AddWatchFlags::IN_DELETE) {
        "delete"
    } else if mask.contains(AddWatchFlags::IN_MOVED_FROM) {
        "moved_from"
    } else if mask.contains(AddWatchFlags::IN_MOVED_TO) {
        "moved_to"
    } else if mask.contains(AddWatchFlags::IN_ATTRIB) {
        "attrib"
    } else if mask.contains(AddWatchFlags::IN_MODIFY) {
        "modify"
    } else {
        "other"
    }
}

/// Walk `starts`, arming a watch on each directory before listing it, so a child created after
/// the listing raises an event and one created before it is listed: nothing falls between.
#[allow(clippy::too_many_arguments)]
fn walk(
    inotify: &Inotify,
    mask: AddWatchFlags,
    mut stack: Vec<Start>,
    filter: &Filter,
    reader: &dyn DirReader,
    budget: Budget,
    already: u64,
    cancel: &AtomicBool,
    progress: &Mutex<PathBuf>,
) -> Result<Vec<Walked>, WatchError> {
    let mut walked = Vec::new();
    while let Some(start) = stack.pop() {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        *progress.lock().unwrap_or_else(|poison| poison.into_inner()) = start.path.clone();
        let wd = match inotify.add_watch(start.path.as_path(), mask) {
            Ok(wd) => wd,
            // Gone, or no longer a directory, between being listed and being watched: nothing to
            // watch, and its removal is the parent's event.
            Err(Errno::ENOENT | Errno::ENOTDIR) if start.parent.is_some() => continue,
            Err(Errno::ENOENT | Errno::ENOTDIR) => {
                return Err(WatchError::SourceMissing {
                    path: start.path,
                    detail: "the source is not an existing directory".to_owned(),
                });
            }
            Err(Errno::ENOSPC) => {
                return Err(budget.exceeded(already + walked.len() as u64, Some(start.path)));
            }
            Err(error) => {
                return Err(WatchError::Setup {
                    path: start.path,
                    detail: format!("cannot watch the directory: {}", error.desc()),
                });
            }
        };
        if cancel.load(Ordering::Acquire) {
            // The deadline passed while the kernel call was blocked. The watch belongs to a tree
            // that has already been given up on; take it back rather than leave it charged.
            let _ = inotify.rm_watch(wd);
            break;
        }
        let dirs = already + walked.len() as u64 + 1;
        if budget.budget.is_some_and(|budget| dirs > budget) {
            return Err(budget.exceeded(dirs, None));
        }
        let names = match reader.subdirectories(&start.path) {
            Ok(names) => names,
            Err(error)
                if start.parent.is_some()
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
            {
                Vec::new()
            }
            Err(error) => {
                return Err(WatchError::Setup {
                    path: start.path,
                    detail: format!("cannot list the directory: {error}"),
                });
            }
        };
        for name in names {
            let relative = join_relative(&start.relative, &name.to_string_lossy());
            if filter.excludes(&relative, true) {
                continue;
            }
            stack.push(Start {
                path: start.path.join(&name),
                relative,
                parent: Some(wd),
                name,
            });
        }
        walked.push(Walked {
            wd,
            parent: start.parent,
            name: start.name,
        });
    }
    Ok(walked)
}

impl TreeWatch {
    /// Watch `root` and every directory beneath it that the filter does not exclude.
    pub fn arm(root: &Path, options: WatchOptions) -> Result<Self, WatchError> {
        Self::arm_with(root, options, Arc::new(FsReader), None)
    }

    /// [`TreeWatch::arm`], with every walk (the arming and each later one) ended as soon as
    /// `stop` is set: a routine disarmed mid-walk does not keep walking until the deadline.
    pub fn arm_until(
        root: &Path,
        options: WatchOptions,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, WatchError> {
        Self::arm_with(root, options, Arc::new(FsReader), Some(stop))
    }

    pub(crate) fn arm_with(
        root: &Path,
        options: WatchOptions,
        reader: Arc<dyn DirReader>,
        stop: Option<Arc<AtomicBool>>,
    ) -> Result<Self, WatchError> {
        let (budget, limit_note) = match options.limit {
            LimitPolicy::Unchecked => (
                Budget {
                    budget: None,
                    limit: None,
                    held: 0,
                },
                Some("the watch budget is not checked".to_owned()),
            ),
            LimitPolicy::Fixed { limit, held } => (
                Budget {
                    budget: Some(watch_budget(limit, held)),
                    limit: Some(limit),
                    held,
                },
                None,
            ),
            LimitPolicy::System => {
                let (held, held_note) = match held_watches_of_current_user() {
                    Ok((watches, _instances)) => (watches, None),
                    Err(cause) => (0, Some(format!("watches already held unknown: {cause}"))),
                };
                match effective_watch_limit() {
                    Ok(limit) => (
                        Budget {
                            budget: Some(watch_budget(limit, held)),
                            limit: Some(limit),
                            held,
                        },
                        held_note,
                    ),
                    // Not a refusal: the kernel's own ENOSPC still stops a walk that does not fit,
                    // and is reported as watch_limit like the budget would have been.
                    Err(cause) => (
                        Budget {
                            budget: None,
                            limit: None,
                            held,
                        },
                        Some(format!(
                            "the watch limit is unreadable ({cause}); budget unchecked"
                        )),
                    ),
                }
            }
        };
        let inotify =
            Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).map_err(|error| {
                match error {
                    Errno::EMFILE => WatchError::Limit(LimitExceeded {
                        dirs: 0,
                        budget: budget.budget,
                        limit: budget.limit,
                        held: budget.held,
                        refused_at: Some(root.to_owned()),
                    }),
                    other => {
                        WatchError::Unavailable(format!("inotify_init1 failed: {}", other.desc()))
                    }
                }
            })?;
        let mut tree = Self {
            inotify: Arc::new(inotify),
            root: root.to_owned(),
            root_wd: None,
            nodes: HashMap::new(),
            filter: Arc::new(options.filter),
            events: options.events,
            budget,
            limit_note,
            deadline: options.setup_deadline,
            reader,
            stop,
        };
        tree.rewalk()?;
        Ok(tree)
    }

    /// How many directories are watched.
    pub fn watch_count(&self) -> usize {
        self.nodes.len()
    }

    /// The per-user limit, the watches held by this user before arming, and the budget.
    pub fn limits(&self) -> (Option<u64>, u64, Option<u64>) {
        (self.budget.limit, self.budget.held, self.budget.budget)
    }

    /// Why the budget is not fully known, when it is not.
    pub fn limit_note(&self) -> Option<&str> {
        self.limit_note.as_deref()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether a directory, relative to the root, is watched. For tests and the probe.
    pub fn is_watched(&self, relative: &str) -> bool {
        self.nodes
            .keys()
            .any(|wd| self.relative(*wd).as_deref() == Some(relative))
    }

    /// Wait up to `timeout` for changes. An empty result is a timeout (or a signal).
    ///
    /// Fails when the source root goes away ([`WatchError::SourceGone`]), when a new subtree no
    /// longer fits the budget, or when a walk of one does not finish within the deadline. Each of
    /// those leaves the tree unusable; the caller drops it.
    pub fn wait(&mut self, timeout: Duration) -> Result<Vec<Change>, WatchError> {
        let ready = {
            let mut descriptors = [PollFd::new(self.inotify.as_fd(), PollFlags::POLLIN)];
            let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
            match poll(&mut descriptors, timeout) {
                Ok(count) => count > 0,
                Err(Errno::EINTR) => false,
                Err(error) => {
                    return Err(WatchError::Setup {
                        path: self.root.clone(),
                        detail: format!("poll on the inotify descriptor failed: {}", error.desc()),
                    });
                }
            }
        };
        if !ready {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        while events.len() < BATCH_EVENTS {
            match self.inotify.read_events() {
                Ok(batch) if batch.is_empty() => break,
                Ok(batch) => events.extend(batch),
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => continue,
                Err(error) => {
                    return Err(WatchError::Setup {
                        path: self.root.clone(),
                        detail: format!("reading inotify events failed: {}", error.desc()),
                    });
                }
            }
        }
        self.process(events)
    }

    fn process(&mut self, events: Vec<InotifyEvent>) -> Result<Vec<Change>, WatchError> {
        let mut changes = Vec::new();
        let mut new_dirs = Vec::new();
        let mut moves: HashMap<u32, WatchDescriptor> = HashMap::new();
        let mut overflow = false;
        let mut rules_changed = false;
        for event in events {
            let mask = event.mask;
            if mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                overflow = true;
                continue;
            }
            if Some(event.wd) == self.root_wd
                && mask.intersects(
                    AddWatchFlags::IN_DELETE_SELF
                        | AddWatchFlags::IN_MOVE_SELF
                        | AddWatchFlags::IN_UNMOUNT
                        | AddWatchFlags::IN_IGNORED,
                )
            {
                return Err(WatchError::SourceGone {
                    path: self.root.clone(),
                });
            }
            if mask.intersects(AddWatchFlags::IN_IGNORED | AddWatchFlags::IN_UNMOUNT) {
                // The watch is gone: the directory was deleted, its filesystem unmounted, or this
                // tree removed it. Its removal was the parent's event.
                self.forget(event.wd, false);
                continue;
            }
            let Some(dir) = self.relative(event.wd) else {
                // A descriptor this tree has already let go of.
                continue;
            };
            let Some(name) = event.name else {
                // An event on a watched directory itself. Its moves and deletion arrive as its
                // parent's events too; only a change of its own metadata is news.
                if mask.contains(AddWatchFlags::IN_ATTRIB) && self.events.attrib && !dir.is_empty()
                {
                    let parent = dir.rsplit_once('/').map_or("", |(parent, _)| parent);
                    changes.push(Change::Path {
                        dir: parent.to_owned(),
                        entry: dir,
                        kind: "attrib",
                    });
                }
                continue;
            };
            let is_dir = mask.contains(AddWatchFlags::IN_ISDIR);
            let entry = join_relative(&dir, &name.to_string_lossy());
            let kind = kind_of(mask);
            if dir.is_empty() && Filter::is_rules_file(&entry) {
                rules_changed = true;
                changes.push(Change::Path { dir, entry, kind });
                continue;
            }
            if self.filter.excludes(&entry, is_dir) {
                continue;
            }
            if is_dir {
                let path = self.root.join(&entry);
                // A walk of a new directory finds the subdirectories made before its watch, and
                // their own creation events then arrive too: one walk is enough.
                let known = self.child(event.wd, &name).is_some();
                if mask.contains(AddWatchFlags::IN_CREATE) && !known {
                    new_dirs.push(Start {
                        path,
                        relative: entry.clone(),
                        parent: Some(event.wd),
                        name: name.clone(),
                    });
                } else if mask.contains(AddWatchFlags::IN_MOVED_FROM) {
                    if let Some(child) = self.child(event.wd, &name) {
                        moves.insert(event.cookie, child);
                    }
                } else if mask.contains(AddWatchFlags::IN_MOVED_TO) {
                    match moves.remove(&event.cookie) {
                        // Renamed inside the tree: the watches stay, only the path moves.
                        Some(child) => self.attach(child, Some(event.wd), name.clone()),
                        None if known => {}
                        None => new_dirs.push(Start {
                            path,
                            relative: entry.clone(),
                            parent: Some(event.wd),
                            name: name.clone(),
                        }),
                    }
                }
            }
            if self.events.reports(kind) {
                changes.push(Change::Path { dir, entry, kind });
            }
        }
        // Moved out of the tree (or to an excluded name): the directory lives on elsewhere and
        // would keep reporting under a path that no longer exists.
        for (_, child) in moves {
            self.forget(child, true);
        }
        if rules_changed {
            // On an invalid ignore file the previous rules stay; the sync names the file.
            if let Ok(rebuilt) = self.filter.rebuilt() {
                self.filter = Arc::new(rebuilt);
            }
        }
        if overflow || rules_changed {
            // Events were lost, or the rules decide differently now: walk the whole tree again.
            // The re-walk adds what is missing and drops what is now excluded or gone.
            self.rewalk()?;
            if overflow {
                changes.push(Change::Overflow);
            }
            return Ok(changes);
        }
        if !new_dirs.is_empty() {
            for walked in self.walk_bounded(new_dirs, self.nodes.len() as u64)? {
                self.attach(walked.wd, walked.parent, walked.name);
                if let Some(dir) = self.relative(walked.wd) {
                    changes.push(Change::Path {
                        dir: dir.clone(),
                        entry: dir,
                        kind: "new_dir",
                    });
                }
            }
        }
        Ok(changes)
    }

    /// Walk the whole tree again and make the arena exactly what it finds.
    fn rewalk(&mut self) -> Result<(), WatchError> {
        let start = Start {
            path: self.root.clone(),
            relative: String::new(),
            parent: None,
            name: OsString::new(),
        };
        let walked = self.walk_bounded(vec![start], 0)?;
        let previous = std::mem::take(&mut self.nodes);
        self.nodes.reserve(walked.len());
        for record in walked {
            if record.parent.is_none() {
                self.root_wd = Some(record.wd);
            }
            self.attach(record.wd, record.parent, record.name);
        }
        for wd in previous.keys() {
            if !self.nodes.contains_key(wd) {
                let _ = self.inotify.rm_watch(*wd);
            }
        }
        Ok(())
    }

    /// Run a walk on its own thread and give up on it at the deadline, naming where it was.
    ///
    /// A directory read on a dying disk can block in the kernel for as long as it likes, and
    /// nothing can interrupt it. The thread is abandoned, not killed: it notices the cancellation
    /// when its call returns, if it ever does, and releases what it armed.
    fn walk_bounded(&self, starts: Vec<Start>, already: u64) -> Result<Vec<Walked>, WatchError> {
        // H9: abandoned walks are capped process-wide, shared with the daemon's other walks.
        let slot = super::walk_slot().map_err(|live| WatchError::WalkCap {
            path: self.root.clone(),
            live,
        })?;
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(
            starts
                .first()
                .map_or_else(|| self.root.clone(), |start| start.path.clone()),
        ));
        let inotify = Arc::clone(&self.inotify);
        let filter = Arc::clone(&self.filter);
        let reader = Arc::clone(&self.reader);
        let mask = watch_mask(&self.events);
        let budget = self.budget;
        let thread_cancel = Arc::clone(&cancel);
        let thread_progress = Arc::clone(&progress);
        std::thread::Builder::new()
            .name("sdsync-watch-walk".to_owned())
            .spawn(move || {
                let _slot = slot;
                let result = walk(
                    &inotify,
                    mask,
                    starts,
                    &filter,
                    reader.as_ref(),
                    budget,
                    already,
                    &thread_cancel,
                    &thread_progress,
                );
                if thread_cancel.load(Ordering::Acquire)
                    && let Ok(walked) = &result
                {
                    for record in walked {
                        let _ = inotify.rm_watch(record.wd);
                    }
                }
                let _ = sender.send(result);
            })
            .map_err(|error| WatchError::Setup {
                path: self.root.clone(),
                detail: format!("cannot start the directory walk: {error}"),
            })?;
        let give_up_at = std::time::Instant::now() + self.deadline;
        let waited = loop {
            let left = give_up_at.saturating_duration_since(std::time::Instant::now());
            // H9: the owner's stop is looked at while the walk runs.
            if self
                .stop
                .as_ref()
                .is_some_and(|stop| stop.load(Ordering::SeqCst))
            {
                cancel.store(true, Ordering::Release);
                return Err(WatchError::Setup {
                    path: self.root.clone(),
                    detail: "the watch was stopped during its directory walk".to_owned(),
                });
            }
            if left.is_zero() {
                break Err(mpsc::RecvTimeoutError::Timeout);
            }
            let slice = if self.stop.is_some() {
                left.min(Duration::from_millis(100))
            } else {
                left
            };
            match receiver.recv_timeout(slice) {
                Ok(result) => break Ok(result),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(disconnected) => break Err(disconnected),
            }
        };
        match waited {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancel.store(true, Ordering::Release);
                let path = progress
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone();
                Err(WatchError::Deadline {
                    path,
                    deadline: self.deadline,
                })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(WatchError::Setup {
                path: self.root.clone(),
                detail: "the directory walk ended without a result".to_owned(),
            }),
        }
    }

    fn child(&self, parent: WatchDescriptor, name: &OsStr) -> Option<WatchDescriptor> {
        self.nodes.get(&parent).and_then(|node| {
            node.children.iter().copied().find(|child| {
                self.nodes
                    .get(child)
                    .is_some_and(|child| &*child.name == name)
            })
        })
    }

    /// Put `wd` under `parent` as `name`, moving it if it was elsewhere.
    fn attach(&mut self, wd: WatchDescriptor, parent: Option<WatchDescriptor>, name: OsString) {
        if let Some(parent) = parent
            && !self.nodes.contains_key(&parent)
        {
            // The parent went away while this directory was being walked: it is not reachable
            // in the tree any more, so do not keep a watch on it.
            self.forget(wd, true);
            return;
        }
        if let Some(existing) = self.nodes.get(&wd) {
            if existing.parent == parent && *existing.name == *name {
                return;
            }
            self.detach(wd);
        }
        let children = self
            .nodes
            .remove(&wd)
            .map(|node| node.children)
            .unwrap_or_default();
        if let Some(parent) = parent {
            // A directory replaced under the same name: the old one's watch is on its way out
            // (IN_IGNORED follows), and the name now belongs to this one.
            if let Some(stale) = self.child(parent, &name) {
                self.detach(stale);
            }
            if let Some(node) = self.nodes.get_mut(&parent) {
                node.children.push(wd);
            }
        }
        self.nodes.insert(
            wd,
            Node {
                parent,
                name: name.into_boxed_os_str(),
                children,
            },
        );
    }

    /// Unlink `wd` from its parent's children.
    fn detach(&mut self, wd: WatchDescriptor) {
        let Some(parent) = self.nodes.get(&wd).and_then(|node| node.parent) else {
            return;
        };
        if let Some(node) = self.nodes.get_mut(&parent) {
            node.children.retain(|child| *child != wd);
        }
    }

    /// Drop `wd` and everything beneath it. `remove_self` also removes the kernel watch on `wd`;
    /// the watches beneath it are always removed, since nothing else would.
    fn forget(&mut self, wd: WatchDescriptor, remove_self: bool) {
        if !self.nodes.contains_key(&wd) {
            if remove_self {
                let _ = self.inotify.rm_watch(wd);
            }
            return;
        }
        self.detach(wd);
        let mut stack = vec![(wd, remove_self)];
        while let Some((current, remove)) = stack.pop() {
            if let Some(node) = self.nodes.remove(&current) {
                stack.extend(node.children.into_iter().map(|child| (child, true)));
            }
            if remove {
                let _ = self.inotify.rm_watch(current);
            }
        }
    }

    /// The path of `wd` relative to the root, or `None` for a descriptor not in the tree.
    fn relative(&self, wd: WatchDescriptor) -> Option<String> {
        let mut names = Vec::new();
        let mut current = wd;
        for _ in 0..=self.nodes.len() {
            let node = self.nodes.get(&current)?;
            match node.parent {
                Some(parent) => {
                    names.push(node.name.to_string_lossy());
                    current = parent;
                }
                None => {
                    names.reverse();
                    return Some(names.join("/"));
                }
            }
        }
        None
    }
}

/// Watches held by the current user's other inotify instances, and how many instances hold them.
///
/// The limit is per user: every other watcher this package runs (one per realtime routine) draws
/// on the same budget. Read from `/proc/<pid>/fdinfo` of this user's processes, which a user may
/// read for its own. Best effort by nature -- a process can start between two reads -- and named
/// when it cannot be read at all.
pub fn held_watches_of_current_user() -> Result<(u64, u64), String> {
    let uid = real_uid(Path::new("/proc/self/status"))
        .ok_or_else(|| "/proc/self/status has no readable Uid line".to_owned())?;
    let own = std::process::id().to_string();
    let entries = std::fs::read_dir("/proc").map_err(|error| format!("/proc: {error}"))?;
    let mut watches = 0;
    let mut instances = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if pid == own || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let process = entry.path();
        if real_uid(&process.join("status")) != Some(uid) {
            continue;
        }
        let Ok(descriptors) = std::fs::read_dir(process.join("fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            let is_inotify = std::fs::read_link(descriptor.path())
                .is_ok_and(|target| target.as_os_str() == "anon_inode:inotify");
            if !is_inotify {
                continue;
            }
            let info = process.join("fdinfo").join(descriptor.file_name());
            if let Ok(text) = std::fs::read_to_string(info) {
                instances += 1;
                watches += text
                    .lines()
                    .filter(|line| line.starts_with("inotify wd:"))
                    .count() as u64;
            }
        }
    }
    Ok((watches, instances))
}

fn real_uid(status: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(status).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Lists like the filesystem, except that a directory named `slow` never answers -- the walk
    /// hangs there the way it does on a disk that stopped responding.
    struct BlockedReader {
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl DirReader for BlockedReader {
        fn subdirectories(&self, path: &Path) -> io::Result<Vec<OsString>> {
            if path.file_name() == Some(OsStr::new("slow")) {
                // Bounded so a failing test cannot leave the thread behind for good.
                let _ = self
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30));
            }
            FsReader.subdirectories(path)
        }
    }

    #[test]
    fn a_walk_blocked_on_one_directory_is_given_up_at_its_deadline_and_names_it() {
        let root = std::env::temp_dir().join(format!(
            "sdsync-watch-hang-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(root.join("fine/deeper")).unwrap();
        std::fs::create_dir_all(root.join("stuck/slow/below")).unwrap();
        let (release, receiver) = mpsc::channel();
        let reader = Arc::new(BlockedReader {
            release: Mutex::new(receiver),
        });
        let deadline = Duration::from_secs(1);
        let started = Instant::now();
        let failure = TreeWatch::arm_with(
            &root,
            WatchOptions {
                filter: Filter::build(&root, Vec::new()).unwrap(),
                events: EventSet::all(),
                limit: LimitPolicy::Unchecked,
                setup_deadline: deadline,
            },
            reader,
            None,
        )
        .err()
        .expect("a blocked walk must not arm");
        let elapsed = started.elapsed();
        assert!(elapsed < deadline + Duration::from_secs(2), "{elapsed:?}");
        assert!(elapsed >= deadline, "{elapsed:?}");
        match &failure {
            WatchError::Deadline {
                path,
                deadline: reported,
            } => {
                assert_eq!(path, &root.join("stuck/slow"));
                assert_eq!(*reported, deadline);
            }
            other => panic!("expected a deadline failure, got {other}"),
        }
        assert_eq!(failure.reason(), "watch_setup_failed");
        let line = failure.to_string();
        assert!(
            line.starts_with("watch_setup_failed deadline=1s: the directory walk did not finish"),
            "{line}"
        );
        assert!(line.ends_with("stuck/slow"), "{line}");
        let _ = release.send(());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// H9: a watch stopped while its walk is blocked ends at once, not at the
    /// (possibly long, grown) deadline; the abandoned walk is cancelled.
    #[test]
    fn a_stop_during_a_blocked_walk_ends_the_arm_at_once() {
        let root = std::env::temp_dir().join(format!(
            "sdsync-watch-stop-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(root.join("stuck/slow/below")).unwrap();
        let (release, receiver) = mpsc::channel();
        let reader = Arc::new(BlockedReader {
            release: Mutex::new(receiver),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                stop.store(true, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        let failure = TreeWatch::arm_with(
            &root,
            WatchOptions {
                filter: Filter::build(&root, Vec::new()).unwrap(),
                events: EventSet::all(),
                limit: LimitPolicy::Unchecked,
                setup_deadline: Duration::from_secs(60),
            },
            reader,
            Some(stop),
        )
        .err()
        .expect("a stopped walk must not arm");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(failure.to_string().contains("stopped during"), "{failure}");
        stopper.join().unwrap();
        let _ = release.send(());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn kinds_follow_the_mask_and_the_mask_follows_the_event_set() {
        assert_eq!(kind_of(AddWatchFlags::IN_CLOSE_WRITE), "close_write");
        assert_eq!(
            kind_of(AddWatchFlags::IN_CREATE | AddWatchFlags::IN_ISDIR),
            "create"
        );
        assert_eq!(kind_of(AddWatchFlags::IN_DELETE), "delete");
        assert_eq!(kind_of(AddWatchFlags::IN_MOVED_FROM), "moved_from");
        assert_eq!(kind_of(AddWatchFlags::IN_MOVED_TO), "moved_to");
        assert_eq!(kind_of(AddWatchFlags::IN_ATTRIB), "attrib");
        assert_eq!(kind_of(AddWatchFlags::IN_MODIFY), "modify");
        assert_eq!(kind_of(AddWatchFlags::IN_OPEN), "other");
        let quiet = watch_mask(&EventSet::default());
        assert!(quiet.contains(AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MOVE));
        assert!(!quiet.intersects(
            AddWatchFlags::IN_CLOSE_WRITE
                | AddWatchFlags::IN_DELETE
                | AddWatchFlags::IN_ATTRIB
                | AddWatchFlags::IN_MODIFY
        ));
        let all = watch_mask(&EventSet::all());
        assert!(all.contains(
            AddWatchFlags::IN_CLOSE_WRITE
                | AddWatchFlags::IN_DELETE
                | AddWatchFlags::IN_ATTRIB
                | AddWatchFlags::IN_MODIFY
                | AddWatchFlags::from_bits_retain(libc::IN_EXCL_UNLINK)
        ));
    }

    #[test]
    fn held_watches_are_readable_and_skip_this_process() {
        // The count is of other processes: a watcher must not count the instance it is arming.
        // A child holding watches is counted; tests/watch_cli.rs proves that end to end.
        let other = Inotify::init(InitFlags::IN_CLOEXEC).unwrap();
        other
            .add_watch(std::env::temp_dir().as_path(), AddWatchFlags::IN_CREATE)
            .unwrap();
        held_watches_of_current_user().unwrap();
        assert!(real_uid(Path::new("/proc/self/status")).is_some());
        assert_eq!(real_uid(Path::new("/proc/self/absent")), None);
    }
}
