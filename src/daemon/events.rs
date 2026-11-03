//! Where the daemon's log events and Activity rows go.
//!
//! `controller.log` keeps the shell's JSON-lines shape
//! (`{"epoch":…,"level":…,"category":…,"event":…,"detail":…}`) and event names,
//! so the dashboard's Service subtab reads it unchanged; `activity.log` keeps the
//! pipe format and the code allowlist (`append_activity`).
//!
//! Cut-over note: while the shell `sdsync-dsm` still appends to `activity.log`,
//! both writers must take the shell's `event-log.lock` (design §2.7). This file
//! appends with `O_APPEND` single writes, which do not interleave lines, but it
//! does not yet take that lock around rotation. See `scratchpad/phase2/cutover.md`.

#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
}

impl FileSink {
    pub(crate) fn new(log_root: &Path) -> Self {
        Self {
            controller_log: log_root.join("controller.log"),
            activity_log: log_root.join("activity.log"),
            lock: Mutex::new(()),
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
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err(format!(
                    "path={} step=refuse cause=not a regular file",
                    path.display()
                ));
            }
            if metadata.len() >= limit {
                rotate(path, keep).map_err(|error| named("rotate", error))?;
            }
        }
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|error| named("open", error))?;
        let mut record = line.to_owned();
        record.push('\n');
        // One write: O_APPEND keeps concurrent lines whole.
        file.write_all(record.as_bytes())
            .map_err(|error| named("write", error))
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

    /// I-14: rotation renames whole files and never drops a line.
    #[test]
    fn rotation_never_drops_a_line() {
        let dir = std::env::temp_dir().join(format!("sdsyncd-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
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
        fs::remove_dir_all(&dir).unwrap();
    }
}
