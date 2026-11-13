//! Paths that keep hitting the same transient cause, counted across runs (fix8 R6, R7).
//!
//! A file that is always being written (an Office lock file, an open mailbox or database, a log)
//! changes under every run, so every run defers it as a transient change and ends partial. Exit 3
//! keeps a realtime routine's change pending, so the routine re-ran for it forever, and the
//! partial-run breaker only slowed that down. The same holds for a file locked for good
//! (`source-unreadable`), one the NAS cannot checksum (`remote-digest-unavailable`) and a deletion
//! DSM keeps refusing for a reason other than permission (`delete-failed`). This record, beside
//! the profile's index, counts the consecutive runs that deferred each path for one such cause
//! ([`DeferReason::streak`]). At [`KEEPS_CHANGING_RUNS`] of the same cause the deferral becomes
//! that cause's lasting reason (`file-keeps-changing`, `source-stays-unreadable`,
//! `remote-digest-keeps-failing`, `delete-keeps-failing`): still named, still tried on every run,
//! but no longer a reason to run again. The count, and so the name, clears the first run the path
//! is not deferred for that cause.
//!
//! The record is independent of the index's database, so it works when the index itself cannot
//! be used. One that cannot be read or written is named (`pair-index-unsettled-unusable`), and its
//! paths stay transient: the safe direction, since a run that is partial is retried.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::IndexIssue;
use crate::plan::Scope;
use crate::sync::{DeferReason, DeferredPath};

/// Consecutive runs that defer a path for the same cause before it is named as lasting.
pub const KEEPS_CHANGING_RUNS: u32 = 3;

/// Paths the record keeps at most. Beyond it a path is not counted, so it stays a transient
/// deferral: a run is retried rather than a path quietly given up on.
pub const UNSETTLED_LIMIT: usize = 1024;

const SCHEMA: &str = "sdsync.unsettled.v1";
const ISSUE: &str = "pair-index-unsettled-unusable";

/// The record's file for a profile whose index file is `database` (`<stem>.redb`).
pub fn record_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".unsettled");
    PathBuf::from(name)
}

/// The consecutive-run counts of one profile, bound to its source and destination.
#[derive(Debug)]
pub struct UnsettledRecord {
    path: PathBuf,
    source: String,
    remote: String,
    /// Path to the streak it is in (see [`DeferReason::streak`]) and how many runs long.
    runs: BTreeMap<String, (String, u32)>,
}

impl UnsettledRecord {
    /// Read the record. A missing file, or one written for another source or destination, is an
    /// empty record. One that cannot be read or does not parse is named, and the run counts from
    /// empty.
    pub fn load(path: &Path, source: &str, remote: &str) -> (Self, Option<IndexIssue>) {
        let mut record = Self {
            path: path.to_path_buf(),
            source: source.to_owned(),
            remote: remote.to_owned(),
            runs: BTreeMap::new(),
        };
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (record, None),
            Err(error) => {
                return (
                    record,
                    Some(record_issue(path, &format!("could not be read: {error}"))),
                );
            }
        };
        let parsed: Option<BTreeMap<String, (String, u32)>> = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(|value| value["schema"] == SCHEMA)
            .and_then(|value| {
                if value["source"] != source || value["remote"] != remote {
                    // Another source or destination: its counts are about other files.
                    return Some(BTreeMap::new());
                }
                value["runs"].as_object().and_then(|runs| {
                    runs.iter()
                        .map(|(relative, streak)| {
                            let cause = streak["cause"].as_str()?;
                            streak["runs"]
                                .as_u64()
                                .and_then(|count| u32::try_from(count).ok())
                                .filter(|count| *count > 0)
                                .map(|count| (relative.clone(), (cause.to_owned(), count)))
                        })
                        .collect()
                })
            });
        match parsed {
            Some(runs) => {
                record.runs = runs;
                (record, None)
            }
            None => (
                record,
                Some(record_issue(
                    path,
                    "is not a record this version wrote; counting starts again from this run",
                )),
            ),
        }
    }

    /// Count this run's deferrals and name the paths that keep hitting the same cause.
    ///
    /// Every `deferred` record whose reason has a streak ([`DeferReason::streak`]) extends its
    /// path's streak when the previous run's was the same cause, and starts one otherwise; at
    /// [`KEEPS_CHANGING_RUNS`] the record becomes the streak's lasting reason, with the count and
    /// the last cause in its detail (`file_keeps_changing path=<p> runs=<n>; last cause ...` for a
    /// change, `keeps_failing path=<p> runs=<n> cause=<reason>; ...` otherwise). A path in the
    /// run's scope that this run did not defer for such a cause has settled, and its streak is
    /// dropped; one outside the scope keeps it. Returns how many were named.
    pub fn count(&mut self, deferred: &mut [DeferredPath], scope: &Scope) -> usize {
        let mut runs: BTreeMap<String, (String, u32)> = self
            .runs
            .iter()
            .filter(|(relative, _)| !in_scope(relative, scope))
            .map(|(relative, streak)| (relative.clone(), streak.clone()))
            .collect();
        let mut named = 0;
        for record in deferred.iter_mut() {
            let Some((cause, lasting)) = record.reason.streak() else {
                continue;
            };
            if runs.contains_key(&record.relative) {
                continue;
            }
            if runs.len() >= UNSETTLED_LIMIT {
                // Not counted: it stays transient, and the run is retried.
                continue;
            }
            let count = match self.runs.get(&record.relative) {
                Some((previous, count)) if previous == cause => count.saturating_add(1),
                _ => 1,
            };
            runs.insert(record.relative.clone(), (cause.to_owned(), count));
            if count >= KEEPS_CHANGING_RUNS {
                let path =
                    crate::observability::InlineAscii::<160>::percent_encoded(&record.relative);
                record.detail = if lasting == DeferReason::FileKeepsChanging {
                    format!(
                        "file_keeps_changing path={} runs={count}; last cause {}: {}",
                        path.as_str(),
                        record.reason,
                        record.detail
                    )
                } else {
                    format!(
                        "keeps_failing path={} runs={count} cause={}; {}",
                        path.as_str(),
                        record.reason,
                        record.detail
                    )
                };
                record.reason = lasting;
                named += 1;
            }
        }
        self.runs = runs;
        named
    }

    /// How many paths the record counts.
    pub fn len(&self) -> usize {
        self.runs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Write the record (private, replaced atomically), or remove it when it counts nothing.
    pub fn store(&self) -> Result<(), IndexIssue> {
        if self.runs.is_empty() {
            return match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(record_issue(
                    &self.path,
                    &format!("could not be removed: {error}"),
                )),
            };
        }
        let runs: serde_json::Map<String, Value> = self
            .runs
            .iter()
            .map(|(relative, (cause, count))| {
                (relative.clone(), json!({"cause": cause, "runs": count}))
            })
            .collect();
        let text = json!({
            "schema": SCHEMA,
            "source": self.source,
            "remote": self.remote,
            "runs": runs,
        })
        .to_string();
        let mut temporary = self.path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let written = (|| -> std::io::Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
        })();
        written.map_err(|error| {
            // The temporary file is ours; a failure to remove it is part of the same named issue.
            let leftover = match fs::remove_file(&temporary) {
                Ok(()) => String::new(),
                Err(remove) if remove.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(remove) => format!(
                    "; {} also could not be removed: {remove}",
                    temporary.display()
                ),
            };
            record_issue(
                &self.path,
                &format!("could not be written: {error}{leftover}"),
            )
        })
    }
}

/// A deferral is recorded for a file or a folder; either way, the run looked at it only if it is
/// where a file of the scope could be ([`Scope::matches`]).
fn in_scope(relative: &str, scope: &Scope) -> bool {
    scope.matches(relative)
}

fn record_issue(path: &Path, cause: &str) -> IndexIssue {
    IndexIssue::warning(
        ISSUE,
        path.display().to_string(),
        format!(
            "the record of paths that keep hitting the same cause {cause}; they stay ordinary \
             transient deferrals (partial runs) until it can be used"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(relative: &str) -> DeferredPath {
        DeferredPath {
            relative: relative.to_owned(),
            reason: DeferReason::SourceChanged,
            detail: "the file changed while it was read".to_owned(),
        }
    }

    fn directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "sdsync-unsettled-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn run(path: &Path, deferred: &mut [DeferredPath]) -> usize {
        let (mut record, issue) = UnsettledRecord::load(path, "/src", "/share/dst");
        assert!(issue.is_none(), "{issue:?}");
        let named = record.count(deferred, &Scope::root());
        record.store().unwrap();
        named
    }

    /// The third consecutive run that defers a path for a change names it lasting; a run that
    /// does not defer it clears the count, and so the name.
    #[test]
    fn a_path_changing_in_three_consecutive_runs_is_named_lasting_until_it_settles() {
        let directory = directory("three");
        let path = record_path(&directory.join("office.redb"));
        for round in 1..=2 {
            let mut deferred = vec![changed("~$Report.docx"), changed("mail.pst")];
            assert_eq!(run(&path, &mut deferred), 0, "run {round}");
            assert!(deferred.iter().all(|record| !record.reason.is_lasting()));
        }
        // mail.pst settled in run 3; the lock file did not.
        let mut deferred = vec![changed("~$Report.docx")];
        assert_eq!(run(&path, &mut deferred), 1);
        assert_eq!(deferred[0].reason, DeferReason::FileKeepsChanging);
        assert!(deferred[0].reason.is_lasting());
        assert!(
            deferred[0].detail.starts_with(
                "file_keeps_changing path=%7E%24Report.docx runs=3; last cause source-changed: "
            ),
            "{}",
            deferred[0].detail
        );
        // mail.pst starts again from one.
        let mut deferred = vec![changed("mail.pst"), changed("~$Report.docx")];
        assert_eq!(run(&path, &mut deferred), 1);
        assert_eq!(deferred[0].reason, DeferReason::SourceChanged);
        assert!(deferred[1].detail.contains("runs=4"));
        // Settled: nothing is counted and the file is removed.
        assert_eq!(run(&path, &mut []), 0);
        assert!(!path.exists());
        let mut deferred = vec![changed("~$Report.docx")];
        assert_eq!(run(&path, &mut deferred), 0, "the count started again");
        let _ = fs::remove_dir_all(&directory);
    }

    /// R7: a file locked for good, a checksum the NAS keeps failing and a deletion it keeps
    /// refusing each become lasting after three runs of the same cause, name the cause, and clear
    /// when the path succeeds. A change of cause starts the count again.
    #[test]
    fn every_looping_cause_is_named_lasting_after_three_runs_of_it() {
        let directory = directory("classes");
        let path = record_path(&directory.join("p.redb"));
        let failing = |relative: &str, reason| DeferredPath {
            relative: relative.to_owned(),
            reason,
            detail: "DSM error 900".to_owned(),
        };
        let classes = [
            (
                DeferReason::SourceUnreadable,
                DeferReason::SourceStaysUnreadable,
            ),
            (
                DeferReason::RemoteDigestUnavailable,
                DeferReason::RemoteDigestKeepsFailing,
            ),
            (DeferReason::DeleteFailed, DeferReason::DeleteKeepsFailing),
        ];
        for round in 1..=3 {
            let mut deferred: Vec<_> = classes
                .iter()
                .map(|(reason, _)| failing(reason.as_str(), *reason))
                .collect();
            let named = run(&path, &mut deferred);
            for (record, (reason, lasting)) in deferred.iter().zip(classes) {
                if round < 3 {
                    assert_eq!(record.reason, reason, "run {round}");
                    assert!(!record.reason.is_lasting());
                } else {
                    assert_eq!(named, 3);
                    assert_eq!(record.reason, lasting);
                    assert!(lasting.is_lasting());
                    assert_eq!(
                        record.detail,
                        format!("keeps_failing path={reason} runs=3 cause={reason}; DSM error 900")
                    );
                }
            }
        }
        // The unreadable file is read at last, the digest fails for another reason (a change),
        // and the deletion still fails: only the last stays named.
        let mut deferred = vec![
            changed("remote-digest-unavailable"),
            failing("delete-failed", DeferReason::DeleteFailed),
        ];
        assert_eq!(run(&path, &mut deferred), 1);
        assert_eq!(
            deferred[0].reason,
            DeferReason::SourceChanged,
            "a new cause counts from one"
        );
        assert_eq!(deferred[1].reason, DeferReason::DeleteKeepsFailing);
        assert!(deferred[1].detail.contains("runs=4"));
        let mut deferred = vec![failing("source-unreadable", DeferReason::SourceUnreadable)];
        assert_eq!(
            run(&path, &mut deferred),
            0,
            "its streak was cleared when it was read"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// Only a looping cause counts; a scoped run keeps the counts outside its scope; a record for
    /// another source starts from empty; a damaged one is named.
    #[test]
    fn the_record_counts_only_changes_and_names_what_it_cannot_read() {
        let directory = directory("rules");
        let path = record_path(&directory.join("p.redb"));
        let lasting = DeferredPath {
            reason: DeferReason::RemoteRefused,
            ..changed("refused.db")
        };
        for _ in 0..3 {
            let mut deferred = vec![lasting.clone(), changed("a/log.txt"), changed("b/log.txt")];
            run(&path, &mut deferred);
            assert_eq!(deferred[0].reason, DeferReason::RemoteRefused);
        }
        let (mut record, _) = UnsettledRecord::load(&path, "/src", "/share/dst");
        assert_eq!(record.len(), 2);
        record.count(&mut [], &Scope::parse("a").unwrap());
        assert_eq!(record.len(), 1, "b/log.txt is outside the scope and kept");

        let (other, issue) = UnsettledRecord::load(&path, "/elsewhere", "/share/dst");
        assert!(issue.is_none());
        assert!(other.is_empty());

        fs::write(&path, b"not json").unwrap();
        let (record, issue) = UnsettledRecord::load(&path, "/src", "/share/dst");
        assert!(record.is_empty());
        let issue = issue.expect("a damaged record is named");
        assert_eq!(issue.code, ISSUE);
        let _ = fs::remove_dir_all(&directory);
    }
}
