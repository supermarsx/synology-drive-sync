//! Where the daemon's log events and Activity rows go.
//!
//! `controller.log` keeps the shell's JSON-lines shape
//! (`{"epoch":…,"level":…,"category":…,"event":…,"detail":…}`) and event names,
//! so the dashboard's Service subtab reads it unchanged; `activity.log` keeps the
//! pipe format and the code allowlist (`append_activity`).
//!
//! The shell `sdsync-dsm` still appends to `activity.log` and `controller.log`.
//! Lines are `O_APPEND` single writes, which never interleave; a rotation is
//! made under the shell's `event-log.lock`, taken by the shell's own claim and
//! link protocol and never waited for: a held lock skips the rotation and the
//! line is kept past the limit, so no log line ever waits on it (H1). A live
//! writer holding it is contention, named once at info and tried again in
//! seconds; only contention that lasts a minute, or a lock that cannot be
//! taken at all, is the Error and the shell's marker (N-D7). An episode ends
//! when the file is found under its limit again, so a later failure is named
//! again (N-D2).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use super::routines::Level;

/// The Activity codes `append_activity` accepts.
pub(crate) const ACTIVITY_CODES: [&str; 31] = [
    "run.started",
    "run.succeeded",
    "run.failed",
    "run.deferred",
    "run.cancelled",
    "routine.deferred",
    "routine.retry_scheduled",
    "routine.retries_exhausted",
    "routine.watch_abandoned",
    "doctor.succeeded",
    "doctor.failed",
    "doctor.inventory",
    "configuration.changed",
    "notification.unavailable",
    "audit.requested",
    "audit.succeeded",
    "audit.failed",
    "audit.outcome_unknown",
    "module.requested",
    "module.succeeded",
    "module.failed",
    "module.outcome_unknown",
    "security.policy_changed",
    "security.log_tail_recovered",
    "security.failed",
    "authentication.failed",
    "bridge.failed",
    "service.started",
    "service.stopped",
    "service.restarted",
    "service.start_failed",
];

/// One Activity row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivityRow {
    pub(crate) code: &'static str,
    /// A profile name, `all` or `none`.
    pub(crate) profile: String,
    pub(crate) state: &'static str,
    pub(crate) category: &'static str,
    pub(crate) level: Level,
    pub(crate) message: String,
}

/// One controller.log event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogEvent {
    pub(crate) level: Level,
    /// `controller` or `scheduler`.
    pub(crate) category: &'static str,
    pub(crate) event: &'static str,
    pub(crate) detail: String,
}

/// Where events go. A failure names its path and cause; the caller records it
/// (I-16) and goes on.
pub(crate) trait EventSink: Send {
    fn log(&self, epoch: u64, event: &LogEvent) -> Result<(), String>;
    fn activity(&self, epoch: u64, row: &ActivityRow) -> Result<(), String>;
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The controller.log line for an event.
pub(crate) fn log_line(epoch: u64, event: &LogEvent) -> String {
    serde_json::json!({
        "epoch": epoch,
        "level": event.level.as_str(),
        "category": event.category,
        "event": event.event,
        "detail": one_line(&event.detail),
    })
    .to_string()
}

/// The activity.log line for a row, or why the shell's validator would refuse it.
pub(crate) fn activity_line(epoch: u64, row: &ActivityRow) -> Result<String, String> {
    if !ACTIVITY_CODES.contains(&row.code) {
        return Err(format!("unknown Activity code {}", row.code));
    }
    let profile_ok =
        row.profile == "none" || row.profile == "all" || super::routines::valid_name(&row.profile);
    if !profile_ok {
        return Err(format!("invalid Activity profile for {}", row.code));
    }
    // `|` separates fields and a control byte would split the record.
    let message: String = row
        .message
        .chars()
        .map(|c| {
            if c == '|' {
                '/'
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .replace(" request_id=", " request_id_");
    Ok(format!(
        "{epoch}|{}|{}|{}|{}|{}|{message}",
        row.code,
        row.profile,
        row.state,
        row.category,
        row.level.as_str()
    ))
}

/// Appends to the package's log files, rotating as the shell does
/// (controller.log 10 MiB x 5, activity.log 1 MiB x 3).
pub(crate) struct FileSink {
    controller_log: PathBuf,
    activity_log: PathBuf,
    lock: Mutex<()>,
    /// The shell's `var/run/event-log.lock`, taken around a rotation so a
    /// shell writer (sdsync-dsm, still appending to activity.log) and the
    /// daemon never rotate the same set at once.
    event_lock: PathBuf,
    /// `var/state/log-rotation.failed`, the shell's marker for a rotation that
    /// could not be made (the line is still appended).
    rotation_marker: PathBuf,
}

/// Every daemon sink rotates under this, whichever instance it is.
static ROTATION: Mutex<()> = Mutex::new(());

/// After a rotation that could not be made, the next attempt for that file
/// waits this long: the line is appended past the limit meanwhile, and no log
/// line ever waits for a lock (H1).
const ROTATION_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// While a live writer holds the lock, the rotation is tried again this soon:
/// contention is a deferral, not a failure (N-D7).
const CONTENTION_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// One log file's rotation episode: open from the first deferral or failure
/// until the file is found under its limit again (rotated here or by the
/// shell, N-D2).
struct Trouble {
    /// When the rotation may next be tried.
    retry_at: Instant,
    /// Since when a live writer has held the lock, across retries.
    contended_since: Option<Instant>,
    /// The deferral was named (info, once per episode).
    deferred_noted: bool,
    /// The failure was named (error and marker, once per episode).
    reported: bool,
}

/// Per log file, its open rotation episode.
static ROTATION_TROUBLE: Mutex<BTreeMap<PathBuf, Trouble>> = Mutex::new(BTreeMap::new());

/// controller.log lines about the logs themselves (a rotation that failed, a
/// stale lock that was cleared), written ahead of the next controller.log line
/// by whichever sink writes it: a sink cannot log through itself mid-append.
static NOTES: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|age| age.as_secs())
        .unwrap_or(0)
}

impl FileSink {
    pub(crate) fn new(log_root: &Path) -> Self {
        let var = log_root.parent().unwrap_or(log_root);
        Self {
            controller_log: log_root.join("controller.log"),
            activity_log: log_root.join("activity.log"),
            lock: Mutex::new(()),
            event_lock: var.join("run/event-log.lock"),
            rotation_marker: var.join("state/log-rotation.failed"),
        }
    }

    /// Queue a controller.log line about the logs (see [`NOTES`]); also on
    /// stderr at once, which the supervisor keeps.
    fn note(&self, level: Level, event: &'static str, detail: String) {
        eprintln!("sdsyncd: {event}: {detail}");
        let line = log_line(
            epoch_now(),
            &LogEvent {
                level,
                category: "controller",
                event,
                detail,
            },
        );
        if let Ok(mut notes) = NOTES.lock() {
            notes.push((self.controller_log.clone(), line));
        }
    }

    /// Rotate `path` if it is still over `limit`, under the shell's
    /// event-log lock, never waiting for it. A rotation that cannot be made
    /// keeps the line (appended past the limit, as the shell does), leaves the
    /// shell's marker naming the file and the cause once per episode, and is
    /// not tried again for [`ROTATION_BACKOFF`].
    ///
    /// A live writer holding the lock (a shell writer mid-rotation) is
    /// contention, not a failure (N-D7): the deferral is named once at info
    /// and retried after [`CONTENTION_RETRY`]; only contention that outlasts
    /// [`ROTATION_BACKOFF`] becomes the Error and the marker.
    fn rotate_locked(&self, path: &Path, limit: u64, keep: usize) {
        let now = Instant::now();
        let backing_off = ROTATION_TROUBLE
            .lock()
            .ok()
            .and_then(|trouble| trouble.get(path).map(|t| t.retry_at))
            .is_some_and(|retry_at| now < retry_at);
        if backing_off {
            return;
        }
        let _all = ROTATION.lock();
        // Another sink may have rotated it while this one waited (H1/S3-7: a
        // second rotation would push a fresh generation out).
        if !fs::symlink_metadata(path).is_ok_and(|m| m.len() >= limit) {
            self.end_episode(path);
            return;
        }
        let outcome = event_lock::take(&self.event_lock).and_then(|taken| {
            for stale in &taken.cleared {
                self.note(
                    Level::Warn,
                    "event_log_lock_stale",
                    format!("path={} cause={stale}", self.event_lock.display()),
                );
            }
            // N-D7: checked again under the lock. A shell writer that held it
            // may have rotated the file meanwhile, and a second rotation
            // would push a fresh generation out.
            if !fs::symlink_metadata(path).is_ok_and(|m| m.len() >= limit) {
                return Ok(());
            }
            // The guard lives until the rotation is done, then releases.
            rotate(path, keep)
                .map_err(|error| event_lock::TakeError::Failed(format!("rotate: {error}")))
        });
        match outcome {
            Ok(()) => self.end_episode(path),
            Err(event_lock::TakeError::Held(cause)) => self.contended(path, now, &cause),
            Err(event_lock::TakeError::Failed(cause)) => self.failed(path, now, &cause),
        }
    }

    /// The file is under its limit (rotated, here or by the shell): its
    /// episode is over, so a later failure is a new one and is named again
    /// (N-D2: an episode left open hid every later failure).
    fn end_episode(&self, path: &Path) {
        let ended = match ROTATION_TROUBLE.lock() {
            Ok(mut trouble) => trouble.remove(path),
            Err(_) => {
                eprintln!(
                    "sdsyncd: the log rotation state is poisoned; {} keeps its episode",
                    path.display()
                );
                None
            }
        };
        let Some(ended) = ended else {
            return;
        };
        if ended.reported {
            self.retire_marker(path);
            self.note(
                Level::Info,
                "event_log_rotation_recovered",
                format!("path={}", path.display()),
            );
        }
    }

    /// A live (or not yet provably stale) writer holds the lock.
    fn contended(&self, path: &Path, now: Instant, cause: &str) {
        let (since, noted, reported) = {
            let Ok(mut trouble) = ROTATION_TROUBLE.lock() else {
                // Without the episode state this is a failure, named.
                self.failed(path, now, cause);
                return;
            };
            let entry = trouble.entry(path.to_owned()).or_insert(Trouble {
                retry_at: now,
                contended_since: None,
                deferred_noted: false,
                reported: false,
            });
            let since = *entry.contended_since.get_or_insert(now);
            entry.retry_at = now + CONTENTION_RETRY;
            let noted = std::mem::replace(&mut entry.deferred_noted, true);
            (since, noted, entry.reported)
        };
        let held_for = now.duration_since(since);
        if held_for >= ROTATION_BACKOFF {
            if !reported {
                self.failed(
                    path,
                    now,
                    &format!("{cause}, for {} s", held_for.as_secs()),
                );
            }
            return;
        }
        if !noted {
            self.note(
                Level::Info,
                "event_log_rotation_deferred",
                format!(
                    "path={} cause={} retry_seconds={} the line is kept past the size limit meanwhile",
                    path.display(),
                    one_line(cause),
                    CONTENTION_RETRY.as_secs()
                ),
            );
        }
    }

    /// The rotation cannot be made: once per episode, the marker and an Error.
    fn failed(&self, path: &Path, now: Instant, cause: &str) {
        let first = match ROTATION_TROUBLE.lock() {
            Ok(mut trouble) => {
                let entry = trouble.entry(path.to_owned()).or_insert(Trouble {
                    retry_at: now,
                    contended_since: None,
                    deferred_noted: false,
                    reported: false,
                });
                entry.retry_at = now + ROTATION_BACKOFF;
                !std::mem::replace(&mut entry.reported, true)
            }
            // Poisoned: named every time rather than never.
            Err(_) => true,
        };
        if first {
            self.record_marker(path, cause);
            self.note(
                Level::Error,
                "event_log_rotation_failed",
                format!(
                    "path={} cause={} retry_seconds={} new lines are kept past the size limit",
                    path.display(),
                    one_line(cause),
                    ROTATION_BACKOFF.as_secs()
                ),
            );
        }
    }

    /// The marker's `path=` line, if it is readable.
    fn marker_path(&self) -> Option<String> {
        let text = fs::read_to_string(&self.rotation_marker).ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix("path="))
            .map(str::to_owned)
    }

    /// `record_log_rotation_failure`: once per episode, named.
    fn record_marker(&self, path: &Path, cause: &str) {
        if self.marker_path().as_deref() == Some(&path.display().to_string()) {
            return;
        }
        if let Err(error) = super::state::atomic_write(
            &self.rotation_marker,
            format!(
                "epoch={}\npath={}\ncause={}\n",
                epoch_now(),
                path.display(),
                one_line(cause)
            )
            .as_bytes(),
        ) {
            eprintln!(
                "sdsyncd: the log rotation marker {} could not be written: {error}",
                self.rotation_marker.display()
            );
        }
    }

    /// `retire_log_rotation_failure`: only the marker of this file (S3-7: the
    /// marker is shared by every log, and another file's episode stays open).
    fn retire_marker(&self, path: &Path) {
        if self.marker_path().as_deref() != Some(&path.display().to_string()) {
            return;
        }
        match fs::remove_file(&self.rotation_marker) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "sdsyncd: the log rotation marker {} could not be removed: {error}",
                self.rotation_marker.display()
            ),
        }
    }

    fn append(&self, path: &Path, limit: u64, keep: usize, line: &str) -> Result<(), String> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| "the log lock is poisoned".to_owned())?;
        let named = |step: &str, error: io::Error| {
            format!("path={} step={step} cause={error}", path.display())
        };
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                    return Err(format!(
                        "path={} step=refuse cause=not a regular file",
                        path.display()
                    ));
                }
                if metadata.len() >= limit {
                    self.rotate_locked(path, limit, keep);
                } else {
                    // Rotated by another writer (the shell) while an episode
                    // was open: it is over (N-D2).
                    self.end_episode(path);
                }
            }
            // Rotated away and not yet recreated: the episode is over too.
            Err(error) if error.kind() == io::ErrorKind::NotFound => self.end_episode(path),
            // Any other error is met, and named, by the open below.
            Err(_) => {}
        }
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|error| named("open", error))?;
        let mut record = String::new();
        if path == self.controller_log
            && let Ok(mut notes) = NOTES.lock()
        {
            notes.retain(|(log, note)| {
                if log == path {
                    record.push_str(note);
                    record.push('\n');
                    false
                } else {
                    true
                }
            });
        }
        record.push_str(line);
        record.push('\n');
        // One write: O_APPEND keeps concurrent lines whole.
        file.write_all(record.as_bytes())
            .map_err(|error| named("write", error))
    }
}

/// The shell's private lock directory, by the shell's own protocol
/// (`acquire_event_log_lock`): a `0700` directory whose only entry is `pid`
/// (pid, start tick, boot id), published by `link(2)` from a complete claim
/// file beside it, so it is never torn by a live writer. Held exactly while
/// `pid` names a live owner: an empty directory is free (H1).
#[cfg(target_os = "linux")]
mod event_lock {
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    pub(super) struct Taken {
        _guard: Guard,
        /// What was cleared on the way in (a dead or torn owner), named.
        pub(super) cleared: Vec<String>,
    }

    struct Guard {
        lock: PathBuf,
        record: String,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            // Only our own record goes; the directory stays for the next
            // writer if it is not empty (the shell's release does the same).
            let owner = self.lock.join("pid");
            if fs::read_to_string(&owner).is_ok_and(|text| text == self.record)
                && let Err(error) = fs::remove_file(&owner)
            {
                eprintln!(
                    "sdsyncd: the event-log lock {} could not be released: {error}",
                    owner.display()
                );
            }
            let _ = fs::remove_dir(&self.lock);
        }
    }

    /// A torn owner record (it does not parse) seen earlier, by identity: one
    /// unchanged for [`TORN_SETTLED`] is stale, as nothing writes `pid` in
    /// place (it is linked in whole).
    static TORN_SEEN: Mutex<Option<(RecordIdentity, Instant)>> = Mutex::new(None);

    /// A record's device, inode, size and mtime.
    type RecordIdentity = (u64, u64, u64, i64);
    const TORN_SETTLED: Duration = Duration::from_secs(2);

    fn boot_epoch() -> Option<i64> {
        let uptime = fs::read_to_string("/proc/uptime").ok()?;
        let seconds: f64 = uptime.split_whitespace().next()?.parse().ok()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64();
        Some((now - seconds) as i64)
    }

    /// Why the lock was not taken.
    pub(super) enum TakeError {
        /// A live writer holds it, or a torn record has not yet settled:
        /// contention, tried again soon (N-D7).
        Held(String),
        /// It cannot be taken or used: a failure, named once per episode.
        Failed(String),
    }

    /// What the owner record at `owner` says about the lock.
    enum Owner {
        /// The record went away meanwhile: the lock may be free now.
        Vanished,
        /// A live writer (or one not provably dead): the cause names it.
        Held(String),
        /// Provably stale, with the cause and the record judged, by device and
        /// inode, so the recovery removes that record and no other.
        Stale(String, (u64, u64)),
    }

    /// Judge the owner record at `owner`.
    fn judge(lock: &Path, owner: &Path) -> Result<Owner, TakeError> {
        let metadata = match fs::symlink_metadata(owner) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Owner::Vanished);
            }
            Err(error) => return Err(TakeError::Failed(format!("{}: {error}", owner.display()))),
        };
        if !metadata.file_type().is_file() {
            return Err(TakeError::Failed(format!(
                "{}: not a regular file",
                owner.display()
            )));
        }
        let recorded = (metadata.dev(), metadata.ino());
        // An unreadable record is not an empty (torn) one: it could name a
        // live writer, so it is never cleared as stale.
        let text = match fs::read_to_string(owner) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Owner::Vanished);
            }
            Err(error) => return Err(TakeError::Failed(format!("{}: {error}", owner.display()))),
        };
        let lines: Vec<&str> = text.lines().collect();
        let well_formed = lines.len() == 3
            && lines[0].parse::<u32>().is_ok_and(|pid| pid > 1)
            && lines[1].parse::<u64>().is_ok_and(|start| start > 0)
            && !lines[2].is_empty();
        if well_formed {
            return Ok(
                if super::super::state::private_lock_owner(lock).is_some() {
                    Owner::Held(format!(
                        "{} is held by a live writer (PID {})",
                        lock.display(),
                        lines[0]
                    ))
                } else {
                    Owner::Stale(format!("its owner PID {} is gone", lines[0]), recorded)
                },
            );
        }
        // Torn: written before this boot, or unchanged across a settle.
        if boot_epoch().is_some_and(|boot| metadata.mtime() < boot) {
            return Ok(Owner::Stale(
                "a torn owner record from before this boot".into(),
                recorded,
            ));
        }
        let torn = || {
            Owner::Held(format!(
                "{} holds a torn owner record ({} bytes), not yet provably stale",
                lock.display(),
                metadata.len()
            ))
        };
        let identity = (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
        );
        let mut seen = TORN_SEEN.lock().map_err(|_| {
            TakeError::Failed("the torn-record state is poisoned".to_owned())
        })?;
        match *seen {
            Some((earlier, at)) if earlier == identity && at.elapsed() >= TORN_SETTLED => {
                *seen = None;
                Ok(Owner::Stale(
                    format!(
                        "a torn owner record unchanged for {} s",
                        TORN_SETTLED.as_secs()
                    ),
                    recorded,
                ))
            }
            Some((earlier, _)) if earlier == identity => Ok(torn()),
            _ => {
                *seen = Some((identity, Instant::now()));
                Ok(torn())
            }
        }
    }

    /// Any other error on the way is a failure.
    impl From<String> for TakeError {
        fn from(cause: String) -> Self {
            Self::Failed(cause)
        }
    }

    /// Take the lock without waiting: `Held` when a live (or not provably
    /// dead) writer holds it, `Failed` when it cannot be taken.
    pub(super) fn take(lock: &Path) -> Result<Taken, TakeError> {
        let pid = std::process::id();
        let start = super::super::super::linux_files::process_start(pid)
            .map_err(|_| "this process's start tick cannot be read".to_owned())?;
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|error| format!("boot id: {error}"))?;
        let record = format!("{pid}\n{start}\n{}\n", boot.trim());
        let claim = PathBuf::from(format!(
            "{}.claim.{pid}.{start}.{}",
            lock.display(),
            boot.trim()
        ));
        let owner = lock.join("pid");
        let mut cleared = Vec::new();
        // Two clears at most (a dead owner, then a torn one): bounded.
        for _ in 0..3 {
            match fs::DirBuilder::new().mode(0o700).create(lock) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("{}: {error}", lock.display()).into()),
            }
            let metadata = fs::symlink_metadata(lock)
                .map_err(|error| format!("{}: {error}", lock.display()))?;
            if !metadata.file_type().is_dir() {
                return Err(format!("{}: not a directory", lock.display()).into());
            }
            // Only `pid` may be inside; anything else makes it unprovable.
            for entry in fs::read_dir(lock).map_err(|error| format!("{}: {error}", lock.display()))?
            {
                let entry = entry.map_err(|error| format!("{}: {error}", lock.display()))?;
                if entry.file_name() != "pid" {
                    return Err(format!(
                        "{} holds {:?}, not only pid",
                        lock.display(),
                        entry.file_name()
                    )
                    .into());
                }
            }
            write_claim(&claim, &record)?;
            let linked = fs::hard_link(&claim, &owner);
            if let Err(error) = fs::remove_file(&claim) {
                eprintln!(
                    "sdsyncd: the event-log claim {} could not be removed: {error}",
                    claim.display()
                );
            }
            match linked {
                Ok(()) => {
                    return Ok(Taken {
                        _guard: Guard {
                            lock: lock.to_owned(),
                            record,
                        },
                        cleared,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("{}: {error}", owner.display()).into()),
            }
            match judge(lock, &owner)? {
                Owner::Vanished => {}
                Owner::Held(cause) => return Err(TakeError::Held(cause)),
                Owner::Stale(cause, judged) => {
                    let recovery = PathBuf::from(format!(
                        "{}.recover.{pid}.{start}.{}",
                        lock.display(),
                        boot.trim()
                    ));
                    if recover(&owner, &recovery, judged)? {
                        cleared.push(cause);
                    }
                }
            }
        }
        Err(TakeError::Held(format!(
            "{} could not be taken: its owner record kept changing",
            lock.display()
        )))
    }

    /// Clear the stale record judged at `owner` the shell's way: renamed to a
    /// name of our own, so one contender takes any one record (N-D7). The
    /// renamed record is checked to be the one judged: another contender may
    /// have cleared it and a live writer linked its own in between. That one
    /// is put back; if it cannot be (a third writer took the lock), the
    /// displaced record is kept at `recovery` and the failure names it.
    /// `Ok(true)` when the judged record was removed, `Ok(false)` when there
    /// was nothing (left) to clear.
    fn recover(owner: &Path, recovery: &Path, judged: (u64, u64)) -> Result<bool, TakeError> {
        match fs::rename(owner, recovery) {
            Ok(()) => {}
            // Another contender cleared it first.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(TakeError::Failed(format!(
                    "{} could not be cleared: {error}",
                    owner.display()
                )));
            }
        }
        let renamed = fs::symlink_metadata(recovery)
            .map(|metadata| (metadata.dev(), metadata.ino()))
            .map_err(|error| format!("{}: {error}", recovery.display()))?;
        if renamed == judged {
            fs::remove_file(recovery).map_err(|error| format!("{}: {error}", recovery.display()))?;
            return Ok(true);
        }
        // Not the record judged: restored, and the lock is held by it.
        match fs::hard_link(recovery, owner) {
            Ok(()) => {
                fs::remove_file(recovery)
                    .map_err(|error| format!("{}: {error}", recovery.display()))?;
                Err(TakeError::Held(format!(
                    "{} was replaced by a new owner record while it was being cleared; it was put back",
                    owner.display()
                )))
            }
            Err(error) => Err(TakeError::Failed(format!(
                "{} was replaced by a new owner record while it was being cleared and could not be put back ({error}); it is kept at {}",
                owner.display(),
                recovery.display()
            ))),
        }
    }

    fn write_claim(claim: &Path, record: &str) -> Result<(), String> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = fs::remove_file(claim);
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(claim)
            .and_then(|mut file| {
                file.write_all(record.as_bytes())?;
                file.sync_data()
            });
        written.map_err(|error| {
            let _ = fs::remove_file(claim);
            format!("{}: {error}", claim.display())
        })
    }
}

/// Without the shell (tests on other platforms) there is no shell writer.
#[cfg(not(target_os = "linux"))]
mod event_lock {
    use std::path::Path;

    pub(super) struct Taken {
        pub(super) cleared: Vec<String>,
    }

    #[allow(dead_code)]
    pub(super) enum TakeError {
        Held(String),
        Failed(String),
    }

    pub(super) fn take(_: &Path) -> Result<Taken, TakeError> {
        Ok(Taken {
            cleared: Vec::new(),
        })
    }
}

/// `base.(keep-1)` -> `base.keep`, …, `base` -> `base.1`. Nothing is dropped
/// until the oldest generation ages out (I-14 keeps whole lines: a rotation
/// only renames files).
fn rotate(base: &Path, keep: usize) -> io::Result<()> {
    let generation = |n: usize| PathBuf::from(format!("{}.{n}", base.display()));
    for n in (1..keep).rev() {
        let from = generation(n);
        if from.exists() {
            fs::rename(&from, generation(n + 1))?;
        }
    }
    fs::rename(base, generation(1))
}

impl EventSink for FileSink {
    fn log(&self, epoch: u64, event: &LogEvent) -> Result<(), String> {
        self.append(
            &self.controller_log,
            10 * 1024 * 1024,
            5,
            &log_line(epoch, event),
        )
    }

    fn activity(&self, epoch: u64, row: &ActivityRow) -> Result<(), String> {
        let line = activity_line(epoch, row)?;
        self.append(&self.activity_log, 1024 * 1024, 3, &line)
    }
}

/// Keeps everything in memory, for tests.
#[derive(Default)]
pub(crate) struct MemorySink {
    pub(crate) logs: Mutex<Vec<LogEvent>>,
    pub(crate) rows: Mutex<Vec<ActivityRow>>,
}

impl EventSink for std::sync::Arc<MemorySink> {
    fn log(&self, _epoch: u64, event: &LogEvent) -> Result<(), String> {
        self.logs
            .lock()
            .map_err(|_| "poisoned".to_owned())?
            .push(event.clone());
        Ok(())
    }
    fn activity(&self, epoch: u64, row: &ActivityRow) -> Result<(), String> {
        activity_line(epoch, row)?;
        self.rows
            .lock()
            .map_err(|_| "poisoned".to_owned())?
            .push(row.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(code: &'static str, message: &str) -> ActivityRow {
        ActivityRow {
            code,
            profile: "photos".into(),
            state: "failed",
            category: "routines",
            level: Level::Error,
            message: message.into(),
        }
    }

    #[test]
    fn activity_lines_keep_the_pipe_format_and_refuse_unknown_codes() {
        assert_eq!(
            activity_line(10, &row("run.failed", "a|b\nc request_id=x")).unwrap(),
            "10|run.failed|photos|failed|routines|error|a/b c request_id_x"
        );
        assert!(activity_line(10, &row("run.exploded", "x")).is_err());
    }

    #[test]
    fn log_lines_are_one_json_object() {
        let line = log_line(
            5,
            &LogEvent {
                level: Level::Warn,
                category: "scheduler",
                event: "run_stalled",
                detail: "profile=a\nphase=scan".into(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["event"], "run_stalled");
        assert_eq!(value["detail"], "profile=a phase=scan");
        assert!(!line.contains('\n'));
    }

    /// A private `var` tree (log, run, state) for one test.
    fn var_tree(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "sdsyncd-events-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        for dir in ["log", "run", "state"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        root
    }

    /// activity.log just over its 1 MiB limit.
    fn activity_over_limit(root: &Path) {
        let line = "1|run.started|none|running|sync|info|pad\n";
        fs::write(
            root.join("log/activity.log"),
            line.repeat(1024 * 1024 / line.len() + 2),
        )
        .unwrap();
    }

    fn started(n: u64) -> ActivityRow {
        ActivityRow {
            code: "run.started",
            profile: "none".into(),
            state: "running",
            category: "sync",
            level: Level::Info,
            message: format!("line {n}"),
        }
    }

    /// H1 (review-int hang probe): an empty `event-log.lock` (a shell writer
    /// killed between its mkdir and its link) is free, as the shell reads it:
    /// it used to cost every log line the full 10 s wait.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_empty_event_lock_directory_is_free_and_never_waited_for() {
        let root = var_tree("empty-lock");
        fs::create_dir_all(root.join("run/event-log.lock")).unwrap();
        activity_over_limit(&root);
        let sink = FileSink::new(&root.join("log"));
        let began = std::time::Instant::now();
        sink.activity(1, &started(1)).unwrap();
        assert!(
            began.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            began.elapsed()
        );
        assert!(root.join("log/activity.log.1").exists(), "rotated");
        assert!(
            !root.join("run/event-log.lock/pid").exists(),
            "the lock was released"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// H1: a live owner is never waited for: the rotation is skipped at once,
    /// the deferral named once (N-D7: contention is not a failure), and not
    /// tried again within the retry, while every line is still appended.
    /// Contention that outlasts the backoff becomes the failure and its marker.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_owner_skips_the_rotation_without_waiting() {
        let root = var_tree("live-lock");
        let lock = root.join("run/event-log.lock");
        fs::create_dir_all(&lock).unwrap();
        let mut holder = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let start = super::super::super::linux_files::process_start(holder.id()).unwrap();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        fs::write(
            lock.join("pid"),
            format!("{}\n{start}\n{}\n", holder.id(), boot.trim()),
        )
        .unwrap();
        activity_over_limit(&root);
        let sink = FileSink::new(&root.join("log"));
        let began = std::time::Instant::now();
        for n in 0..5 {
            sink.activity(n, &started(n)).unwrap();
        }
        sink.log(
            1,
            &LogEvent {
                level: Level::Info,
                category: "controller",
                event: "probe",
                detail: String::new(),
            },
        )
        .unwrap();
        assert!(began.elapsed() < std::time::Duration::from_secs(1));
        assert!(!root.join("log/activity.log.1").exists());
        let activity = fs::read_to_string(root.join("log/activity.log")).unwrap();
        assert!(activity.ends_with("line 4\n"), "every line is kept");
        assert!(!root.join("state/log-rotation.failed").exists());
        let controller = fs::read_to_string(root.join("log/controller.log")).unwrap();
        assert_eq!(
            controller.matches("event_log_rotation_deferred").count(),
            1,
            "{controller}"
        );
        assert!(controller.contains("held by a live writer"), "{controller}");
        // Still held a minute later: now a failure, named once, with its marker.
        let path = root.join("log/activity.log");
        {
            let mut trouble = ROTATION_TROUBLE.lock().unwrap();
            let trouble = trouble.get_mut(&path).expect("the deferral opened an episode");
            let past = Instant::now()
                .checked_sub(ROTATION_BACKOFF + std::time::Duration::from_secs(1))
                .expect("the monotonic clock is past one minute");
            trouble.contended_since = Some(past);
            trouble.retry_at = past;
        }
        sink.activity(6, &started(6)).unwrap();
        sink.activity(7, &started(7)).unwrap();
        probe_line(&sink);
        let controller = fs::read_to_string(root.join("log/controller.log")).unwrap();
        eprintln!("{controller}");
        assert_eq!(
            controller.matches("event_log_rotation_failed").count(),
            1,
            "{controller}"
        );
        let marker = fs::read_to_string(root.join("state/log-rotation.failed")).unwrap();
        assert!(
            marker.contains(&format!("path={}", path.display())),
            "{marker}"
        );
        let _ = holder.kill();
        let _ = holder.wait();
        let _ = fs::remove_dir_all(&root);
    }

    /// H1: a dead owner, and a torn record written before this boot, are
    /// stale: cleared (by the shell's rename-to-recover), named, rotated.
    #[cfg(target_os = "linux")]
    #[test]
    fn dead_and_torn_owners_are_cleared_by_name() {
        let root = var_tree("stale-lock");
        let lock = root.join("run/event-log.lock");
        fs::create_dir_all(&lock).unwrap();
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let gone_pid = gone.id();
        gone.wait().unwrap();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        fs::write(
            lock.join("pid"),
            format!("{gone_pid}\n12345\n{}\n", boot.trim()),
        )
        .unwrap();
        activity_over_limit(&root);
        let sink = FileSink::new(&root.join("log"));
        sink.activity(1, &started(1)).unwrap();
        assert!(root.join("log/activity.log.1").exists());
        // A torn record from before this boot (the release removed the
        // directory: a power cut leaves it with its torn owner).
        fs::create_dir_all(&lock).unwrap();
        fs::write(lock.join("pid"), b"12").unwrap();
        fs::File::options()
            .write(true)
            .open(lock.join("pid"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000))
            .unwrap();
        activity_over_limit(&root);
        sink.activity(2, &started(2)).unwrap();
        assert!(root.join("log/activity.log.2").exists());
        sink.log(
            1,
            &LogEvent {
                level: Level::Info,
                category: "controller",
                event: "probe",
                detail: String::new(),
            },
        )
        .unwrap();
        let controller = fs::read_to_string(root.join("log/controller.log")).unwrap();
        assert!(
            controller.contains("is gone") && controller.contains("before this boot"),
            "{controller}"
        );
        assert_eq!(controller.matches("event_log_lock_stale").count(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    fn probe_line(sink: &FileSink) {
        sink.log(
            1,
            &LogEvent {
                level: Level::Info,
                category: "controller",
                event: "probe",
                detail: String::new(),
            },
        )
        .unwrap();
    }

    /// N-D2: a rotation episode ends when the file is found under its limit
    /// again (the shell rotated it), so a later failure is a new episode and
    /// is named again. The episode used to stay open for good: the second
    /// failure went unreported.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_later_rotation_failure_is_named_again() {
        let root = var_tree("second-episode");
        let lock = root.join("run/event-log.lock");
        // A lock directory holding something besides `pid` cannot be taken:
        // a failure, named at once.
        let wedge = |on: bool| {
            if on {
                fs::create_dir_all(lock.join("stray")).unwrap();
            } else {
                fs::remove_dir_all(&lock).unwrap();
            }
        };
        wedge(true);
        activity_over_limit(&root);
        let sink = FileSink::new(&root.join("log"));
        sink.activity(1, &started(1)).unwrap();
        probe_line(&sink);
        // The shell rotates: the lock is free, the log is small, the marker
        // is retired.
        wedge(false);
        fs::rename(
            root.join("log/activity.log"),
            root.join("log/activity.log.1"),
        )
        .unwrap();
        fs::remove_file(root.join("state/log-rotation.failed")).unwrap();
        sink.activity(2, &started(2)).unwrap();
        // Later: over the limit and wedged again.
        wedge(true);
        activity_over_limit(&root);
        sink.activity(3, &started(3)).unwrap();
        probe_line(&sink);
        let controller = fs::read_to_string(root.join("log/controller.log")).unwrap();
        eprintln!("{controller}");
        assert_eq!(
            controller.matches("event_log_rotation_failed").count(),
            2,
            "both episodes are named: {controller}"
        );
        assert!(
            fs::read_to_string(root.join("state/log-rotation.failed"))
                .is_ok_and(|m| m.contains("activity.log")),
            "the marker names the second episode"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// N-D7: a torn owner record that has not yet settled is reported as
    /// torn, not as "held by a live writer" naming whatever its first line
    /// says.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_torn_owner_is_reported_as_torn() {
        let root = var_tree("torn-named");
        let lock = root.join("run/event-log.lock");
        fs::create_dir_all(&lock).unwrap();
        fs::write(lock.join("pid"), b"12").unwrap();
        activity_over_limit(&root);
        let sink = FileSink::new(&root.join("log"));
        sink.activity(1, &started(1)).unwrap();
        probe_line(&sink);
        let controller = fs::read_to_string(root.join("log/controller.log")).unwrap();
        eprintln!("{controller}");
        assert!(
            !controller.contains("held by a live writer"),
            "a torn record names no writer: {controller}"
        );
        assert!(controller.contains("torn"), "{controller}");
        let _ = fs::remove_dir_all(&root);
    }

    /// N-D7: a live shell writer holding the lock is contention, not a
    /// failure: no Error and no amber marker for the next minute. The line is
    /// kept, the deferral is named at info, and it is tried again soon.
    #[cfg(target_os = "linux")]
    #[test]
    fn live_contention_is_not_an_error() {
        let root = var_tree("contention");
        let lock = root.join("run/event-log.lock");
        fs::create_dir_all(&lock).unwrap();
        let mut holder = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let start = super::super::super::linux_files::process_start(holder.id()).unwrap();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        fs::write(
            lock.join("pid"),
            format!("{}\n{start}\n{}\n", holder.id(), boot.trim()),
        )
        .unwrap();
        activity_over_limit(&root);
        let sink = FileSink::new(&root.join("log"));
        sink.activity(1, &started(1)).unwrap();
        probe_line(&sink);
        let controller = fs::read_to_string(root.join("log/controller.log")).unwrap();
        eprintln!("{controller}");
        assert!(
            !root.join("state/log-rotation.failed").exists(),
            "no amber marker for a writer that is merely busy"
        );
        assert!(
            !controller.contains("\"level\":\"error\""),
            "no Error for contention: {controller}"
        );
        assert!(
            controller.contains("event_log_rotation_deferred")
                && controller.contains(&format!("PID {}", holder.id())),
            "the deferral is named with its holder: {controller}"
        );
        let _ = holder.kill();
        let _ = holder.wait();
        let _ = fs::remove_dir_all(&root);
    }

    /// S3-7: the rotation marker is shared by every log; a rotation retires
    /// only its own file's episode.
    #[test]
    fn a_rotation_retires_only_its_own_marker() {
        let root = var_tree("marker");
        let other = format!("path={}\n", root.join("log/controller.log").display());
        fs::write(
            root.join("state/log-rotation.failed"),
            format!("epoch=1\n{other}cause=x\n"),
        )
        .unwrap();
        activity_over_limit(&root);
        FileSink::new(&root.join("log"))
            .activity(1, &started(1))
            .unwrap();
        assert!(root.join("log/activity.log.1").exists());
        let marker = fs::read_to_string(root.join("state/log-rotation.failed")).unwrap();
        assert!(marker.contains(&other), "{marker}");
        let _ = fs::remove_dir_all(&root);
    }

    /// S3-7: two sinks that both find the file over its limit rotate it once
    /// (the size is checked again under the rotation lock).
    #[test]
    fn two_sinks_over_the_limit_rotate_once() {
        use std::sync::{Arc, Barrier};
        let root = var_tree("twice");
        activity_over_limit(&root);
        let barrier = Arc::new(Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|n| {
                let barrier = Arc::clone(&barrier);
                let log = root.join("log");
                std::thread::spawn(move || {
                    let sink = FileSink::new(&log);
                    barrier.wait();
                    sink.activity(n, &started(n)).unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(root.join("log/activity.log.1").exists());
        assert!(
            !root.join("log/activity.log.2").exists(),
            "a fresh generation was rotated out"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// I-14: rotation renames whole files and never drops a line.
    #[test]
    fn rotation_never_drops_a_line() {
        let root = var_tree("drops");
        let dir = root.join("log");
        let sink = FileSink::new(&dir);
        let path = dir.join("activity.log");
        for n in 0..50 {
            sink.append(
                &path,
                200,
                3,
                &format!("{n}|run.started|none|running|sync|info|line {n}"),
            )
            .unwrap();
        }
        let mut lines = Vec::new();
        for generation in [
            "activity.log.3",
            "activity.log.2",
            "activity.log.1",
            "activity.log",
        ] {
            if let Ok(text) = fs::read_to_string(dir.join(generation)) {
                lines.extend(text.lines().map(str::to_owned));
            }
        }
        // The newest lines are all present, in order, whole.
        let numbers: Vec<u32> = lines
            .iter()
            .map(|l| l.split('|').next().unwrap().parse().unwrap())
            .collect();
        assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "{numbers:?}");
        assert_eq!(*numbers.last().unwrap(), 49);
        assert!(dir.join("activity.log.1").exists(), "it rotated");
        fs::remove_dir_all(&root).unwrap();
    }
}
