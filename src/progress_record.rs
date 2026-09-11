//! The progress record a long-running operation publishes while a dashboard waits on it.
//!
//! This is the write half of the DSM request-progress contract. The read half lives in the bridge
//! binary, which resolves the phase id this writer emits against the catalogue in [`crate`] and
//! never renders anything the record itself supplies. The two halves therefore agree on exactly
//! three things: the schema string, the record's key set, and the phase ids -- all of which come
//! from the library so that neither binary can drift from the other by editing its own copy.
//!
//! Three properties are deliberate and load-bearing.
//!
//! **It is independent of [`crate::progress::ProgressMode`].** The DSM path runs the core with
//! progress suppressed, because there is no terminal to render to; a record writer wired through
//! the terminal renderer would therefore be dead on the one path it exists for. Nothing in this
//! module consults the mode, the format, or whether a stream is a terminal.
//!
//! **It is advisory and cannot fail a run.** Every filesystem error here is swallowed. A full
//! disk, a removed directory, or a permission change degrades the operation to the plain `pending`
//! it reported before progress existed -- it never turns a successful sync into a failed one.
//!
//! **It is throttled, because the reader cannot see faster.** The dashboard's result poll ramps to
//! one request every 2 000 ms, so a record written more often than that is written for nobody. The
//! cost of an emission is stated in [`ProgressRecorder::write_now`].

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{PhaseSpec, operation_phases};

/// The document schema both halves agree on.
///
/// The bridge declares this string again rather than importing it, because its copy is what a
/// packaging check reads; a unit test there asserts the two are equal, so the duplicate cannot
/// drift silently.
pub const PROGRESS_SCHEMA: &str = "sdsync.dsm-request-progress.v1";

/// The floor between two emissions.
///
/// Derived, not chosen: the dashboard's result poll ramps to one request every 2 000 ms, so this
/// is the interval below which an extra write cannot be observed by anyone.
pub const WRITE_INTERVAL: Duration = Duration::from_millis(2_000);

/// How many counted items pass before the writer consults the clock.
///
/// This is the *cheap* half of the throttle. Ticking is a relaxed atomic increment; only every
/// `COUNT_TRIGGER`th tick pays for a monotonic clock read, and only a tick that also clears
/// [`WRITE_INTERVAL`] pays for the write itself. Over a 20 000-entry walk the clock is read 78
/// times and almost every one of those is absorbed by the interval.
pub const COUNT_TRIGGER: u64 = 256;

/// A phase index of zero means no phase has been entered yet, so nothing is published.
const NO_PHASE: usize = 0;

/// Publishes one job's phase and running count to a file the bridge polls.
///
/// Cloneable only behind an `Arc`: a single run has one record, and every worker that counts into
/// it shares that one. All mutable state is atomic or behind a mutex that is taken only on the
/// emission path, so a tick from a sync worker costs one relaxed increment and a compare.
#[derive(Debug)]
pub struct ProgressRecorder {
    final_path: PathBuf,
    temporary_path: PathBuf,
    request_id: String,
    job_id: String,
    phases: &'static [PhaseSpec],
    /// One-based index into `phases`; [`NO_PHASE`] until the first phase is entered.
    phase: AtomicUsize,
    count: AtomicU64,
    last_write: Mutex<Option<Instant>>,
}

impl ProgressRecorder {
    /// Build a recorder for `path`, or `None` if this invocation should publish nothing.
    ///
    /// `path` names the record file and carries the job's identity in its stem, as
    /// `<request id>.<job id>.json`. Both halves of the binding travel in the path because the
    /// core is handed one value and must be able to write a record that names the request *and*
    /// the job it belongs to -- that pair is what stops a record written for one request being
    /// reported against another, and the reader checks it against ids it derived itself.
    ///
    /// Returns `None` rather than an error for every rejection, including an unparseable stem and
    /// an operation with no catalogue. Publishing nothing is always a safe outcome; failing a sync
    /// because its progress file was named wrongly is not.
    #[must_use]
    pub fn new(path: &Path, operation: &str) -> Option<Self> {
        let phases = operation_phases(operation)?;
        let stem = path.file_stem()?.to_str()?;
        let (request_id, job_id) = stem.split_once('.')?;
        // Shape only. The bridge re-validates both ids against the ones it resolved for itself, so
        // this is hygiene against a malformed argument rather than the security boundary.
        if !is_lowercase_hex(request_id) || !is_lowercase_hex(job_id) || job_id.contains('.') {
            return None;
        }
        let parent = path.parent()?;
        let file_name = path.file_name()?.to_str()?;
        Some(Self {
            final_path: path.to_owned(),
            temporary_path: parent.join(format!(".{file_name}.tmp")),
            request_id: request_id.to_owned(),
            job_id: job_id.to_owned(),
            phases,
            phase: AtomicUsize::new(NO_PHASE),
            count: AtomicU64::new(0),
            last_write: Mutex::new(None),
        })
    }

    /// Enter a named phase, resetting the running count and publishing immediately.
    ///
    /// A phase change always writes, regardless of the interval: phases are the part of progress
    /// an operator actually navigates by, there are at most sixteen of them in the whole design,
    /// and holding one back for up to two seconds would report the wrong step for longer than the
    /// short phases take to run.
    ///
    /// An id this operation's catalogue does not contain is ignored, which keeps a mistyped call
    /// site from silently publishing some other operation's step number.
    pub fn phase(&self, id: &str) {
        let Some(index) = self.phases.iter().position(|spec| spec.id == id) else {
            return;
        };
        let step = index + 1;
        if self.phase.swap(step, Ordering::Relaxed) == step {
            return;
        }
        self.count.store(0, Ordering::Relaxed);
        self.write_now(step, 0);
        if let Ok(mut last) = self.last_write.lock() {
            *last = Some(Instant::now());
        }
    }

    /// Count one item within the current phase.
    pub fn tick(&self) {
        self.add(1);
    }

    /// Count `amount` items within the current phase.
    ///
    /// The whole hot path is here: a relaxed increment and a modulo. Everything more expensive is
    /// behind the `COUNT_TRIGGER` boundary, and the write itself is behind the interval as well.
    pub fn add(&self, amount: u64) {
        if amount == 0 {
            return;
        }
        let total = self.count.fetch_add(amount, Ordering::Relaxed) + amount;
        // True exactly when this increment crossed a multiple of the trigger, so a caller that
        // adds in batches still wakes the writer once per boundary rather than never.
        if total % COUNT_TRIGGER >= amount {
            return;
        }
        let step = self.phase.load(Ordering::Relaxed);
        if step == NO_PHASE {
            return;
        }
        let Ok(mut last) = self.last_write.lock() else {
            return;
        };
        let now = Instant::now();
        if last.is_some_and(|previous| now.duration_since(previous) < WRITE_INTERVAL) {
            return;
        }
        self.write_now(step, total);
        *last = Some(now);
    }

    /// Write one record: create private, write, rename over the old one.
    ///
    /// Cost is five syscalls and no fork: `open`, `fchmod`, `write`, `close`, `rename`. The
    /// document is a little over 200 bytes, comfortably inside the reader's 1 KiB ceiling, and it
    /// is deliberately not fsynced -- a progress record that is lost to a power cut costs a
    /// dashboard one stale step, and paying for durability here on an armv7 unit would cost the
    /// sync far more than the record is worth.
    ///
    /// The rename is what makes a partial record unobservable: a reader either sees the previous
    /// document or this one, never half of either.
    fn write_now(&self, step: usize, count: u64) {
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        let Some(spec) = self.phases.get(step.wrapping_sub(1)) else {
            return;
        };
        let document = format!(
            concat!(
                "{{\"schema\":\"{}\",\"request_id\":\"{}\",\"job_id\":\"{}\",",
                "\"section\":\"{}\",\"updated_at\":{},\"count\":{}}}"
            ),
            PROGRESS_SCHEMA, self.request_id, self.job_id, spec.id, updated_at, count
        );
        let _ = self.publish(document.as_bytes());
    }

    fn publish(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = options.open(&self.temporary_path)?;
        set_private(&file)?;
        file.write_all(bytes)?;
        drop(file);
        fs::rename(&self.temporary_path, &self.final_path)
    }
}

/// The staging file is this writer's own; the published record is not.
///
/// Only the temporary file is cleaned up here. The record itself outlives the run deliberately:
/// the process that created the path is the one that removes it, which is what keeps a core that
/// was killed mid-phase from being the reason a record disappears, and what makes the published
/// document observable after the fact rather than only during the run. A record left behind is
/// inert in any case -- it is bound to one request and one job, and progress is rendered only
/// while that job is pending.
impl Drop for ProgressRecorder {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary_path);
    }
}

/// The reader accepts a record only at mode 0600 and owned by the package user.
///
/// Set explicitly rather than relying on the creation mode, because the inherited umask can clear
/// bits from it and the resulting file would be refused on read -- which would show up as progress
/// that silently never appears, the hardest failure in this design to notice.
#[cfg(unix)]
fn set_private(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

/// Nothing reads the record on a non-Unix host, so there are no permissions to satisfy.
#[cfg(not(unix))]
fn set_private(_file: &fs::File) -> std::io::Result<()> {
    Ok(())
}

fn is_lowercase_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn temporary_directory(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("sdsync-progress-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("temporary directory");
        directory
    }

    fn record_path(directory: &Path) -> PathBuf {
        directory.join(format!("{}.{}.json", "0".repeat(32), "a".repeat(48)))
    }

    fn read_document(path: &Path) -> BTreeMap<String, serde_json::Value> {
        let bytes = fs::read(path).expect("a record must exist");
        serde_json::from_slice(&bytes).expect("the record must be a JSON object")
    }

    /// The published document is exactly the six keys the reader validates, and no more.
    ///
    /// The UI validates this object by exact key count, so a seventh key does not produce a
    /// slightly wrong render -- it drops the whole record and progress silently disappears. That
    /// makes the key set worth pinning on the writing side as well as the reading side.
    #[test]
    fn a_published_record_carries_exactly_the_six_agreed_keys() {
        let directory = temporary_directory("six-keys");
        let path = record_path(&directory);
        let recorder = ProgressRecorder::new(&path, "sync-status").expect("the path must parse");

        recorder.phase("compare");
        let document = read_document(&path);
        assert_eq!(
            document.keys().cloned().collect::<Vec<_>>(),
            [
                "count",
                "job_id",
                "request_id",
                "schema",
                "section",
                "updated_at"
            ],
            "the record's key set is pinned on both sides of the wire"
        );
        assert_eq!(document["schema"], serde_json::json!(PROGRESS_SCHEMA));
        assert_eq!(document["section"], serde_json::json!("compare"));
        assert_eq!(document["count"], serde_json::json!(0));

        drop(recorder);
        let _ = fs::remove_dir_all(&directory);
    }

    /// A phase change publishes at once; ticks inside a phase are held to the interval.
    ///
    /// This is the negative control for the throttle. Without it the writer would issue one write
    /// and one rename per file, which on an armv7 unit walking twenty thousand files is the
    /// difference between a rounding error and a measurable tax on the sync it is reporting.
    #[test]
    fn phase_changes_publish_immediately_and_ticks_are_throttled() {
        let directory = temporary_directory("throttle");
        let path = record_path(&directory);
        let recorder = ProgressRecorder::new(&path, "sync-status").expect("the path must parse");

        recorder.phase("scan_local");
        assert_eq!(
            read_document(&path)["section"],
            serde_json::json!("scan_local")
        );

        // Far more than the count trigger, all of it inside one interval: the record must still
        // report the count from the phase change rather than the latest tick.
        for _ in 0..(COUNT_TRIGGER * 8) {
            recorder.tick();
        }
        assert_eq!(
            read_document(&path)["count"],
            serde_json::json!(0),
            "an emission inside the interval must be suppressed"
        );

        recorder.phase("compare");
        let document = read_document(&path);
        assert_eq!(document["section"], serde_json::json!("compare"));
        assert_eq!(
            document["count"],
            serde_json::json!(0),
            "entering a phase restarts its count"
        );

        drop(recorder);
        let _ = fs::remove_dir_all(&directory);
    }

    /// Nothing is published for a path or an operation that does not resolve.
    #[test]
    fn an_unusable_path_or_operation_publishes_nothing() {
        let directory = temporary_directory("rejects");
        let request = "0".repeat(32);
        let job = "a".repeat(48);

        for (case, path) in [
            (
                "no job id in the stem",
                directory.join(format!("{request}.json")),
            ),
            (
                "the request id is not hex",
                directory.join(format!("not-hex.{job}.json")),
            ),
            (
                "the job id is not hex",
                directory.join(format!("{request}.NOT-HEX.json")),
            ),
        ] {
            assert!(
                ProgressRecorder::new(&path, "sync-status").is_none(),
                "{case} must publish nothing"
            );
        }
        assert!(
            ProgressRecorder::new(&record_path(&directory), "set-secret").is_none(),
            "an operation with no catalogue must publish nothing"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// A phase this operation does not have is ignored rather than published.
    ///
    /// The catalogue is per-operation on the reading side too, so publishing another operation's
    /// id would produce a record the bridge drops -- progress that silently never appears. Failing
    /// at the call site instead keeps the mistake local to the phase that was mistyped.
    #[test]
    fn a_phase_outside_this_operations_catalogue_is_ignored() {
        let directory = temporary_directory("foreign-phase");
        let path = record_path(&directory);
        let recorder = ProgressRecorder::new(&path, "sync-status").expect("the path must parse");

        recorder.phase("upload");
        assert!(
            !path.exists(),
            "a phase from another catalogue must not be published"
        );
        recorder.phase("scan_local");
        assert_eq!(
            read_document(&path)["section"],
            serde_json::json!("scan_local")
        );

        drop(recorder);
        let _ = fs::remove_dir_all(&directory);
    }

    /// Every phase id in every catalogue resolves through the library resolver.
    ///
    /// The writer names phases by string, so a catalogue entry that the resolver cannot find would
    /// be a phase that is published and then dropped on read. Walking both tables together is what
    /// makes that unrepresentable rather than merely unlikely.
    #[test]
    fn every_catalogued_phase_resolves_for_its_own_operation() {
        for (operation, specs) in crate::PHASE_SPECS {
            for (index, spec) in specs.iter().enumerate() {
                let resolved = crate::operation_phase(operation, spec.id)
                    .unwrap_or_else(|| panic!("{operation}/{} did not resolve", spec.id));
                assert_eq!(resolved.label, spec.label);
                assert_eq!(usize::from(resolved.step), index + 1);
                assert_eq!(usize::from(resolved.total), specs.len());
                assert_eq!(resolved.unit, spec.unit);
            }
        }
    }
}
