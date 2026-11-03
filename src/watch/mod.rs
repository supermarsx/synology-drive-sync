//! Native recursive directory watching for realtime routines.
//!
//! DSM ships no `inotifywait`, and the controller looked for nothing else, so on a stock NAS every
//! realtime routine was demoted to polling. This module watches a source tree with inotify itself.
//! The DSM bridge exposes it as `sdsync-dsm-api --watch`, an `inotifywait`-compatible subset the
//! shell controller spawns in place of the missing tool, and as `--watch-probe`, which an operator
//! runs on the NAS to see what inotify does for one source as the package user.
//!
//! The parts that decide *what* is watched and *what* a change means live here and are portable:
//! the exclusion filter (the DSM-managed names plus the profile's own rules, through the same
//! matcher the sync uses, so the watch set never disagrees with what a run would consider), the
//! watch budget, the coalescer and the argument parser. The inotify tree itself is Linux-only
//! ([`TreeWatch`]); every other platform reports `watch_unavailable` by name.
//!
//! One [`TreeWatch`] owns one inotify instance and one source. Two sources may nest (profile
//! overlap is refused only with deletion), and inotify hands back the same watch descriptor for
//! one directory however it is reached on one instance, so per-tree instances are what keep two
//! routines' arenas from sharing nodes.

pub mod cli;
#[cfg(target_os = "linux")]
mod tree;

#[cfg(target_os = "linux")]
pub use tree::{TreeWatch, WatchOptions, held_watches_of_current_user};

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::local::{DEFAULT_IGNORE_FILE, IgnoreRules};
use crate::path::is_dsm_managed;

/// How long arming a tree, or any later walk of part of it, may take before the watcher gives up
/// and names the directory it was stuck on. Long enough for a hibernating disk to spin up.
pub const DEFAULT_SETUP_DEADLINE: Duration = Duration::from_secs(120);

/// At most one marker line per this interval: a copy of ten thousand files is one line every
/// 100 ms, not ten thousand lines, so the controller's event marker stays small.
pub const COALESCE_INTERVAL: Duration = Duration::from_millis(100);

/// The per-user watch limit. World-readable; only root can raise it.
pub const MAX_USER_WATCHES_PATH: &str = "/proc/sys/fs/inotify/max_user_watches";

/// The same limit for the current user namespace, on kernels that count watches per namespace
/// (5.11 and later). A container's own limit can be lower than the host's; DSM's kernels have no
/// such file, and then only [`MAX_USER_WATCHES_PATH`] applies.
pub const NAMESPACE_WATCHES_PATH: &str = "/proc/sys/user/max_inotify_watches";

/// The largest profile fragment the watcher reads. Real fragments are a few hundred bytes.
const FRAGMENT_LIMIT: u64 = 256 * 1024;

/// Something in the watched tree changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Change {
    /// An entry changed. `dir` is the directory whose contents a scoped sync must re-read,
    /// relative to the source root (`""` for the root); `entry` is the changed path itself.
    Path {
        dir: String,
        entry: String,
        kind: &'static str,
    },
    /// The kernel dropped events (`IN_Q_OVERFLOW`): the whole tree has to be treated as changed.
    /// The watch set has already been re-walked when this is reported.
    Overflow,
}

/// Why a tree could not be watched, or stopped being watched.
///
/// [`WatchError::reason`] is a `backend_reason` token the controller and the dashboard already
/// know; this module adds none, because the bridge validates that vocabulary and a new value would
/// fail the whole snapshot closed.
#[derive(Debug)]
pub enum WatchError {
    /// The command line is not one this subset accepts.
    Usage(String),
    /// inotify itself is not available here.
    Unavailable(String),
    /// The source does not exist, is not a directory, or is a symbolic link.
    SourceMissing { path: PathBuf, detail: String },
    /// The source root was deleted, moved or unmounted while watched.
    SourceGone { path: PathBuf },
    /// The profile's exclude rules could not be read.
    Rules(String),
    /// The tree needs more watches than the budget allows, or the kernel refused one.
    Limit(LimitExceeded),
    /// A walk did not finish within its deadline; `path` is where it was stuck.
    Deadline { path: PathBuf, deadline: Duration },
    /// A directory could not be watched or listed.
    Setup { path: PathBuf, detail: String },
    /// The event marker could not be written.
    Output(String),
}

/// The numbers behind a `watch_limit` refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitExceeded {
    /// Directories counted when the walk stopped: one past the budget for a refusal, or the
    /// count at the moment the kernel said no.
    pub dirs: u64,
    pub budget: Option<u64>,
    pub limit: Option<u64>,
    pub held: u64,
    /// Where `inotify_add_watch` returned `ENOSPC`, when it was the kernel that refused.
    pub refused_at: Option<PathBuf>,
}

impl WatchError {
    /// The `backend_reason` this failure leaves a realtime routine with.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Limit(_) => "watch_limit",
            Self::Unavailable(_) => "watch_unavailable",
            Self::SourceMissing { .. } | Self::SourceGone { .. } => "source_missing",
            Self::Usage(_)
            | Self::Rules(_)
            | Self::Deadline { .. }
            | Self::Setup { .. }
            | Self::Output(_) => "watch_setup_failed",
        }
    }

    /// The process exit status `--watch` ends with. Nothing parses it -- the controller reads the
    /// first stderr line -- but each cause keeps its own so a log of exits is still readable.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 64,
            Self::SourceMissing { .. } | Self::SourceGone { .. } => 66,
            Self::Unavailable(_) => 69,
            Self::Setup { .. } => 71,
            Self::Deadline { .. } => 72,
            Self::Limit(_) => 73,
            Self::Output(_) => 74,
            Self::Rules(_) => 78,
        }
    }
}

/// One line: the reason first, then the numbers or the cause, and a path last. The controller
/// keeps only the first 120 printable bytes of the first stderr line, and a path on a NAS share is
/// long, so the path is what gets cut, never the cause.
impl fmt::Display for WatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = self.reason();
        match self {
            Self::Usage(detail) => write!(formatter, "{reason} usage: {detail}"),
            Self::Unavailable(detail) => write!(formatter, "{reason}: {detail}"),
            Self::SourceMissing { path, detail } => {
                write!(formatter, "{reason}: {detail}; path={}", path.display())
            }
            Self::SourceGone { path } => write!(
                formatter,
                "{reason}: the source was deleted, moved or unmounted while watched; path={}",
                path.display()
            ),
            Self::Rules(detail) => write!(formatter, "{reason} rules: {detail}"),
            Self::Limit(limit) => {
                write!(formatter, "{reason} dirs={}", limit.dirs)?;
                if let Some(budget) = limit.budget {
                    write!(formatter, " budget={budget}")?;
                }
                match limit.limit {
                    Some(value) => write!(formatter, " limit={value}")?,
                    None => write!(formatter, " limit=unread")?,
                }
                write!(formatter, " held={}", limit.held)?;
                match &limit.refused_at {
                    Some(path) => write!(
                        formatter,
                        " at={}: the kernel refused another watch (ENOSPC)",
                        path.display()
                    )?,
                    None => write!(
                        formatter,
                        ": more directories than 80% of the per-user inotify limit leaves"
                    )?,
                }
                write!(
                    formatter,
                    "; raise fs.inotify.max_user_watches (root sysctl) or narrow the source"
                )
            }
            Self::Deadline { path, deadline } => write!(
                formatter,
                "{reason} deadline={}s: the directory walk did not finish in time; path={}",
                deadline.as_secs(),
                path.display()
            ),
            Self::Setup { path, detail } => {
                write!(formatter, "{reason}: {detail}; path={}", path.display())
            }
            Self::Output(detail) => write!(formatter, "{reason} output: {detail}"),
        }
    }
}

impl std::error::Error for WatchError {}

/// What may be watched and reported: everything except the DSM-managed names (`@eaDir`,
/// `#recycle`, ...) and whatever the profile's rules exclude.
///
/// The rules are [`IgnoreRules`], the sync's own matcher: the built-in defaults, the source's
/// `.sdsyncignore`, then the profile's `excludes`. A path the sync would never carry raises no
/// event, and a directory it would never descend into costs no watch.
pub struct Filter {
    source: PathBuf,
    extra: Vec<String>,
    rules: IgnoreRules,
}

impl Filter {
    /// Rules for `source` plus the profile's `extra` patterns.
    pub fn build(source: &Path, extra: Vec<String>) -> Result<Self, WatchError> {
        let rules = IgnoreRules::build(source, &extra).map_err(|error| match &error {
            crate::Error::FileIo { source: io, .. }
                if io.kind() == std::io::ErrorKind::NotFound =>
            {
                WatchError::SourceMissing {
                    path: source.to_owned(),
                    detail: "the source does not exist".to_owned(),
                }
            }
            crate::Error::UnsupportedLocalEntry { path, reason } if path.as_path() == source => {
                WatchError::SourceMissing {
                    path: source.to_owned(),
                    detail: reason.clone(),
                }
            }
            _ => WatchError::Rules(error.to_string()),
        })?;
        Ok(Self {
            source: source.to_owned(),
            extra,
            rules,
        })
    }

    /// The rules read again, after `.sdsyncignore` changed. A caller keeps the previous filter
    /// on failure: the sync itself refuses an invalid ignore file and says so, and watching by
    /// the last good rules until then loses nothing.
    pub fn rebuilt(&self) -> Result<Self, WatchError> {
        Self::build(&self.source, self.extra.clone())
    }

    /// Whether `relative` (slash-separated, relative to the source) is outside the watch.
    pub fn excludes(&self, relative: &str, is_dir: bool) -> bool {
        !relative.is_empty()
            && (is_dsm_managed(relative) || self.rules.is_ignored(relative, is_dir))
    }

    /// Whether `relative` is the source's own ignore file, whose change re-reads the rules.
    pub fn is_rules_file(relative: &str) -> bool {
        relative == DEFAULT_IGNORE_FILE
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    /// How many profile patterns this filter carries on top of the defaults.
    pub fn profile_patterns(&self) -> usize {
        self.extra.len()
    }
}

/// The profile's `excludes`, read from its fragment (`profiles.d/<name>.toml`).
///
/// A fragment that cannot be read or parsed is a named failure rather than "no excludes": the
/// controller and the sync read the same file, so a watcher that guessed would be the only
/// component disagreeing about it.
pub fn profile_excludes(fragment: &Path) -> Result<Vec<String>, WatchError> {
    use std::io::Read;
    let failure = |detail: String| {
        WatchError::Rules(format!("profile fragment {}: {detail}", fragment.display()))
    };
    let file = std::fs::File::open(fragment).map_err(|error| failure(error.to_string()))?;
    let mut text = String::new();
    file.take(FRAGMENT_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|error| failure(error.to_string()))?;
    if text.len() as u64 > FRAGMENT_LIMIT {
        return Err(failure(format!("larger than {FRAGMENT_LIMIT} bytes")));
    }
    let table: toml::Table = toml::from_str(&text).map_err(|error| failure(error.to_string()))?;
    match table.get("excludes") {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                toml::Value::String(pattern) => Ok(pattern.clone()),
                _ => Err(failure(
                    "excludes holds a value that is not a string".to_owned(),
                )),
            })
            .collect(),
        Some(_) => Err(failure("excludes is not an array".to_owned())),
    }
}

/// The watch budget for one tree: 80% of the per-user limit, less the watches this user already
/// holds. The 20% margin is the shell controller's, kept so the two agree on what fits.
pub fn watch_budget(limit: u64, held: u64) -> u64 {
    (limit / 10 * 8).saturating_sub(held)
}

/// The watch limit that applies to this process: [`MAX_USER_WATCHES_PATH`], or the current user
/// namespace's limit where that is lower. An unreadable namespace limit is not an error -- most
/// kernels have none -- but an unreadable per-user limit is.
pub fn effective_watch_limit() -> Result<u64, String> {
    let limit = read_watch_limit(Path::new(MAX_USER_WATCHES_PATH))
        .map_err(|cause| format!("{MAX_USER_WATCHES_PATH}: {cause}"))?;
    Ok(match read_watch_limit(Path::new(NAMESPACE_WATCHES_PATH)) {
        Ok(namespace) => limit.min(namespace),
        Err(_) => limit,
    })
}

/// A watch limit, read whole (a byte-at-a-time read of procfs gets one digit).
pub fn read_watch_limit(path: &Path) -> Result<u64, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value = text.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{value:?} is not a count"));
    }
    match value.parse::<u64>() {
        Ok(0) => Err("the limit is zero".to_owned()),
        Ok(limit) => Ok(limit),
        Err(_) => Ok(u64::MAX),
    }
}

/// How the watch limit is decided.
#[derive(Clone, Debug)]
pub enum LimitPolicy {
    /// Read [`effective_watch_limit`] and count what this user already holds.
    System,
    /// A fixed limit and held count, for tests.
    Fixed { limit: u64, held: u64 },
    /// No budget: only the kernel's own `ENOSPC` refuses.
    Unchecked,
}

/// The event kinds `-e` asked to be reported. Directory bookkeeping (creation, moves, removal)
/// happens whatever is asked; this only decides which changes produce a marker line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventSet {
    pub close_write: bool,
    pub create: bool,
    pub delete: bool,
    pub moved_from: bool,
    pub moved_to: bool,
    pub attrib: bool,
    pub modify: bool,
}

impl EventSet {
    /// Everything that can change what a sync sees.
    pub fn all() -> Self {
        Self {
            close_write: true,
            create: true,
            delete: true,
            moved_from: true,
            moved_to: true,
            attrib: true,
            modify: true,
        }
    }

    fn add(&mut self, name: &str) -> Result<(), WatchError> {
        match name {
            "close_write" => self.close_write = true,
            "create" => self.create = true,
            "delete" => self.delete = true,
            "move" => {
                self.moved_from = true;
                self.moved_to = true;
            }
            "moved_from" => self.moved_from = true,
            "moved_to" => self.moved_to = true,
            "attrib" => self.attrib = true,
            "modify" => self.modify = true,
            other => {
                return Err(WatchError::Usage(format!(
                    "event {other:?} is not supported (close_write, create, delete, move, \
                     moved_from, moved_to, attrib, modify)"
                )));
            }
        }
        Ok(())
    }

    /// Whether a change of `kind` (a [`Change::Path`] kind) is reported.
    pub fn reports(&self, kind: &str) -> bool {
        match kind {
            "close_write" => self.close_write,
            "create" => self.create,
            "delete" => self.delete,
            "moved_from" => self.moved_from,
            "moved_to" => self.moved_to,
            "attrib" => self.attrib,
            "modify" => self.modify,
            // A directory found by a walk after its creation was reported: what is inside it may
            // predate its watch, so it is always a change.
            "new_dir" => true,
            _ => false,
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Holds at most one pending marker and releases it no more than once per interval.
#[derive(Debug)]
pub struct Coalescer {
    interval: Duration,
    last: Option<Instant>,
    pending: Option<bool>,
}

impl Coalescer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            pending: None,
        }
    }

    /// Record a change; `overflow` sticks until the line is taken.
    pub fn note(&mut self, overflow: bool) {
        self.pending = Some(self.pending.unwrap_or(false) || overflow);
    }

    /// How long until a pending line may be written; `None` when nothing is pending.
    pub fn wait_hint(&self, now: Instant) -> Option<Duration> {
        self.pending?;
        Some(match self.last {
            Some(last) => (last + self.interval).saturating_duration_since(now),
            None => Duration::ZERO,
        })
    }

    /// The pending line, if one is due: `Some(true)` for an overflow line.
    pub fn take(&mut self, now: Instant) -> Option<bool> {
        if self.wait_hint(now)? > Duration::ZERO {
            return None;
        }
        self.last = Some(now);
        self.pending.take()
    }

    /// The pending line regardless of the interval, for a watcher about to exit.
    pub fn take_now(&mut self) -> Option<bool> {
        self.pending.take()
    }
}

/// `entry` under `dir`, both relative and slash-separated.
pub fn join_relative(dir: &str, entry: &str) -> String {
    if dir.is_empty() {
        entry.to_owned()
    } else {
        format!("{dir}/{entry}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_the_controllers_eighty_percent_less_what_is_held() {
        assert_eq!(watch_budget(8192, 0), 6552);
        assert_eq!(watch_budget(8192, 552), 6000);
        assert_eq!(watch_budget(64, 100), 0);
        assert_eq!(watch_budget(9, 0), 0);
    }

    #[test]
    fn the_limit_is_read_whole_and_refused_by_name_when_malformed() {
        let directory =
            std::env::temp_dir().join(format!("sdsync-watch-limit-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("max_user_watches");
        std::fs::write(&file, "524288\n").unwrap();
        assert_eq!(read_watch_limit(&file), Ok(524_288));
        std::fs::write(&file, "0\n").unwrap();
        assert!(read_watch_limit(&file).is_err());
        std::fs::write(&file, "lots\n").unwrap();
        assert!(read_watch_limit(&file).unwrap_err().contains("lots"));
        std::fs::write(&file, "99999999999999999999999\n").unwrap();
        assert_eq!(read_watch_limit(&file), Ok(u64::MAX));
        assert!(read_watch_limit(&directory.join("absent")).is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn every_failure_names_an_existing_backend_reason() {
        // The bridge validates backend_reason against a closed list; these are all on it.
        let known = [
            "watch_limit",
            "watch_unavailable",
            "watch_setup_failed",
            "source_missing",
        ];
        let failures = [
            WatchError::Usage("x".into()),
            WatchError::Unavailable("x".into()),
            WatchError::SourceMissing {
                path: "/s".into(),
                detail: "x".into(),
            },
            WatchError::SourceGone { path: "/s".into() },
            WatchError::Rules("x".into()),
            WatchError::Limit(LimitExceeded {
                dirs: 1,
                budget: Some(0),
                limit: Some(1),
                held: 0,
                refused_at: None,
            }),
            WatchError::Deadline {
                path: "/s".into(),
                deadline: Duration::from_secs(1),
            },
            WatchError::Setup {
                path: "/s".into(),
                detail: "x".into(),
            },
            WatchError::Output("x".into()),
        ];
        let mut codes = std::collections::BTreeSet::new();
        for failure in &failures {
            assert!(known.contains(&failure.reason()), "{failure}");
            assert!(
                failure.to_string().starts_with(failure.reason()),
                "{failure}"
            );
            codes.insert(failure.exit_code());
        }
        // SourceMissing and SourceGone share one status; every other cause has its own.
        assert_eq!(codes.len(), failures.len() - 1);
    }

    #[test]
    fn a_limit_refusal_puts_the_numbers_inside_the_controllers_120_byte_window() {
        let refusal = WatchError::Limit(LimitExceeded {
            dirs: 6553,
            budget: Some(6552),
            limit: Some(8192),
            held: 0,
            refused_at: None,
        })
        .to_string();
        assert!(
            refusal.starts_with("watch_limit dirs=6553 budget=6552 limit=8192 held=0: "),
            "{refusal}"
        );
        assert!(refusal.contains("max_user_watches"), "{refusal}");
        let enospc = WatchError::Limit(LimitExceeded {
            dirs: 40,
            budget: None,
            limit: None,
            held: 3,
            refused_at: Some("/volume1/share/deep".into()),
        })
        .to_string();
        assert!(
            enospc.starts_with("watch_limit dirs=40 limit=unread held=3 at=/volume1/share/deep: "),
            "{enospc}"
        );
        assert!(enospc.contains("ENOSPC"), "{enospc}");
    }

    #[test]
    fn the_coalescer_releases_at_most_one_line_per_interval_and_keeps_overflow() {
        let start = Instant::now();
        let mut coalescer = Coalescer::new(Duration::from_millis(100));
        assert_eq!(coalescer.wait_hint(start), None);
        assert_eq!(coalescer.take(start), None);
        coalescer.note(false);
        assert_eq!(coalescer.wait_hint(start), Some(Duration::ZERO));
        assert_eq!(coalescer.take(start), Some(false));
        // A burst inside the interval is held, and an overflow in it sticks.
        coalescer.note(false);
        coalescer.note(true);
        coalescer.note(false);
        let soon = start + Duration::from_millis(40);
        assert_eq!(coalescer.wait_hint(soon), Some(Duration::from_millis(60)));
        assert_eq!(coalescer.take(soon), None);
        let later = start + Duration::from_millis(100);
        assert_eq!(coalescer.take(later), Some(true));
        assert_eq!(coalescer.take(later + Duration::from_secs(1)), None);
        coalescer.note(false);
        assert_eq!(coalescer.take_now(), Some(false));
        assert_eq!(coalescer.take_now(), None);
    }

    #[test]
    fn the_filter_excludes_dsm_names_and_profile_rules_but_never_the_root() {
        let source =
            std::env::temp_dir().join(format!("sdsync-watch-filter-{}", std::process::id()));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(DEFAULT_IGNORE_FILE), "build/\n").unwrap();
        let filter = Filter::build(&source, vec!["*.tmp".to_owned()]).unwrap();
        assert!(!filter.excludes("", true));
        assert!(filter.excludes("@eaDir", true));
        assert!(filter.excludes("photos/@eaDir", true));
        assert!(filter.excludes("photos/@eaDir/thumb.jpg", false));
        assert!(filter.excludes("#recycle", true));
        assert!(filter.excludes("draft.tmp", false));
        assert!(filter.excludes("build", true));
        assert!(filter.excludes("desktop.ini", false));
        assert!(!filter.excludes("docs/report.pdf", false));
        assert!(!filter.excludes("docs", true));
        assert_eq!(filter.profile_patterns(), 1);
        assert!(Filter::is_rules_file(".sdsyncignore"));
        assert!(!Filter::is_rules_file("docs/.sdsyncignore"));
        // A rebuild reads the ignore file again; an invalid one keeps the previous rules.
        std::fs::write(source.join(DEFAULT_IGNORE_FILE), "").unwrap();
        let rebuilt = filter.rebuilt().unwrap();
        assert!(!rebuilt.excludes("build", true));
        assert!(rebuilt.excludes("draft.tmp", false));
        std::fs::remove_dir_all(&source).unwrap();
        assert!(matches!(
            Filter::build(&source, Vec::new()),
            Err(WatchError::SourceMissing { .. })
        ));
    }

    #[test]
    fn profile_excludes_come_from_the_fragment_and_a_bad_one_is_named() {
        let directory =
            std::env::temp_dir().join(format!("sdsync-watch-fragment-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let fragment = directory.join("alpha.toml");
        std::fs::write(
            &fragment,
            "source = \"/volume1/alpha\"\nexcludes = [\"*.tmp\", \"cache/\"]\n",
        )
        .unwrap();
        assert_eq!(profile_excludes(&fragment).unwrap(), ["*.tmp", "cache/"]);
        std::fs::write(&fragment, "source = \"/volume1/alpha\"\n").unwrap();
        assert!(profile_excludes(&fragment).unwrap().is_empty());
        std::fs::write(&fragment, "excludes = \"*.tmp\"\n").unwrap();
        let failure = profile_excludes(&fragment).unwrap_err();
        assert_eq!(failure.reason(), "watch_setup_failed");
        assert!(failure.to_string().contains("not an array"), "{failure}");
        std::fs::write(&fragment, "excludes = [1]\n").unwrap();
        assert!(profile_excludes(&fragment).is_err());
        std::fs::write(&fragment, "excludes = [\n").unwrap();
        assert!(profile_excludes(&fragment).is_err());
        std::fs::write(&fragment, vec![b'#'; FRAGMENT_LIMIT as usize + 1]).unwrap();
        assert!(
            profile_excludes(&fragment)
                .unwrap_err()
                .to_string()
                .contains("larger")
        );
        assert!(profile_excludes(&directory.join("absent.toml")).is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn the_event_set_reports_what_was_asked_and_always_a_new_directory() {
        let mut events = EventSet::default();
        assert!(events.is_empty());
        for name in ["close_write", "create", "delete", "move"] {
            events.add(name).unwrap();
        }
        assert!(events.reports("close_write"));
        assert!(events.reports("moved_to"));
        assert!(events.reports("new_dir"));
        assert!(!events.reports("attrib"));
        assert!(!events.reports("modify"));
        assert!(!events.reports("something"));
        events.add("attrib").unwrap();
        events.add("modify").unwrap();
        events.add("moved_from").unwrap();
        events.add("moved_to").unwrap();
        assert_eq!(events, EventSet::all());
        assert_eq!(
            events.add("open").unwrap_err().reason(),
            "watch_setup_failed"
        );
    }

    #[test]
    fn join_relative_treats_the_root_as_empty() {
        assert_eq!(join_relative("", "a"), "a");
        assert_eq!(join_relative("a/b", "c"), "a/b/c");
    }
}
