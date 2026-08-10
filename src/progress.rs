//! Thread-safe aggregate progress accounting and terminal-aware rendering.
//!
//! Progress is deliberately identified by numeric operation IDs. The API never accepts
//! credentials, URLs, headers, or free-form diagnostic text, so progress output cannot
//! accidentally serialize authentication material.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Decide when progress records are emitted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProgressMode {
    /// Emit progress only when the selected output stream is a terminal.
    #[default]
    Auto,
    /// Emit progress even when output is redirected.
    Always,
    /// Never emit progress.
    Never,
}

impl ProgressMode {
    pub fn enabled(self, is_terminal: bool) -> bool {
        match self {
            Self::Auto => is_terminal,
            Self::Always => true,
            Self::Never => false,
        }
    }
}

/// Progress output is either a compact human line or one JSON object per line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProgressFormat {
    #[default]
    Human,
    Ndjson,
}

/// A bounded set of operation names. No arbitrary message or secret field is accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    Upload,
    CreateDirectory,
    DeleteEntry,
    ScanLocal,
    ScanRemote,
}

impl OperationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::CreateDirectory => "create_directory",
            Self::DeleteEntry => "delete_entry",
            Self::ScanLocal => "scan_local",
            Self::ScanRemote => "scan_remote",
        }
    }

    fn is_file(self) -> bool {
        matches!(self, Self::Upload)
    }
}

impl fmt::Display for OperationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Expected aggregate work. `operations` includes uploads, creates, deletes, and scans;
/// `files` and `bytes` describe uploads only.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProgressTotals {
    pub operations: u64,
    pub files: u64,
    pub bytes: u64,
}

/// The reason for a per-operation update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateKind {
    Started,
    AttemptStarted,
    AttemptReset,
    Advanced,
    Succeeded,
    Failed,
}

impl UpdateKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::AttemptStarted => "attempt_started",
            Self::AttemptReset => "attempt_reset",
            Self::Advanced => "advanced",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// A secret-free per-operation update suitable for human or structured output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationUpdate {
    pub operation_id: u64,
    pub operation: OperationKind,
    pub kind: UpdateKind,
    pub attempt: u32,
    /// Logical byte position within the current attempt.
    pub attempt_bytes: u64,
    pub total_bytes: u64,
}

/// A consistent-enough lock-free aggregate view. Concurrent updates can land immediately
/// after the individual atomic reads, which is normal for live progress reporting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgressSnapshot {
    pub totals: ProgressTotals,
    pub completed_operations: u64,
    pub failed_operations: u64,
    pub completed_files: u64,
    pub failed_files: u64,
    /// Successfully completed bytes plus the byte position of active attempts.
    pub logical_bytes: u64,
    /// Bytes observed across every attempt, including retries.
    pub wire_bytes: u64,
    pub active_operations: u64,
    pub elapsed: Duration,
    pub throughput_bytes_per_second: f64,
    pub eta: Option<Duration>,
}

impl ProgressSnapshot {
    pub fn file_fraction(self) -> Option<f64> {
        (self.totals.files > 0).then(|| {
            (self.completed_files.min(self.totals.files) as f64) / (self.totals.files as f64)
        })
    }

    pub fn byte_fraction(self) -> Option<f64> {
        (self.totals.bytes > 0).then(|| {
            (self.logical_bytes.min(self.totals.bytes) as f64) / (self.totals.bytes as f64)
        })
    }
}

#[derive(Debug)]
pub enum ProgressError {
    OperationFinished,
    UnknownOperation,
    CounterUnavailable,
}

impl fmt::Display for ProgressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OperationFinished => "progress operation is already finished",
            Self::UnknownOperation => "progress operation is not active",
            Self::CounterUnavailable => "progress counter is unavailable",
        })
    }
}

impl std::error::Error for ProgressError {}

pub type ProgressResult<T> = std::result::Result<T, ProgressError>;

#[derive(Debug)]
struct ActiveOperation {
    operation: OperationKind,
    total_bytes: u64,
    attempt: u32,
    attempt_bytes: u64,
}

#[derive(Debug)]
struct ProgressInner {
    totals: ProgressTotals,
    started: Instant,
    next_id: AtomicU64,
    completed_operations: AtomicU64,
    failed_operations: AtomicU64,
    completed_files: AtomicU64,
    failed_files: AtomicU64,
    logical_bytes: AtomicU64,
    wire_bytes: AtomicU64,
    active: Mutex<HashMap<u64, ActiveOperation>>,
}

/// Shared progress state. Cloning the tracker is cheap and safe for upload workers.
#[derive(Clone, Debug)]
pub struct ProgressTracker {
    inner: Arc<ProgressInner>,
}

impl ProgressTracker {
    pub fn new(totals: ProgressTotals) -> Self {
        Self {
            inner: Arc::new(ProgressInner {
                totals,
                started: Instant::now(),
                next_id: AtomicU64::new(1),
                completed_operations: AtomicU64::new(0),
                failed_operations: AtomicU64::new(0),
                completed_files: AtomicU64::new(0),
                failed_files: AtomicU64::new(0),
                logical_bytes: AtomicU64::new(0),
                wire_bytes: AtomicU64::new(0),
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Start an operation. The observer should call `begin_attempt` when the underlying
    /// transfer emits its first `AttemptStarted` event. For non-upload work, pass `0` bytes.
    pub fn start(&self, operation: OperationKind, total_bytes: u64) -> OperationHandle {
        let operation_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let state = ActiveOperation {
            operation,
            total_bytes,
            attempt: 0,
            attempt_bytes: 0,
        };
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(operation_id, state);
        OperationHandle {
            shared: Arc::new(HandleShared {
                tracker: self.clone(),
                operation_id,
                finished: AtomicBool::new(false),
            }),
        }
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        let elapsed = self.inner.started.elapsed();
        let wire_bytes = self.inner.wire_bytes.load(Ordering::Relaxed);
        let logical_bytes = self.inner.logical_bytes.load(Ordering::Relaxed);
        let throughput = if elapsed.is_zero() {
            0.0
        } else {
            wire_bytes as f64 / elapsed.as_secs_f64()
        };
        let remaining = self.inner.totals.bytes.saturating_sub(logical_bytes);
        let eta = (remaining > 0 && throughput.is_finite() && throughput > 0.0)
            .then(|| Duration::from_secs_f64(remaining as f64 / throughput));
        let active_operations = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len() as u64;
        ProgressSnapshot {
            totals: self.inner.totals,
            completed_operations: self.inner.completed_operations.load(Ordering::Relaxed),
            failed_operations: self.inner.failed_operations.load(Ordering::Relaxed),
            completed_files: self.inner.completed_files.load(Ordering::Relaxed),
            failed_files: self.inner.failed_files.load(Ordering::Relaxed),
            logical_bytes,
            wire_bytes,
            active_operations,
            elapsed,
            throughput_bytes_per_second: throughput,
            eta,
        }
    }

    fn begin_attempt(&self, operation_id: u64) -> ProgressResult<OperationUpdate> {
        let mut active = self.active()?;
        let operation = active
            .get_mut(&operation_id)
            .ok_or(ProgressError::UnknownOperation)?;
        subtract_saturating(&self.inner.logical_bytes, operation.attempt_bytes);
        operation.attempt_bytes = 0;
        operation.attempt = operation.attempt.saturating_add(1);
        Ok(operation.update(operation_id, UpdateKind::AttemptStarted))
    }

    fn reset_attempt(&self, operation_id: u64) -> ProgressResult<OperationUpdate> {
        let mut active = self.active()?;
        let operation = active
            .get_mut(&operation_id)
            .ok_or(ProgressError::UnknownOperation)?;
        subtract_saturating(&self.inner.logical_bytes, operation.attempt_bytes);
        operation.attempt_bytes = 0;
        Ok(operation.update(operation_id, UpdateKind::AttemptReset))
    }

    fn advance(&self, operation_id: u64, delta: u64) -> ProgressResult<OperationUpdate> {
        let mut active = self.active()?;
        let operation = active
            .get_mut(&operation_id)
            .ok_or(ProgressError::UnknownOperation)?;
        let available = operation
            .total_bytes
            .saturating_sub(operation.attempt_bytes);
        let accepted = delta.min(available);
        operation.attempt_bytes = operation.attempt_bytes.saturating_add(accepted);
        self.inner
            .logical_bytes
            .fetch_add(accepted, Ordering::Relaxed);
        self.inner.wire_bytes.fetch_add(accepted, Ordering::Relaxed);
        Ok(operation.update(operation_id, UpdateKind::Advanced))
    }

    fn finish(&self, operation_id: u64, succeeded: bool) -> ProgressResult<OperationUpdate> {
        let mut active = self.active()?;
        let mut operation = active
            .remove(&operation_id)
            .ok_or(ProgressError::UnknownOperation)?;
        let kind = if succeeded {
            let remaining = operation
                .total_bytes
                .saturating_sub(operation.attempt_bytes);
            operation.attempt_bytes = operation.attempt_bytes.saturating_add(remaining);
            self.inner
                .logical_bytes
                .fetch_add(remaining, Ordering::Relaxed);
            self.inner
                .wire_bytes
                .fetch_add(remaining, Ordering::Relaxed);
            self.inner
                .completed_operations
                .fetch_add(1, Ordering::Relaxed);
            if operation.operation.is_file() {
                self.inner.completed_files.fetch_add(1, Ordering::Relaxed);
            }
            UpdateKind::Succeeded
        } else {
            subtract_saturating(&self.inner.logical_bytes, operation.attempt_bytes);
            operation.attempt_bytes = 0;
            self.inner.failed_operations.fetch_add(1, Ordering::Relaxed);
            if operation.operation.is_file() {
                self.inner.failed_files.fetch_add(1, Ordering::Relaxed);
            }
            UpdateKind::Failed
        };
        Ok(operation.update(operation_id, kind))
    }

    fn active(&self) -> ProgressResult<std::sync::MutexGuard<'_, HashMap<u64, ActiveOperation>>> {
        self.inner
            .active
            .lock()
            .map_err(|_| ProgressError::CounterUnavailable)
    }
}

impl ActiveOperation {
    fn update(&self, operation_id: u64, kind: UpdateKind) -> OperationUpdate {
        OperationUpdate {
            operation_id,
            operation: self.operation,
            kind,
            attempt: self.attempt,
            attempt_bytes: self.attempt_bytes,
            total_bytes: self.total_bytes,
        }
    }
}

fn subtract_saturating(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(value))
    });
}

#[derive(Debug)]
struct HandleShared {
    tracker: ProgressTracker,
    operation_id: u64,
    finished: AtomicBool,
}

impl Drop for HandleShared {
    fn drop(&mut self) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            let _ = self.tracker.finish(self.operation_id, false);
        }
    }
}

/// Cloneable operation observer for multipart-reader callbacks and retry loops.
#[derive(Clone, Debug)]
pub struct OperationHandle {
    shared: Arc<HandleShared>,
}

impl OperationHandle {
    pub fn operation_id(&self) -> u64 {
        self.shared.operation_id
    }

    /// Reset the current byte position and begin the next retry attempt.
    pub fn begin_attempt(&self) -> ProgressResult<OperationUpdate> {
        self.ensure_active()?;
        self.shared.tracker.begin_attempt(self.shared.operation_id)
    }

    /// Reset the current byte position without incrementing the attempt number.
    pub fn reset_attempt(&self) -> ProgressResult<OperationUpdate> {
        self.ensure_active()?;
        self.shared.tracker.reset_attempt(self.shared.operation_id)
    }

    /// Advance the current attempt. Excess bytes are clamped to the declared total.
    pub fn advance(&self, delta: u64) -> ProgressResult<OperationUpdate> {
        self.ensure_active()?;
        self.shared.tracker.advance(self.shared.operation_id, delta)
    }

    pub fn finish_success(&self) -> ProgressResult<OperationUpdate> {
        self.finish(true)
    }

    pub fn fail(&self) -> ProgressResult<OperationUpdate> {
        self.finish(false)
    }

    fn finish(&self, succeeded: bool) -> ProgressResult<OperationUpdate> {
        if self.shared.finished.swap(true, Ordering::AcqRel) {
            return Err(ProgressError::OperationFinished);
        }
        self.shared
            .tracker
            .finish(self.shared.operation_id, succeeded)
    }

    fn ensure_active(&self) -> ProgressResult<()> {
        if self.shared.finished.load(Ordering::Acquire) {
            Err(ProgressError::OperationFinished)
        } else {
            Ok(())
        }
    }
}

/// Stateful renderer that overwrites one terminal line in human mode and emits stable
/// records when redirected or using NDJSON.
pub struct ProgressRenderer<W: Write> {
    writer: W,
    enabled: bool,
    interactive: bool,
    format: ProgressFormat,
    last_width: usize,
}

impl<W: Write> ProgressRenderer<W> {
    pub fn new(writer: W, mode: ProgressMode, format: ProgressFormat, is_terminal: bool) -> Self {
        Self {
            writer,
            enabled: mode.enabled(is_terminal),
            interactive: is_terminal && format == ProgressFormat::Human,
            format,
            last_width: 0,
        }
    }

    pub fn render(
        &mut self,
        snapshot: &ProgressSnapshot,
        update: Option<&OperationUpdate>,
    ) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let line = match self.format {
            ProgressFormat::Human => human_progress(snapshot, update),
            ProgressFormat::Ndjson => json_progress(snapshot, update).to_string(),
        };
        if self.interactive {
            let padding = self.last_width.saturating_sub(line.chars().count());
            write!(self.writer, "\r{line}{:padding$}", "")?;
            self.writer.flush()?;
            self.last_width = line.chars().count();
        } else {
            writeln!(self.writer, "{line}")?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if self.enabled && self.interactive && self.last_width > 0 {
            writeln!(self.writer)?;
            self.last_width = 0;
        }
        self.writer.flush()
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

fn human_progress(snapshot: &ProgressSnapshot, update: Option<&OperationUpdate>) -> String {
    let eta = snapshot
        .eta
        .map(format_duration)
        .unwrap_or_else(|| "--:--".to_owned());
    let mut line = format!(
        "Progress {}/{} files, {}/{} at {}/s, {} active, ETA {eta}",
        snapshot.completed_files,
        snapshot.totals.files,
        human_bytes(snapshot.logical_bytes),
        human_bytes(snapshot.totals.bytes),
        human_bytes(snapshot.throughput_bytes_per_second.max(0.0) as u64),
        snapshot.active_operations,
    );
    if let Some(update) = update {
        use std::fmt::Write as _;
        let _ = write!(
            line,
            " | op={} {} {} attempt={}",
            update.operation_id,
            update.operation,
            update.kind.as_str(),
            update.attempt
        );
    }
    line
}

fn json_progress(
    snapshot: &ProgressSnapshot,
    update: Option<&OperationUpdate>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "schema": "sdsync.progress.v1",
        "totals": {
            "operations": snapshot.totals.operations,
            "files": snapshot.totals.files,
            "bytes": snapshot.totals.bytes,
        },
        "completed_operations": snapshot.completed_operations,
        "failed_operations": snapshot.failed_operations,
        "completed_files": snapshot.completed_files,
        "failed_files": snapshot.failed_files,
        "logical_bytes": snapshot.logical_bytes,
        "wire_bytes": snapshot.wire_bytes,
        "active_operations": snapshot.active_operations,
        "elapsed_ms": duration_millis(snapshot.elapsed),
        "throughput_bytes_per_second": snapshot.throughput_bytes_per_second,
        "eta_ms": snapshot.eta.map(duration_millis),
    });
    if let Some(update) = update {
        value["update"] = serde_json::json!({
            "operation_id": update.operation_id,
            "operation": update.operation.as_str(),
            "kind": update.kind.as_str(),
            "attempt": update.attempt,
            "attempt_bytes": update.attempt_bytes,
            "total_bytes": update.total_bytes,
        });
    }
    value
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_modes_are_terminal_aware() {
        assert!(ProgressMode::Auto.enabled(true));
        assert!(!ProgressMode::Auto.enabled(false));
        assert!(ProgressMode::Always.enabled(false));
        assert!(!ProgressMode::Never.enabled(true));
    }

    #[test]
    fn retry_reset_does_not_overcount_logical_bytes() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 100,
        });
        let operation = tracker.start(OperationKind::Upload, 100);
        operation.begin_attempt().unwrap();
        operation.advance(80).unwrap();
        assert_eq!(tracker.snapshot().logical_bytes, 80);
        let retry = operation.begin_attempt().unwrap();
        assert_eq!(retry.attempt, 2);
        assert_eq!(tracker.snapshot().logical_bytes, 0);
        assert_eq!(tracker.snapshot().wire_bytes, 80);
        operation.advance(100).unwrap();
        operation.finish_success().unwrap();
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.logical_bytes, 100);
        assert_eq!(snapshot.wire_bytes, 180);
        assert_eq!(snapshot.completed_files, 1);
        assert_eq!(snapshot.active_operations, 0);
    }

    #[test]
    fn reset_attempt_preserves_attempt_number_and_wire_bytes() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 10,
        });
        let operation = tracker.start(OperationKind::Upload, 10);
        operation.begin_attempt().unwrap();
        operation.advance(7).unwrap();

        let reset = operation.reset_attempt().unwrap();
        assert_eq!(reset.kind, UpdateKind::AttemptReset);
        assert_eq!(reset.attempt, 1);
        assert_eq!(reset.attempt_bytes, 0);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.logical_bytes, 0);
        assert_eq!(snapshot.wire_bytes, 7);
        operation.fail().unwrap();
    }

    #[test]
    fn explicit_failure_rolls_back_logical_bytes_and_finishes_the_handle() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 5,
        });
        let operation = tracker.start(OperationKind::Upload, 5);
        operation.begin_attempt().unwrap();
        let update = operation.advance(99).unwrap();
        assert_eq!(update.attempt_bytes, 5);
        let failed = operation.fail().unwrap();
        assert_eq!(failed.kind, UpdateKind::Failed);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.failed_operations, 1);
        assert_eq!(snapshot.failed_files, 1);
        assert_eq!(snapshot.logical_bytes, 0);
        assert_eq!(snapshot.wire_bytes, 5);
        assert!(matches!(
            operation.advance(1),
            Err(ProgressError::OperationFinished)
        ));
    }

    #[test]
    fn non_file_success_does_not_increment_file_counters() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 0,
            bytes: 0,
        });
        let operation = tracker.start(OperationKind::CreateDirectory, 0);
        let completed = operation.finish_success().unwrap();
        assert_eq!(completed.kind, UpdateKind::Succeeded);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.completed_operations, 1);
        assert_eq!(snapshot.completed_files, 0);
        assert_eq!(snapshot.failed_files, 0);
    }

    #[test]
    fn progress_fractions_are_optional_and_clamped() {
        let empty = ProgressTracker::new(ProgressTotals::default()).snapshot();
        assert_eq!(empty.file_fraction(), None);
        assert_eq!(empty.byte_fraction(), None);

        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 4,
        });
        let operation = tracker.start(OperationKind::Upload, 8);
        operation.finish_success().unwrap();
        let over_total = tracker.snapshot();
        assert_eq!(over_total.file_fraction(), Some(1.0));
        assert_eq!(over_total.byte_fraction(), Some(1.0));
    }

    #[test]
    fn cloned_handles_finish_exactly_once() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 8,
        });
        let operation = tracker.start(OperationKind::Upload, 8);
        operation.begin_attempt().unwrap();
        let callback = operation.clone();
        callback.advance(3).unwrap();
        operation.finish_success().unwrap();
        assert!(matches!(
            callback.finish_success(),
            Err(ProgressError::OperationFinished)
        ));
        assert_eq!(tracker.snapshot().completed_operations, 1);
    }

    #[test]
    fn parallel_updates_are_aggregated_safely() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 8,
            files: 8,
            bytes: 8_000,
        });
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let operation = tracker.start(OperationKind::Upload, 1_000);
                scope.spawn(move || {
                    operation.begin_attempt().unwrap();
                    let reader = operation.clone();
                    for _ in 0..10 {
                        reader.advance(100).unwrap();
                    }
                    operation.finish_success().unwrap();
                });
            }
        });
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.completed_files, 8);
        assert_eq!(snapshot.logical_bytes, 8_000);
        assert_eq!(snapshot.wire_bytes, 8_000);
        assert_eq!(snapshot.active_operations, 0);
        assert!(snapshot.throughput_bytes_per_second > 0.0);
    }

    #[test]
    fn abandoned_handle_is_recorded_as_failure() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 10,
        });
        {
            let operation = tracker.start(OperationKind::Upload, 10);
            operation.begin_attempt().unwrap();
            operation.advance(4).unwrap();
        }
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.failed_operations, 1);
        assert_eq!(snapshot.failed_files, 1);
        assert_eq!(snapshot.logical_bytes, 0);
        assert_eq!(snapshot.wire_bytes, 4);
    }

    #[test]
    fn renderer_supports_disabled_human_and_ndjson_modes() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 1,
        });
        let snapshot = tracker.snapshot();

        let mut disabled =
            ProgressRenderer::new(Vec::new(), ProgressMode::Auto, ProgressFormat::Human, false);
        disabled.render(&snapshot, None).unwrap();
        assert!(disabled.into_inner().is_empty());

        let mut renderer = ProgressRenderer::new(
            Vec::new(),
            ProgressMode::Always,
            ProgressFormat::Ndjson,
            false,
        );
        renderer.render(&snapshot, None).unwrap();
        let output = String::from_utf8(renderer.into_inner()).unwrap();
        let value: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(value["schema"], "sdsync.progress.v1");
    }

    #[test]
    fn ndjson_renderer_includes_the_bounded_operation_update() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 10,
        });
        let operation = tracker.start(OperationKind::Upload, 10);
        operation.begin_attempt().unwrap();
        let update = operation.advance(4).unwrap();
        let mut renderer = ProgressRenderer::new(
            Vec::new(),
            ProgressMode::Always,
            ProgressFormat::Ndjson,
            false,
        );
        renderer.render(&tracker.snapshot(), Some(&update)).unwrap();
        let output = String::from_utf8(renderer.into_inner()).unwrap();
        let value: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(value["update"]["operation"], "upload");
        assert_eq!(value["update"]["kind"], "advanced");
        assert_eq!(value["update"]["attempt"], 1);
        assert_eq!(value["update"]["attempt_bytes"], 4);
        assert_eq!(value["active_operations"], 1);
        operation.fail().unwrap();
    }

    #[test]
    fn interactive_renderer_uses_carriage_return_and_finishes_one_line() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 1,
        });
        let snapshot = tracker.snapshot();
        let mut renderer = ProgressRenderer::new(
            Vec::new(),
            ProgressMode::Always,
            ProgressFormat::Human,
            true,
        );
        renderer.render(&snapshot, None).unwrap();
        renderer.finish().unwrap();
        renderer.finish().unwrap();
        let output = String::from_utf8(renderer.into_inner()).unwrap();
        assert!(output.starts_with('\r'));
        assert_eq!(output.matches('\r').count(), 1);
        assert_eq!(output.matches('\n').count(), 1);
    }

    #[test]
    fn human_units_and_durations_are_stable_at_boundaries() {
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_duration(Duration::from_secs(59)), "00:59");
        assert_eq!(format_duration(Duration::from_secs(60)), "01:00");
        assert_eq!(format_duration(Duration::from_secs(3_661)), "01:01:01");
    }

    #[test]
    fn operation_kind_as_str_and_display_cover_every_variant() {
        let cases = [
            (OperationKind::Upload, "upload"),
            (OperationKind::CreateDirectory, "create_directory"),
            (OperationKind::DeleteEntry, "delete_entry"),
            (OperationKind::ScanLocal, "scan_local"),
            (OperationKind::ScanRemote, "scan_remote"),
        ];
        for (kind, expected) in cases {
            assert_eq!(kind.as_str(), expected);
            assert_eq!(kind.to_string(), expected);
        }
    }

    #[test]
    fn update_kind_as_str_covers_every_variant() {
        let cases = [
            (UpdateKind::Started, "started"),
            (UpdateKind::AttemptStarted, "attempt_started"),
            (UpdateKind::AttemptReset, "attempt_reset"),
            (UpdateKind::Advanced, "advanced"),
            (UpdateKind::Succeeded, "succeeded"),
            (UpdateKind::Failed, "failed"),
        ];
        for (kind, expected) in cases {
            assert_eq!(kind.as_str(), expected);
        }
    }

    #[test]
    fn progress_error_display_covers_every_variant() {
        assert_eq!(
            ProgressError::OperationFinished.to_string(),
            "progress operation is already finished"
        );
        assert_eq!(
            ProgressError::UnknownOperation.to_string(),
            "progress operation is not active"
        );
        assert_eq!(
            ProgressError::CounterUnavailable.to_string(),
            "progress counter is unavailable"
        );
    }

    #[test]
    fn operation_handle_exposes_its_operation_id() {
        let tracker = ProgressTracker::new(ProgressTotals::default());
        let first = tracker.start(OperationKind::ScanLocal, 0);
        let second = tracker.start(OperationKind::ScanRemote, 0);
        assert_eq!(first.operation_id(), 1);
        assert_eq!(second.operation_id(), 2);
        first.finish_success().unwrap();
        second.finish_success().unwrap();
    }

    #[test]
    fn human_renderer_includes_the_bounded_operation_update() {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: 1,
            files: 1,
            bytes: 10,
        });
        let operation = tracker.start(OperationKind::Upload, 10);
        let update = operation.begin_attempt().unwrap();
        let mut renderer = ProgressRenderer::new(
            Vec::new(),
            ProgressMode::Always,
            ProgressFormat::Human,
            false,
        );
        renderer.render(&tracker.snapshot(), Some(&update)).unwrap();
        let output = String::from_utf8(renderer.into_inner()).unwrap();
        assert_eq!(
            output.trim_end(),
            format!(
                "Progress 0/1 files, 0 B/10 B at 0 B/s, 1 active, ETA --:-- | op={} upload attempt_started attempt=1",
                update.operation_id
            )
        );
        operation.fail().unwrap();
    }
}
