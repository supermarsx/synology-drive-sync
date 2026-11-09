//! How long after a change a file's facts may still not show a later one: the racy windows.
//!
//! A record proven while its file could still change without its facts moving is stored racy and
//! never served. Two mechanisms hide a change that way:
//!
//! - **Timestamp granularity.** Two writes within one clock tick leave one change time. The
//!   original two-second window covered this, and is kept as the floor.
//! - **Shared mappings (D1).** Linux moves mtime and ctime when a store through a `MAP_SHARED`
//!   mapping first dirties a clean page (`page_mkwrite` calls `file_update_time`), and not on
//!   later stores into the page while it stays dirty. The page stays dirty until writeback
//!   cleans it: the flusher wakes every `dirty_writeback_centisecs` and writes back inodes dirty
//!   for longer than `dirty_expire_centisecs`, so a page dirtied at the change time is clean by
//!   the change time plus their sum, and the next store faults again and moves ctime. A record
//!   whose change time is older than that bound (plus a margin) when its read began was read
//!   after every store the change time could hide. ext4 and btrfs both update the times in
//!   `page_mkwrite`; btrfs (DSM's default) writes dirty pages through the same flusher, so its
//!   own 30-second transaction commit does not lengthen the bound.
//!
//! The local window is read from `/proc/sys/vm` once per process. Elsewhere, and when the values
//! cannot be read, [`FALLBACK_WINDOW_NS`] is used: on Linux that is named
//! (`pair-index-writeback-unknown`); on macOS, which exposes no writeback interval to read and
//! writes dirty mapped pages back on its periodic sync, it is the platform's documented value.
//! Periodic writeback switched off (`dirty_writeback_centisecs` = 0) bounds nothing: no local
//! evidence is served or carried (`pair-index-writeback-unbounded`).
//!
//! The NAS side has the same kernel behaviour, but its settings cannot be read from here: DSM
//! ships the kernel defaults (30 s + 5 s), so the NAS window is the same 60 s.

use std::sync::OnceLock;

use super::IndexIssue;

/// The floor of every window: two writes within one coarse clock tick share a change time.
pub const RACY_FLOOR_NS: i64 = 2_000_000_000;
/// Added to the kernel's writeback bound: the flusher's pass over a large dirty backlog on a
/// busy NAS takes time after it wakes, and a page under writeback when a store lands is
/// redirtied, not cleaned. With the kernel defaults (30 s + 5 s) the window is 60 s.
pub const WRITEBACK_MARGIN_NS: i64 = 25_000_000_000;
/// The window when the kernel's writeback settings cannot be read, and on hosts without them:
/// the Linux defaults' bound with the margin.
pub const FALLBACK_WINDOW_NS: i64 = 60_000_000_000;
/// The NAS side's window, in the listing's whole seconds (see the module documentation).
pub const NAS_WINDOW_SECONDS: i64 = 60;
/// How far the NAS clock (`Date` of an `MD5.start`) may stand from the local clock before the
/// remote racy rule stops trusting it (D3). Within it the 60 s NAS window still covers the 35 s
/// writeback bound; beyond it the clock may be a proxy's or simply wrong.
pub const NAS_CLOCK_TOLERANCE_SECONDS: i64 = 20;

#[cfg(target_os = "linux")]
const EXPIRE_PATH: &str = "/proc/sys/vm/dirty_expire_centisecs";
const WRITEBACK_PATH: &str = "/proc/sys/vm/dirty_writeback_centisecs";

/// The windows a run applies, and how the local one was found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RacyWindows {
    /// A local change time within this of the read start makes a record racy.
    pub local_ns: i64,
    /// A NAS change time within this of the `MD5.start` clock makes a record racy.
    pub remote_seconds: i64,
    /// No bound exists on this host: no local evidence is served or carried, and every record is
    /// stored racy.
    pub unbounded: bool,
    /// The named condition when the local window is not the kernel's own bound.
    pub issue: Option<IndexIssue>,
}

impl RacyWindows {
    /// Fixed windows, as a test or an embedding application states them.
    pub fn fixed(local_ns: i64, remote_seconds: i64) -> Self {
        Self {
            local_ns: local_ns.max(RACY_FLOOR_NS),
            remote_seconds: remote_seconds.max(RACY_FLOOR_NS / 1_000_000_000),
            unbounded: false,
            issue: None,
        }
    }

    /// This host's windows: read once per process, so a fallback is named once.
    pub fn for_host() -> Self {
        static HOST: OnceLock<RacyWindows> = OnceLock::new();
        HOST.get_or_init(|| test_override().unwrap_or_else(detect))
            .clone()
    }

    /// The windows from the two writeback settings, in centiseconds, or why each could not be
    /// read as `(path, cause)`.
    pub fn from_writeback_settings(
        expire: Result<u64, (String, String)>,
        writeback: Result<u64, (String, String)>,
    ) -> Self {
        match (expire, writeback) {
            (Ok(_), Ok(0)) => Self {
                local_ns: FALLBACK_WINDOW_NS,
                remote_seconds: NAS_WINDOW_SECONDS,
                unbounded: true,
                issue: Some(IndexIssue::warning(
                    "pair-index-writeback-unbounded",
                    WRITEBACK_PATH,
                    "periodic writeback is switched off (0), so a write through a shared memory \
                     mapping can stay out of the file's times indefinitely; no local evidence is \
                     served and every pair is verified live",
                )),
            },
            (Ok(expire), Ok(writeback)) => {
                let bound_ns = i64::try_from(expire.saturating_add(writeback))
                    .unwrap_or(i64::MAX)
                    .saturating_mul(10_000_000);
                Self {
                    local_ns: bound_ns
                        .saturating_add(WRITEBACK_MARGIN_NS)
                        .max(RACY_FLOOR_NS),
                    remote_seconds: NAS_WINDOW_SECONDS,
                    unbounded: false,
                    issue: None,
                }
            }
            (Err((path, cause)), _) | (_, Err((path, cause))) => Self {
                local_ns: FALLBACK_WINDOW_NS,
                remote_seconds: NAS_WINDOW_SECONDS,
                unbounded: false,
                issue: Some(IndexIssue::warning(
                    "pair-index-writeback-unknown",
                    path,
                    format!(
                        "the kernel's writeback interval could not be read ({cause}), so a pair \
                         is treated as racy for {} s after its last change",
                        FALLBACK_WINDOW_NS / 1_000_000_000
                    ),
                )),
            },
        }
    }
}

#[cfg(target_os = "linux")]
fn detect() -> RacyWindows {
    let read = |path: &str| -> Result<u64, (String, String)> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| (path.to_owned(), format!("{path}: {error}")))?;
        text.trim().parse::<u64>().map_err(|_| {
            (
                path.to_owned(),
                format!("{path} holds {:?}, not a number", text.trim()),
            )
        })
    };
    RacyWindows::from_writeback_settings(read(EXPIRE_PATH), read(WRITEBACK_PATH))
}

/// No writeback interval to read: the fallback, which is this platform's documented window
/// rather than a failure, so nothing is named.
#[cfg(not(target_os = "linux"))]
fn detect() -> RacyWindows {
    RacyWindows {
        local_ns: FALLBACK_WINDOW_NS,
        remote_seconds: NAS_WINDOW_SECONDS,
        unbounded: false,
        issue: None,
    }
}

/// `SDSYNC_TEST_PAIR_INDEX_RACY_WINDOW_MS` shortens both windows, below the floor if asked, so
/// the end-to-end suites need not wait a minute for fixtures to settle. Debug builds only: a
/// release build, which is what ships, never reads it.
#[cfg(debug_assertions)]
fn test_override() -> Option<RacyWindows> {
    let millis = std::env::var("SDSYNC_TEST_PAIR_INDEX_RACY_WINDOW_MS")
        .ok()?
        .parse::<i64>()
        .ok()?
        .max(0);
    Some(RacyWindows {
        local_ns: millis.saturating_mul(1_000_000),
        remote_seconds: (millis.saturating_add(999) / 1_000).max(1),
        unbounded: false,
        issue: None,
    })
}

#[cfg(not(debug_assertions))]
fn test_override() -> Option<RacyWindows> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unread(path: &str) -> Result<u64, (String, String)> {
        Err((path.to_owned(), format!("{path}: Permission denied")))
    }

    #[test]
    fn the_window_is_the_writeback_bound_plus_the_margin_with_a_two_second_floor() {
        let defaults = RacyWindows::from_writeback_settings(Ok(3_000), Ok(500));
        assert_eq!(defaults.local_ns, 60_000_000_000);
        assert!(!defaults.unbounded);
        assert_eq!(defaults.issue, None);
        let long = RacyWindows::from_writeback_settings(Ok(360_000), Ok(500));
        assert_eq!(long.local_ns, 3_605_000_000_000 + WRITEBACK_MARGIN_NS);
        let saturated = RacyWindows::from_writeback_settings(Ok(u64::MAX), Ok(u64::MAX));
        assert_eq!(saturated.local_ns, i64::MAX);
        let fixed = RacyWindows::fixed(0, 0);
        assert_eq!((fixed.local_ns, fixed.remote_seconds), (RACY_FLOOR_NS, 2));
    }

    #[test]
    fn an_unreadable_setting_falls_back_to_sixty_seconds_and_is_named_with_its_path() {
        for windows in [
            RacyWindows::from_writeback_settings(
                unread("/proc/sys/vm/dirty_expire_centisecs"),
                Ok(500),
            ),
            RacyWindows::from_writeback_settings(
                Ok(3_000),
                unread("/proc/sys/vm/dirty_writeback_centisecs"),
            ),
            RacyWindows::from_writeback_settings(
                Err((
                    "/proc/sys/vm/dirty_expire_centisecs".to_owned(),
                    "holds \"x\", not a number".to_owned(),
                )),
                Ok(500),
            ),
        ] {
            assert_eq!(windows.local_ns, FALLBACK_WINDOW_NS);
            assert!(!windows.unbounded);
            let issue = windows.issue.expect("the fallback is named");
            assert_eq!(issue.code, "pair-index-writeback-unknown");
            assert!(issue.path.starts_with("/proc/sys/vm/dirty_"), "{issue:?}");
            assert!(issue.cause.contains("60 s"), "{issue:?}");
        }
    }

    #[test]
    fn periodic_writeback_switched_off_bounds_nothing() {
        let windows = RacyWindows::from_writeback_settings(Ok(3_000), Ok(0));
        assert!(windows.unbounded);
        let issue = windows.issue.expect("named");
        assert_eq!(issue.code, "pair-index-writeback-unbounded");
        assert_eq!(issue.path, "/proc/sys/vm/dirty_writeback_centisecs");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_host_reads_its_own_settings() {
        let windows = detect();
        assert!(windows.local_ns >= RACY_FLOOR_NS);
        // A readable /proc names nothing unless writeback is off.
        if std::fs::read_to_string(WRITEBACK_PATH).is_ok_and(|text| text.trim() != "0")
            && std::fs::read_to_string(EXPIRE_PATH).is_ok()
        {
            assert_eq!(windows.issue, None);
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn hosts_without_writeback_settings_use_the_documented_sixty_seconds() {
        let windows = detect();
        assert_eq!(windows.local_ns, FALLBACK_WINDOW_NS);
        assert_eq!(windows.remote_seconds, NAS_WINDOW_SECONDS);
        assert!(!windows.unbounded);
        assert_eq!(windows.issue, None);
    }
}
