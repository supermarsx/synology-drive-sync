//! Routine configuration, routine state and the scheduling decisions over them:
//! a port of the controller's `load_*_runtime` loaders, `process_routines`
//! evaluation and outcome bookkeeping, and `routine_retry_delay`
//! (`libexec/sdsync-controller`, `libexec/sdsync-common`).
//!
//! Everything here is pure: inputs are file bytes and facts, outputs are typed
//! records plus [`Effect`]s (log lines, Activity rows, a state save, an
//! escalation) that the control thread applies. Nothing here touches a share.
//!
//! The file formats are read as-is (design §3.1): same keys, same defaults, the
//! first non-empty occurrence wins, unknown keys are ignored, at most 256 lines
//! are read, and the same clamps apply. The routine state is written back with
//! the same 19 keys in the same order (`write_routine_state`), so the bridge's
//! snapshot reader and a rollback shell controller both read it unchanged.

#![forbid(unsafe_code)]

use super::window::{WindowError, WindowNote, WindowState, clamp_clock_step};

/// `dsm_delete_limit`: the largest deletion bound any package file may carry.
pub(crate) const DSM_DELETE_LIMIT: u64 = 2_147_483_647;
/// `realtime_exhausted_cooldown`: how long a realtime routine whose retry ladder
/// ran out waits before its still-pending change is tried again.
pub(crate) const REALTIME_EXHAUSTED_COOLDOWN: u64 = 900;
/// The floor after a cancelled run, a busy refusal or a launch that failed.
pub(crate) const RETRY_FLOOR_SECONDS: u64 = 30;

/// The first non-empty value of each key among the first `max_lines` lines, as
/// the shell's `while IFS= read -r line` loaders read a `key=value` document.
///
/// For any key the shell knows this is exactly its "first occurrence wins"
/// rule: an empty value never marks a key as seen, so a later non-empty one
/// still wins. Values are not trimmed (`read -r` with an empty IFS keeps them).
pub(crate) struct KvDocument {
    pairs: Vec<(String, String)>,
}

impl KvDocument {
    pub(crate) fn parse(bytes: &[u8], max_lines: usize) -> Self {
        let text = String::from_utf8_lossy(bytes);
        let mut pairs: Vec<(String, String)> = Vec::new();
        for line in text.split('\n').take(max_lines) {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if value.is_empty() || pairs.iter().any(|(seen, _)| seen == key) {
                continue;
            }
            pairs.push((key.to_owned(), value.to_owned()));
        }
        Self { pairs }
    }

    pub(crate) fn empty() -> Self {
        Self { pairs: Vec::new() }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(seen, _)| seen == key)
            .map(|(_, value)| value.as_str())
    }

    fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }
}

/// `decimal_at_most CANDIDATE LIMIT`, digit by digit so no width overflows.
pub(crate) fn decimal_at_most(candidate: &str, limit: u64) -> bool {
    if candidate.is_empty() || !candidate.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let trimmed = candidate.trim_start_matches('0');
    let limit = limit.to_string();
    if trimmed.len() != limit.len() {
        return trimmed.len() < limit.len();
    }
    trimmed <= limit.as_str()
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// A shell `[ "$a" -le N ]` over a digit string of any width.
fn digits_le(value: &str, limit: u64) -> bool {
    decimal_at_most(value, limit)
}

/// A shell `[ "$a" -ge N ]` over a digit string of any width.
fn digits_ge(value: &str, floor: u64) -> bool {
    floor == 0 || !decimal_at_most(value, floor - 1)
}

fn parse_u64(value: &str) -> Option<u64> {
    if digits(value) {
        value.parse().ok()
    } else {
        None
    }
}

/// `[A-Za-z0-9_-]+`: a profile or routine name.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `[a-z0-9_]+`: a reason token as the routine state stores it.
pub(crate) fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

// ---------------------------------------------------------------- configuration

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Plan,
    Sync,
}

impl Action {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Sync => "sync",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Interval {
        interval: u64,
    },
    Daily {
        weekdays: String,
        start: String,
        end: String,
    },
    Realtime {
        debounce: u64,
        poll: u64,
    },
}

impl Mode {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Interval { .. } => "interval",
            Self::Daily { .. } => "daily",
            Self::Realtime { .. } => "realtime",
        }
    }
}

/// A routine that passed every check `process_routines` makes before it reads
/// the routine's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Routine {
    pub(crate) action: Action,
    pub(crate) mode: Mode,
    pub(crate) retries: u64,
    /// Already clamped to the five-minute runtime ceiling.
    pub(crate) backoff: u64,
    pub(crate) retry_exponential: bool,
    pub(crate) allow_delete: bool,
    pub(crate) max_total_delete: u64,
    pub(crate) depends_on: String,
}

/// Why a routine cannot be evaluated: the shell's `routine_skip PROFILE FIELD`.
/// Reported once per (routine, field) per daemon lifetime or reload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SkipField {
    ProfileMissing,
    Boolean,
    Action,
    Mode,
    Number,
    RetryCount,
    RetryBackoffSeconds,
    MaxTotalDelete,
    IntervalSeconds,
    Weekdays,
    TimeWindowStart,
    TimeWindowEnd,
    RealtimeTiming,
    DebounceSeconds,
    PollSeconds,
    ProfileMaxDelete,
    TimeWindow,
    /// The routine's conf or profile fragment cannot be read (named
    /// `config_unreadable` with its path and cause), and nothing was read
    /// before: never read as absent (S2-2).
    Unreadable,
}

impl SkipField {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ProfileMissing => "profile_missing",
            Self::Boolean => "boolean",
            Self::Action => "action",
            Self::Mode => "mode",
            Self::Number => "number",
            Self::RetryCount => "retry_count",
            Self::RetryBackoffSeconds => "retry_backoff_seconds",
            Self::MaxTotalDelete => "max_total_delete",
            Self::IntervalSeconds => "interval_seconds",
            Self::Weekdays => "weekdays",
            Self::TimeWindowStart => "time_window_start",
            Self::TimeWindowEnd => "time_window_end",
            Self::RealtimeTiming => "realtime_timing",
            Self::DebounceSeconds => "debounce_seconds",
            Self::PollSeconds => "poll_seconds",
            Self::ProfileMaxDelete => "profile_max_delete",
            Self::TimeWindow => "time_window",
            Self::Unreadable => "unreadable",
        }
    }
}

fn weekdays_shape(value: &str) -> bool {
    !value.is_empty()
        && value
            .split(',')
            .all(|part| part.len() == 1 && (b'1'..=b'7').contains(&part.as_bytes()[0]))
}

/// `^([0-1][0-9]|2[0-3]):[0-5][0-9]$`.
fn clock_shape(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 5
        && b[2] == b':'
        && ((matches!(b[0], b'0' | b'1') && b[1].is_ascii_digit())
            || (b[0] == b'2' && (b'0'..=b'3').contains(&b[1])))
        && (b'0'..=b'5').contains(&b[3])
        && b[4].is_ascii_digit()
}

/// A realtime routine's quiet period when its record names none. The shell
/// controller's `load_routine_config_runtime` default is the same number; it was
/// 45 until the speed round, because a run now costs seconds, not a minute.
pub(crate) const DEFAULT_DEBOUNCE_SECONDS: &str = "15";

/// `load_routine_config_runtime` and the checks `process_routines` makes on it,
/// in the shell's order. `Ok(None)` is a disabled routine (not a skip).
pub(crate) fn parse_routine(bytes: Option<&[u8]>) -> Result<Option<Routine>, SkipField> {
    let doc = bytes.map_or_else(KvDocument::empty, |b| KvDocument::parse(b, 256));
    let enabled = doc.get_or("enabled", "false");
    let action = doc.get_or("action", "sync");
    let mode = doc.get_or("mode", "interval");
    let interval = doc.get_or("interval_seconds", "3600");
    let weekdays = doc.get_or("weekdays", "1,2,3,4,5,6,7");
    let start = doc.get_or("time_window_start", "00:00");
    let end = doc.get_or("time_window_end", "23:59");
    let debounce = doc.get_or("debounce_seconds", DEFAULT_DEBOUNCE_SECONDS);
    let retries = doc.get_or("retry_count", "5");
    let backoff = doc.get_or("retry_backoff_seconds", "60");
    let exponential = doc.get_or("retry_exponential", "true");
    let poll = doc.get_or("poll_seconds", "30");
    let delete = doc.get_or("allow_delete", "false");
    let max_total = doc.get_or("max_total_delete", "100");
    let depends_on = doc.get_or("depends_on", "");

    let boolean = |v: &str| matches!(v, "true" | "false");
    if !boolean(enabled) || !boolean(delete) || !boolean(exponential) {
        return Err(SkipField::Boolean);
    }
    if enabled != "true" {
        return Ok(None);
    }
    let action = match action {
        "plan" => Action::Plan,
        "sync" => Action::Sync,
        _ => return Err(SkipField::Action),
    };
    if !matches!(mode, "interval" | "daily" | "realtime") {
        return Err(SkipField::Mode);
    }
    if !digits(retries) || !digits(backoff) || !digits(max_total) {
        return Err(SkipField::Number);
    }
    if !digits_le(retries, 5) {
        return Err(SkipField::RetryCount);
    }
    if !digits_ge(backoff, 10) {
        return Err(SkipField::RetryBackoffSeconds);
    }
    if !decimal_at_most(max_total, DSM_DELETE_LIMIT) {
        return Err(SkipField::MaxTotalDelete);
    }
    // Older releases accepted a larger base: keep those configs, apply the
    // five-minute runtime ceiling.
    let backoff = if digits_le(backoff, 300) {
        parse_u64(backoff).unwrap_or(300)
    } else {
        300
    };
    let mode = match mode {
        "interval" => {
            if !digits(interval) || !digits_ge(interval, 60) || !digits_le(interval, 2_592_000) {
                return Err(SkipField::IntervalSeconds);
            }
            Mode::Interval {
                interval: parse_u64(interval).ok_or(SkipField::IntervalSeconds)?,
            }
        }
        "daily" => {
            if !weekdays_shape(weekdays) {
                return Err(SkipField::Weekdays);
            }
            if !clock_shape(start) {
                return Err(SkipField::TimeWindowStart);
            }
            if !clock_shape(end) {
                return Err(SkipField::TimeWindowEnd);
            }
            Mode::Daily {
                weekdays: weekdays.to_owned(),
                start: start.to_owned(),
                end: end.to_owned(),
            }
        }
        _ => {
            if !digits(debounce) || !digits(poll) {
                return Err(SkipField::RealtimeTiming);
            }
            if !digits_ge(debounce, 1) || !digits_le(debounce, 3_600) {
                return Err(SkipField::DebounceSeconds);
            }
            if !digits_ge(poll, 5) || !digits_le(poll, 3_600) {
                return Err(SkipField::PollSeconds);
            }
            Mode::Realtime {
                debounce: parse_u64(debounce).ok_or(SkipField::DebounceSeconds)?,
                poll: parse_u64(poll).ok_or(SkipField::PollSeconds)?,
            }
        }
    };
    Ok(Some(Routine {
        action,
        mode,
        retries: parse_u64(retries).unwrap_or(5),
        backoff,
        retry_exponential: exponential == "true",
        allow_delete: delete == "true",
        // Bounded by DSM_DELETE_LIMIT above.
        max_total_delete: parse_u64(max_total).unwrap_or(0),
        depends_on: depends_on.to_owned(),
    }))
}

/// What the scheduler reads from a profile fragment (`load_routine_profile_runtime`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProfileFacts {
    /// `max-delete = N`, `None` when absent, malformed or duplicated.
    pub(crate) max_delete: Option<u64>,
    /// `source = "…"`, `None` when absent.
    pub(crate) source: Option<String>,
}

/// `load_routine_profile_runtime` plus the `profile_max_delete` checks.
///
/// A duplicated `max-delete` or `source` line is refused rather than resolved
/// to its first occurrence (I-2): the routine's delete gate compares against
/// this value, and silently picking one of two is how a weaker bound would be
/// admitted. The caller skips the routine as `profile_max_delete`.
pub(crate) fn parse_profile_facts(bytes: Option<&[u8]>) -> Result<ProfileFacts, SkipField> {
    let Some(bytes) = bytes else {
        return Err(SkipField::ProfileMaxDelete);
    };
    let text = String::from_utf8_lossy(bytes);
    let mut max: Option<String> = None;
    let mut max_seen = false;
    let mut source: Option<String> = None;
    let mut source_seen = false;
    for line in text.split('\n').take(4_096) {
        if let Some(value) = line.strip_prefix("max-delete = ") {
            if max_seen {
                max = None;
                break;
            }
            max_seen = true;
            max = Some(value.to_owned());
        } else if let Some(rest) = line.strip_prefix("source = \"") {
            let Some(value) = rest.strip_suffix('"') else {
                continue;
            };
            if source_seen {
                max = None;
                break;
            }
            source_seen = true;
            source = Some(value.to_owned());
        }
    }
    let max = max.unwrap_or_default();
    if !digits(&max) || !decimal_at_most(&max, DSM_DELETE_LIMIT) {
        return Err(SkipField::ProfileMaxDelete);
    }
    Ok(ProfileFacts {
        max_delete: parse_u64(&max),
        source,
    })
}

/// The routine's own deletion bound is weaker than its profile's: it can never
/// be admitted, and is deferred `delete_bound` (I-2).
pub(crate) fn delete_bound_violated(routine: &Routine, profile_max: u64) -> bool {
    routine.allow_delete && routine.max_total_delete < profile_max
}

/// `routine_retry_delay BASE ATTEMPT EXPONENTIAL`: the whole-action retry delay,
/// fixed or doubling, bounded to five minutes. `None` for arguments the shell
/// refuses (status 64).
pub(crate) fn retry_delay(base: u64, attempt: u64, exponential: bool) -> Option<u64> {
    if base < 10 || !(1..=5).contains(&attempt) {
        return None;
    }
    let mut delay = base.min(300);
    if exponential {
        for _ in 1..attempt {
            if delay >= 150 {
                delay = 300;
                break;
            }
            delay *= 2;
        }
    }
    Some(delay)
}

/// `dependencies_allow LIST`: every named profile's completion record must say
/// `succeeded`. `profile_state` answers the `state=` value of a profile's state
/// file (`setting FILE state never`), or `None` when the file is absent or a
/// symlink.
pub(crate) fn dependencies_allow(
    list: &str,
    profile_state: &dyn Fn(&str) -> Option<String>,
) -> bool {
    if list.is_empty() {
        return true;
    }
    list.split(',').all(|name| {
        valid_name(name) && profile_state(name).is_some_and(|state| state == "succeeded")
    })
}

/// `setting FILE state never` over a profile state document: the first `state=`
/// line, or the default when it is empty or absent.
pub(crate) fn profile_completion_state(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.split('\n')
        .find_map(|line| line.strip_prefix("state="))
        .filter(|value| !value.is_empty())
        .unwrap_or("never")
        .to_owned()
}

// ---------------------------------------------------------------- aggregate schedule

/// `schedule.conf`, normalised the way the controller loop normalises it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Schedule {
    pub(crate) enabled: bool,
    pub(crate) interval: u64,
    pub(crate) allow_delete: bool,
    pub(crate) max_total_delete: u64,
    /// `schedule_invalid field=max_total_delete` was logged by the shell.
    pub(crate) invalid_max_total: bool,
}

pub(crate) fn parse_schedule(bytes: Option<&[u8]>) -> Schedule {
    let doc = bytes.map_or_else(KvDocument::empty, |b| KvDocument::parse(b, 256));
    let mut enabled = doc.get_or("enabled", "false") == "true";
    let allow_delete = doc.get_or("allow_delete", "false") == "true";
    let interval = doc.get_or("interval_seconds", "3600");
    let mut interval = parse_u64(interval).unwrap_or(3_600);
    if !(60..=2_592_000).contains(&interval) {
        interval = 3_600;
    }
    let max = doc.get_or("max_total_delete", "100");
    let mut invalid_max_total = false;
    let max_total_delete = if decimal_at_most(max, DSM_DELETE_LIMIT) {
        parse_u64(max).unwrap_or(100)
    } else {
        enabled = false;
        invalid_max_total = true;
        100
    };
    Schedule {
        enabled,
        interval,
        allow_delete,
        max_total_delete,
        invalid_max_total,
    }
}

/// `schedule.state`: the aggregate's persisted deadline (`load_schedule_deadline`).
pub(crate) fn parse_schedule_deadline(bytes: Option<&[u8]>) -> u64 {
    let Some(bytes) = bytes else { return 0 };
    let text = String::from_utf8_lossy(bytes);
    text.split('\n')
        .find_map(|line| line.strip_prefix("next_run_epoch="))
        .and_then(parse_u64)
        .unwrap_or(0)
}

/// The aggregate deadline at start or reload: keep the persisted one unless the
/// cadence now asks for an earlier one; a deadline that passed while stopped
/// runs a minute after start.
pub(crate) fn schedule_initial_deadline(now: u64, interval: u64, persisted: u64) -> u64 {
    let mut next = now + interval;
    if persisted > 0 {
        if persisted <= now {
            next = now + 60;
        } else if persisted < next {
            next = persisted;
        }
    }
    next
}

/// The aggregate's next deadline and log verdict after a run exit.
pub(crate) fn schedule_after_run(
    now: u64,
    interval: u64,
    exit: i32,
    cancelled: bool,
) -> (u64, Level, &'static str) {
    if cancelled {
        return (now + 60, Level::Info, "cancelled");
    }
    match exit {
        0 => (now + interval, Level::Info, "ok"),
        3 => (now + interval, Level::Warn, "deferred_paths"),
        75 => (now + RETRY_FLOOR_SECONDS, Level::Info, "busy"),
        _ => (now + interval, Level::Error, "failed"),
    }
}

// ---------------------------------------------------------------- routine state

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoutineRunState {
    Never,
    Scheduled,
    Running,
    Succeeded,
    Failed,
    Deferred,
}

impl RoutineRunState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Scheduled => "scheduled",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Deferred => "deferred",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "never" => Self::Never,
            "scheduled" => Self::Scheduled,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "deferred" => Self::Deferred,
            _ => return None,
        })
    }
}

/// The five-value `backend` the bridge validates fail-closed (`api:16611`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    None,
    Interval,
    Daily,
    Inotify,
    Polling,
}

impl Backend {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Interval => "interval",
            Self::Daily => "daily",
            Self::Inotify => "inotify",
            Self::Polling => "polling",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "none" => Self::None,
            "interval" => Self::Interval,
            "daily" => Self::Daily,
            "inotify" => Self::Inotify,
            "polling" => Self::Polling,
            _ => return None,
        })
    }
}

/// The nine-value `backend_reason` vocabulary the bridge validates. The daemon
/// writes no new value (design §2.3, R2): an unknown one fails the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackendReason {
    Ok,
    SourceMissing,
    SourceSymlink,
    WatchUnavailable,
    WatchSetupFailed,
    WatchLimit,
    WatchStopped,
    PollUnreadable,
    PollUnavailable,
}

impl BackendReason {
    pub(crate) const ALL: [Self; 9] = [
        Self::Ok,
        Self::SourceMissing,
        Self::SourceSymlink,
        Self::WatchUnavailable,
        Self::WatchSetupFailed,
        Self::WatchLimit,
        Self::WatchStopped,
        Self::PollUnreadable,
        Self::PollUnavailable,
    ];
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::SourceMissing => "source_missing",
            Self::SourceSymlink => "source_symlink",
            Self::WatchUnavailable => "watch_unavailable",
            Self::WatchSetupFailed => "watch_setup_failed",
            Self::WatchLimit => "watch_limit",
            Self::WatchStopped => "watch_stopped",
            Self::PollUnreadable => "poll_unreadable",
            Self::PollUnavailable => "poll_unavailable",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value)
    }
}

/// `defer_reason`, repaired to `none` when unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeferReason {
    None,
    Window,
    Dependency,
    DeleteBound,
    Source,
    Busy,
}

impl DeferReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Window => "window",
            Self::Dependency => "dependency",
            Self::DeleteBound => "delete_bound",
            Self::Source => "source",
            Self::Busy => "busy",
        }
    }
    fn parse(value: &str) -> Self {
        match value {
            "window" => Self::Window,
            "dependency" => Self::Dependency,
            "delete_bound" => Self::DeleteBound,
            "source" => Self::Source,
            "busy" => Self::Busy,
            _ => Self::None,
        }
    }
}

/// `routines/<p>.state`: the controller's 19 append-only keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutineState {
    pub(crate) state: RoutineRunState,
    pub(crate) next_run: u64,
    pub(crate) last_success: u64,
    pub(crate) last_attempt: u64,
    pub(crate) retry_attempt: u64,
    pub(crate) last_day: String,
    pub(crate) last_poll: u64,
    pub(crate) fingerprint: String,
    pub(crate) backend: Backend,
    pub(crate) backend_reason: BackendReason,
    pub(crate) pending_fingerprint: String,
    pub(crate) pending_since: u64,
    pub(crate) event_first_seen: u64,
    pub(crate) poll_retries: u64,
    pub(crate) defer_reason: DeferReason,
    pub(crate) last_exit: i64,
    pub(crate) last_reason: String,
    pub(crate) partial_streak: u64,
    pub(crate) escalated: u64,
    /// The last instant a change (or a due cadence) was seen for the routine.
    pub(crate) last_trigger: u64,
    /// The last instant a run of the routine finished (a refusal is not a run).
    pub(crate) last_run: u64,
    /// What is looking at the source for the routine right now.
    pub(crate) watching_via: WatchingVia,
    /// Whether anything is observing the routine (the idle-or-blind answer).
    pub(crate) observe: Observe,
    /// Why not, as one token (`none` while observed).
    pub(crate) observe_cause: String,
    /// When the current observation state began (0 while observed).
    pub(crate) observe_since: u64,
}

/// What looks at a routine's source: the dashboard's "watching via".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchingVia {
    /// Native change events (inotify).
    Native,
    /// A stat walk of the tree.
    Poll,
    /// The clock: an interval or daily routine needs no watcher.
    Schedule,
    None,
}

impl WatchingVia {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Poll => "poll",
            Self::Schedule => "schedule",
            Self::None => "none",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "native" => Self::Native,
            "poll" => Self::Poll,
            "schedule" => Self::Schedule,
            "none" => Self::None,
            _ => return None,
        })
    }
}

/// Whether a routine is being observed. `Degraded` still has an observer (a
/// backstop is down, a poll is the only watcher); `Blind` has none; `Disabled`
/// is the user's own switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Observe {
    Ok,
    Degraded,
    Blind,
    Disabled,
}

impl Observe {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Blind => "blind",
            Self::Disabled => "disabled",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ok" => Self::Ok,
            "degraded" => Self::Degraded,
            "blind" => Self::Blind,
            "disabled" => Self::Disabled,
            _ => return None,
        })
    }
}

impl Default for RoutineState {
    fn default() -> Self {
        Self {
            state: RoutineRunState::Never,
            next_run: 0,
            last_success: 0,
            last_attempt: 0,
            retry_attempt: 0,
            last_day: "none".into(),
            last_poll: 0,
            fingerprint: "none".into(),
            backend: Backend::None,
            backend_reason: BackendReason::Ok,
            pending_fingerprint: "0".into(),
            pending_since: 0,
            event_first_seen: 0,
            poll_retries: 0,
            defer_reason: DeferReason::None,
            last_exit: -1,
            last_reason: "none".into(),
            partial_streak: 0,
            escalated: 0,
            last_trigger: 0,
            last_run: 0,
            watching_via: WatchingVia::None,
            observe: Observe::Ok,
            observe_cause: "none".into(),
            observe_since: 0,
        }
    }
}

/// `valid_source_fingerprint`: `v3:` followed by digits and colons.
fn valid_source_fingerprint(value: &str) -> bool {
    value.strip_prefix("v3:").is_some_and(|body| {
        !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit() || b == b':')
    })
}

/// A value safe to splice into a `key=value` line: one printable ASCII line.
fn line_safe(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

impl RoutineState {
    /// `load_routine_state_runtime` plus the validation `process_routines`
    /// applies. `Err` is `routine_state_invalid`: the routine is skipped with a
    /// warning, never evaluated from a guess (I-4).
    pub(crate) fn parse(bytes: Option<&[u8]>) -> Result<Self, &'static str> {
        let mut state = Self::default();
        let Some(bytes) = bytes else {
            return Ok(state);
        };
        let doc = KvDocument::parse(bytes, 256);
        let number = |key: &str, default: u64| -> Result<u64, &'static str> {
            match doc.get(key) {
                None => Ok(default),
                Some(value) => parse_u64(value).ok_or("number"),
            }
        };
        state.state = RoutineRunState::parse(doc.get_or("state", "never")).ok_or("state")?;
        state.next_run = number("next_run_epoch", 0)?;
        state.last_success = number("last_success_epoch", 0)?;
        state.last_attempt = number("last_attempt_epoch", 0)?;
        state.retry_attempt = number("retry_attempt", 0)?;
        state.last_poll = number("last_poll_epoch", 0)?;
        state.pending_since = number("pending_since_epoch", 0)?;
        state.event_first_seen = number("event_first_seen_epoch", 0)?;
        state.poll_retries = number("poll_retries", 0)?;
        let text = |key: &str, default: &str| {
            let value = doc.get_or(key, default);
            if line_safe(value) {
                value.to_owned()
            } else {
                default.to_owned()
            }
        };
        state.last_day = text("last_day", "none");
        state.fingerprint = text("fingerprint", "none");
        // Repaired, never refused: a value the bridge would reject must not be
        // written back, and the shell recomputes it every turn anyway.
        state.backend = Backend::parse(doc.get_or("backend", "none")).unwrap_or(Backend::None);
        state.backend_reason =
            BackendReason::parse(doc.get_or("backend_reason", "ok")).unwrap_or(BackendReason::Ok);
        let pending = doc.get_or("pending_fingerprint", "0");
        state.pending_fingerprint = if pending == "0" || valid_source_fingerprint(pending) {
            pending.to_owned()
        } else {
            "0".into()
        };
        state.defer_reason = DeferReason::parse(doc.get_or("defer_reason", "none"));
        let last_exit = doc.get_or("last_exit_code", "-1");
        state.last_exit = if last_exit == "-1" {
            -1
        } else {
            parse_u64(last_exit)
                .and_then(|value| i64::try_from(value).ok())
                .unwrap_or(-1)
        };
        let last_reason = doc.get_or("last_reason", "none");
        state.last_reason = if valid_token(last_reason) {
            last_reason.to_owned()
        } else {
            "none".into()
        };
        let streak = doc.get_or("partial_streak", "0");
        state.partial_streak = if !digits(streak) {
            0
        } else if streak.len() > 4 {
            9_999
        } else {
            parse_u64(streak).unwrap_or(0)
        };
        state.escalated = doc.get("escalated_epoch").and_then(parse_u64).unwrap_or(0);
        // Additive liveness fields: repaired, never refused, like every field
        // after `partial_streak`. A state from before them reads as observed.
        state.last_trigger = doc
            .get("last_trigger_epoch")
            .and_then(parse_u64)
            .unwrap_or(0);
        state.last_run = doc.get("last_run_epoch").and_then(parse_u64).unwrap_or(0);
        state.watching_via =
            WatchingVia::parse(doc.get_or("watching_via", "none")).unwrap_or(WatchingVia::None);
        state.observe = Observe::parse(doc.get_or("observe_state", "ok")).unwrap_or(Observe::Ok);
        let cause = doc.get_or("observe_cause", "none");
        state.observe_cause = if valid_token(cause) {
            cause.to_owned()
        } else {
            "none".into()
        };
        state.observe_since = doc
            .get("observe_since_epoch")
            .and_then(parse_u64)
            .unwrap_or(0);
        Ok(state)
    }

    /// `write_routine_state`: the same 25 keys in the same order.
    pub(crate) fn render(&self) -> Vec<u8> {
        let token = |value: &str| {
            if valid_token(value) {
                value.to_owned()
            } else {
                "none".into()
            }
        };
        let line = |value: &str, default: &str| {
            if line_safe(value) {
                value.to_owned()
            } else {
                default.to_owned()
            }
        };
        format!(
            "state={}\nnext_run_epoch={}\nlast_success_epoch={}\nlast_attempt_epoch={}\n\
             retry_attempt={}\nlast_day={}\nlast_poll_epoch={}\nfingerprint={}\nbackend={}\n\
             backend_reason={}\npending_fingerprint={}\npending_since_epoch={}\n\
             event_first_seen_epoch={}\npoll_retries={}\n\
             defer_reason={}\nlast_exit_code={}\nlast_reason={}\n\
             partial_streak={}\nescalated_epoch={}\n\
             last_trigger_epoch={}\nlast_run_epoch={}\nwatching_via={}\n\
             observe_state={}\nobserve_cause={}\nobserve_since_epoch={}\n",
            self.state.as_str(),
            self.next_run,
            self.last_success,
            self.last_attempt,
            self.retry_attempt,
            line(&self.last_day, "none"),
            self.last_poll,
            line(&self.fingerprint, "none"),
            self.backend.as_str(),
            self.backend_reason.as_str(),
            line(&self.pending_fingerprint, "0"),
            self.pending_since,
            self.event_first_seen,
            self.poll_retries,
            self.defer_reason.as_str(),
            self.last_exit.max(-1),
            token(&self.last_reason),
            self.partial_streak,
            self.escalated,
            self.last_trigger,
            self.last_run,
            self.watching_via.as_str(),
            self.observe.as_str(),
            token(&self.observe_cause),
            self.observe_since,
        )
        .into_bytes()
    }
}

// ---------------------------------------------------------------- effects

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Something the control thread must do after a decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// A `controller.log` event (`controller_log_event LEVEL EVENT DETAIL scheduler`).
    Log {
        level: Level,
        event: &'static str,
        detail: String,
    },
    /// An Activity row (`controller_activity CODE PROFILE STATE MESSAGE CATEGORY LEVEL`).
    Activity {
        code: &'static str,
        state: &'static str,
        message: String,
        level: Level,
    },
    /// Write `routines/<p>.state` now.
    Save,
    /// The retry ladder ran out on a sync routine: system log and desktop alert,
    /// under the alert policy's cooldown (`routine_escalate_alert`).
    Escalate { exit: i32 },
}

fn log(level: Level, event: &'static str, detail: String) -> Effect {
    Effect::Log {
        level,
        event,
        detail,
    }
}

fn activity(code: &'static str, state: &'static str, message: &str, level: Level) -> Effect {
    Effect::Activity {
        code,
        state,
        message: message.to_owned(),
        level,
    }
}

// ---------------------------------------------------------------- evaluation

/// What the realtime change source says about one realtime routine this turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RealtimeView {
    pub(crate) backend: Backend,
    pub(crate) reason: BackendReason,
    /// A pending change has been quiet for the debounce period, or hit the ceiling.
    pub(crate) due: bool,
}

#[allow(
    dead_code,
    reason = "Missing and Symlink arrive from the Phase-1 change source's root check"
)]
/// The routine's source as the daemon found it (one `symlink_metadata`, done by
/// the caller off the control thread's critical path or with a bounded probe).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceStatus {
    Ok,
    Missing,
    Symlink,
}

/// Everything one evaluation reads besides the routine and its state.
pub(crate) struct Inputs<'a> {
    pub(crate) name: &'a str,
    pub(crate) now: u64,
    pub(crate) profile_max_delete: u64,
    /// The daily window from this turn's instant; `None` for other modes.
    pub(crate) window: Option<Result<WindowState, WindowError>>,
    /// Realtime only.
    pub(crate) realtime: Option<RealtimeView>,
    pub(crate) source: SourceStatus,
    pub(crate) dependencies_ok: &'a dyn Fn(&str) -> bool,
    pub(crate) shutdown: bool,
}

/// The verdict of one evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Not due this turn (resting, deferred, waiting for a deadline).
    Idle,
    /// The routine cannot be evaluated: skip, reported once per field.
    Skip(SkipField),
    /// The local clock could not be read; reported once per episode.
    ClockUnavailable,
    /// Dispatch now. The state was set to `running` (and a Save emitted); the
    /// caller must not start the run unless that save succeeded.
    Dispatch(Dispatch),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dispatch {
    /// The window day the run is for (daily), recorded as `last_day` when it completes.
    pub(crate) window_day: Option<String>,
    /// `new` or `continuing`: a busy episode re-check writes no new Activity row.
    pub(crate) deferral_continuing: bool,
}

/// One routine's turn of `process_routines`, from the clock-step clamp to the
/// dispatch. `state` is updated in place; `effects` collects what to apply.
pub(crate) fn evaluate(
    routine: &Routine,
    state: &mut RoutineState,
    inputs: &Inputs<'_>,
    effects: &mut Vec<Effect>,
) -> Verdict {
    let now = inputs.now;
    let name = inputs.name;
    let previous_reason = state.backend_reason;
    let previous_backend = state.backend;

    // A deadline further ahead than any this scheduler writes is a clock that
    // stepped backwards.
    let horizon = match routine.mode {
        Mode::Interval { interval } if interval > REALTIME_EXHAUSTED_COOLDOWN => interval,
        _ => REALTIME_EXHAUSTED_COOLDOWN,
    };
    let mut stepped = false;
    if let Some(clamped) = clamp_clock_step(state.next_run as i64, now as i64, horizon as i64) {
        effects.push(log(
            Level::Warn,
            "clock_step_detected",
            format!(
                "profile={name} field=next_run_epoch ahead_seconds={}",
                state.next_run - now
            ),
        ));
        state.next_run = clamped as u64;
        stepped = true;
    }
    if clamp_clock_step(state.last_poll as i64, now as i64, 0).is_some() {
        effects.push(log(
            Level::Warn,
            "clock_step_detected",
            format!(
                "profile={name} field=last_poll_epoch ahead_seconds={}",
                state.last_poll - now
            ),
        ));
        state.last_poll = 0;
        stepped = true;
    }
    if stepped {
        effects.push(Effect::Save);
    }

    // A routine still recorded running was interrupted: the daemon that
    // launched it stopped before the outcome could be recorded. The caller never
    // evaluates the routine whose run is in flight.
    if state.state == RoutineRunState::Running {
        state.state = RoutineRunState::Failed;
        state.last_reason = "interrupted".into();
        state.last_exit = -1;
        effects.push(Effect::Save);
        effects.push(activity(
            "run.failed",
            "failed",
            "Routine run was interrupted before its outcome was recorded",
            Level::Warn,
        ));
        effects.push(log(
            Level::Warn,
            "routine_interrupted",
            format!("profile={name}"),
        ));
    }

    let mut window_due = false;
    let mut window_day = None;
    let mut window_note = None;
    let mut window_open = true;
    let window_error = match &inputs.window {
        Some(Ok(window)) => {
            window_open = window.open;
            window_due = window.due;
            window_day = Some(window.day.clone());
            window_note = window.note;
            None
        }
        Some(Err(error)) => Some(*error),
        None => None,
    };

    if delete_bound_violated(routine, inputs.profile_max_delete) {
        // backend is deliberately none: nothing observes a routine that cannot run.
        if state.state != RoutineRunState::Deferred
            || state.defer_reason != DeferReason::DeleteBound
        {
            state.state = RoutineRunState::Deferred;
            state.defer_reason = DeferReason::DeleteBound;
            state.backend = Backend::None;
            state.backend_reason = BackendReason::Ok;
            effects.push(Effect::Save);
            effects.push(activity(
                "routine.deferred",
                "deferred",
                "Routine cannot run: its deletion bound is weaker than the profile's max-delete",
                Level::Warn,
            ));
            effects.push(log(
                Level::Warn,
                "routine_deferred",
                format!("profile={name} reason=delete_bound"),
            ));
        }
        return Verdict::Idle;
    }
    match window_error {
        Some(WindowError::InvalidTime) => return Verdict::Skip(SkipField::TimeWindow),
        Some(WindowError::ClockUnavailable) => return Verdict::ClockUnavailable,
        None => {}
    }
    if !window_open {
        // Recorded once, on entry; a routine whose last run succeeded or failed
        // keeps showing that outcome.
        let keep = matches!(
            state.state,
            RoutineRunState::Succeeded | RoutineRunState::Failed
        ) || (state.state == RoutineRunState::Deferred
            && state.defer_reason == DeferReason::Window);
        if !keep {
            state.state = RoutineRunState::Deferred;
            state.defer_reason = DeferReason::Window;
            state.backend = Backend::Daily;
            state.backend_reason = BackendReason::Ok;
            effects.push(Effect::Save);
            effects.push(activity(
                "routine.deferred",
                "deferred",
                "Routine outside configured window",
                Level::Info,
            ));
        }
        return Verdict::Idle;
    }

    let mut due = false;
    // A busy package is re-checked on its own short deadline; a daily routine
    // has no deadline of its own, so next_run_epoch is only a not-before floor.
    let busy_floor = state.state == RoutineRunState::Deferred
        && state.next_run > now
        && state.defer_reason == DeferReason::Busy;
    let daily_floor = matches!(routine.mode, Mode::Daily { .. })
        && state.retry_attempt == 0
        && state.next_run > now;
    if busy_floor || daily_floor {
        return Verdict::Idle;
    } else if state.retry_attempt > 0 {
        // A pending whole-action retry owns admission until its deadline.
        if state.next_run == 0 {
            effects.push(log(
                Level::Warn,
                "routine_state_invalid",
                format!("profile={name} field=retry_deadline"),
            ));
            return Verdict::Idle;
        } else if now >= state.next_run {
            due = true;
        } else {
            return Verdict::Idle;
        }
    } else {
        match &routine.mode {
            Mode::Interval { interval } => {
                state.backend = Backend::Interval;
                state.backend_reason = BackendReason::Ok;
                if state.next_run == 0 {
                    state.next_run = now + interval;
                    state.state = RoutineRunState::Scheduled;
                    effects.push(Effect::Save);
                } else if now >= state.next_run {
                    due = true;
                }
            }
            Mode::Daily { .. } => {
                state.backend = Backend::Daily;
                state.backend_reason = BackendReason::Ok;
                due = window_due;
            }
            Mode::Realtime { .. } => {
                if inputs.source != SourceStatus::Ok {
                    let (reason, word) = match inputs.source {
                        SourceStatus::Symlink => (BackendReason::SourceSymlink, "symlink"),
                        _ => (BackendReason::SourceMissing, "missing"),
                    };
                    let transition = state.state != RoutineRunState::Deferred
                        || state.defer_reason != DeferReason::Source
                        || reason != previous_reason;
                    state.backend_reason = reason;
                    state.backend = Backend::None;
                    state.state = RoutineRunState::Deferred;
                    state.defer_reason = DeferReason::Source;
                    if transition {
                        effects.push(Effect::Save);
                        effects.push(activity(
                            "routine.deferred",
                            "deferred",
                            "Routine source is unavailable",
                            Level::Warn,
                        ));
                        effects.push(log(
                            Level::Warn,
                            "routine_source_unavailable",
                            format!("profile={name} reason={word}"),
                        ));
                    }
                    return Verdict::Idle;
                }
                let view = inputs.realtime.unwrap_or(RealtimeView {
                    backend: Backend::Polling,
                    reason: BackendReason::WatchUnavailable,
                    due: false,
                });
                state.backend = view.backend;
                state.backend_reason = view.reason;
                due = view.due;
            }
        }
    }
    if (state.backend, state.backend_reason) != (previous_backend, previous_reason)
        && !effects.contains(&Effect::Save)
    {
        // The degraded reason is reported on transition, not every turn.
        effects.push(Effect::Save);
    }
    if !due {
        return Verdict::Idle;
    }
    // A realtime routine has no cadence of its own, so next_run_epoch is its
    // not-before instant (floors, cool-downs). A suppressed dispatch keeps its
    // pending change.
    if matches!(routine.mode, Mode::Realtime { .. })
        && state.retry_attempt == 0
        && state.next_run > now
    {
        return Verdict::Idle;
    }
    if !(inputs.dependencies_ok)(&routine.depends_on) {
        let transition = state.state != RoutineRunState::Deferred
            || state.defer_reason != DeferReason::Dependency;
        state.state = RoutineRunState::Deferred;
        state.defer_reason = DeferReason::Dependency;
        if let Mode::Interval { .. } = routine.mode {
            state.next_run = now + RETRY_FLOOR_SECONDS;
            effects.push(Effect::Save);
        } else if transition {
            effects.push(Effect::Save);
        }
        if transition {
            effects.push(activity(
                "routine.deferred",
                "deferred",
                "Routine dependency has not succeeded",
                Level::Warn,
            ));
            effects.push(log(
                Level::Info,
                "routine_deferred",
                format!("profile={name} reason=dependency"),
            ));
        }
        return Verdict::Idle;
    }
    // I-12: no new work once shutdown is latched.
    if inputs.shutdown {
        return Verdict::Idle;
    }
    let deferral_continuing =
        state.state == RoutineRunState::Deferred && state.defer_reason == DeferReason::Busy;
    let window_day = if let Mode::Daily { start, end, .. } = &routine.mode {
        if window_note == Some(WindowNote::DstGap) {
            effects.push(log(
                Level::Warn,
                "routine_window_dst_gap",
                format!(
                    "profile={name} window_day={} start={start} end={end}",
                    window_day.as_deref().unwrap_or("none")
                ),
            ));
        }
        window_day
    } else {
        None
    };
    state.state = RoutineRunState::Running;
    state.last_attempt = now;
    // A dispatch is a trigger: the change (or the cadence) that made it due.
    state.last_trigger = now;
    if !effects.contains(&Effect::Save) {
        effects.push(Effect::Save);
    }
    Verdict::Dispatch(Dispatch {
        window_day,
        deferral_continuing,
    })
}

/// How a run ended, as the executor names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunEnd {
    /// The child's exit code; signals are `128 + signo`. `-1` when it never started.
    pub(crate) exit: i32,
    /// An operator or the daemon's own stop cancelled it (TERM, 130/143, or a
    /// KILL the daemon sent after the cancel grace).
    pub(crate) cancelled: bool,
    /// The failure reason token for a failed run (`core_failed`, `run_stalled`, …).
    pub(crate) reason: String,
    /// The run was refused before it was admitted (`admission_refused`).
    pub(crate) admission_refused: bool,
}

/// The outcome bookkeeping after a routine run (the tail of `process_routines`).
///
/// `record_day` is the window day the dispatch was for, or today for a daily
/// routine dispatched without one (`${routine_dispatch_day:-$(date +%Y-%m-%d)}`).
pub(crate) fn apply_outcome(
    routine: &Routine,
    state: &mut RoutineState,
    name: &str,
    end: &RunEnd,
    finished: u64,
    record_day: &str,
    effects: &mut Vec<Effect>,
) {
    let exit = end.exit;
    // A refusal never ran; every other end is the last run (any outcome).
    if !end.admission_refused {
        state.last_run = finished;
    }
    state.last_exit = i64::from(exit).max(-1);
    state.defer_reason = DeferReason::None;
    let close_day = |state: &mut RoutineState| {
        state.last_day = record_day.to_owned();
        state.next_run = 0;
    };
    if exit == 0 && !end.cancelled {
        state.state = RoutineRunState::Succeeded;
        state.last_reason = "ok".into();
        state.last_success = finished;
        state.retry_attempt = 0;
        state.partial_streak = 0;
        match routine.mode {
            Mode::Interval { interval } => state.next_run = finished + interval,
            Mode::Daily { .. } => close_day(state),
            Mode::Realtime { .. } => state.next_run = 0,
        }
    } else if exit == 3 && !end.cancelled {
        // Partial: finished with named paths deferred. A completion for the
        // cadence and for dependents; consumes no retry. A realtime routine keeps
        // its change pending (the loop merges the dirty set back, I-3).
        state.state = RoutineRunState::Succeeded;
        state.last_reason = "deferred_paths".into();
        state.last_success = finished;
        state.retry_attempt = 0;
        match routine.mode {
            Mode::Interval { interval } => state.next_run = finished + interval,
            Mode::Daily { .. } => close_day(state),
            Mode::Realtime { .. } => {
                state.partial_streak = state.partial_streak.saturating_add(1);
                let delay = if state.partial_streak <= 5 {
                    retry_delay(
                        routine.backoff,
                        state.partial_streak,
                        routine.retry_exponential,
                    )
                    .unwrap_or(routine.backoff)
                } else {
                    REALTIME_EXHAUSTED_COOLDOWN
                };
                state.next_run = finished + delay;
                if state.partial_streak == 6 {
                    effects.push(activity(
                        "routine.retries_exhausted",
                        "deferred",
                        &format!(
                            "Routine deferred paths 6 runs in a row; it now re-runs every {} minutes until they sync",
                            REALTIME_EXHAUSTED_COOLDOWN / 60
                        ),
                        Level::Warn,
                    ));
                    effects.push(log(
                        Level::Warn,
                        "routine_partial_backoff",
                        format!(
                            "profile={name} runs={} next={}",
                            state.partial_streak, state.next_run
                        ),
                    ));
                }
            }
        }
    } else if exit == 75 && !end.cancelled {
        // Refused, not failed: consumes no retry, re-checked after the floor.
        state.state = RoutineRunState::Deferred;
        state.defer_reason = DeferReason::Busy;
        state.last_reason = if end.admission_refused {
            "admission_refused".into()
        } else {
            "busy".into()
        };
        state.next_run = finished + RETRY_FLOOR_SECONDS;
    } else if end.reason == "core_oom_killed" {
        // A lasting, user-fixable condition (daemon/lasting.rs): retrying only
        // repeats the kill. Failed, honestly; no ladder and no escalation; the
        // control thread holds every automatic run of the profile until Run
        // now or a settings change, and the cadence moves on as after a run.
        state.state = RoutineRunState::Failed;
        state.last_reason = end.reason.clone();
        state.retry_attempt = 0;
        state.partial_streak = 0;
        match routine.mode {
            Mode::Interval { interval } => state.next_run = finished + interval,
            Mode::Daily { .. } => close_day(state),
            Mode::Realtime { .. } => state.next_run = 0,
        }
        effects.push(activity(
            "routine.deferred",
            "deferred",
            "Routine stopped: the sync core was killed by the out-of-memory killer. It does not run again on its own until you click Run now or change this profile's settings or the performance level",
            Level::Warn,
        ));
    } else if end.cancelled {
        // Consumes no attempt and does not burn a daily last_day (I-5: failed,
        // never succeeded). An owed retry keeps its count; the floor bounds its
        // deadline from below.
        state.state = RoutineRunState::Failed;
        state.last_reason = "cancelled".into();
        let floor = finished + RETRY_FLOOR_SECONDS;
        if state.retry_attempt == 0 || state.next_run < floor {
            state.next_run = floor;
        }
    } else {
        state.state = RoutineRunState::Failed;
        state.partial_streak = 0;
        state.last_reason = if valid_token(&end.reason) {
            end.reason.clone()
        } else {
            "core_failed".into()
        };
        if state.retry_attempt < routine.retries {
            state.retry_attempt += 1;
            match retry_delay(
                routine.backoff,
                state.retry_attempt,
                routine.retry_exponential,
            ) {
                Some(delay) => {
                    state.next_run = finished + delay;
                    effects.push(activity(
                        "routine.retry_scheduled",
                        "scheduled",
                        &format!("Routine retry scheduled in {delay}s"),
                        Level::Info,
                    ));
                }
                None => effects.push(log(
                    Level::Warn,
                    "routine_invalid",
                    format!("profile={name} field=retry_delay"),
                )),
            }
        } else {
            state.retry_attempt = 0;
            match routine.mode {
                Mode::Interval { interval } => state.next_run = finished + interval,
                Mode::Daily { .. } => close_day(state),
                Mode::Realtime { .. } => state.next_run = finished + REALTIME_EXHAUSTED_COOLDOWN,
            }
            if routine.retries > 0 {
                effects.push(activity(
                    "routine.retries_exhausted",
                    "failed",
                    &format!(
                        "Routine failed {} times in a row ({}); retrying stopped until its next scheduled attempt",
                        routine.retries + 1,
                        state.last_reason
                    ),
                    Level::Error,
                ));
                if routine.action == Action::Sync {
                    effects.push(Effect::Escalate { exit });
                }
                effects.push(log(
                    Level::Error,
                    "routine_retries_exhausted",
                    format!(
                        "profile={name} attempts={} exit={exit} reason={}",
                        routine.retries + 1,
                        state.last_reason
                    ),
                ));
            }
        }
    }
    effects.push(Effect::Save);
    let level = if end.cancelled || matches!(exit, 0 | 75 | 130 | 143) {
        Level::Info
    } else if exit == 3 {
        Level::Warn
    } else {
        Level::Error
    };
    effects.push(log(
        level,
        "routine_run",
        format!(
            "profile={name} action={} exit={exit} next={} backend={} reason={}",
            routine.action.as_str(),
            state.next_run,
            state.backend.as_str(),
            state.last_reason
        ),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routine(conf: &str) -> Result<Option<Routine>, SkipField> {
        parse_routine(Some(conf.as_bytes()))
    }

    #[test]
    fn kv_documents_read_like_the_shell_loaders() {
        let doc = KvDocument::parse(b"a=\na=1\na=2\nb\n=x\nc=d=e\n", 256);
        assert_eq!(
            doc.get("a"),
            Some("1"),
            "an empty value never marks a key seen"
        );
        assert_eq!(doc.get("b"), None);
        assert_eq!(doc.get(""), Some("x"));
        assert_eq!(doc.get("c"), Some("d=e"));
        let mut long = "x=1\n".repeat(256);
        long.push_str("late=1\n");
        assert_eq!(KvDocument::parse(long.as_bytes(), 256).get("late"), None);
        // A final line without its newline still counts.
        assert_eq!(KvDocument::parse(b"k=v", 256).get("k"), Some("v"));
        // read -r keeps a carriage return, which then fails the value's check.
        assert_eq!(KvDocument::parse(b"k=v\r\n", 256).get("k"), Some("v\r"));
    }

    #[test]
    fn decimal_bounds_never_overflow() {
        assert!(decimal_at_most("2147483647", DSM_DELETE_LIMIT));
        assert!(!decimal_at_most("2147483648", DSM_DELETE_LIMIT));
        assert!(decimal_at_most("0002147483647", DSM_DELETE_LIMIT));
        assert!(!decimal_at_most(
            "99999999999999999999999",
            DSM_DELETE_LIMIT
        ));
        assert!(decimal_at_most("0", 0));
        assert!(!decimal_at_most("", 5));
        assert!(!decimal_at_most("-1", 5));
        assert!(digits_ge("99999999999999999999999", 10));
        assert!(!digits_ge("9", 10));
    }

    #[test]
    fn defaults_are_the_shells_and_disabled_is_not_a_skip() {
        assert_eq!(parse_routine(None), Ok(None));
        let parsed = routine("enabled=true\n").unwrap().unwrap();
        assert_eq!(parsed.action, Action::Sync);
        assert_eq!(parsed.mode, Mode::Interval { interval: 3600 });
        assert_eq!(parsed.retries, 5);
        assert_eq!(parsed.backoff, 60);
        assert!(parsed.retry_exponential);
        assert!(!parsed.allow_delete);
        assert_eq!(parsed.max_total_delete, 100);
        assert_eq!(parsed.depends_on, "");
    }

    #[test]
    fn every_invalid_field_is_named_in_the_shells_order() {
        let cases: &[(&str, SkipField)] = &[
            ("enabled=yes\n", SkipField::Boolean),
            ("enabled=false\nallow_delete=1\n", SkipField::Boolean),
            ("enabled=true\naction=push\n", SkipField::Action),
            ("enabled=true\nmode=hourly\n", SkipField::Mode),
            ("enabled=true\nretry_count=x\n", SkipField::Number),
            ("enabled=true\nretry_count=6\n", SkipField::RetryCount),
            (
                "enabled=true\nretry_backoff_seconds=9\n",
                SkipField::RetryBackoffSeconds,
            ),
            (
                "enabled=true\nmax_total_delete=2147483648\n",
                SkipField::MaxTotalDelete,
            ),
            (
                "enabled=true\ninterval_seconds=59\n",
                SkipField::IntervalSeconds,
            ),
            (
                "enabled=true\ninterval_seconds=2592001\n",
                SkipField::IntervalSeconds,
            ),
            (
                "enabled=true\nmode=daily\nweekdays=1,8\n",
                SkipField::Weekdays,
            ),
            (
                "enabled=true\nmode=daily\nweekdays=,1\n",
                SkipField::Weekdays,
            ),
            (
                "enabled=true\nmode=daily\ntime_window_start=24:00\n",
                SkipField::TimeWindowStart,
            ),
            (
                "enabled=true\nmode=daily\ntime_window_end=1:00\n",
                SkipField::TimeWindowEnd,
            ),
            (
                "enabled=true\nmode=realtime\ndebounce_seconds=x\n",
                SkipField::RealtimeTiming,
            ),
            (
                "enabled=true\nmode=realtime\ndebounce_seconds=0\n",
                SkipField::DebounceSeconds,
            ),
            (
                "enabled=true\nmode=realtime\ndebounce_seconds=3601\n",
                SkipField::DebounceSeconds,
            ),
            (
                "enabled=true\nmode=realtime\npoll_seconds=4\n",
                SkipField::PollSeconds,
            ),
        ];
        for (conf, field) in cases {
            assert_eq!(routine(conf), Err(*field), "{conf}");
        }
        // An empty value is skipped by the loader, so the default stands.
        assert!(
            routine("enabled=true\nmode=daily\nweekdays=\n")
                .unwrap()
                .is_some()
        );
        // A disabled routine with a bad action is just disabled (the shell checks
        // booleans, then enabled, then the rest).
        assert_eq!(routine("enabled=false\naction=push\n"), Ok(None));
    }

    #[test]
    fn backoff_is_clamped_and_modes_carry_their_fields() {
        let parsed = routine("enabled=true\nretry_backoff_seconds=900\n")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.backoff, 300);
        let daily = routine(
            "enabled=true\nmode=daily\nweekdays=1,3,5\ntime_window_start=22:00\ntime_window_end=02:00\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            daily.mode,
            Mode::Daily {
                weekdays: "1,3,5".into(),
                start: "22:00".into(),
                end: "02:00".into()
            }
        );
        let realtime =
            routine("enabled=true\nmode=realtime\ndebounce_seconds=1\npoll_seconds=3600\n")
                .unwrap()
                .unwrap();
        assert_eq!(
            realtime.mode,
            Mode::Realtime {
                debounce: 1,
                poll: 3600
            }
        );
        // A record that names no debounce gets the default (15 s, down from 45);
        // one that names a value keeps it, whatever it is.
        let defaulted = routine("enabled=true\nmode=realtime\n").unwrap().unwrap();
        assert_eq!(
            defaulted.mode,
            Mode::Realtime {
                debounce: 15,
                poll: 30
            }
        );
        for kept in [45, 90] {
            let named = routine(&format!(
                "enabled=true\nmode=realtime\ndebounce_seconds={kept}\n"
            ))
            .unwrap()
            .unwrap();
            assert_eq!(
                named.mode,
                Mode::Realtime {
                    debounce: kept,
                    poll: 30
                }
            );
        }
        // First occurrence wins, unknown keys are ignored.
        let first = routine("enabled=true\nmode=daily\nmode=interval\nfull_every_seconds=1\n")
            .unwrap()
            .unwrap();
        assert!(matches!(first.mode, Mode::Daily { .. }));
    }

    #[test]
    fn profile_facts_refuse_duplicates_and_missing_bounds() {
        let ok = parse_profile_facts(Some(b"source = \"/volume1/a\"\nmax-delete = 50\n")).unwrap();
        assert_eq!(ok.max_delete, Some(50));
        assert_eq!(ok.source.as_deref(), Some("/volume1/a"));
        for bad in [
            &b"source = \"/volume1/a\"\n"[..],
            b"max-delete = 5\nmax-delete = 50\n",
            b"max-delete = 5\nsource = \"/a\"\nsource = \"/b\"\n",
            b"max-delete = x\n",
            b"max-delete = 2147483648\n",
        ] {
            assert_eq!(
                parse_profile_facts(Some(bad)),
                Err(SkipField::ProfileMaxDelete),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(parse_profile_facts(None), Err(SkipField::ProfileMaxDelete));
    }

    /// I-2: the routine's own bound must be at least the profile's.
    #[test]
    fn no_delete_without_allow_delete_and_bound() {
        let mut r = routine("enabled=true\nallow_delete=true\nmax_total_delete=10\n")
            .unwrap()
            .unwrap();
        assert!(delete_bound_violated(&r, 11));
        assert!(!delete_bound_violated(&r, 10));
        r.allow_delete = false;
        assert!(
            !delete_bound_violated(&r, 1_000),
            "no deletion, no bound to check"
        );
    }

    /// The `routine_retry_delay` table.
    #[test]
    fn retry_ladder_matches_the_shell_table() {
        let table: &[(u64, u64, bool, Option<u64>)] = &[
            (10, 1, true, Some(10)),
            (10, 2, true, Some(20)),
            (10, 3, true, Some(40)),
            (10, 4, true, Some(80)),
            (10, 5, true, Some(160)),
            (60, 1, true, Some(60)),
            (60, 2, true, Some(120)),
            (60, 3, true, Some(240)),
            (60, 4, true, Some(300)),
            (60, 5, true, Some(300)),
            (150, 2, true, Some(300)),
            (149, 2, true, Some(298)),
            (149, 3, true, Some(300)),
            (300, 1, true, Some(300)),
            (900, 1, false, Some(300)),
            (60, 5, false, Some(60)),
            (9, 1, true, None),
            (60, 0, true, None),
            (60, 6, true, None),
        ];
        for (base, attempt, exponential, expected) in table {
            assert_eq!(
                retry_delay(*base, *attempt, *exponential),
                *expected,
                "base={base} attempt={attempt} exponential={exponential}"
            );
        }
    }

    #[test]
    fn dependencies_need_every_profile_succeeded() {
        let states = |name: &str| match name {
            "a" => Some("succeeded".to_owned()),
            "b" => Some("failed".to_owned()),
            _ => None,
        };
        assert!(dependencies_allow("", &states));
        assert!(dependencies_allow("a", &states));
        assert!(!dependencies_allow("a,b", &states));
        assert!(!dependencies_allow("a,missing", &states));
        assert!(!dependencies_allow("a,", &states));
        assert!(!dependencies_allow("../a", &states));
        assert_eq!(
            profile_completion_state(b"x=1\nstate=succeeded\n"),
            "succeeded"
        );
        assert_eq!(profile_completion_state(b"state=\n"), "never");
    }

    #[test]
    fn routine_state_round_trips_in_the_shells_key_order() {
        let mut state = RoutineState::default();
        assert_eq!(
            String::from_utf8(state.render()).unwrap(),
            "state=never\nnext_run_epoch=0\nlast_success_epoch=0\nlast_attempt_epoch=0\n\
             retry_attempt=0\nlast_day=none\nlast_poll_epoch=0\nfingerprint=none\nbackend=none\n\
             backend_reason=ok\npending_fingerprint=0\npending_since_epoch=0\n\
             event_first_seen_epoch=0\npoll_retries=0\ndefer_reason=none\nlast_exit_code=-1\n\
             last_reason=none\npartial_streak=0\nescalated_epoch=0\n\
             last_trigger_epoch=0\nlast_run_epoch=0\nwatching_via=none\n\
             observe_state=ok\nobserve_cause=none\nobserve_since_epoch=0\n"
        );
        state.state = RoutineRunState::Failed;
        state.next_run = 1_800_000_000;
        state.last_day = "2026-11-03".into();
        state.backend = Backend::Inotify;
        state.backend_reason = BackendReason::WatchLimit;
        state.last_exit = 137;
        state.last_reason = "run_stalled".into();
        state.pending_fingerprint = "v3:1:2".into();
        let rendered = state.render();
        assert_eq!(RoutineState::parse(Some(&rendered)).unwrap(), state);
    }

    #[test]
    fn routine_state_repairs_additive_fields_and_refuses_core_ones() {
        let repaired = RoutineState::parse(Some(
            b"state=failed\nbackend=bogus\nbackend_reason=new_reason\ndefer_reason=nope\n\
              last_exit_code=x\nlast_reason=Bad Reason\npartial_streak=123456\n\
              pending_fingerprint=v2:abc\nescalated_epoch=-3\n",
        ))
        .unwrap();
        assert_eq!(repaired.backend, Backend::None);
        assert_eq!(repaired.backend_reason, BackendReason::Ok);
        assert_eq!(repaired.defer_reason, DeferReason::None);
        assert_eq!(repaired.last_exit, -1);
        assert_eq!(repaired.last_reason, "none");
        assert_eq!(repaired.partial_streak, 9_999);
        assert_eq!(repaired.pending_fingerprint, "0");
        assert_eq!(repaired.escalated, 0);
        for bad in [
            &b"state=paused\n"[..],
            b"next_run_epoch=soon\n",
            b"retry_attempt=-1\n",
            b"poll_retries=1x\n",
        ] {
            assert!(RoutineState::parse(Some(bad)).is_err());
        }
    }

    #[test]
    fn schedule_normalises_like_the_controller_loop() {
        let s = parse_schedule(Some(
            b"enabled=true\ninterval_seconds=30\nmax_total_delete=7\n",
        ));
        assert!(s.enabled);
        assert_eq!(s.interval, 3600);
        assert_eq!(s.max_total_delete, 7);
        let bad = parse_schedule(Some(b"enabled=true\nmax_total_delete=x\n"));
        assert!(!bad.enabled && bad.invalid_max_total);
        assert_eq!(bad.max_total_delete, 100);
        assert!(!parse_schedule(None).enabled);
        assert_eq!(parse_schedule_deadline(Some(b"next_run_epoch=123\n")), 123);
        assert_eq!(parse_schedule_deadline(Some(b"next_run_epoch=x\n")), 0);
        // A start keeps an earlier persisted deadline; a passed one runs at +60.
        assert_eq!(schedule_initial_deadline(1_000, 3_600, 0), 4_600);
        assert_eq!(schedule_initial_deadline(1_000, 3_600, 2_000), 2_000);
        assert_eq!(schedule_initial_deadline(1_000, 3_600, 900), 1_060);
        assert_eq!(schedule_initial_deadline(1_000, 3_600, 9_000), 4_600);
        assert_eq!(schedule_after_run(100, 3_600, 75, false).0, 130);
        assert_eq!(
            schedule_after_run(100, 3_600, 143, true),
            (160, Level::Info, "cancelled")
        );
        assert_eq!(schedule_after_run(100, 3_600, 3, false).2, "deferred_paths");
    }

    // ------------------------------------------------------------ evaluation

    fn no_deps(_: &str) -> bool {
        true
    }

    fn inputs<'a>(now: u64, deps: &'a dyn Fn(&str) -> bool) -> Inputs<'a> {
        Inputs {
            name: "photos",
            now,
            profile_max_delete: 100,
            window: None,
            realtime: None,
            source: SourceStatus::Ok,
            dependencies_ok: deps,
            shutdown: false,
        }
    }

    fn interval_routine() -> Routine {
        routine("enabled=true\ninterval_seconds=600\nretry_count=2\nretry_backoff_seconds=10\n")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn an_interval_routine_schedules_then_dispatches_at_its_deadline() {
        let r = interval_routine();
        let mut state = RoutineState::default();
        let mut effects = Vec::new();
        assert_eq!(
            evaluate(&r, &mut state, &inputs(1_000, &no_deps), &mut effects),
            Verdict::Idle
        );
        assert_eq!(state.next_run, 1_600);
        assert_eq!(state.state, RoutineRunState::Scheduled);
        assert!(effects.contains(&Effect::Save));
        effects.clear();
        assert_eq!(
            evaluate(&r, &mut state, &inputs(1_599, &no_deps), &mut effects),
            Verdict::Idle
        );
        let verdict = evaluate(&r, &mut state, &inputs(1_600, &no_deps), &mut effects);
        assert!(matches!(verdict, Verdict::Dispatch(_)));
        assert_eq!(state.state, RoutineRunState::Running);
        assert_eq!(state.last_attempt, 1_600);
    }

    /// I-12: nothing is dispatched once shutdown is latched.
    #[test]
    fn no_new_run_after_term() {
        let r = interval_routine();
        let mut state = RoutineState {
            next_run: 1_000,
            ..RoutineState::default()
        };
        let mut i = inputs(2_000, &no_deps);
        i.shutdown = true;
        assert_eq!(evaluate(&r, &mut state, &i, &mut Vec::new()), Verdict::Idle);
        assert_ne!(state.state, RoutineRunState::Running);
    }

    #[test]
    fn a_running_record_left_behind_reads_interrupted() {
        let r = interval_routine();
        let mut state = RoutineState {
            state: RoutineRunState::Running,
            next_run: 1_500,
            ..RoutineState::default()
        };
        let mut effects = Vec::new();
        evaluate(&r, &mut state, &inputs(1_000, &no_deps), &mut effects);
        assert_eq!(state.state, RoutineRunState::Failed);
        assert_eq!(state.last_reason, "interrupted");
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Activity {
                code: "run.failed",
                ..
            }
        )));
    }

    #[test]
    fn a_clock_step_back_is_clamped_and_logged() {
        let r = interval_routine();
        let mut state = RoutineState {
            next_run: 1_000_000,
            ..RoutineState::default()
        };
        let mut effects = Vec::new();
        let verdict = evaluate(&r, &mut state, &inputs(1_000, &no_deps), &mut effects);
        assert!(
            matches!(verdict, Verdict::Dispatch(_)),
            "clamped to now, so due now"
        );
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Log {
                event: "clock_step_detected",
                ..
            }
        )));
    }

    #[test]
    fn a_delete_bound_routine_is_deferred_once_with_backend_none() {
        let r = routine("enabled=true\nallow_delete=true\nmax_total_delete=5\n")
            .unwrap()
            .unwrap();
        let mut state = RoutineState::default();
        let mut effects = Vec::new();
        assert_eq!(
            evaluate(&r, &mut state, &inputs(1_000, &no_deps), &mut effects),
            Verdict::Idle
        );
        assert_eq!(state.defer_reason, DeferReason::DeleteBound);
        assert_eq!(state.backend, Backend::None);
        assert_eq!(effects.iter().filter(|e| **e == Effect::Save).count(), 1);
        effects.clear();
        evaluate(&r, &mut state, &inputs(1_005, &no_deps), &mut effects);
        assert!(
            effects.is_empty(),
            "recorded once, not every turn: {effects:?}"
        );
    }

    #[test]
    fn a_dependency_defers_an_interval_routine_by_the_floor() {
        let r = interval_routine();
        let mut state = RoutineState {
            next_run: 1_000,
            ..RoutineState::default()
        };
        let blocked = |_: &str| false;
        let mut effects = Vec::new();
        assert_eq!(
            evaluate(&r, &mut state, &inputs(1_000, &blocked), &mut effects),
            Verdict::Idle
        );
        assert_eq!(state.defer_reason, DeferReason::Dependency);
        assert_eq!(state.next_run, 1_030);
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, Effect::Activity { .. }))
                .count(),
            1
        );
    }

    fn daily_routine() -> Routine {
        routine("enabled=true\nmode=daily\ntime_window_start=01:00\ntime_window_end=03:00\n")
            .unwrap()
            .unwrap()
    }

    fn window(
        open: bool,
        due: bool,
        note: Option<WindowNote>,
    ) -> Option<Result<WindowState, WindowError>> {
        Some(Ok(WindowState {
            open,
            due,
            day: "2026-03-08".into(),
            weekday: 7,
            note,
        }))
    }

    #[test]
    fn a_daily_window_dispatch_records_its_day_and_names_a_dst_gap() {
        let r = daily_routine();
        let mut state = RoutineState::default();
        let mut i = inputs(1_000, &no_deps);
        i.window = window(true, true, Some(WindowNote::DstGap));
        let mut effects = Vec::new();
        let verdict = evaluate(&r, &mut state, &i, &mut effects);
        assert_eq!(
            verdict,
            Verdict::Dispatch(Dispatch {
                window_day: Some("2026-03-08".into()),
                deferral_continuing: false
            })
        );
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Log {
                event: "routine_window_dst_gap",
                level: Level::Warn,
                ..
            }
        )));
        let mut effects = Vec::new();
        let end = RunEnd {
            exit: 0,
            cancelled: false,
            reason: "ok".into(),
            admission_refused: false,
        };
        apply_outcome(
            &r,
            &mut state,
            "photos",
            &end,
            1_100,
            "2026-03-08",
            &mut effects,
        );
        assert_eq!(state.last_day, "2026-03-08");
        assert_eq!(state.state, RoutineRunState::Succeeded);
    }

    #[test]
    fn a_closed_window_is_recorded_once_and_keeps_a_finished_outcome() {
        let r = daily_routine();
        let mut state = RoutineState::default();
        let mut i = inputs(1_000, &no_deps);
        i.window = window(false, false, None);
        let mut effects = Vec::new();
        evaluate(&r, &mut state, &i, &mut effects);
        assert_eq!(state.defer_reason, DeferReason::Window);
        assert_eq!(effects.len(), 2);
        effects.clear();
        evaluate(&r, &mut state, &i, &mut effects);
        assert!(effects.is_empty());
        let mut done = RoutineState {
            state: RoutineRunState::Succeeded,
            ..RoutineState::default()
        };
        evaluate(&r, &mut done, &i, &mut effects);
        assert_eq!(done.state, RoutineRunState::Succeeded);
    }

    /// I-4: an unreadable clock never dispatches a daily routine.
    #[test]
    fn unreadable_state_or_clock_never_dispatches() {
        let r = daily_routine();
        let mut state = RoutineState::default();
        let mut i = inputs(1_000, &no_deps);
        i.window = Some(Err(WindowError::ClockUnavailable));
        assert_eq!(
            evaluate(&r, &mut state, &i, &mut Vec::new()),
            Verdict::ClockUnavailable
        );
        assert!(RoutineState::parse(Some(b"state=what\n")).is_err());
    }

    fn realtime_routine() -> Routine {
        routine("enabled=true\nmode=realtime\ndebounce_seconds=5\n")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_realtime_routine_dispatches_when_its_change_is_due_and_names_its_backend() {
        let r = realtime_routine();
        let mut state = RoutineState::default();
        let mut i = inputs(1_000, &no_deps);
        i.realtime = Some(RealtimeView {
            backend: Backend::Inotify,
            reason: BackendReason::Ok,
            due: false,
        });
        let mut effects = Vec::new();
        assert_eq!(evaluate(&r, &mut state, &i, &mut effects), Verdict::Idle);
        assert_eq!(state.backend, Backend::Inotify);
        assert!(
            effects.contains(&Effect::Save),
            "a backend transition is saved"
        );
        i.realtime = Some(RealtimeView {
            backend: Backend::Polling,
            reason: BackendReason::WatchLimit,
            due: true,
        });
        let verdict = evaluate(&r, &mut state, &i, &mut Vec::new());
        assert!(matches!(verdict, Verdict::Dispatch(_)));
        assert_eq!(state.backend_reason, BackendReason::WatchLimit);
    }

    #[test]
    fn a_missing_realtime_source_is_deferred_once_per_transition() {
        let r = realtime_routine();
        let mut state = RoutineState::default();
        let mut i = inputs(1_000, &no_deps);
        i.source = SourceStatus::Missing;
        let mut effects = Vec::new();
        evaluate(&r, &mut state, &i, &mut effects);
        assert_eq!(state.backend_reason, BackendReason::SourceMissing);
        assert_eq!(state.defer_reason, DeferReason::Source);
        assert_eq!(state.backend, Backend::None);
        assert!(effects.contains(&Effect::Save));
        effects.clear();
        evaluate(&r, &mut state, &i, &mut effects);
        assert!(effects.is_empty());
        i.source = SourceStatus::Symlink;
        evaluate(&r, &mut state, &i, &mut effects);
        assert_eq!(state.backend_reason, BackendReason::SourceSymlink);
        assert!(effects.contains(&Effect::Save));
    }

    fn end(exit: i32, cancelled: bool, reason: &str) -> RunEnd {
        RunEnd {
            exit,
            cancelled,
            reason: reason.into(),
            admission_refused: false,
        }
    }

    /// An OOM kill takes no retry and escalates nothing: retrying repeats it.
    #[test]
    fn an_oom_kill_takes_no_retry_and_names_the_block() {
        let r = interval_routine();
        let mut state = RoutineState::default();
        let mut effects = Vec::new();
        apply_outcome(
            &r,
            &mut state,
            "photos",
            &end(137, false, "core_oom_killed"),
            100,
            "d",
            &mut effects,
        );
        assert_eq!(state.state, RoutineRunState::Failed);
        assert_eq!(state.last_reason, "core_oom_killed");
        assert_eq!(state.retry_attempt, 0);
        assert!(!effects.iter().any(|e| matches!(e, Effect::Escalate { .. })));
        assert!(
            effects
                .iter()
                .any(|e| format!("{e:?}").contains("out-of-memory killer")),
            "{effects:?}"
        );
        // A plain kill still climbs the ladder.
        let mut state = RoutineState::default();
        apply_outcome(
            &r,
            &mut state,
            "photos",
            &end(137, false, "core_killed"),
            100,
            "d",
            &mut Vec::new(),
        );
        assert_eq!(state.retry_attempt, 1);
    }

    #[test]
    fn failures_climb_the_ladder_then_exhaust_and_escalate() {
        let r = interval_routine(); // retries=2, backoff 10, exponential
        let mut state = RoutineState::default();
        let mut effects = Vec::new();
        apply_outcome(
            &r,
            &mut state,
            "photos",
            &end(1, false, "core_failed"),
            100,
            "d",
            &mut effects,
        );
        assert_eq!((state.retry_attempt, state.next_run), (1, 110));
        apply_outcome(
            &r,
            &mut state,
            "photos",
            &end(1, false, "core_failed"),
            200,
            "d",
            &mut effects,
        );
        assert_eq!((state.retry_attempt, state.next_run), (2, 220));
        effects.clear();
        apply_outcome(
            &r,
            &mut state,
            "photos",
            &end(1, false, "run_stalled"),
            300,
            "d",
            &mut effects,
        );
        assert_eq!(state.retry_attempt, 0);
        assert_eq!(state.next_run, 900);
        assert_eq!(state.last_reason, "run_stalled");
        assert!(effects.contains(&Effect::Escalate { exit: 1 }));
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Activity {
                code: "routine.retries_exhausted",
                ..
            }
        )));
    }

    /// I-5: a cancelled run is failed/cancelled, never succeeded, and consumes no retry.
    #[test]
    fn cancelled_is_failed_cancelled_never_succeeded() {
        let r = daily_routine();
        let mut state = RoutineState {
            retry_attempt: 1,
            next_run: 50,
            last_day: "2026-03-07".into(),
            ..RoutineState::default()
        };
        let mut effects = Vec::new();
        apply_outcome(
            &r,
            &mut state,
            "photos",
            &end(137, true, "cancelled"),
            100,
            "2026-03-08",
            &mut effects,
        );
        assert_eq!(state.state, RoutineRunState::Failed);
        assert_eq!(state.last_reason, "cancelled");
        assert_eq!(state.retry_attempt, 1, "no attempt consumed");
        assert_eq!(state.next_run, 130, "floored");
        assert_eq!(state.last_day, "2026-03-07", "the day is not burned");
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Log {
                level: Level::Info,
                event: "routine_run",
                ..
            }
        )));
    }

    #[test]
    fn busy_defers_without_a_retry_and_partial_counts_as_success() {
        let r = realtime_routine();
        let mut state = RoutineState::default();
        let mut effects = Vec::new();
        apply_outcome(
            &r,
            &mut state,
            "p",
            &end(75, false, "busy"),
            100,
            "d",
            &mut effects,
        );
        assert_eq!(state.state, RoutineRunState::Deferred);
        assert_eq!(state.defer_reason, DeferReason::Busy);
        assert_eq!(state.next_run, 130);
        assert_eq!(state.retry_attempt, 0);
        for streak in 1..=6u64 {
            apply_outcome(
                &r,
                &mut state,
                "p",
                &end(3, false, "deferred_paths"),
                1_000,
                "d",
                &mut effects,
            );
            assert_eq!(state.partial_streak, streak);
            assert_eq!(state.state, RoutineRunState::Succeeded);
        }
        assert_eq!(state.next_run, 1_000 + REALTIME_EXHAUSTED_COOLDOWN);
        apply_outcome(
            &r,
            &mut state,
            "p",
            &end(0, false, "ok"),
            2_000,
            "d",
            &mut effects,
        );
        assert_eq!(state.partial_streak, 0);
        assert_eq!(state.next_run, 0);
    }
}
