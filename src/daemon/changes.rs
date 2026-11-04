//! The seam between the scheduler and a realtime change source.
//!
//! Phase 1 (`watch`, native inotify) plugs in here by implementing
//! [`ChangeSource`]; this phase ships only the interface, the debounce state
//! ([`DirtySet`]) and a fake for tests. The source never blocks the control
//! thread: `arm` and `disarm` are requests, and everything the source learns
//! (changed directories, overflow, a degraded backend, a vanished source)
//! comes back as a [`ChangeEvent`] message.
//!
//! Debounce and coalescing follow design §2.3: a routine's change is due when
//! it has been quiet for `debounce`, or when it has been pending for the
//! ceiling `min(3600, max(4 x debounce, debounce + 60))`. Everything is on the
//! monotonic clock, so a wall-clock step cannot hold or fire a debounce.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::routines::{Backend, BackendReason, SourceStatus};

/// What a change source reports. Delivered to the control thread as a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChangeEvent {
    /// Something changed in `dir`, relative to the routine's source (`""` is the root).
    Changed { routine: String, dir: String },
    /// The kernel queue overflowed: events were lost. `None` means every routine.
    Overflow { routine: Option<String> },
    /// The routine's backend as the source now runs it, with the existing
    /// vocabulary (`inotify`/`polling`, `ok`/`watch_limit`/…) and a detail naming
    /// numbers and paths (`"120000 directories, limit 65536 (per-user); …"`).
    Backend {
        routine: String,
        backend: Backend,
        reason: BackendReason,
        detail: String,
    },
    /// The routine's source root vanished, became a symlink, or came back.
    Source {
        routine: String,
        status: SourceStatus,
    },
    /// Something the source decided that is worth a log line but changes no
    /// state by itself: why a catch-up run was asked for, a degraded filter.
    /// `warn` for a degradation, info otherwise.
    Notice {
        routine: String,
        warn: bool,
        detail: String,
    },
}

/// What a routine is armed with. A change to any of it re-arms the routine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArmSpec {
    pub(crate) routine: String,
    /// The profile's `source`.
    pub(crate) source: PathBuf,
    /// The profile fragment, whose exclude rules the watch set follows.
    pub(crate) fragment: PathBuf,
    /// The fragment's length and modification time when armed: an edited
    /// exclude list re-arms the routine.
    pub(crate) fragment_stamp: Option<(u64, u128)>,
    /// The routine's `poll` interval, for the polling fallback.
    pub(crate) poll: Duration,
    /// The last successful run (epoch seconds), for the start-up catch-up: a
    /// tree that changed after it while nothing watched it is synced whole.
    pub(crate) last_success_epoch: Option<u64>,
    /// Which arming this is. Events from an older arming are dropped.
    pub(crate) generation: u64,
}

impl ArmSpec {
    /// Whether `other` watches the same thing (everything but the generation
    /// and the catch-up epoch).
    pub(crate) fn same_target(&self, other: &Self) -> bool {
        self.routine == other.routine
            && self.source == other.source
            && self.fragment == other.fragment
            && self.fragment_stamp == other.fragment_stamp
            && self.poll == other.poll
    }
}

/// Where a change source delivers its events: the arming's generation and the
/// event. `false` means the control thread is gone and the source should stop.
pub(crate) type ChangeSink = std::sync::Arc<dyn Fn(u64, ChangeEvent) -> bool + Send + Sync>;

/// Builds the change source once the control thread's channel exists.
pub(crate) type ChangeSourceFactory = Box<dyn FnOnce(ChangeSink) -> Box<dyn ChangeSource> + Send>;

/// A realtime change source. Non-blocking requests: arming, walking and every
/// failure happen on the source's own threads and come back as events.
pub(crate) trait ChangeSource: Send {
    /// Start watching for `spec.routine` (replacing an earlier arming of it);
    /// report the backend as a [`ChangeEvent::Backend`].
    fn arm(&mut self, spec: ArmSpec);
    /// Stop watching for `routine`.
    fn disarm(&mut self, routine: &str);
}

/// The most directories a dirty set names before it collapses to "whole tree".
pub(crate) const DIRTY_CAP: usize = 1024;

/// `min(3600, max(4 x debounce, debounce + 60))`.
pub(crate) fn ceiling(debounce: Duration) -> Duration {
    (debounce * 4)
        .max(debounce + Duration::from_secs(60))
        .min(Duration::from_secs(3600))
}

/// The pending change of one realtime routine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DirtySet {
    pub(crate) dirs: BTreeSet<String>,
    pub(crate) whole_tree: bool,
    pub(crate) first_seen: Option<Instant>,
    pub(crate) last_event: Option<Instant>,
}

impl DirtySet {
    pub(crate) fn is_empty(&self) -> bool {
        !self.whole_tree && self.dirs.is_empty()
    }

    pub(crate) fn mark(&mut self, dir: &str, now: Instant) {
        if !self.whole_tree {
            self.dirs.insert(dir.trim_matches('/').to_owned());
            if self.dirs.len() > DIRTY_CAP {
                self.dirs.clear();
                self.whole_tree = true;
            }
        }
        self.first_seen.get_or_insert(now);
        self.last_event = Some(now);
    }

    pub(crate) fn mark_whole_tree(&mut self, now: Instant) {
        self.dirs.clear();
        self.whole_tree = true;
        self.first_seen.get_or_insert(now);
        self.last_event = Some(now);
    }

    /// Quiet for the debounce, or pending past the ceiling.
    pub(crate) fn due(&self, now: Instant, debounce: Duration) -> bool {
        match (self.first_seen, self.last_event) {
            (Some(first), Some(last)) if !self.is_empty() => {
                now.duration_since(last) >= debounce
                    || now.duration_since(first) >= ceiling(debounce)
            }
            _ => false,
        }
    }

    /// When this set becomes due, if it is pending.
    pub(crate) fn due_at(&self, debounce: Duration) -> Option<Instant> {
        match (self.first_seen, self.last_event) {
            (Some(first), Some(last)) if !self.is_empty() => {
                Some((last + debounce).min(first + ceiling(debounce)))
            }
            _ => None,
        }
    }

    /// The scope to sync: `None` for the whole tree, else the lowest common
    /// ancestor of the dirty directories (`None` too when that is the root).
    pub(crate) fn scope(&self) -> Option<String> {
        if self.whole_tree || self.dirs.is_empty() {
            return None;
        }
        let mut common: Option<Vec<&str>> = None;
        for dir in &self.dirs {
            let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
            common = Some(match common {
                None => parts,
                Some(prefix) => prefix
                    .iter()
                    .zip(&parts)
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| *a)
                    .collect(),
            });
        }
        common
            .filter(|parts| !parts.is_empty())
            .map(|parts| parts.join("/"))
    }

    /// I-3: a dispatched change that did not succeed comes back. The merged set
    /// keeps the earliest first-seen, so the ceiling still bounds it.
    pub(crate) fn merge(&mut self, other: DirtySet) {
        if other.is_empty() {
            return;
        }
        if other.whole_tree {
            self.dirs.clear();
            self.whole_tree = true;
        } else if !self.whole_tree {
            self.dirs.extend(other.dirs);
            if self.dirs.len() > DIRTY_CAP {
                self.dirs.clear();
                self.whole_tree = true;
            }
        }
        self.first_seen = match (self.first_seen, other.first_seen) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.last_event = match (self.last_event, other.last_event) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}

/// `text` cut to at most `max` bytes with its tail kept (S3-12). The detail's
/// last words are the ones the dashboard points at (`next_walk_deadline_seconds=N`
/// and what happens next), so the middle goes, marked with `...`. `text` is the
/// sanitized, ASCII-only line, so every byte is a character (counted in
/// characters, so a stray multi-byte one cannot split).
pub(crate) fn bound_detail(text: &str, max: usize) -> String {
    const GAP: &str = "...";
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max || max <= GAP.len() {
        return chars.into_iter().take(max).collect();
    }
    let head = (max - GAP.len()) / 3;
    let tail = max - GAP.len() - head;
    let mut bounded: String = chars[..head].iter().collect();
    bounded.push_str(GAP);
    bounded.extend(&chars[chars.len() - tail..]);
    bounded
}

/// What a backend report is about, apart from numbers that change on every
/// reading: the detail without its skipped-entries account (`3 entries
/// skipped: a, b, c`), whose count and sample differ from one poll to the next
/// while the cause stays the same.
fn backend_cause(detail: &str) -> &str {
    let at = [" entries skipped: ", " entry skipped: "]
        .iter()
        .filter_map(|marker| detail.find(marker))
        .min();
    match at {
        Some(at) => detail[..at]
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches("; ")
            .trim_end(),
        None => detail,
    }
}

/// What the scheduler logs of one routine's backend reports (S3-12). The change
/// sources send a report whenever their account changes, and a polling tree
/// whose skipped entries differ on every reading changes it on every reading. A
/// run of reports with one backend, reason and cause is one episode: the first
/// is logged whole and the rest are counted; the report that ends the episode
/// carries the count. The latest detail is always in `ctl status`.
#[derive(Debug, Default)]
pub(crate) struct BackendReports {
    last: Option<(Backend, BackendReason, String)>,
    suppressed: u64,
}

/// What to do with one backend report.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Log it. `previous` is the episode it ends, with the reports that
    /// episode did not log.
    Log {
        previous: Option<(Backend, BackendReason, u64)>,
    },
    /// The episode goes on: counted, not logged.
    Suppress,
}

impl BackendReports {
    pub(crate) fn admit(&mut self, backend: Backend, reason: BackendReason, detail: &str) -> Admit {
        let cause = backend_cause(detail);
        if self
            .last
            .as_ref()
            .is_some_and(|(b, r, c)| *b == backend && *r == reason && c == cause)
        {
            self.suppressed += 1;
            return Admit::Suppress;
        }
        let ended = self.last.replace((backend, reason, cause.to_owned()));
        let suppressed = std::mem::take(&mut self.suppressed);
        Admit::Log {
            previous: ended
                .filter(|_| suppressed > 0)
                .map(|(b, r, _)| (b, r, suppressed)),
        }
    }

    /// The routine is re-armed or disarmed: the episode is over. Returns the
    /// reports it did not log, so the caller can say so.
    pub(crate) fn end(&mut self) -> Option<(Backend, BackendReason, u64)> {
        let suppressed = std::mem::take(&mut self.suppressed);
        self.last
            .take()
            .filter(|_| suppressed > 0)
            .map(|(b, r, _)| (b, r, suppressed))
    }
}

/// A change source for tests: records what it was asked, emits nothing by itself.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FakeChangeSource {
    pub(crate) calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(test)]
impl ChangeSource for FakeChangeSource {
    fn arm(&mut self, spec: ArmSpec) {
        self.calls.lock().unwrap().push(format!(
            "arm {} {} gen={}",
            spec.routine,
            spec.source.display(),
            spec.generation
        ));
    }
    fn disarm(&mut self, routine: &str) {
        self.calls.lock().unwrap().push(format!("disarm {routine}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceilings_match_the_shell() {
        assert_eq!(ceiling(Duration::from_secs(1)), Duration::from_secs(61));
        assert_eq!(ceiling(Duration::from_secs(45)), Duration::from_secs(180));
        assert_eq!(
            ceiling(Duration::from_secs(3600)),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn a_change_is_due_after_quiet_or_at_the_ceiling() {
        let t0 = Instant::now();
        let debounce = Duration::from_secs(10);
        let mut set = DirtySet::default();
        assert!(!set.due(t0, debounce));
        set.mark("a/b", t0);
        assert!(!set.due(t0 + Duration::from_secs(9), debounce));
        assert!(set.due(t0 + Duration::from_secs(10), debounce));
        // Events keep arriving every 5 s: only the ceiling (70 s) fires.
        let mut busy = DirtySet::default();
        for n in 0..20 {
            busy.mark("a", t0 + Duration::from_secs(n * 5));
            let now = t0 + Duration::from_secs(n * 5 + 1);
            assert_eq!(busy.due(now, debounce), n * 5 + 1 >= 70, "at {}", n * 5 + 1);
        }
        assert_eq!(set.due_at(debounce), Some(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn the_scope_is_the_lowest_common_ancestor() {
        let now = Instant::now();
        let mut set = DirtySet::default();
        set.mark("Projects/2026/q4", now);
        assert_eq!(set.scope().as_deref(), Some("Projects/2026/q4"));
        set.mark("Projects/2026/q3/deep", now);
        assert_eq!(set.scope().as_deref(), Some("Projects/2026"));
        set.mark("Other", now);
        assert_eq!(set.scope(), None, "the root is a full run");
        let mut whole = DirtySet::default();
        whole.mark_whole_tree(now);
        assert_eq!(whole.scope(), None);
    }

    #[test]
    fn too_many_directories_collapse_to_the_whole_tree() {
        let now = Instant::now();
        let mut set = DirtySet::default();
        for n in 0..=DIRTY_CAP {
            set.mark(&format!("d{n}"), now);
        }
        assert!(set.whole_tree && set.dirs.is_empty());
    }

    #[test]
    fn a_bounded_detail_keeps_its_tail() {
        let text = format!("{}tail=1", "x".repeat(400));
        let bounded = bound_detail(&text, 240);
        assert_eq!(bounded.len(), 240, "{bounded}");
        assert!(
            bounded.ends_with("tail=1") && bounded.contains("..."),
            "{bounded}"
        );
        assert_eq!(bound_detail("short", 240), "short");
        assert_eq!(bound_detail("abcdef", 3), "abc");
    }

    #[test]
    fn the_skip_account_is_not_part_of_the_cause() {
        assert_eq!(
            backend_cause("watch limit; 12 entries skipped: a, b, and 9 more"),
            "watch limit"
        );
        assert_eq!(backend_cause("1 entry skipped: a (PermissionDenied)"), "");
        assert_eq!(backend_cause("watch limit"), "watch limit");
        assert_eq!(
            backend_cause("walk_deadline_exceeded: next_walk_deadline_seconds=240"),
            "walk_deadline_exceeded: next_walk_deadline_seconds=240"
        );
    }

    #[test]
    fn backend_reports_are_counted_per_episode() {
        use Backend::{Inotify, Polling};
        use BackendReason::{Ok as Fine, WatchLimit, WatchSetupFailed};
        let mut reports = BackendReports::default();
        assert_eq!(
            reports.admit(Polling, WatchLimit, "limit; 1 entry skipped: a"),
            Admit::Log { previous: None }
        );
        assert_eq!(
            reports.admit(Polling, WatchLimit, "limit; 2 entries skipped: a, b"),
            Admit::Suppress
        );
        assert_eq!(reports.admit(Polling, WatchLimit, "limit"), Admit::Suppress);
        // A new cause under the same reason is a new episode, and says what the old one hid.
        assert_eq!(
            reports.admit(Polling, WatchLimit, "limit; walk_deadline_exceeded"),
            Admit::Log {
                previous: Some((Polling, WatchLimit, 2))
            }
        );
        assert_eq!(
            reports.admit(Polling, WatchSetupFailed, "limit; walk_deadline_exceeded"),
            Admit::Log { previous: None },
            "an episode that hid nothing says nothing"
        );
        assert_eq!(
            reports.admit(Polling, WatchSetupFailed, "limit; walk_deadline_exceeded"),
            Admit::Suppress
        );
        assert_eq!(
            reports.admit(Inotify, Fine, "3 directories watched"),
            Admit::Log {
                previous: Some((Polling, WatchSetupFailed, 1))
            }
        );
        // A re-arm ends an episode and hands back what it hid.
        assert_eq!(
            reports.admit(Inotify, Fine, "3 directories watched"),
            Admit::Suppress
        );
        assert_eq!(reports.end(), Some((Inotify, Fine, 1)));
        assert_eq!(reports.end(), None);
        assert!(matches!(
            reports.admit(Inotify, Fine, "3 directories watched"),
            Admit::Log { previous: None }
        ));
    }

    #[test]
    fn a_failed_dispatch_merges_back_and_keeps_its_age() {
        let t0 = Instant::now();
        let mut dispatched = DirtySet::default();
        dispatched.mark("a", t0);
        let mut live = DirtySet::default();
        live.mark("b", t0 + Duration::from_secs(5));
        live.merge(dispatched);
        assert_eq!(live.dirs.len(), 2);
        assert_eq!(live.first_seen, Some(t0));
        assert_eq!(live.last_event, Some(t0 + Duration::from_secs(5)));
    }
}
