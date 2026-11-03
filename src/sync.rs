use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use crate::api::{ApiClient, RemoteInventory, RemotePathState, UploadObserver};
use crate::cancel::CancellationToken;
use crate::local::{IgnoreRules, LocalEntry, LocalInventory};
use crate::path::RemoteRoot;
use crate::plan::{CompareMode, PlanOptions, RemoteSnapshot, SyncPlan};
use crate::{Error, Result};

/// What standard of proof a snapshot check's content comparison takes.
///
/// Recorded on the check rather than inferred from whether a digest is present, because the
/// presence of a digest says nothing about where the bytes came from — and that is the only thing
/// that decides which evidence is sufficient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentEvidence {
    /// Content this process uploaded moments ago, against a fingerprint of the bytes it sent.
    /// The destination's length and File Station's own MD5 settle it; transferring the file back
    /// to re-hash it proves nothing further and comes from the same server over the same session.
    Uploaded,
    /// Content the *server* produced from bytes this process never saw (a server-side copy), or a
    /// digest that authorises destroying data (a mirror deletion's guard). Read the bytes back and
    /// hash them MD5, CRC32 and SHA-256.
    ServerProduced,
}

#[derive(Clone, Debug)]
struct SnapshotCheck {
    remote_path: String,
    expected: RemoteSnapshot,
    deleting: bool,
    evidence: ContentEvidence,
    /// The local modification time, in milliseconds, when the expected time was taken from the
    /// *local* clock rather than from a remote listing. Such a check compares with
    /// [`crate::plan::mtimes_agree`], which knows that DSM may floor or round the milliseconds, and
    /// a time outside it is a warning rather than a change.
    ///
    /// Set for every destination this run wrote (an upload, a server copy, a copy's guard). The
    /// content of those is proved by size and digest; the modification time is only whatever DSM
    /// chose to keep, and whether DSM rounds or floors the milliseconds an upload sends, or stamps
    /// a server copy with the time of the copy, cannot be settled from here. Requiring an exact
    /// second failed every mirror run on a DSM that rounds.
    local_mtime_ms: Option<i64>,
}

/// How one entry of a snapshot batch compared with the destination re-read for the batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotVerdict {
    Matches,
    /// Matches, except that DSM did not keep the modification time this run sent.
    MtimeNotKept,
    /// Absent, a different kind, a different size, a different path, or (for a check whose time
    /// came from a remote listing) a different modification time.
    Changed,
}

trait SyncOperations: Clone + Send + Sync {
    fn preflight_upload_readable(&self, local: &crate::local::LocalEntry) -> Result<()>;
    fn preflight_upload_source(
        &self,
        local: &crate::local::LocalEntry,
        cancellation: &CancellationToken,
    ) -> Result<()>;
    fn delete_non_recursive(&self, root: &RemoteRoot, remote_path: &str) -> Result<()>;
    fn create_folder(&self, remote_path: &str) -> Result<()>;
    fn supports_rename(&self) -> bool;
    fn rename(&self, root: &RemoteRoot, remote_path: &str, new_name: &str) -> Result<String>;
    fn copy_file_verified(
        &self,
        root: &RemoteRoot,
        action: &crate::plan::CopyAction,
        cancellation: &CancellationToken,
    ) -> Result<()>;
    fn verify_remote_snapshot_batch(
        &self,
        root: &RemoteRoot,
        scope: &crate::plan::Scope,
        rules: &crate::local::IgnoreRules,
        checks: &[SnapshotCheck],
        cancellation: &CancellationToken,
    ) -> Result<Vec<SnapshotVerdict>>;
    fn verify_remote_metadata_snapshot(
        &self,
        remote_path: &str,
        expected: &RemoteSnapshot,
        cancellation: &CancellationToken,
    ) -> Result<()>;
    fn verify_remote_content(
        &self,
        remote_path: &str,
        expected_size: u64,
        expected_md5: crate::integrity::ContentMd5,
        cancellation: &CancellationToken,
    ) -> Result<()>;
    fn verify_uploaded_content(
        &self,
        remote_path: &str,
        expected_size: u64,
        expected_md5: crate::integrity::ContentMd5,
        cancellation: &CancellationToken,
    ) -> Result<()>;
    fn remote_path_state(&self, remote_path: &str) -> Result<RemotePathState>;
    fn upload(
        &self,
        local: &crate::local::LocalEntry,
        remote_path: &str,
        observer: Option<UploadObserver>,
        cancellation: &CancellationToken,
    ) -> Result<()>;
}

impl SyncOperations for ApiClient {
    fn preflight_upload_readable(&self, local: &crate::local::LocalEntry) -> Result<()> {
        self.preflight_upload_readable(local)
    }

    fn preflight_upload_source(
        &self,
        local: &crate::local::LocalEntry,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.preflight_upload_source(local, cancellation)
    }

    fn delete_non_recursive(&self, root: &RemoteRoot, remote_path: &str) -> Result<()> {
        self.delete_non_recursive(root, remote_path)
    }

    fn create_folder(&self, remote_path: &str) -> Result<()> {
        self.create_folder(remote_path)
    }

    fn supports_rename(&self) -> bool {
        self.supports_rename()
    }

    fn rename(&self, root: &RemoteRoot, remote_path: &str, new_name: &str) -> Result<String> {
        self.rename(root, remote_path, new_name)
    }

    fn copy_file_verified(
        &self,
        root: &RemoteRoot,
        action: &crate::plan::CopyAction,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.copy_file_verified(
            root,
            &action.from_remote_path,
            &action.to_remote_path,
            action.expected_size,
            action.content_md5,
            cancellation,
        )
    }

    fn verify_remote_snapshot_batch(
        &self,
        root: &RemoteRoot,
        scope: &crate::plan::Scope,
        rules: &crate::local::IgnoreRules,
        checks: &[SnapshotCheck],
        cancellation: &CancellationToken,
    ) -> Result<Vec<SnapshotVerdict>> {
        verify_api_remote_snapshot_batch(self, root, scope, rules, checks, cancellation)
    }

    fn verify_remote_metadata_snapshot(
        &self,
        remote_path: &str,
        expected: &RemoteSnapshot,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.verify_remote_metadata_snapshot(
            remote_path,
            expected.kind,
            expected.size,
            expected.mtime_seconds,
            expected.require_mtime,
            cancellation,
        )
    }

    fn verify_remote_content(
        &self,
        remote_path: &str,
        expected_size: u64,
        expected_md5: crate::integrity::ContentMd5,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.verify_remote_content(remote_path, expected_size, expected_md5, cancellation)
    }

    fn verify_uploaded_content(
        &self,
        remote_path: &str,
        expected_size: u64,
        expected_md5: crate::integrity::ContentMd5,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.verify_uploaded_content(remote_path, expected_size, expected_md5, cancellation)
    }

    fn remote_path_state(&self, remote_path: &str) -> Result<RemotePathState> {
        self.remote_path_state(remote_path)
    }

    fn upload(
        &self,
        local: &crate::local::LocalEntry,
        remote_path: &str,
        observer: Option<UploadObserver>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.upload_observed(local, remote_path, observer, cancellation)
    }
}

/// Re-read the destination once and judge every snapshot in the batch against it.
///
/// Scoped, and pruned by the run's own rules, because it has to answer the same question the
/// planner's inventory answered. An unscoped walk here was both a cost and a correctness problem: a
/// `--scope docs/q3` run paid a full recursive inventory of the whole destination per batch, up to
/// four times per run, and a subtree outside the scope that the sync account cannot list returns DSM
/// code 105, which propagates -- failing a scoped run *after its uploads had landed* for a directory
/// it was explicitly told to ignore, and reporting it as a snapshot problem.
///
/// Returns one verdict per check rather than the first mismatch, so the caller can keep exactly the
/// entries that changed and act on the rest. Before this, one remote file edited by someone else
/// during a long run failed the run and took every other planned deletion with it.
fn verify_api_remote_snapshot_batch(
    client: &ApiClient,
    root: &RemoteRoot,
    scope: &crate::plan::Scope,
    rules: &crate::local::IgnoreRules,
    checks: &[SnapshotCheck],
    cancellation: &CancellationToken,
) -> Result<Vec<SnapshotVerdict>> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    cancellation.check()?;
    let current_inventory = client
        .remote_inventory_scoped(root, scope, rules, usize::MAX, cancellation)?
        .inventory;
    cancellation.check()?;
    // Keyed by the remote path, not by the check's relative: a file whose local spelling is the NFD
    // form of an NFC remote name is uploaded to the remote spelling, and looking the local spelling
    // up in a remote inventory reported a change that never happened.
    let keys = checks
        .iter()
        .map(|check| root.relative(&check.remote_path).ok())
        .collect::<Vec<_>>();
    let deleting: BTreeSet<&str> = checks
        .iter()
        .zip(&keys)
        .filter(|(check, _)| check.deleting)
        .filter_map(|(_, key)| key.as_deref())
        .collect();
    let verdicts = checks
        .iter()
        .zip(&keys)
        .map(|(check, key)| match key {
            Some(key) => snapshot_verdict(check, key, &current_inventory, &deleting),
            None => SnapshotVerdict::Changed,
        })
        .collect();
    cancellation.check()?;
    Ok(verdicts)
}

fn snapshot_verdict(
    check: &SnapshotCheck,
    key: &str,
    current_inventory: &RemoteInventory,
    deleting: &BTreeSet<&str>,
) -> SnapshotVerdict {
    let Some(current) = current_inventory.entries.get(key) else {
        return SnapshotVerdict::Changed;
    };
    if current.remote_path != check.remote_path
        || current.kind != check.expected.kind
        || current.size != check.expected.size
    {
        return SnapshotVerdict::Changed;
    }
    if check.deleting && check.expected.kind == crate::local::EntryKind::Directory {
        let prefix = format!("{key}/");
        if current_inventory
            .entries
            .range::<str, _>((std::ops::Bound::Excluded(key), std::ops::Bound::Unbounded))
            .take_while(|(candidate, _)| candidate.starts_with(&prefix))
            .any(|(candidate, _)| !deleting.contains(candidate.as_str()))
        {
            return SnapshotVerdict::Changed;
        }
    }
    if check.expected.require_mtime && current.mtime_seconds != check.expected.mtime_seconds {
        let Some(local_mtime_ms) = check.local_mtime_ms else {
            return SnapshotVerdict::Changed;
        };
        if !crate::plan::mtimes_agree(local_mtime_ms, current.mtime_seconds) {
            return SnapshotVerdict::MtimeNotKept;
        }
    }
    SnapshotVerdict::Matches
}

/// Keep the first reported upload failure, except that a substantive failure always outranks a
/// cancellation.
///
/// Once one worker fails, the others abort their in-flight transfers and report `Cancelled`. The
/// order those results reach the collector is not deterministic, so without this preference a
/// real transport or DSM failure could be reported as "operation cancelled" and exit 130 instead
/// of 1. A genuine Ctrl-C produces only `Cancelled` results and is unaffected.
fn record_worker_error(first_error: &mut Option<Error>, error: Error) {
    match first_error {
        None => *first_error = Some(error),
        Some(Error::Cancelled) if !matches!(error, Error::Cancelled) => *first_error = Some(error),
        Some(_) => {}
    }
}

/// Whether a remote-only path is present locally, at the spelling the plan paired it with.
#[derive(Clone, Debug, Eq, PartialEq)]
enum LocalPresence {
    Absent,
    /// Present, with the bytes of the local spelling found. Each component is the paired one
    /// exactly, or the one local name equal to it under Unicode normalisation.
    Present {
        spelling: String,
    },
    /// More than one local name in one folder is equal to a component under Unicode normalisation,
    /// so which of them stands for the remote entry cannot be told.
    Ambiguous {
        path: String,
    },
    /// The remote name cannot exist on this host's filesystem at all: a Windows host cannot hold
    /// `notes.` or `notes ` (Win32 strips the trailing dot or space and finds `notes` instead),
    /// nor a reserved device name or a character NTFS refuses.
    Unrepresentable,
}

/// Re-establish the precondition that made a remote entry a deletion candidate.
///
/// A mirror deletion is selected because the path is absent from the local inventory, and that
/// inventory was read before the upload phase — minutes to hours earlier on a large content-mode
/// tree. Every other precondition in this function is re-checked immediately before the mutation
/// it guards; this one was the exception, and the window it left is not theoretical. An editor or
/// build step that unlinks and recreates rather than renaming leaves the file absent for the
/// instant the scan reads that directory, and the remote copy — with every revision DSM holds of
/// it — is then deleted while a live local file sits next to it.
///
/// `relative` is the deletion's recorded local spelling ([`crate::plan::DeleteAction::local_relative`]):
/// beneath a case-renamed or a normalisation-paired folder, the folder is spelled the local way.
///
/// Each component is matched by its exact bytes or, failing that, by the single local name equal to
/// it under Unicode normalisation: the planner pairs NFC and NFD spellings of one name, and a file
/// recreated as `naïve.txt` decomposed beneath a paired folder is the file the remote composed
/// `naïve.txt` stands for. The bytes found are returned, so a kept deletion uploads that file.
/// Never by case: Windows and macOS resolve a path case-insensitively and Win32 strips a trailing
/// dot or space, so a plain stat of the remote spelling finds a *different* local file, and its
/// bytes were uploaded under the remote spelling on every run. So a stat is never taken as the
/// answer; each parent directory is read and every name compared. A name the host cannot represent
/// is reported as such rather than resolved to its alias.
///
/// Reported as absent only on a definite `NotFound` or no matching name. Any other error means
/// absence was not established, and a delete may not proceed on a precondition nobody could read:
/// the caller sees the filesystem error instead.
fn local_presence(source_root: &std::path::Path, relative: &str) -> Result<LocalPresence> {
    if !representable_on_this_host(relative) {
        return Ok(LocalPresence::Unrepresentable);
    }
    let path = source_root.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        // No other spelling of this path is equal to it under normalisation, so a stat that finds
        // nothing settles it without reading a directory.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && has_a_single_normal_form(relative) =>
        {
            return Ok(LocalPresence::Absent);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidFilename => {
            return Ok(LocalPresence::Unrepresentable);
        }
        Err(source) => return Err(Error::FileIo { path, source }),
    }
    find_local_spelling(source_root, relative)
}

/// Whether no other string is equal to `relative` under Unicode normalisation.
///
/// True of ASCII, except the three characters a compatibility singleton composes to: KELVIN SIGN to
/// `K`, GREEK QUESTION MARK to `;`, and GREEK VARIA to a backtick.
fn has_a_single_normal_form(relative: &str) -> bool {
    relative
        .bytes()
        .all(|byte| byte.is_ascii() && !matches!(byte, b'K' | b';' | b'`'))
}

/// Walk `relative` down from `source_root`, one directory listing per component.
fn find_local_spelling(source_root: &std::path::Path, relative: &str) -> Result<LocalPresence> {
    let mut parent = source_root.to_path_buf();
    let mut spelling = String::with_capacity(relative.len());
    for component in relative.split('/') {
        let wanted = crate::path::nfc(component);
        let entries = match std::fs::read_dir(&parent) {
            Ok(entries) => entries,
            // A folder found a moment ago is gone again: absent.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LocalPresence::Absent);
            }
            Err(source) => {
                return Err(Error::FileIo {
                    path: parent,
                    source,
                });
            }
        };
        let mut exact = false;
        let mut equivalent: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::FileIo {
                path: parent.clone(),
                source,
            })?;
            let name = entry.file_name();
            if name == std::ffi::OsStr::new(component) {
                exact = true;
            } else if let Some(text) = name.to_str()
                && crate::path::nfc(text) == wanted
            {
                equivalent.push(text.to_owned());
            }
        }
        let found = match (exact, equivalent.len()) {
            (true, 0) => component.to_owned(),
            (false, 1) => equivalent.remove(0),
            (false, 0) => return Ok(LocalPresence::Absent),
            _ => {
                if !spelling.is_empty() {
                    spelling.push('/');
                }
                spelling.push_str(component);
                return Ok(LocalPresence::Ambiguous { path: spelling });
            }
        };
        if !spelling.is_empty() {
            spelling.push('/');
        }
        spelling.push_str(&found);
        parent.push(&found);
    }
    Ok(LocalPresence::Present { spelling })
}
/// Whether this host's filesystem can hold a path spelled `relative` at all.
#[cfg(windows)]
fn representable_on_this_host(relative: &str) -> bool {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    relative.split('/').all(|component| {
        let stem = component.split('.').next().unwrap_or(component);
        !component.ends_with(['.', ' '])
            && !component
                .chars()
                .any(|character| character < ' ' || "<>:\"\\|?*".contains(character))
            && !RESERVED
                .iter()
                .any(|reserved| stem.trim_end().eq_ignore_ascii_case(reserved))
    })
}

#[cfg(not(windows))]
fn representable_on_this_host(_relative: &str) -> bool {
    true
}

fn delete_snapshot_check(action: &crate::plan::DeleteAction) -> SnapshotCheck {
    SnapshotCheck {
        remote_path: action.remote_path.clone(),
        expected: action.snapshot.clone(),
        deleting: true,
        // This digest authorises destroying remote data and every revision DSM holds of it. It is
        // required to carry full proof, and full proof is not something File Station can compute.
        evidence: ContentEvidence::ServerProduced,
        // Taken from the remote listing itself, so it is exact.
        local_mtime_ms: None,
    }
}

fn destination_snapshot_check(action: &crate::plan::CopyAction) -> SnapshotCheck {
    SnapshotCheck {
        remote_path: action.to_remote_path.clone(),
        expected: RemoteSnapshot {
            kind: crate::local::EntryKind::File,
            size: action.expected_size,
            mtime_seconds: action.local.mtime_ms.div_euclid(1000),
            content_md5: Some(action.content_md5),
            require_mtime: true,
        },
        deleting: false,
        // The NAS duplicated these bytes on its own; this process never saw them.
        evidence: ContentEvidence::ServerProduced,
        // Whether `CopyMove` keeps the source's modification time or stamps the copy with the
        // time it ran is DSM's choice. The bytes are read back and hashed either way.
        local_mtime_ms: Some(action.local.mtime_ms),
    }
}

fn guard_snapshot_check(guard: &crate::plan::DestinationGuard) -> SnapshotCheck {
    SnapshotCheck {
        remote_path: guard.remote_path.clone(),
        expected: RemoteSnapshot {
            kind: crate::local::EntryKind::File,
            size: guard.expected_size,
            mtime_seconds: guard.expected_mtime_seconds,
            content_md5: Some(guard.content_md5),
            require_mtime: true,
        },
        deleting: false,
        // A server-side copy's destination, and the proof that authorises deleting its source.
        // Both halves of that call for the strong standard.
        evidence: ContentEvidence::ServerProduced,
        local_mtime_ms: Some(guard.local.mtime_ms),
    }
}

fn upload_destination_snapshot_check(action: &crate::plan::UploadAction) -> SnapshotCheck {
    SnapshotCheck {
        remote_path: action.remote_path.clone(),
        expected: RemoteSnapshot {
            kind: crate::local::EntryKind::File,
            size: action.local.size,
            mtime_seconds: action.local.mtime_ms.div_euclid(1000),
            content_md5: action.local.content_md5,
            require_mtime: true,
        },
        deleting: false,
        // Content this run uploaded, already verified once against a fingerprint of the bytes that
        // went onto the wire. Before this the check was indistinguishable from a deletion guard, so
        // a `--compare metadata --delete` sync of 1 GB uploaded a gigabyte and then downloaded it
        // straight back and ran a full SHA-256 pass over it -- the exact cost the post-upload
        // verification work was measured against, bypassed on the one path that deletes.
        evidence: ContentEvidence::Uploaded,
        // DSM rounding the uploaded milliseconds rather than flooring them failed every mirror run
        // here, before a single deletion, while the content was proven equal.
        local_mtime_ms: Some(action.local.mtime_ms),
    }
}

fn remote_snapshot_changed(remote_path: &str) -> Error {
    Error::RemoteSnapshotChanged(remote_path.to_owned())
}

fn verify_live_snapshot<O: SyncOperations>(
    client: &O,
    check: &SnapshotCheck,
    cancellation: &CancellationToken,
) -> Result<()> {
    let mut expected = check.expected.clone();
    if check.local_mtime_ms.is_some() {
        // Settled by the batch, within tolerance; the bytes are what this check proves.
        expected.require_mtime = false;
    }
    client.verify_remote_metadata_snapshot(&check.remote_path, &expected, cancellation)?;
    if let Some(expected_md5) = check.expected.content_md5 {
        match check.evidence {
            ContentEvidence::Uploaded => client.verify_uploaded_content(
                &check.remote_path,
                check.expected.size,
                expected_md5,
                cancellation,
            )?,
            ContentEvidence::ServerProduced => client.verify_remote_content(
                &check.remote_path,
                check.expected.size,
                expected_md5,
                cancellation,
            )?,
        }
    }
    Ok(())
}

/// Whether the destination of an upload is still what the plan was derived from.
///
/// `Upload` runs with `overwrite=true`, and the plan behind it was read from an inventory that may
/// be hours old. Without this, a file someone else wrote to the destination in between, or edited
/// there, was silently replaced. One `getinfo` per transfer is the price, and it is a request, not
/// a transfer.
///
/// An absent destination always passes: nothing can be lost by writing where nothing is. A
/// destination the plan expected to be absent that now exists, a directory, or a file whose size or
/// modification time is not the one planned against, is someone else's write — kept, and the
/// upload deferred to a run that plans against it.
fn upload_precondition_holds<O: SyncOperations>(
    client: &O,
    remote_path: &str,
    expected: Option<&RemoteSnapshot>,
) -> Result<bool> {
    Ok(match (client.remote_path_state(remote_path)?, expected) {
        (RemotePathState::Absent, _) => true,
        (RemotePathState::Directory, _) | (RemotePathState::File { .. }, None) => false,
        (
            RemotePathState::File {
                size,
                mtime_seconds,
            },
            Some(expected),
        ) => {
            expected.kind == crate::local::EntryKind::File
                && size == expected.size
                && mtime_seconds.is_none_or(|mtime| mtime == expected.mtime_seconds)
        }
    })
}

#[derive(Clone, Copy, Debug)]
pub struct ExecuteOptions {
    pub jobs: usize,
    pub dry_run: bool,
}

/// Why a planned mirror deletion was kept instead of performed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteKeptReason {
    /// The local path the deletion was scheduled against exists again.
    ///
    /// The normal outcome of an editor or build step that unlinks and recreates rather than
    /// renaming: the file was absent for the instant the scan read that directory.
    LocalPathExists,
    /// A deletion beneath this directory was kept, so the directory is not empty.
    ///
    /// Not a race and not a judgement call: `Delete.delete` is issued non-recursively, so a
    /// directory with a surviving child cannot be deleted at all. Issuing it anyway returned a
    /// file-operation error that is not retryable and killed the run, with nothing connecting the
    /// failure to the refusal a few entries earlier.
    KeptDescendant,
}

/// Why one path the run planned for was left for the next run.
///
/// Each of these used to fail the whole run, usually before anything was transferred, and every
/// scheduled run after it failed the same way: one open mailbox file or one log being appended to
/// meant nothing synced at all. They are now confined to the path they concern. That path is not
/// uploaded, not deleted and not used to justify deleting anything else; the rest of the run
/// proceeds; and the run reports the path by name and ends as *partial*, so the next run picks it
/// up and an operator can see why it has not converged.
///
/// The wire names ([`Self::as_str`]) are part of the `sdsync.sync.v1` result document.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DeferReason {
    /// The local file changed while the run was reading or sending it. Nothing torn was written:
    /// an upload refuses to end its body when the bytes it sent are not the planned bytes.
    SourceChanged,
    /// The local file disappeared while the run was working.
    SourceVanished,
    /// The local file could not be opened or read: locked by another program, or not readable by
    /// this account.
    SourceUnreadable,
    /// The local entry is of a kind the sync never carries (a link, a device, an unsupported name).
    UnsupportedLocalEntry,
    /// File Station could not produce a digest for the remote file, so it can neither be compared
    /// nor guard a deletion.
    RemoteDigestUnavailable,
    /// DSM refused this one file to the account (105) while the session still answered a request
    /// every account may make: its permissions keep it out of reach until someone changes them.
    RemoteUnreadable,
    /// The remote entry is no longer what the plan was derived from. It was left as found.
    RemoteChanged,
    /// The remote name cannot exist on this host's filesystem, so whether it is present locally
    /// cannot be asked. It is never deleted on that account.
    UnrepresentableName,
    /// DSM's listing of the containing directory did not return every entry it counted, so absence
    /// from the listing proves nothing and no deletion there is made.
    ListingIncomplete,
    /// DSM finished a server-side copy and reported that it failed, and the destination then held
    /// an entry the copy did not prove, so nothing was uploaded over it. Without an entry there,
    /// the file is uploaded instead in the same run and nothing is deferred.
    ServerCopyFailed,
    /// DSM refused the deletion.
    DeleteFailed,
    /// DSM refused this one path: its name (too long, illegal on the volume), this account's
    /// permissions on it, or its size for the destination filesystem. Every other path proceeds.
    /// Lasting: it recurs until someone renames the path or changes its rights, and a partial run
    /// would re-trigger a realtime profile forever. It is named on every run, as a warning, never
    /// as a plain green run. A refusal that is really about the share (most paths refused for one
    /// cause) fails the run instead.
    RemoteRefused,
    /// This account may not read the local file or folder. Not a lock that clears: it stays out of
    /// reach until someone changes its permissions.
    LocalPermissionDenied,
    /// A case-only rename on the NAS did not finish: File Station refused the rename, the old
    /// spelling holds entries a rename must not move, or the NAS still holds the old spelling beside
    /// the new one. Nothing was deleted on its account. Lasting, as only someone renaming or
    /// removing it in File Station settles it; the rename is still tried on every run where it can
    /// be.
    CaseRenamePending,
    /// A NAS entry the remote walk will not carry (a name this tool cannot address, a link out of
    /// the destination, a folder seen twice). Never deleted, overwritten or descended.
    RemoteSkipped,
    /// A local name, path or entry kind Synology Drive cannot hold: a name it refuses, a path too
    /// long once the destination is prepended, a name that is not UTF-8, a FIFO, socket or device,
    /// or a modification time before 1970. It used to fail the whole scan; it is now this path's,
    /// named as a warning on every run until it is renamed or excluded.
    UnsupportedEntry,
    /// DSM refused to delete this entry for permission (105 once the session is proven alive, or
    /// 407). That recurs until someone changes the folder's rights, so it is lasting.
    DeleteNotPermitted,
    /// The whole tree (recorded against `.`, the source root) needs more memory than this run may
    /// use, so nothing was listed, compared or written (SC-2). Lasting: it recurs until the
    /// profile is split, and a partial run would re-trigger a realtime profile forever.
    TreeTooLargeForMemory,
}

impl DeferReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceChanged => "source-changed",
            Self::SourceVanished => "source-vanished",
            Self::SourceUnreadable => "source-unreadable",
            Self::UnsupportedLocalEntry => "unsupported-local-entry",
            Self::RemoteDigestUnavailable => "remote-digest-unavailable",
            Self::RemoteUnreadable => "remote-unreadable",
            Self::RemoteChanged => "remote-changed",
            Self::UnrepresentableName => "unrepresentable-name",
            Self::ListingIncomplete => "listing-incomplete",
            Self::ServerCopyFailed => "server-copy-failed",
            Self::DeleteFailed => "delete-failed",
            Self::RemoteRefused => "remote-refused",
            Self::LocalPermissionDenied => "local-permission-denied",
            Self::CaseRenamePending => "case-rename-pending",
            Self::RemoteSkipped => "remote-skipped",
            Self::UnsupportedEntry => "unsupported-entry",
            Self::DeleteNotPermitted => "delete-not-permitted",
            Self::TreeTooLargeForMemory => "tree-too-large-for-memory",
        }
    }

    /// One sentence for an operator, naming what happens next.
    pub fn describe(self) -> &'static str {
        match self {
            Self::SourceChanged => "changed during the run; retried next run",
            Self::SourceVanished => "deleted locally during the run; retried next run",
            Self::SourceUnreadable => {
                "could not be read (locked or not permitted); retried next run"
            }
            Self::UnsupportedLocalEntry => "is not a regular file or directory the sync carries",
            Self::RemoteDigestUnavailable => {
                "the NAS could not compute its checksum; compared again next run"
            }
            Self::RemoteUnreadable => {
                "the NAS does not let this account read it; never compared or deleted until its permissions change"
            }
            Self::RemoteChanged => "changed on the NAS after planning; left as found",
            Self::UnrepresentableName => {
                "has a NAS name this computer cannot hold; never deleted on that account"
            }
            Self::ListingIncomplete => {
                "is in a NAS folder whose listing was incomplete; no deletion was made there"
            }
            Self::ServerCopyFailed => {
                "the NAS-side copy failed and left an entry at the destination; the next run replaces it with an upload"
            }
            Self::DeleteFailed => "the NAS refused the deletion; retried next run",
            Self::RemoteRefused => {
                "the NAS refuses this name or this account's access to it; named on every run until it is renamed or its permissions change"
            }
            Self::LocalPermissionDenied => {
                "this account may not read it (permission denied); left out of every run until its permissions change"
            }
            Self::CaseRenamePending => {
                "a case-only rename on the NAS is not finished; nothing was deleted on its account; named on every run until it is renamed or removed in File Station"
            }
            Self::RemoteSkipped => {
                "has a NAS name or link this tool will not follow; never deleted, overwritten or descended; named every run until it is renamed or removed on the NAS"
            }
            Self::UnsupportedEntry => {
                "has a name, path or file type Synology Drive cannot hold; named every run until it is renamed or excluded"
            }
            Self::DeleteNotPermitted => {
                "the NAS does not let this account delete it; kept until its permissions change"
            }
            Self::TreeTooLargeForMemory => {
                "the tree needs more memory than this run may use, so nothing was synced; split the profile with a scope or into subfolder profiles"
            }
        }
    }

    /// Whether this path will be left out of every run until someone changes it, rather than
    /// picked up by the next one. A folder DSM lists short in the same way on repeated reads (ABE
    /// hiding what the account may not read) stays short on the next run too. A lasting deferral
    /// is reported like any other but does not make the run partial: a run that could never be
    /// anything but partial would re-run forever under a realtime trigger, and its warning would
    /// become noise nobody reads.
    pub fn is_lasting(self) -> bool {
        matches!(
            self,
            Self::UnrepresentableName
                | Self::UnsupportedLocalEntry
                | Self::ListingIncomplete
                | Self::RemoteUnreadable
                | Self::LocalPermissionDenied
                | Self::DeleteNotPermitted
                | Self::RemoteRefused
                | Self::RemoteSkipped
                | Self::UnsupportedEntry
                | Self::CaseRenamePending
                | Self::TreeTooLargeForMemory
        )
    }

    /// Every reason, so readers can be checked against the whole vocabulary.
    pub const ALL: [Self; 18] = [
        Self::SourceChanged,
        Self::SourceVanished,
        Self::SourceUnreadable,
        Self::UnsupportedLocalEntry,
        Self::RemoteDigestUnavailable,
        Self::RemoteUnreadable,
        Self::RemoteChanged,
        Self::UnrepresentableName,
        Self::ListingIncomplete,
        Self::ServerCopyFailed,
        Self::DeleteFailed,
        Self::RemoteRefused,
        Self::LocalPermissionDenied,
        Self::CaseRenamePending,
        Self::RemoteSkipped,
        Self::UnsupportedEntry,
        Self::DeleteNotPermitted,
        Self::TreeTooLargeForMemory,
    ];

    /// The reason a failure reading one local file defers it, or `None` when the failure is not
    /// about that file and must still fail the run.
    pub fn for_local_error(error: &Error) -> Option<Self> {
        match error {
            Error::SourceChanged(_) => Some(Self::SourceChanged),
            Error::FileIo { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                Some(Self::SourceVanished)
            }
            // A reparse point with no data behind it (an app execution alias under WindowsApps)
            // can be listed but never opened. That is a property of the entry, not a lock that
            // clears: deferring it as unreadable made every run partial forever.
            Error::FileIo { source, .. } if is_entry_without_data(source) => {
                Some(Self::UnsupportedLocalEntry)
            }
            // This account may not read it. Not a lock that clears, so lasting: classed as
            // unreadable, every run was partial forever with a promise of a retry that could not
            // succeed. (`ERROR_ACCESS_DENIED` is the same kind on Windows.)
            Error::FileIo { source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Some(Self::LocalPermissionDenied)
            }
            Error::FileIo { .. } => Some(Self::SourceUnreadable),
            Error::UnsupportedLocalEntry { .. } => Some(Self::UnsupportedLocalEntry),
            _ => None,
        }
    }

    /// The reason a failure writing one path defers it: a local failure as
    /// [`Self::for_local_error`] has it, or DSM refusing that one path for a lasting reason.
    pub fn for_path_error(error: &Error) -> Option<Self> {
        match error {
            Error::RemoteRefused { .. } => Some(Self::RemoteRefused),
            other => Self::for_local_error(other),
        }
    }
}

/// The reason a deletion DSM refused is deferred.
///
/// A permission refusal (105 once the session is proven alive, 407) recurs on every run until
/// someone changes the folder's rights, so it is lasting. Anything else DSM says about the entry
/// (900 among them) is retried by the next run.
fn delete_refusal_reason(error: &Error) -> DeferReason {
    if matches!(error.api_code(), Some(105 | 407)) {
        DeferReason::DeleteNotPermitted
    } else {
        DeferReason::DeleteFailed
    }
}

/// Whether opening a local entry failed because the entry has no readable data at all.
///
/// Windows answers `ERROR_CANT_ACCESS_FILE` (1920) or `ERROR_CANT_RESOLVE_FILENAME` (1921) for a
/// reparse point whose target cannot be opened as a file, such as an app execution alias.
#[cfg(windows)]
fn is_entry_without_data(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(1920 | 1921))
}

#[cfg(not(windows))]
fn is_entry_without_data(_error: &std::io::Error) -> bool {
    false
}

impl std::fmt::Display for DeferReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One path the run left for the next run, by name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredPath {
    /// Root-relative, `/`-separated.
    pub relative: String,
    pub reason: DeferReason,
    /// The underlying error or observation, for the operator. Never a credential.
    pub detail: String,
}

/// The paths deferred so far, indexed for the questions execution and reconciliation ask.
#[derive(Clone, Debug, Default)]
pub struct DeferralIndex {
    exact: BTreeMap<String, DeferReason>,
    /// The same paths under [`crate::path::portable_fold`], the key two spellings share when a
    /// Synology Drive client cannot hold both: case folded after Unicode normalisation.
    folded: BTreeMap<String, (String, DeferReason)>,
}

impl DeferralIndex {
    pub fn from_records(records: &[DeferredPath]) -> Self {
        let mut index = Self::default();
        for record in records {
            index.insert(&record.relative, record.reason);
        }
        index
    }

    /// Record `relative`. False when it was already recorded, in which case the first reason stays.
    pub fn insert(&mut self, relative: &str, reason: DeferReason) -> bool {
        if self.exact.contains_key(relative) {
            return false;
        }
        self.exact.insert(relative.to_owned(), reason);
        self.folded
            .entry(crate::path::portable_fold(relative))
            .or_insert_with(|| (relative.to_owned(), reason));
        true
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }

    /// The deferred path that `relative` is another spelling of, holds, or lies beneath, compared
    /// case folded and normalised.
    ///
    /// The question a mirror deletion has to ask. A case-only rename is planned as an upload of the
    /// new spelling and a deletion of the old one, with nothing else linking the two: when the
    /// upload was deferred and the deletion still ran, the NAS held neither spelling. A deletion
    /// whose path folds onto a deferred one, or onto a directory above or below it, is the other
    /// half of something that did not happen and must not happen alone.
    pub fn spelling_related(&self, relative: &str) -> Option<(&str, DeferReason)> {
        let folded = crate::path::portable_fold(relative);
        let mut candidate = folded.as_str();
        loop {
            if let Some((path, reason)) = self.folded.get(candidate) {
                return Some((path, *reason));
            }
            match candidate.rfind('/') {
                Some(slash) => candidate = &candidate[..slash],
                None => break,
            }
        }
        let prefix = format!("{folded}/");
        self.folded
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .filter(|(candidate, _)| folded.is_empty() || candidate.starts_with(&prefix))
            .map(|(_, (path, reason))| (path.as_str(), *reason))
    }

    /// The reason `relative`, or a directory above it, was deferred.
    pub fn covering(&self, relative: &str) -> Option<DeferReason> {
        let mut candidate = relative;
        loop {
            if let Some(reason) = self.exact.get(candidate) {
                return Some(*reason);
            }
            candidate = &candidate[..candidate.rfind('/')?];
        }
    }

    /// Whether a deferred path lies beneath `relative`.
    pub fn has_descendant(&self, relative: &str) -> bool {
        if relative.is_empty() {
            return !self.exact.is_empty();
        }
        let prefix = format!("{relative}/");
        self.exact
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .is_some_and(|(candidate, _)| candidate.starts_with(&prefix))
    }

    /// Whether `relative` is deferred, lies beneath a deferred path, or holds one.
    pub fn relates_to(&self, relative: &str) -> bool {
        self.covering(relative).is_some() || self.has_descendant(relative)
    }
}

/// One remote mutation, reported as it happens.
///
/// Every variant but [`Self::RemoteExtraDeleteSkipped`] and [`Self::PathDeferred`] corresponds to
/// exactly one successful operation, so a caller can count progress by matching on the variant
/// instead of parsing the rendered text. `Display` produces the diagnostic line shown under `-v`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionEvent {
    /// A remote entry of the wrong kind was removed to make room for the planned entry.
    TypeConflictDeleted {
        remote_path: String,
    },
    DirectoryCreated {
        remote_path: String,
    },
    /// The server duplicated existing remote content instead of accepting an upload.
    RemoteContentCopied {
        from_remote_path: String,
        to_remote_path: String,
        bytes: u64,
    },
    /// A planned server-side copy the server refused to start, uploaded instead. The transfer runs
    /// with an upload observer from the caller's factory, exactly as a planned upload does, so a
    /// progress caller that counts observed transfers has already counted it.
    CopyFallbackUploaded {
        relative: String,
        bytes: u64,
    },
    Uploaded {
        relative: String,
        bytes: u64,
    },
    /// A remote entry with no local counterpart was removed by mirror deletion.
    RemoteExtraDeleted {
        remote_path: String,
    },
    /// A planned mirror deletion that did not happen, and why.
    ///
    /// The one variant that reports work *not* done. It is not a failure, and it is reported rather
    /// than silently skipped because the alternative is an operator who cannot tell a mirror that
    /// converged from one that declined to. A kept deletion whose local path is a file is uploaded
    /// later in the same run, which is reported as [`Self::KeptDeleteUploaded`].
    RemoteExtraDeleteSkipped {
        remote_path: String,
        relative: String,
        reason: DeleteKeptReason,
    },
    /// A file whose planned deletion was kept, uploaded in the same run so the mirror converges.
    ///
    /// Distinct from [`Self::Uploaded`] because the plan did not schedule it: it was discovered
    /// during the delete phase, so a progress caller counting against the plan's totals must not
    /// credit it twice.
    KeptDeleteUploaded {
        relative: String,
        bytes: u64,
    },
    /// A path this run planned for and left for the next run. See [`DeferReason`].
    PathDeferred {
        relative: String,
        reason: DeferReason,
    },
    /// A case-only rename carried out on the NAS by File Station Rename.
    Renamed {
        from_remote_path: String,
        to_remote_path: String,
    },
}

impl std::fmt::Display for ExecutionEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TypeConflictDeleted { remote_path } => {
                write!(formatter, "deleted type conflict: {remote_path}")
            }
            Self::DirectoryCreated { remote_path } => {
                write!(formatter, "created directory: {remote_path}")
            }
            Self::RemoteContentCopied {
                from_remote_path,
                to_remote_path,
                ..
            } => write!(
                formatter,
                "copied remote content: {from_remote_path} -> {to_remote_path}"
            ),
            Self::CopyFallbackUploaded { relative, .. } => {
                write!(formatter, "server copy unavailable; uploaded: {relative}")
            }
            Self::Uploaded { relative, .. } => write!(formatter, "uploaded: {relative}"),
            Self::RemoteExtraDeleted { remote_path } => {
                write!(formatter, "deleted remote extra: {remote_path}")
            }
            Self::RemoteExtraDeleteSkipped {
                remote_path,
                relative,
                reason: DeleteKeptReason::LocalPathExists,
            } => write!(
                formatter,
                "kept remote extra; local path exists again ({relative}): {remote_path}"
            ),
            Self::RemoteExtraDeleteSkipped {
                remote_path,
                relative,
                reason: DeleteKeptReason::KeptDescendant,
            } => write!(
                formatter,
                "kept remote extra; a kept deletion beneath it leaves it non-empty ({relative}): {remote_path}"
            ),
            Self::KeptDeleteUploaded { relative, .. } => {
                write!(
                    formatter,
                    "uploaded a kept deletion's local file: {relative}"
                )
            }
            Self::PathDeferred { relative, reason } => write!(
                formatter,
                "deferred ({reason}): {relative} {}",
                reason.describe()
            ),
            Self::Renamed {
                from_remote_path,
                to_remote_path,
            } => write!(formatter, "renamed: {from_remote_path} -> {to_remote_path}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExecutionReport {
    pub deleted: usize,
    pub created: usize,
    pub copied: usize,
    pub uploaded: usize,
    pub uploaded_bytes: u64,
    /// Mirror deletions kept rather than performed: the local path reappeared before the delete
    /// ran, or a kept deletion beneath a directory left it non-empty.
    pub deletes_skipped: usize,
    /// Files uploaded because their planned deletion was kept, counted separately from `uploaded`
    /// so a caller can tell work the plan scheduled from work the run discovered.
    ///
    /// Their bytes are in `uploaded_bytes`, because that total is what moved over the wire.
    pub kept_deletes_uploaded: usize,
    /// Paths left for the next run during execution, each by name. See [`DeferReason`].
    pub deferred: Vec<DeferredPath>,
    /// Server-side copies, and the copy guards of deletions, whose destination DSM gave a
    /// modification time outside the tolerance of the source's. Their bytes were verified. Uploads
    /// are counted by the client, which is the only place that sees the time DSM kept.
    pub mtime_not_kept: usize,
    /// Planned server-side copies DSM refused to start, uploaded instead. Counted in `uploaded`
    /// too; named here because a fallback costs the transfer the copy was planned to save.
    pub copy_fallbacks: usize,
    /// Case-only renames carried out with File Station Rename.
    pub renamed: usize,
}

impl ExecutionReport {
    fn defer(
        &mut self,
        index: &mut DeferralIndex,
        relative: &str,
        reason: DeferReason,
        detail: String,
        report: &mut impl FnMut(ExecutionEvent),
    ) {
        if index.insert(relative, reason) {
            self.deferred.push(DeferredPath {
                relative: relative.to_owned(),
                reason,
                detail,
            });
            report(ExecutionEvent::PathDeferred {
                relative: relative.to_owned(),
                reason,
            });
        }
    }

    /// Defer `relative` for a failure that concerns only that path (its local file, or DSM
    /// refusing its name), or return the failure.
    fn defer_local(
        &mut self,
        index: &mut DeferralIndex,
        relative: &str,
        error: Error,
        report: &mut impl FnMut(ExecutionEvent),
    ) -> Result<()> {
        match DeferReason::for_path_error(&error) {
            Some(reason) => {
                self.defer(index, relative, reason, error.to_string(), report);
                Ok(())
            }
            None => Err(error),
        }
    }
}

pub fn execute(
    client: &ApiClient,
    root: &RemoteRoot,
    plan: &SyncPlan,
    options: ExecuteOptions,
    report: impl FnMut(ExecutionEvent),
) -> Result<ExecutionReport> {
    execute_with(client, root, plan, options, report)
}

pub type UploadObserverFactory =
    Arc<dyn Fn(&LocalEntry) -> Option<UploadObserver> + Send + Sync + 'static>;

pub fn execute_observed(
    client: &ApiClient,
    root: &RemoteRoot,
    plan: &SyncPlan,
    options: ExecuteOptions,
    cancellation: CancellationToken,
    observer_factory: UploadObserverFactory,
    report: impl FnMut(ExecutionEvent),
) -> Result<ExecutionReport> {
    let mut completed = ExecutionReport::default();
    execute_with_observer(
        client,
        root,
        plan,
        options,
        cancellation,
        observer_factory,
        &mut completed,
        report,
    )?;
    Ok(completed)
}

/// Execute, reporting what completed even when the run then fails.
///
/// The same execution as [`execute_observed`], with the running totals in a place the caller still
/// owns after an error. A run that uploaded nine hundred files and then failed used to return only
/// the failure, so every count of the work that had succeeded was destroyed by the thing that
/// stopped it — and the operator's monitoring reported a failed backup for a backup that had
/// uploaded nine hundred files.
#[allow(clippy::too_many_arguments)]
pub fn execute_observed_reporting_partial(
    client: &ApiClient,
    root: &RemoteRoot,
    plan: &SyncPlan,
    options: ExecuteOptions,
    cancellation: CancellationToken,
    observer_factory: UploadObserverFactory,
    completed: &mut ExecutionReport,
    report: impl FnMut(ExecutionEvent),
) -> Result<()> {
    execute_with_observer(
        client,
        root,
        plan,
        options,
        cancellation,
        observer_factory,
        completed,
        report,
    )
}

fn execute_with<O: SyncOperations>(
    client: &O,
    root: &RemoteRoot,
    plan: &SyncPlan,
    options: ExecuteOptions,
    report: impl FnMut(ExecutionEvent),
) -> Result<ExecutionReport> {
    let mut completed = ExecutionReport::default();
    execute_with_observer(
        client,
        root,
        plan,
        options,
        CancellationToken::default(),
        Arc::new(|_| None),
        &mut completed,
        report,
    )?;
    Ok(completed)
}

/// What one upload worker settled for one file.
enum UploadOutcome {
    Uploaded,
    /// The reason, the detail, and the refusal when DSM refused the path.
    Deferred(DeferReason, String, Option<Refusal>),
    Failed(Error),
}

/// Fails the run when DSM's per-path refusals are really one refusal of the share.
///
/// A refusal of one path is that path's deferral and the run goes on. But one class of cause
/// behind most of what the run handles (an account that may not write the share, a destination
/// volume that rejects every name of some kind) would otherwise end as hundreds of deferrals and a
/// run that merely reads amber. So the run fails, naming the class, what DSM answered and a sample
/// of the paths, once at least three refusals share one class and they are more than half of what
/// the run considered.
///
/// What the run considered (`D`) is every planned folder create, server copy and upload that DSM
/// adjudicated or may still adjudicate, plus the files the plan found already in sync. Each planned
/// write settles exactly once, as accepted, refused, or excluded (left out for a reason of its own:
/// a deferral covering it, a refused folder above it, a local file that changed). An exclusion
/// takes it out of `D`, so `D` only ever shrinks and each refusal count only grows: checked after
/// every settlement, the breaker trips mid-run exactly when the end of the run would have, and
/// stops sending writes it already knows will be refused (G-2).
///
/// Writes beneath a refused folder are excluded, neither attempts nor refusals: counted as
/// attempts they diluted a share-wide refusal of every new folder below the threshold, and counted
/// as refusals they would convict the share from paths DSM never saw.
///
/// The files already in sync are in `D` because lasting refusals are retried on every run while
/// files in sync leave the plan (BK-1): three names the volume rejects were, by the second quiet
/// run, three refusals of three writes, and every later run failed, blaming the share. The price:
/// on a share already in sync, a refusal that newly covers most of what changed reads amber with
/// every path named, not red, unless it is also most of the share.
#[derive(Debug)]
struct RefusalBreaker {
    /// Planned writes not yet settled.
    pending: usize,
    /// Planned writes DSM accepted or refused.
    adjudicated: usize,
    /// Files the plan found already in sync.
    in_sync: usize,
    by_class: BTreeMap<crate::error::RefusalClass, RefusalTally>,
}

/// The refusals of one class.
#[derive(Debug, Default)]
struct RefusalTally {
    refused: usize,
    /// The first ten refused paths.
    sample: Vec<String>,
    /// How many refusals answered each `operation:code`.
    codes: BTreeMap<String, usize>,
}

/// How many refused paths a share-wide failure names; the rest are in the result's deferrals.
const REFUSAL_SAMPLE: usize = 10;

/// A refusal of one path: its class, and what DSM answered (`operation:code`).
type Refusal = (crate::error::RefusalClass, String);

fn refusal_of(error: &Error) -> Option<Refusal> {
    let class = error.refusal_class()?;
    let code = error
        .refusal_cause_key()
        .unwrap_or_else(|| class.as_str().to_owned());
    Some((class, code))
}

impl RefusalBreaker {
    fn new(pending: usize, in_sync: usize) -> Self {
        Self {
            pending,
            adjudicated: 0,
            in_sync,
            by_class: BTreeMap::new(),
        }
    }

    fn settle(&mut self) {
        debug_assert!(self.pending > 0, "a planned write settled twice");
        self.pending = self.pending.saturating_sub(1);
    }

    /// A planned write left out without DSM adjudicating it.
    fn excluded(&mut self) {
        self.settle();
    }

    fn accepted(&mut self) {
        self.settle();
        self.adjudicated += 1;
    }

    /// A planned write DSM refused. An error that is not a refusal of the path settles it as
    /// excluded: it is that path's own problem (a local file that changed), not DSM's verdict.
    fn settled_by(&mut self, error: &Error, relative: &str) {
        self.settled_as(refusal_of(error), relative);
    }

    /// [`Self::settled_by`], for a refusal already reduced to its class and `operation:code`.
    fn settled_as(&mut self, refusal: Option<Refusal>, relative: &str) {
        let Some((class, code)) = refusal else {
            self.excluded();
            return;
        };
        self.settle();
        self.adjudicated += 1;
        let tally = self.by_class.entry(class).or_default();
        tally.refused += 1;
        if tally.sample.len() < REFUSAL_SAMPLE {
            tally.sample.push(relative.to_owned());
        }
        *tally.codes.entry(code).or_default() += 1;
    }

    fn considered(&self) -> usize {
        self.adjudicated + self.pending + self.in_sync
    }

    fn check(&self) -> Result<()> {
        let considered = self.considered();
        for (class, tally) in &self.by_class {
            if tally.refused >= 3 && tally.refused * 2 > considered {
                return Err(Error::RemoteRefusalsShareWide {
                    class: *class,
                    refused: tally.refused,
                    considered,
                    in_sync: self.in_sync,
                    not_attempted: self.pending,
                    codes: tally
                        .codes
                        .iter()
                        .map(|(code, count)| (code.clone(), *count))
                        .collect(),
                    sample: tally.sample.clone(),
                    more: tally.refused - tally.sample.len(),
                });
            }
        }
        Ok(())
    }
}

/// Settle, as excluded, every still-open planned write that a deferral now covers.
fn exclude_covered<'a>(
    breaker: &mut RefusalBreaker,
    open: &mut [bool],
    relatives: impl Iterator<Item = &'a str>,
    deferrals: &DeferralIndex,
) {
    for (open, relative) in open.iter_mut().zip(relatives) {
        if *open && deferrals.covering(relative).is_some() {
            *open = false;
            breaker.excluded();
        }
    }
}

/// The detail recorded when a destination was found changed since planning.
fn remote_changed_detail(remote_path: &str) -> String {
    format!("{remote_path} is no longer the entry the plan was derived from; it was left as found")
}

#[allow(clippy::too_many_arguments)]
fn execute_with_observer<O: SyncOperations>(
    client: &O,
    root: &RemoteRoot,
    plan: &SyncPlan,
    options: ExecuteOptions,
    cancellation: CancellationToken,
    observer_factory: UploadObserverFactory,
    completed: &mut ExecutionReport,
    mut report: impl FnMut(ExecutionEvent),
) -> Result<()> {
    if options.dry_run {
        return Ok(());
    }
    let mut deferrals = DeferralIndex::default();

    // Open every scheduled source before a type-conflict deletion can remove remote data. The
    // question here is whether the transfer can still happen at all, not whether the content is
    // still what was planned -- the upload itself fingerprints the bytes it sends and fails on a
    // change, and a copy re-checks its own source against its digest immediately before copying.
    // A source that cannot be opened, or changed since it was scanned, is deferred by name and
    // takes the replacement of its remote counterpart with it; the rest of the run proceeds.
    for action in &plan.uploads {
        cancellation.check()?;
        if let Err(error) = client.preflight_upload_readable(&action.local) {
            completed.defer_local(&mut deferrals, &action.local.relative, error, &mut report)?;
        }
    }
    for action in &plan.copies {
        cancellation.check()?;
        if let Err(error) = client.preflight_upload_readable(&action.local) {
            completed.defer_local(&mut deferrals, &action.to_relative, error, &mut report)?;
        }
    }

    let pre_deletes = plan
        .pre_deletes
        .iter()
        .filter(|action| deferrals.covering(&action.relative).is_none())
        .collect::<Vec<_>>();
    let pre_delete_checks = pre_deletes
        .iter()
        .map(|action| delete_snapshot_check(action))
        .collect::<Vec<_>>();
    let verdicts = client.verify_remote_snapshot_batch(
        root,
        &plan.scope,
        &plan.rules,
        &pre_delete_checks,
        &cancellation,
    )?;
    let mut kept_pre_deletes = BTreeSet::new();
    for ((action, check), verdict) in pre_deletes.iter().zip(&pre_delete_checks).zip(verdicts) {
        cancellation.check()?;
        // `sort_plan` orders these deepest-first, so a kept child is decided before its parent.
        let changed = verdict == SnapshotVerdict::Changed
            || kept_descendant(&kept_pre_deletes, &action.relative)
            || match verify_live_snapshot(client, check, &cancellation) {
                Err(Error::RemoteSnapshotChanged(_)) => true,
                Err(error) => return Err(error),
                Ok(()) => false,
            };
        if changed {
            // Kept, and so is everything planned at or beneath it: the replacement cannot land
            // where the entry it was to replace still stands.
            kept_pre_deletes.insert(action.relative.clone());
            completed.defer(
                &mut deferrals,
                &action.relative,
                DeferReason::RemoteChanged,
                remote_changed_detail(&action.remote_path),
                &mut report,
            );
            continue;
        }
        client.delete_non_recursive(root, &action.remote_path)?;
        completed.deleted += 1;
        report(ExecutionEvent::TypeConflictDeleted {
            remote_path: action.remote_path.clone(),
        });
    }
    // Every planned write is counted once, as the breaker settles it. One a deferral already
    // covers (a local preflight, a kept type-conflict deletion) was never going to be written.
    let open_creates: Vec<bool> = plan
        .creates
        .iter()
        .map(|action| deferrals.covering(&action.relative).is_none())
        .collect();
    let mut open_copies: Vec<bool> = plan
        .copies
        .iter()
        .map(|action| deferrals.covering(&action.to_relative).is_none())
        .collect();
    let mut open_uploads: Vec<bool> = plan
        .uploads
        .iter()
        .map(|action| deferrals.covering(&action.local.relative).is_none())
        .collect();
    let mut breaker = RefusalBreaker::new(
        [&open_creates, &open_copies, &open_uploads]
            .iter()
            .map(|open| open.iter().filter(|open| **open).count())
            .sum(),
        plan.unchanged_files,
    );
    for (action, open) in plan.creates.iter().zip(&open_creates) {
        cancellation.check()?;
        if !open {
            continue;
        }
        // Beneath a folder DSM refused a moment ago.
        if deferrals.covering(&action.relative).is_some() {
            breaker.excluded();
            breaker.check()?;
            continue;
        }
        match client.create_folder(&action.remote_path) {
            Ok(()) => {
                breaker.accepted();
                completed.created += 1;
                report(ExecutionEvent::DirectoryCreated {
                    remote_path: action.remote_path.clone(),
                });
            }
            // DSM refused this one folder (its name, this account's rights on its parent). Its
            // subtree is covered by the deferral and left out; every other folder and upload
            // proceeds. It used to fail the run before a single upload.
            Err(error @ Error::RemoteRefused { .. }) => {
                breaker.settled_by(&error, &action.relative);
                completed.defer(
                    &mut deferrals,
                    &action.relative,
                    DeferReason::RemoteRefused,
                    error.to_string(),
                    &mut report,
                );
                breaker.check()?;
            }
            Err(error) => return Err(error),
        }
    }
    // Everything beneath a refused folder is covered now, and left out before any copy or upload is
    // sent: a refusal of every new folder trips here, with nothing transferred.
    exclude_covered(
        &mut breaker,
        &mut open_copies,
        plan.copies.iter().map(|action| action.to_relative.as_str()),
        &deferrals,
    );
    exclude_covered(
        &mut breaker,
        &mut open_uploads,
        plan.uploads
            .iter()
            .map(|action| action.local.relative.as_str()),
        &deferrals,
    );
    breaker.check()?;

    let copies = plan
        .copies
        .iter()
        .zip(&open_copies)
        .filter(|(_, open)| **open)
        .map(|(action, _)| action)
        .collect::<Vec<_>>();
    let copy_source_checks = copies
        .iter()
        .map(|action| SnapshotCheck {
            remote_path: action.from_remote_path.clone(),
            expected: action.source_snapshot.clone(),
            deleting: false,
            // The bytes about to be duplicated on the NAS. This process never sent them, so the
            // only way to know they are the content the plan matched is to read them.
            evidence: ContentEvidence::ServerProduced,
            local_mtime_ms: None,
        })
        .collect::<Vec<_>>();
    let verdicts = client.verify_remote_snapshot_batch(
        root,
        &plan.scope,
        &plan.rules,
        &copy_source_checks,
        &cancellation,
    )?;
    let mut copied = Vec::new();
    for ((action, check), verdict) in copies.iter().zip(&copy_source_checks).zip(verdicts) {
        cancellation.check()?;
        if verdict == SnapshotVerdict::Changed {
            breaker.excluded();
            completed.defer(
                &mut deferrals,
                &action.to_relative,
                DeferReason::RemoteChanged,
                remote_changed_detail(&action.from_remote_path),
                &mut report,
            );
            breaker.check()?;
            continue;
        }
        if let Err(error) = client.preflight_upload_source(&action.local, &cancellation) {
            breaker.excluded();
            completed.defer_local(&mut deferrals, &action.to_relative, error, &mut report)?;
            breaker.check()?;
            continue;
        }
        match verify_live_snapshot(client, check, &cancellation) {
            Err(Error::RemoteSnapshotChanged(_)) => {
                breaker.excluded();
                completed.defer(
                    &mut deferrals,
                    &action.to_relative,
                    DeferReason::RemoteChanged,
                    remote_changed_detail(&action.from_remote_path),
                    &mut report,
                );
                breaker.check()?;
                continue;
            }
            result => result?,
        }
        match client.copy_file_verified(root, action, &cancellation) {
            Ok(()) => {
                breaker.accepted();
                completed.copied += 1;
                copied.push(*action);
                cancellation.tick();
                report(ExecutionEvent::RemoteContentCopied {
                    from_remote_path: action.from_remote_path.clone(),
                    to_remote_path: action.to_remote_path.clone(),
                    bytes: action.expected_size,
                });
                // The copy holds the content the plan matched, verified by reading it back. A
                // local edit since then is the next run's upload, and must not authorise
                // deleting the copy's source on the strength of this one.
                if let Err(error) = client.preflight_upload_source(&action.local, &cancellation) {
                    completed.defer_local(
                        &mut deferrals,
                        &action.to_relative,
                        error,
                        &mut report,
                    )?;
                }
            }
            // A copy that never started, or one DSM says ended in failure, is uploaded instead, in
            // this run. `ServerCopyFailed` is only ever built from DSM's report that the task
            // ended (every other outcome of the status poll stops the task and fails the run), so
            // no copy can still be writing here; and the upload happens only while the
            // destination is still absent. Deferred instead, a copy that failed the same way every
            // run was partial forever (G-4).
            Err(copy_error @ (Error::ServerCopyNotStarted | Error::ServerCopyFailed { .. })) => {
                if !upload_precondition_holds(client, &action.to_remote_path, None)? {
                    breaker.excluded();
                    let (reason, detail) = match &copy_error {
                        // The failed copy left something behind, or someone wrote there since.
                        // Now that the destination holds an entry, the next run plans an upload
                        // over it rather than this copy, so this does not repeat.
                        Error::ServerCopyFailed { .. } => (
                            DeferReason::ServerCopyFailed,
                            format!(
                                "{copy_error}; the destination now holds an entry, which the \
                                 next run replaces with an upload"
                            ),
                        ),
                        _ => (
                            DeferReason::RemoteChanged,
                            remote_changed_detail(&action.to_remote_path),
                        ),
                    };
                    completed.defer(
                        &mut deferrals,
                        &action.to_relative,
                        reason,
                        detail,
                        &mut report,
                    );
                    breaker.check()?;
                    continue;
                }
                // Observed like any other upload. A DSM without `CopyMove` v3, or one that rejects
                // every copy deterministically, turns every planned copy into this transfer, and
                // with no observer a multi-gigabyte fallback reported no progress at all.
                // Cancellation no longer depends on the observer — `ObservedReader` consults the
                // token itself — but the bytes still have to be visible.
                if let Err(error) = client.upload(
                    &action.local,
                    &action.to_remote_path,
                    observer_factory(&action.local),
                    &cancellation,
                ) {
                    // A refusal of the fallback is a refusal like any other upload's, and counts
                    // towards the share-wide verdict; anything that is not about this path (a
                    // full volume, 415/416) fails the run here.
                    breaker.settled_by(&error, &action.to_relative);
                    completed.defer_local(
                        &mut deferrals,
                        &action.to_relative,
                        error,
                        &mut report,
                    )?;
                    breaker.check()?;
                    continue;
                }
                breaker.accepted();
                completed.uploaded += 1;
                completed.uploaded_bytes = completed
                    .uploaded_bytes
                    .saturating_add(action.expected_size);
                copied.push(*action);
                cancellation.tick();
                completed.copy_fallbacks += 1;
                report(ExecutionEvent::CopyFallbackUploaded {
                    relative: action.local.relative.clone(),
                    bytes: action.expected_size,
                });
            }
            Err(error) => return Err(error),
        }
    }
    let copy_destination_checks = copied
        .iter()
        .map(|action| destination_snapshot_check(action))
        .collect::<Vec<_>>();
    let verdicts = client.verify_remote_snapshot_batch(
        root,
        &plan.scope,
        &plan.rules,
        &copy_destination_checks,
        &cancellation,
    )?;
    for (check, verdict) in copy_destination_checks.iter().zip(verdicts) {
        match verdict {
            SnapshotVerdict::Changed => return Err(remote_snapshot_changed(&check.remote_path)),
            SnapshotVerdict::MtimeNotKept => completed.mtime_not_kept += 1,
            SnapshotVerdict::Matches => {}
        }
        verify_live_snapshot(client, check, &cancellation)?;
    }

    exclude_covered(
        &mut breaker,
        &mut open_uploads,
        plan.uploads
            .iter()
            .map(|action| action.local.relative.as_str()),
        &deferrals,
    );
    breaker.check()?;
    let uploads = plan
        .uploads
        .iter()
        .zip(&open_uploads)
        .filter(|(_, open)| **open)
        .map(|(action, _)| action.clone())
        .collect::<VecDeque<_>>();
    if !uploads.is_empty() {
        let worker_count = options.jobs.clamp(1, 16).min(uploads.len());
        let queue = Arc::new(Mutex::new(uploads));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        // The breaker is judged by the workers, under this lock and in the order DSM answered, and
        // never by the collector below. The collector also runs the progress observer, which may
        // block on a full pipe or a slow embedder; a verdict that waited for it let every worker
        // send its next upload while the share's refusals queued up unjudged (34 and 38 uploads of
        // 40 where the bound is 24, on a loaded runner). Here the 21st refusal sets `stop` before
        // its worker looks for more work, and a peer checks `stop` right before each request, so
        // at most the other `jobs - 1` workers have one request in flight when the verdict falls.
        let judge = Mutex::new(&mut breaker);
        let tripped: Mutex<Option<Error>> = Mutex::new(None);

        thread::scope(|scope| {
            for _ in 0..worker_count {
                let queue = Arc::clone(&queue);
                let stop = Arc::clone(&stop);
                let judge = &judge;
                let tripped = &tripped;
                let sender = sender.clone();
                let worker_client = (*client).clone();
                let observer_factory = Arc::clone(&observer_factory);
                let cancellation = cancellation.clone();
                scope.spawn(move || {
                    loop {
                        if stop.load(Ordering::Acquire) || cancellation.is_cancelled() {
                            break;
                        }
                        let action = match queue.lock() {
                            Ok(mut queue) => queue.pop_front(),
                            Err(_) => {
                                let _ = sender.send((
                                    String::new(),
                                    0,
                                    UploadOutcome::Failed(Error::Message(
                                        "upload work queue was poisoned".to_owned(),
                                    )),
                                ));
                                stop.store(true, Ordering::Release);
                                break;
                            }
                        };
                        let Some(action) = action else {
                            break;
                        };
                        let relative = action.local.relative.clone();
                        let size = action.local.size;
                        let outcome = match upload_precondition_holds(
                            &worker_client,
                            &action.remote_path,
                            action.expected_remote.as_ref(),
                        ) {
                            Ok(false) => UploadOutcome::Deferred(
                                DeferReason::RemoteChanged,
                                remote_changed_detail(&action.remote_path),
                                None,
                            ),
                            Err(error) => UploadOutcome::Failed(error),
                            Ok(true) => {
                                // The last look before the request, after the precondition's own
                                // round trip: a verdict reached meanwhile is honoured here. The
                                // file is then never sent, and is counted by the breaker as not
                                // attempted, like the rest of the queue.
                                if stop.load(Ordering::Acquire) || cancellation.is_cancelled() {
                                    break;
                                }
                                let user_observer = observer_factory(&action.local);
                                let cancellation_for_observer = cancellation.clone();
                                // `thread::scope` cannot return until every in-flight upload
                                // finishes, so the observer also watches `stop`: without it a
                                // cancelled run, or a run whose peer worker already failed, would
                                // keep streaming this file until the full `--timeout` elapsed.
                                // Returning `false` makes the body reader fail with
                                // `Interrupted`, which aborts the request instead of waiting.
                                let stop_for_observer = Arc::clone(&stop);
                                let observer: Option<UploadObserver> =
                                    Some(Arc::new(move |event| {
                                        let user_continues = user_observer
                                            .as_ref()
                                            .is_none_or(|observer| observer(event));
                                        user_continues
                                            && !cancellation_for_observer.is_cancelled()
                                            && !stop_for_observer.load(Ordering::Acquire)
                                    }));
                                match worker_client.upload(
                                    &action.local,
                                    &action.remote_path,
                                    observer,
                                    &cancellation,
                                ) {
                                    Ok(()) => UploadOutcome::Uploaded,
                                    // One file that changed under its own transfer, or whose
                                    // name DSM refuses, is that file's problem; its peers keep
                                    // going.
                                    Err(error) => match DeferReason::for_path_error(&error) {
                                        Some(reason) => UploadOutcome::Deferred(
                                            reason,
                                            error.to_string(),
                                            refusal_of(&error),
                                        ),
                                        None => UploadOutcome::Failed(error),
                                    },
                                }
                            }
                        };
                        match &outcome {
                            UploadOutcome::Failed(_) => stop.store(true, Ordering::Release),
                            UploadOutcome::Uploaded | UploadOutcome::Deferred(..) => {
                                let Ok(mut breaker) = judge.lock() else {
                                    // Nothing can be judged any more: stop, and say so.
                                    stop.store(true, Ordering::Release);
                                    let _ = sender.send((
                                        relative,
                                        size,
                                        UploadOutcome::Failed(Error::Message(
                                            "upload refusal breaker was poisoned".to_owned(),
                                        )),
                                    ));
                                    break;
                                };
                                match &outcome {
                                    UploadOutcome::Deferred(_, _, refusal) => {
                                        breaker.settled_as(refusal.clone(), &relative);
                                    }
                                    _ => breaker.accepted(),
                                }
                                // Known now: stop sending writes the share will refuse. Peers
                                // interrupted mid-request answer `Cancelled`, which never
                                // replaces this as the run's error.
                                if let Err(error) = breaker.check() {
                                    stop.store(true, Ordering::Release);
                                    if let Ok(mut tripped) = tripped.lock() {
                                        tripped.get_or_insert(error);
                                    }
                                }
                            }
                        }
                        if sender.send((relative, size, outcome)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(sender);

            let mut first_error = None;
            for (relative, size, outcome) in receiver {
                match outcome {
                    UploadOutcome::Uploaded => {
                        completed.uploaded += 1;
                        completed.uploaded_bytes = completed.uploaded_bytes.saturating_add(size);
                        // Counted on completion, in the collector, rather than in each worker:
                        // this thread already serialises every finished upload, so the running
                        // total costs one relaxed increment and no contention between workers.
                        cancellation.tick();
                        report(ExecutionEvent::Uploaded {
                            relative,
                            bytes: size,
                        });
                    }
                    UploadOutcome::Deferred(reason, detail, _) => {
                        completed.defer(&mut deferrals, &relative, reason, detail, &mut report);
                    }
                    UploadOutcome::Failed(error) => record_worker_error(&mut first_error, error),
                }
            }
            // Every worker has finished: the verdict, if one fell, is complete.
            if let Some(error) = tripped.lock().ok().and_then(|mut tripped| tripped.take()) {
                record_worker_error(&mut first_error, error);
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            Ok(())
        })?;
    }
    // Once more, whatever the run held: a run of only folders or only copies is judged too, and
    // always before a single mirror deletion.
    breaker.check()?;

    cancellation.check()?;

    // Remote-only deletion is deliberately last. Any earlier failure returns before this point.
    // File Station has no conditional-delete primitive, so metadata is checked in one fresh
    // phase inventory and content-mode files are rehashed immediately before each delete. The
    // nonrecursive delete remains the final directory race guard.
    //
    // A path deferred above is not uploaded, and nothing here is deleted on its account; a
    // deletion elsewhere is still justified by that path's own absence and its own guards.
    let landed_uploads = plan
        .uploads
        .iter()
        .filter(|action| deferrals.covering(&action.local.relative).is_none())
        .collect::<Vec<_>>();
    let upload_destination_checks = if plan.post_deletes.is_empty() {
        Vec::new()
    } else {
        // Openable and unchanged, not re-hashed. What a mirror deletion has to be sure of is that
        // every upload landed, and the destination checks below prove that from the remote side
        // against a fingerprint of the bytes the upload sent. Whether the local file has changed
        // *since* then does not bear on deleting a different, remote-only path -- and asking cost a
        // third and fourth full read and MD5/CRC32/SHA-256 pass over every uploaded file. A local
        // change since the upload is therefore not a reason to stop here.
        for action in &landed_uploads {
            cancellation.check()?;
            if let Err(error) = client.preflight_upload_readable(&action.local)
                && DeferReason::for_local_error(&error).is_none()
            {
                return Err(error);
            }
        }
        landed_uploads
            .iter()
            .map(|action| upload_destination_snapshot_check(action))
            .collect::<Vec<_>>()
    };
    let mut post_delete_checks = plan
        .post_deletes
        .iter()
        .map(delete_snapshot_check)
        .collect::<Vec<_>>();
    let guard_owners = plan
        .post_deletes
        .iter()
        .enumerate()
        .filter_map(|(index, action)| {
            action
                .destination_guard
                .as_ref()
                .map(|guard| (index, guard))
        })
        .collect::<Vec<_>>();
    post_delete_checks.extend(
        guard_owners
            .iter()
            .map(|(_, guard)| guard_snapshot_check(guard)),
    );
    post_delete_checks.extend(upload_destination_checks.iter().cloned());
    let verdicts = client.verify_remote_snapshot_batch(
        root,
        &plan.scope,
        &plan.rules,
        &post_delete_checks,
        &cancellation,
    )?;
    let delete_count = plan.post_deletes.len();
    let mut delete_changed = vec![false; delete_count];
    for (index, verdict) in verdicts.iter().enumerate() {
        if index < delete_count {
            delete_changed[index] = *verdict == SnapshotVerdict::Changed;
        } else if let Some((owner, _)) = guard_owners.get(index - delete_count) {
            match verdict {
                SnapshotVerdict::Changed => delete_changed[*owner] = true,
                SnapshotVerdict::MtimeNotKept => completed.mtime_not_kept += 1,
                SnapshotVerdict::Matches => {}
            }
        } else if *verdict == SnapshotVerdict::Changed {
            // An upload this run verified moments ago no longer stands. A rename is an upload of
            // the new name and a deletion of the old, with nothing linking the two, so no deletion
            // may proceed on a destination that cannot show its uploads landed.
            return Err(remote_snapshot_changed(
                &post_delete_checks[index].remote_path,
            ));
        }
    }
    for check in &upload_destination_checks {
        verify_live_snapshot(client, check, &cancellation)?;
    }
    // Relatives whose deletion was kept, for whatever reason. `sort_plan` orders post-deletes
    // deepest-first, so every descendant is decided before the directory that holds it and this
    // set is complete by the time an ancestor is considered.
    let mut kept_relatives: BTreeSet<String> = BTreeSet::new();
    // The subset kept because the local file is back, which this run then uploads: the deletion's
    // relative, and the local spelling it was found at.
    let mut reappeared: BTreeMap<String, String> = BTreeMap::new();
    for (index, action) in plan.post_deletes.iter().enumerate() {
        cancellation.check()?;
        // A kept child leaves its parent non-empty, and `Delete.delete` is issued non-recursively.
        // The phase batch's descendant guard approved the parent *before* the per-entry re-check
        // removed the child from the deleting set, so without this the run issued a delete DSM was
        // certain to refuse with a non-retryable file-operation error -- killing a run whose only
        // unusual event was a deliberate refusal two entries earlier.
        if kept_descendant(&kept_relatives, &action.relative) {
            kept_relatives.insert(action.relative.clone());
            completed.deletes_skipped += 1;
            report(ExecutionEvent::RemoteExtraDeleteSkipped {
                remote_path: action.remote_path.clone(),
                relative: action.relative.clone(),
                reason: DeleteKeptReason::KeptDescendant,
            });
            continue;
        }
        // The other half of a rename, or a deletion beneath or above a deferred path, never runs
        // on its own: deleting the old spelling of a file whose new spelling was not uploaded
        // left the NAS with neither.
        if let Some((other, reason)) = deferrals.spelling_related(&action.relative) {
            let record = withheld_with(&action.relative, other, reason);
            kept_relatives.insert(action.relative.clone());
            completed.defer(
                &mut deferrals,
                &record.relative,
                record.reason,
                record.detail,
                &mut report,
            );
            continue;
        }
        // Asked at the local spelling the plan paired the entry with: under a case-renamed or a
        // normalisation-paired folder, the local file is spelled the local way.
        let deferral = match local_presence(&plan.source_root, &action.local_relative) {
            Ok(LocalPresence::Absent) => None,
            Ok(LocalPresence::Present { spelling }) => {
                kept_relatives.insert(action.relative.clone());
                reappeared.insert(action.relative.clone(), spelling);
                completed.deletes_skipped += 1;
                report(ExecutionEvent::RemoteExtraDeleteSkipped {
                    remote_path: action.remote_path.clone(),
                    relative: action.relative.clone(),
                    reason: DeleteKeptReason::LocalPathExists,
                });
                continue;
            }
            // Present, but as more than one name: kept, and nothing is uploaded on a guess.
            Ok(LocalPresence::Ambiguous { path }) => Some((
                DeferReason::SourceChanged,
                format!(
                    "{path:?} is spelled more than one way locally, differing only by Unicode \
                     normalisation, so whether {:?} is back cannot be told; the NAS copy is kept \
                     and nothing is uploaded",
                    action.relative
                ),
            )),
            Ok(LocalPresence::Unrepresentable) => Some((
                DeferReason::UnrepresentableName,
                format!(
                    "{:?} cannot be spelled on this host's filesystem",
                    action.relative
                ),
            )),
            // Absence was not established, so the remote copy stays.
            Err(error) => Some((DeferReason::SourceUnreadable, error.to_string())),
        };
        let deferral = deferral.or_else(|| {
            delete_changed[index].then(|| {
                (
                    DeferReason::RemoteChanged,
                    remote_changed_detail(&action.remote_path),
                )
            })
        });
        let deferral = match (deferral, &action.destination_guard) {
            (Some(deferral), _) => Some(deferral),
            (None, Some(guard)) => match deferrals.covering(&guard.local.relative) {
                Some(reason) => Some((
                    reason,
                    format!(
                        "its content was to be proven by {:?}, which was deferred",
                        guard.local.relative
                    ),
                )),
                None => {
                    match client
                        .preflight_upload_source(&guard.local, &cancellation)
                        .and_then(|()| {
                            verify_live_snapshot(
                                client,
                                &guard_snapshot_check(guard),
                                &cancellation,
                            )
                        }) {
                        Ok(()) => None,
                        Err(
                            Error::RemoteSnapshotChanged(path)
                            | Error::ContentVerificationFailed(path),
                        ) => Some((DeferReason::RemoteChanged, remote_changed_detail(&path))),
                        Err(error) => match DeferReason::for_local_error(&error) {
                            Some(reason) => Some((reason, error.to_string())),
                            None => return Err(error),
                        },
                    }
                }
            },
            (None, None) => None,
        };
        let deferral = match deferral {
            Some(deferral) => Some(deferral),
            None => {
                match verify_live_snapshot(client, &delete_snapshot_check(action), &cancellation) {
                    Ok(()) => None,
                    // The entry no longer holds the content its deletion was authorised on.
                    Err(
                        Error::RemoteSnapshotChanged(path) | Error::ContentVerificationFailed(path),
                    ) => Some((DeferReason::RemoteChanged, remote_changed_detail(&path))),
                    Err(error) => return Err(error),
                }
            }
        };
        if let Some((reason, detail)) = deferral {
            kept_relatives.insert(action.relative.clone());
            completed.defer(
                &mut deferrals,
                &action.relative,
                reason,
                detail,
                &mut report,
            );
            continue;
        }
        match client.delete_non_recursive(root, &action.remote_path) {
            Ok(()) => {
                completed.deleted += 1;
                report(ExecutionEvent::RemoteExtraDeleted {
                    remote_path: action.remote_path.clone(),
                });
            }
            Err(error) => {
                // Named before anything else happens, so a run that stops here still says which
                // deletion it stopped on, after the ones it had already made.
                kept_relatives.insert(action.relative.clone());
                // A DSM refusal concerns this entry. Anything else concerns every deletion after
                // it, so the run stops: the transport, cancellation, and the session -- including a
                // re-login that failed, which DSM also answers with an API code. Treating that as a
                // refusal of this entry attempted every remaining deletion on a dead session and
                // named each one `delete-failed`, "retried next run".
                let concerns_run =
                    !matches!(error, Error::Api { .. } | Error::RemoteRefused { .. })
                        || error.is_session_failure();
                completed.defer(
                    &mut deferrals,
                    &action.relative,
                    if concerns_run {
                        DeferReason::DeleteFailed
                    } else {
                        delete_refusal_reason(&error)
                    },
                    error.to_string(),
                    &mut report,
                );
                if concerns_run {
                    return Err(error);
                }
            }
        }
    }

    // A kept deletion means the local file is back, and the remote copy it was scheduled against is
    // still there and stale. Uploading it here is what makes the run converge. The alternative was a
    // summary line promising that "the next run uploads it" — true of the next run, and no answer at
    // all for this one, which then reported a reconciliation failure it had itself created.
    //
    // Same preflight, same transfer, same post-condition as any other upload. Only the plan's
    // knowledge of it differs, which is why it is a separate event.
    for (relative, spelling) in &reappeared {
        cancellation.check()?;
        // Read at the spelling the presence check found, which beneath a renamed or a
        // normalisation-paired folder is not the NAS spelling. Asked by the NAS spelling, a kept
        // deletion under a renamed folder found nothing and was silently never uploaded (D3-7).
        let mut local = match crate::local::entry_at(&plan.source_root, spelling) {
            Ok(Some(local)) => local,
            // Back a moment ago, not now. The deletion stays kept, nothing is uploaded, and the
            // path is named: the NAS still holds the content its deletion was planned on.
            Ok(None) => {
                let (reason, detail) = match std::fs::symlink_metadata(
                    plan.source_root.join(spelling),
                ) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                        DeferReason::SourceVanished,
                        format!(
                            "{spelling:?} was back when the deletion of {relative:?} was checked \
                             and is gone again; the NAS copy is kept"
                        ),
                    ),
                    Err(error) => {
                        completed.defer_local(
                            &mut deferrals,
                            relative,
                            Error::FileIo {
                                path: plan.source_root.join(spelling),
                                source: error,
                            },
                            &mut report,
                        )?;
                        continue;
                    }
                    // A link, a reparse point or an entry with attributes the walk never carries:
                    // the path exists but is not payload.
                    Ok(_) => (
                        DeferReason::UnsupportedLocalEntry,
                        format!(
                            "{spelling:?} came back as an entry the sync does not carry (a link, \
                             a reparse point or a system file); the NAS copy of {relative:?} is \
                             kept"
                        ),
                    ),
                };
                completed.defer(&mut deferrals, relative, reason, detail, &mut report);
                continue;
            }
            Err(error) => {
                completed.defer_local(&mut deferrals, relative, error, &mut report)?;
                continue;
            }
        };
        // A directory that reappeared needs nothing: the remote entry that was going to be deleted
        // is a directory too, so the two already agree. A kind flip between them is a genuine
        // divergence and is left for reconciliation to report rather than resolved by guesswork.
        if local.kind != crate::local::EntryKind::File {
            continue;
        }
        // `entry_at` applies no ignore rules. The planner never pairs a deletion candidate with an
        // excluded local spelling (`plan::is_protected` asks both spellings), so this does not
        // fire; if it ever did, an excluded file is not uploaded, and the deletion stays kept.
        if plan.rules.is_ignored(spelling, false) {
            completed.defer(
                &mut deferrals,
                relative,
                DeferReason::SourceChanged,
                format!(
                    "{spelling:?} is excluded, so it is not uploaded in place of {relative:?}; the \
                     NAS copy is kept"
                ),
                &mut report,
            );
            continue;
        }
        let Some(target) = kept_upload_target(plan, relative) else {
            continue;
        };
        if let Err(error) = crate::local::promote_to_full_fingerprint(&mut local, &cancellation)
            .and_then(|()| client.preflight_upload_readable(&local))
        {
            completed.defer_local(&mut deferrals, relative, error, &mut report)?;
            continue;
        }
        if !upload_precondition_holds(client, &target.remote_path, Some(&target.snapshot))? {
            completed.defer(
                &mut deferrals,
                relative,
                DeferReason::RemoteChanged,
                remote_changed_detail(&target.remote_path),
                &mut report,
            );
            continue;
        }
        let bytes = local.size;
        if let Err(error) = client.upload(
            &local,
            &target.remote_path,
            observer_factory(&local),
            &cancellation,
        ) {
            completed.defer_local(&mut deferrals, relative, error, &mut report)?;
            continue;
        }
        completed.uploaded += 1;
        completed.kept_deletes_uploaded += 1;
        completed.uploaded_bytes = completed.uploaded_bytes.saturating_add(bytes);
        cancellation.tick();
        report(ExecutionEvent::KeptDeleteUploaded {
            relative: relative.clone(),
            bytes,
        });
    }

    execute_renames(
        client,
        root,
        plan,
        &cancellation,
        completed,
        &mut deferrals,
        &mut report,
    )
}

/// Carry the plan's case-only renames, last, deepest first.
///
/// Last, because everything else the plan does to these entries was planned against the spelling
/// they have now. A rename that does not happen therefore leaves nothing half done: the content is
/// already where the NAS keeps it, nothing was deleted on its account, and the rename is named
/// `case-rename-pending` and retried by the next run. The upload-and-delete it replaces could leave
/// both spellings on the NAS, which every later run then froze as a lasting collision.
fn execute_renames<O: SyncOperations>(
    client: &O,
    root: &RemoteRoot,
    plan: &SyncPlan,
    cancellation: &CancellationToken,
    completed: &mut ExecutionReport,
    deferrals: &mut DeferralIndex,
    report: &mut impl FnMut(ExecutionEvent),
) -> Result<()> {
    if plan.renames.is_empty() {
        return Ok(());
    }
    if !client.supports_rename() {
        for action in &plan.renames {
            completed.defer(
                deferrals,
                &action.relative,
                DeferReason::CaseRenamePending,
                format!(
                    "this DSM does not offer SYNO.FileStation.Rename, so {:?} cannot be renamed to {:?}; rename it in File Station",
                    action.from_remote_path, action.new_name
                ),
                report,
            );
        }
        return Ok(());
    }
    for action in &plan.renames {
        cancellation.check()?;
        // Something at or beneath it was left for the next run: the entry is not in the state the
        // rename was planned on, so it is not moved, and the rename waits with it.
        if let Some((other, _)) = deferrals.spelling_related(&action.relative) {
            let detail = format!(
                "{:?} was not renamed to {:?} because {other:?} beneath it was left for the next run",
                action.from_remote_path, action.new_name
            );
            completed.defer(
                deferrals,
                &action.relative,
                DeferReason::CaseRenamePending,
                detail,
                report,
            );
            continue;
        }
        match client.rename(root, &action.from_remote_path, &action.new_name) {
            Ok(renamed) => {
                completed.renamed += 1;
                report(ExecutionEvent::Renamed {
                    from_remote_path: action.from_remote_path.clone(),
                    to_remote_path: renamed,
                });
            }
            // The transport, the session and cancellation concern every rename after this one.
            Err(error)
                if !matches!(
                    error,
                    Error::Api { .. }
                        | Error::RemoteRefused { .. }
                        | Error::ContentVerificationFailed(_)
                ) || error.is_session_failure() =>
            {
                return Err(error);
            }
            Err(error) => completed.defer(
                deferrals,
                &action.relative,
                DeferReason::CaseRenamePending,
                format!(
                    "renaming {:?} to {:?} failed: {error}",
                    action.from_remote_path, action.new_name
                ),
                report,
            ),
        }
    }
    Ok(())
}

/// True when a kept deletion lies beneath `relative`.
///
/// A range query rather than a scan: the set is ordered, so every candidate starts at the prefix.
fn kept_descendant(kept: &BTreeSet<String>, relative: &str) -> bool {
    let prefix = format!("{relative}/");
    kept.range(prefix.clone()..)
        .next()
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

/// The delete action a kept deletion's local file should be uploaded over.
///
/// Taken from the delete action itself: it is the path the planner mapped for this relative, under
/// the same root and through the same validation, so the follow-up upload cannot reach anywhere the
/// plan could not. Its snapshot is what the upload's precondition checks the destination against.
fn kept_upload_target<'plan>(
    plan: &'plan SyncPlan,
    relative: &str,
) -> Option<&'plan crate::plan::DeleteAction> {
    plan.post_deletes
        .iter()
        .find(|action| action.relative == relative && action.kind == crate::local::EntryKind::File)
}

/// Fill the digests a plan needs, leaving out every file whose digest could not be had.
///
/// The planning half of per-path deferral, shared by the command line and the SDK so the two
/// cannot disagree about which failures a run survives. Content mode hashes the comparison set
/// locally and asks File Station for the remote side; a forced mirror needs only the strong
/// remote digests that guard its deletions; the other modes need nothing.
///
/// A file that changed or vanished while it was hashed, or could not be read, and a remote file
/// whose digest File Station could not produce, are withheld from the local inventory with
/// [`LocalInventory::withhold`] before the plan is built, and the remote inventory is left alone.
/// That is the whole of the safety argument: the planner shields a withheld path (and its remote
/// ancestors) from every operation, so it is neither uploaded nor deleted, the run proceeds without
/// it, and nothing is decided on the evidence that failed. Each one is returned by name.
#[allow(clippy::too_many_arguments)]
pub fn populate_plan_digests(
    client: &ApiClient,
    local: &mut LocalInventory,
    remote: &mut RemoteInventory,
    rules: &IgnoreRules,
    compare: CompareMode,
    server_copy: bool,
    delete: bool,
    cancellation: &CancellationToken,
) -> Result<Vec<DeferredPath>> {
    populate_plan_digests_with_listings(
        client,
        local,
        remote,
        rules,
        compare,
        server_copy,
        delete,
        &[],
        cancellation,
    )
}

/// [`populate_plan_digests`] for a remote inventory with folders DSM listed short.
///
/// A deletion inside one of `incomplete` is withheld after planning
/// ([`withhold_deletions_in_incomplete_listings`]), so the full download its strong digest costs is
/// not paid for it either.
#[allow(clippy::too_many_arguments)]
pub fn populate_plan_digests_with_listings(
    client: &ApiClient,
    local: &mut LocalInventory,
    remote: &mut RemoteInventory,
    rules: &IgnoreRules,
    compare: CompareMode,
    server_copy: bool,
    delete: bool,
    incomplete: &[String],
    cancellation: &CancellationToken,
) -> Result<Vec<DeferredPath>> {
    populate_plan_digests_evidenced(
        client,
        local,
        remote,
        rules,
        DigestSelection {
            compare,
            server_copy,
            delete,
            incomplete,
        },
        None,
        cancellation,
    )
    .map(|(deferred, _)| deferred)
}

/// What a digest pass selects for: the run's compare mode and the facts its selection depends on.
#[derive(Clone, Copy, Debug)]
pub struct DigestSelection<'a> {
    pub compare: CompareMode,
    pub server_copy: bool,
    pub delete: bool,
    /// Folders DSM listed short; see [`populate_plan_digests_with_listings`].
    pub incomplete: &'a [String],
}

/// [`populate_plan_digests_with_listings`], letting `evidence` vouch for comparison pairs instead
/// of both being read live. See [`crate::digest_evidence`].
///
/// Only content mode consults `evidence`: a forced run compares nothing, and the metadata modes
/// read no content. The returned report says what the evidence did, including a canary
/// disagreement, after which every vouched pair was re-verified live in this same pass.
pub fn populate_plan_digests_evidenced(
    client: &ApiClient,
    local: &mut LocalInventory,
    remote: &mut RemoteInventory,
    rules: &IgnoreRules,
    selection: DigestSelection<'_>,
    evidence: Option<&mut dyn crate::digest_evidence::EvidenceSource>,
    cancellation: &CancellationToken,
) -> Result<(Vec<DeferredPath>, crate::digest_evidence::EvidenceReport)> {
    let DigestSelection {
        compare,
        server_copy,
        delete,
        incomplete,
    } = selection;
    let mut report = crate::digest_evidence::EvidenceReport::default();
    let mut deferred = Vec::new();
    // A remote file whose deletion a short listing withholds is not worth its guard's download,
    // and a content-mode deletion planned without one fails the plan. Taken out up front, by name,
    // as the deferral it would have become.
    if delete && matches!(compare, CompareMode::Content | CompareMode::Force) {
        for relative in crate::plan::deletions_withheld_by_listing(local, remote, rules, incomplete)
        {
            local.withhold(
                &relative,
                crate::local::SkipReason::Deferred {
                    detail: format!(
                        "{}: {LISTING_INCOMPLETE_DETAIL}",
                        DeferReason::ListingIncomplete
                    ),
                },
            );
            deferred.push(DeferredPath {
                relative,
                reason: DeferReason::ListingIncomplete,
                detail: LISTING_INCOMPLETE_DETAIL.to_owned(),
            });
        }
    }
    match compare {
        CompareMode::Content => {
            client.require_content_fingerprint_api()?;
            let (mut comparison, mut strong, spellings) = crate::plan::select_plan_digests_paired(
                local,
                remote,
                rules,
                server_copy,
                delete,
                incomplete,
            );
            let vouched = match evidence {
                Some(source) => source.vouch(local, remote, &comparison, &strong, &spellings),
                None => crate::digest_evidence::Vouched::default(),
            };
            crate::digest_evidence::apply_vouched(local, remote, &mut comparison, &vouched.served);
            report.served = vouched.served.len();
            report.verified_live = comparison.difference(&strong).count();
            populate_digests_live(
                client,
                local,
                remote,
                &mut comparison,
                &mut strong,
                &mut deferred,
                cancellation,
            )?;
            if !vouched.canaries.is_empty() {
                crate::digest_evidence::check_canaries(
                    local,
                    remote,
                    &vouched.canaries,
                    &mut report,
                );
            }
            if report.canary_mismatch.is_some() && !vouched.served.is_empty() {
                // The evidence was wrong about a pair whose facts all held, so none of what it
                // vouched for is trusted: every pair is read again, now, on both sides it vouched.
                let mut redo = crate::digest_evidence::undo_vouched(local, remote, &vouched.served);
                report.fell_back = true;
                report.served = 0;
                report.verified_live += redo.len();
                populate_digests_live(
                    client,
                    local,
                    remote,
                    &mut redo,
                    &mut BTreeSet::new(),
                    &mut deferred,
                    cancellation,
                )?;
            }
        }
        CompareMode::Force if delete => {
            client.require_content_fingerprint_api()?;
            // Forced runs compare nothing, so every digest here guards a deletion and must be
            // strong. Unchanged by the comparison split.
            let selected = crate::plan::select_deletion_guard_hashes_excluding(
                local, remote, rules, incomplete,
            );
            let failed = client.populate_remote_content_fingerprints_deferring(
                remote,
                &selected,
                cancellation,
            )?;
            for (relative, error) in failed {
                let reason = remote_digest_reason(&error);
                deferred.push(withhold(local, relative, reason, &error));
            }
        }
        CompareMode::Force | CompareMode::Metadata | CompareMode::SizeOnly => {}
    }
    Ok((deferred, report))
}

/// Hash every local file still without a digest and ask File Station for the `comparison` and
/// `strong` remote digests, withholding each file either side could not produce.
fn populate_digests_live(
    client: &ApiClient,
    local: &mut LocalInventory,
    remote: &mut RemoteInventory,
    comparison: &mut BTreeSet<String>,
    strong: &mut BTreeSet<String>,
    deferred: &mut Vec<DeferredPath>,
    cancellation: &CancellationToken,
) -> Result<()> {
    // Local strength mirrors remote strength, entry for entry: MD5 where the remote side
    // will only carry MD5, full strength everywhere else. Uploads promoted out of the
    // comparison set are raised by `promote_upload_fingerprints_deferring` before
    // execution. Hours of this send DSM nothing, so the session is kept alive through it.
    let failed_local = client
        .keep_session_alive_while(crate::api::SESSION_KEEPALIVE_INTERVAL, || {
            crate::local::populate_content_md5_selective_deferring(local, comparison, cancellation)
        })?;
    for (relative, error) in failed_local {
        let reason = DeferReason::for_local_error(&error).unwrap_or(DeferReason::SourceUnreadable);
        comparison.remove(&relative);
        strong.remove(&relative);
        deferred.push(withhold(local, relative, reason, &error));
    }
    let failed_remote = client.populate_remote_content_digests_deferring(
        remote,
        comparison,
        strong,
        cancellation,
    )?;
    for (relative, error) in failed_remote {
        let reason = remote_digest_reason(&error);
        deferred.push(withhold(local, relative, reason, &error));
    }
    Ok(())
}

/// Why a deletion inside a folder DSM listed short was withheld.
const LISTING_INCOMPLETE_DETAIL: &str =
    "the listing of its folder returned a different number of entries than the NAS counted";

/// The deferral a remote digest failure gets: an account refused this one file (lasting), or the
/// NAS could not produce its checksum this time.
fn remote_digest_reason(error: &Error) -> DeferReason {
    if error.api_code() == Some(105) {
        DeferReason::RemoteUnreadable
    } else {
        DeferReason::RemoteDigestUnavailable
    }
}

/// Whether a plan skip is a deferral the next run is expected to pick up.
///
/// Every deferral named in a plan is tagged `<reason>: <cause>` (see [`withhold`] and
/// [`record_deferrals_in_plan`]), so a plan with no result section can still tell a transient
/// deferral from a lasting one.
pub fn skip_is_transient_deferral(entry: &crate::local::SkippedEntry) -> bool {
    let crate::local::SkipReason::Deferred { detail } = &entry.reason else {
        return false;
    };
    !DeferReason::ALL.iter().any(|reason| {
        reason.is_lasting()
            && detail
                .strip_prefix(reason.as_str())
                .is_some_and(|rest| rest.starts_with(':'))
    })
}

/// Take one path out of the run, shielded rather than absent, and name it.
fn withhold(
    local: &mut LocalInventory,
    relative: String,
    reason: DeferReason,
    error: &Error,
) -> DeferredPath {
    let detail = error.to_string();
    // Tagged with the reason, as `record_deferrals_in_plan` tags every other deferral, so the plan
    // alone can say whether the path is lasting.
    local.withhold(
        &relative,
        crate::local::SkipReason::Deferred {
            detail: format!("{reason}: {detail}"),
        },
    );
    DeferredPath {
        relative,
        reason,
        detail,
    }
}

/// Raise every upload to a full fingerprint, deferring the files that cannot be read for it.
///
/// [`crate::plan::promote_upload_fingerprints`] with the per-path rule: a file that changed or
/// vanished since it was scanned is removed from the plan with everything that depends on it, and
/// returned by name, rather than failing the run before a single transfer. In metadata mode this
/// is the first time any upload is read at all, which made it the most common place for one busy
/// file to stop everything.
pub fn promote_upload_fingerprints_deferring(
    client: &ApiClient,
    plan: &mut SyncPlan,
    cancellation: &CancellationToken,
) -> Result<Vec<DeferredPath>> {
    let mut deferred = Vec::new();
    client.keep_session_alive_while(crate::api::SESSION_KEEPALIVE_INTERVAL, || -> Result<()> {
        for upload in &mut plan.uploads {
            cancellation.check()?;
            if let Err(error) =
                crate::local::promote_to_full_fingerprint(&mut upload.local, cancellation)
            {
                let Some(reason) = DeferReason::for_local_error(&error) else {
                    return Err(error);
                };
                deferred.push(DeferredPath {
                    relative: upload.local.relative.clone(),
                    reason,
                    detail: error.to_string(),
                });
            }
        }
        Ok(())
    })?;
    let withheld = remove_deferred_from_plan(plan, &DeferralIndex::from_records(&deferred));
    deferred.extend(withheld);
    Ok(deferred)
}

/// Remove every operation a deferred path takes with it.
///
/// Its upload or copy, the type-conflict deletion that would have made room for it, and a mirror
/// deletion whose server-copy guard it was to be: each of those is decided on the deferred path's
/// evidence, and none of it may happen without that evidence.
pub fn remove_deferred_from_plan(
    plan: &mut SyncPlan,
    deferrals: &DeferralIndex,
) -> Vec<DeferredPath> {
    let mut withheld = Vec::new();
    if deferrals.is_empty() {
        return withheld;
    }
    plan.uploads
        .retain(|action| deferrals.covering(&action.local.relative).is_none());
    plan.copies
        .retain(|action| deferrals.covering(&action.to_relative).is_none());
    plan.pre_deletes
        .retain(|action| deferrals.covering(&action.relative).is_none());
    plan.post_deletes.retain(|action| {
        let related = action
            .destination_guard
            .as_ref()
            .and_then(|guard| {
                deferrals
                    .covering(&guard.local.relative)
                    .map(|reason| (guard.local.relative.as_str(), reason))
            })
            .or_else(|| deferrals.spelling_related(&action.relative));
        match related {
            Some((other, reason)) => {
                withheld.push(withheld_with(&action.relative, other, reason));
                false
            }
            None => true,
        }
    });
    plan.upload_bytes = plan
        .uploads
        .iter()
        .map(|action| action.local.size)
        .chain(plan.copies.iter().map(|action| action.expected_size))
        .fold(0_u64, u64::saturating_add);
    withheld
}

/// The record for a mirror deletion kept because the path it depended on was deferred.
fn withheld_with(relative: &str, other: &str, reason: DeferReason) -> DeferredPath {
    DeferredPath {
        relative: relative.to_owned(),
        reason,
        detail: format!(
            "kept because {other:?}, which it is a spelling of or depends on, was deferred"
        ),
    }
}

/// Every deferral the plan's `skipped` list names that `deferred` does not, as records.
///
/// The local walk withholds a folder it could not read, and the planner a case-only rename it could
/// not finish, as skips tagged `deferred` with a `<reason>: <cause>` detail. A run's result is built
/// from its deferral records, so without these the path would be in `plan.skipped`, counted nowhere,
/// and the run complete. Records already present (a withheld digest failure) are left alone.
pub fn deferrals_in_plan_skips(plan: &SyncPlan, deferred: &[DeferredPath]) -> Vec<DeferredPath> {
    let known: HashSet<&str> = deferred
        .iter()
        .map(|record| record.relative.as_str())
        .collect();
    plan.skipped
        .iter()
        .filter(|entry| !known.contains(entry.relative.as_str()))
        .filter_map(|entry| {
            let crate::local::SkipReason::Deferred { detail } = &entry.reason else {
                return None;
            };
            DeferReason::ALL.iter().find_map(|reason| {
                let cause = detail.strip_prefix(reason.as_str())?.strip_prefix(':')?;
                Some(DeferredPath {
                    relative: entry.relative.clone(),
                    reason: *reason,
                    detail: cause.trim_start().to_owned(),
                })
            })
        })
        .collect()
}

/// Keep every folder holding a remote entry the walk would not carry, and name each one.
///
/// Such an entry (a name this tool cannot address, a link out of the destination, a folder seen
/// twice) is never in the inventory's entries, or is there as a mount point, so the planner cannot
/// see that its folder is not empty. A deletion of that folder, or of any folder above it, is
/// withheld here, by name; a type replacement that needed one is withheld with everything that
/// depended on it. Each entry nobody excluded is named as a lasting `remote-skipped` deferral.
pub fn protect_skipped_remote_entries(
    plan: &mut SyncPlan,
    skipped: &[crate::api::SkippedRemoteEntry],
) -> Vec<DeferredPath> {
    let mut deferred = Vec::new();
    if skipped.is_empty() {
        return deferred;
    }
    // A folder the account may not list, where the source holds content too, is named by the plan
    // as the more specific `remote-unreadable`: that content is not uploaded either.
    let unreadable: HashSet<String> = plan
        .skipped
        .iter()
        .filter(|skip| {
            matches!(&skip.reason, crate::local::SkipReason::Deferred { detail }
                if detail.starts_with("remote-unreadable:"))
        })
        .map(|skip| skip.relative.clone())
        .collect();
    // Every folder that holds one, and every folder above those.
    let mut kept: BTreeMap<String, String> = BTreeMap::new();
    for entry in skipped {
        let named_by_plan = matches!(entry.reason, crate::api::RemoteSkipReason::UnlistableFolder)
            && unreadable.contains(&entry.display());
        if !entry.excluded && !named_by_plan {
            deferred.push(DeferredPath {
                relative: entry.display(),
                reason: DeferReason::RemoteSkipped,
                detail: entry.detail(),
            });
        }
        let mut folder = entry.parent.as_str();
        loop {
            kept.entry(folder.to_owned())
                .or_insert_with(|| entry.display());
            match folder.rfind('/') {
                Some(slash) => folder = &folder[..slash],
                None if folder.is_empty() => break,
                None => folder = "",
            }
        }
    }
    let mut withheld_replacements: Vec<String> = Vec::new();
    let withhold = |action: &crate::plan::DeleteAction, deferred: &mut Vec<DeferredPath>| {
        let Some(holding) = kept.get(&action.relative) else {
            return true;
        };
        deferred.push(DeferredPath {
            relative: action.relative.clone(),
            reason: DeferReason::RemoteSkipped,
            detail: format!(
                "kept because {holding:?} beneath it is a NAS entry this tool will not follow"
            ),
        });
        false
    };
    plan.post_deletes
        .retain(|action| withhold(action, &mut deferred));
    plan.pre_deletes.retain(|action| {
        let keep = withhold(action, &mut deferred);
        if !keep {
            withheld_replacements.push(action.relative.clone());
        }
        keep
    });
    if !withheld_replacements.is_empty() {
        // The replacement cannot land where the entry it was to replace still stands.
        let replaced = |relative: &str| {
            withheld_replacements.iter().any(|path| {
                relative == path
                    || relative
                        .strip_prefix(path.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        };
        plan.uploads
            .retain(|action| !replaced(&action.local.relative));
        plan.copies.retain(|action| !replaced(&action.to_relative));
        plan.creates.retain(|action| !replaced(&action.relative));
        plan.pre_deletes
            .retain(|action| !replaced(&action.relative));
        plan.upload_bytes = plan
            .uploads
            .iter()
            .map(|action| action.local.size)
            .chain(plan.copies.iter().map(|action| action.expected_size))
            .fold(0_u64, u64::saturating_add);
    }
    deferred
}

/// Name every deferral in the plan's `skipped` list, tagged `deferred`, unless it is there already.
///
/// Planning-time withholds arrive there on their own; promotion deferrals and withheld deletions
/// did not, so `plan` (which has no result section) silently omitted a file that changed before it
/// could be fingerprinted.
pub fn record_deferrals_in_plan(plan: &mut SyncPlan, deferred: &[DeferredPath]) {
    let mut added = false;
    // Indexed once rather than scanned per deferral: a run that defers thousands of paths (a NAS
    // that cannot checksum, a folder listed short) made this quadratic.
    let mut named: HashSet<String> = plan
        .skipped
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    for record in deferred {
        if !named.insert(record.relative.clone()) {
            continue;
        }
        plan.skipped.push(crate::local::SkippedEntry {
            relative: record.relative.clone(),
            reason: crate::local::SkipReason::Deferred {
                detail: format!("{}: {}", record.reason, record.detail),
            },
        });
        added = true;
    }
    if added {
        plan.skipped
            .sort_by(|left, right| left.relative.cmp(&right.relative));
    }
}

/// Shield a scoped run's path where the NAS holds it under another spelling.
///
/// A scoped walk reads only the scope's own path on the NAS, each folder of it by its exact
/// spelling, so it cannot see `Photos/` when the scope is `photos`. Planned on that walk, the run
/// created `photos/` beside `Photos/` -- without `--delete` -- and every mirror run after it froze
/// both. So before planning, each folder of the scope path that the NAS does not hold by this
/// spelling has its parent's names listed (`list_names`, one non-recursive listing each), and a
/// sibling that is the same name under another case or normalisation withholds that folder, and so
/// the scope beneath it, as a `remote-case-collision`. The unscoped mirror run carries the rename.
pub fn withhold_scope_spelling_conflicts(
    root: &RemoteRoot,
    scope: &crate::plan::Scope,
    local: &mut LocalInventory,
    remote: &RemoteInventory,
    mut list_names: impl FnMut(&str) -> Result<(Vec<String>, bool)>,
) -> Result<()> {
    if scope.is_root() || !remote.root_exists {
        return Ok(());
    }
    let mut parent = String::new();
    for name in scope.as_str().split('/') {
        let relative = if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}/{name}")
        };
        if !remote.entries.contains_key(&relative) {
            let fold = crate::path::portable_fold(name);
            let (names, complete) = list_names(&root.join(&parent)?)?;
            if let Some(other) = names.iter().find(|candidate| {
                *candidate != name && crate::path::portable_fold(candidate) == fold
            }) {
                let other = if parent.is_empty() {
                    other.clone()
                } else {
                    format!("{parent}/{other}")
                };
                local.withhold(
                    &relative,
                    crate::local::SkipReason::RemoteCaseCollision { other },
                );
                return Ok(());
            }
            // A listing DSM returned short cannot prove the other spelling absent: the scope is
            // withheld, by name, rather than created on the strength of it.
            if !complete {
                local.withhold(
                    &relative,
                    crate::local::SkipReason::Deferred {
                        detail: format!(
                            "{}: the NAS listing of {:?} was incomplete, so a differently spelled \
                             {name:?} there cannot be ruled out; nothing was created",
                            DeferReason::ListingIncomplete,
                            if parent.is_empty() {
                                "."
                            } else {
                                parent.as_str()
                            }
                        ),
                    },
                );
            }
            // Absent here, so nothing beneath it is on the NAS either.
            return Ok(());
        }
        parent = relative;
    }
    Ok(())
}

/// The deferrals for remote folders that disappeared while the walk listed them (ADV3-P-7).
///
/// The folder was in its parent's listing and answered "no such folder" when listed itself: it
/// was deleted on the NAS during the walk. That clears by itself, so it is a transient
/// `remote-changed`, not a failed run. The walk has already dropped the folder from the inventory
/// and marked its parent's listing incomplete, so nothing there is deleted on the strength of it.
pub fn vanished_remote_folder_deferrals(vanished: &[String]) -> Vec<DeferredPath> {
    vanished
        .iter()
        .map(|relative| DeferredPath {
            relative: relative.clone(),
            reason: DeferReason::RemoteChanged,
            detail: "the NAS folder was deleted while it was being listed; nothing in the folder \
                     above it was deleted on the strength of that listing"
                .to_owned(),
        })
        .collect()
}

/// What [`withhold_comma_ambiguous_mutations`] asks the NAS about a path the run's inventory does
/// not hold.
pub trait RemoteEntryLookup {
    /// Whether an entry exists at `remote_path`, by a self-checking read.
    fn remote_entry_exists(&self, remote_path: &str) -> Result<bool>;
}

impl RemoteEntryLookup for ApiClient {
    fn remote_entry_exists(&self, remote_path: &str) -> Result<bool> {
        ApiClient::remote_entry_exists(self, remote_path)
    }
}

/// Withhold every deletion, rename and server copy whose NAS path could address another entry if
/// DSM split a path list at commas (NAS question N1).
///
/// File Station takes each path as a one-element JSON list, `["/team/t/a,b.txt"]`. Whether DSM
/// ever splits that at the comma is not known. If it did, the only path it could address besides
/// the intended one is the part before the first comma, so a mutation is withheld exactly when
/// that part names an existing entry: one the run's inventory holds, or, outside it, one a
/// self-checking read finds. Each is the lasting `remote-skipped`, named on every run until it is
/// known how DSM parses commas; a comma path with no such entry before it is carried as usual.
///
/// A withheld deletion keeps every folder above it, which a non-recursive delete could not remove
/// anyway, and a withheld type replacement keeps the content that was to replace it. A withheld
/// server copy is uploaded instead.
pub fn withhold_comma_ambiguous_mutations(
    client: &impl RemoteEntryLookup,
    plan: &mut SyncPlan,
    remote: &RemoteInventory,
) -> Result<Vec<DeferredPath>> {
    let listed: HashSet<&str> = remote
        .entries
        .values()
        .map(|entry| entry.remote_path.as_str())
        .collect();
    let mut probed: BTreeMap<String, bool> = BTreeMap::new();
    let mut addressable = |remote_path: &str| -> Result<Option<String>> {
        let Some(prefix) = crate::api::comma_split_prefix(remote_path) else {
            return Ok(None);
        };
        let exists = if listed.contains(prefix) {
            true
        } else if let Some(exists) = probed.get(prefix) {
            *exists
        } else {
            let exists = client.remote_entry_exists(prefix)?;
            probed.insert(prefix.to_owned(), exists);
            exists
        };
        Ok(exists.then(|| prefix.to_owned()))
    };
    let detail = |remote_path: &str, prefix: &str, what: &str| {
        format!(
            "{remote_path:?} is not {what}: the part before its first comma, {prefix:?}, is an \
             existing NAS entry, and until it is known how DSM parses a comma in a path list the \
             request could address that entry instead"
        )
    };
    let mut deferred = Vec::new();

    // Deletions that could address another entry, and every folder above one, with the deletion
    // that keeps it.
    let mut ambiguous: BTreeSet<String> = BTreeSet::new();
    let mut above: BTreeMap<String, String> = BTreeMap::new();
    for action in plan.pre_deletes.iter().chain(&plan.post_deletes) {
        if let Some(prefix) = addressable(&action.remote_path)? {
            deferred.push(DeferredPath {
                relative: action.relative.clone(),
                reason: DeferReason::RemoteSkipped,
                detail: detail(&action.remote_path, &prefix, "deleted"),
            });
            ambiguous.insert(action.relative.clone());
            let mut folder = action.relative.as_str();
            while let Some((parent, _)) = folder.rsplit_once('/') {
                above
                    .entry(parent.to_owned())
                    .or_insert_with(|| action.relative.clone());
                folder = parent;
            }
        }
    }
    if !ambiguous.is_empty() {
        let mut withhold = |action: &crate::plan::DeleteAction| -> bool {
            if ambiguous.contains(&action.relative) {
                return false;
            }
            let Some(beneath) = above.get(&action.relative) else {
                return true;
            };
            deferred.push(DeferredPath {
                relative: action.relative.clone(),
                reason: DeferReason::RemoteSkipped,
                detail: format!(
                    "kept because {beneath:?} beneath it is not deleted until it is known how DSM \
                     parses a comma in a path list"
                ),
            });
            false
        };
        plan.post_deletes.retain(&mut withhold);
        let mut withheld_replacements = Vec::new();
        plan.pre_deletes.retain(|action| {
            let keep = withhold(action);
            if !keep {
                withheld_replacements.push(action.relative.clone());
            }
            keep
        });
        if !withheld_replacements.is_empty() {
            // The replacement cannot land where the entry it was to replace still stands.
            let replaced = |relative: &str| {
                withheld_replacements.iter().any(|path| {
                    relative == path
                        || relative
                            .strip_prefix(path.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
            };
            plan.uploads.retain(|action| {
                let keep = !replaced(&action.local.relative);
                if !keep {
                    plan.upload_bytes = plan.upload_bytes.saturating_sub(action.local.size);
                }
                keep
            });
            plan.copies.retain(|action| !replaced(&action.to_relative));
            plan.creates.retain(|action| !replaced(&action.relative));
            plan.pre_deletes
                .retain(|action| !replaced(&action.relative));
        }
    }

    let mut renames = Vec::with_capacity(plan.renames.len());
    for action in std::mem::take(&mut plan.renames) {
        match addressable(&action.from_remote_path)? {
            Some(prefix) => deferred.push(DeferredPath {
                relative: action.relative.clone(),
                reason: DeferReason::RemoteSkipped,
                detail: detail(&action.from_remote_path, &prefix, "renamed"),
            }),
            None => renames.push(action),
        }
    }
    plan.renames = renames;

    let mut copies = Vec::with_capacity(plan.copies.len());
    for action in std::mem::take(&mut plan.copies) {
        if addressable(&action.from_remote_path)?.is_some() {
            // Not named: the file still lands, by an upload that names nothing but itself.
            plan.upload_bytes = plan.upload_bytes.saturating_add(action.local.size);
            plan.uploads.push(crate::plan::UploadAction {
                local: action.local,
                remote_path: action.to_remote_path,
                reason: crate::plan::ChangeReason::MissingRemote,
                expected_remote: None,
            });
        } else {
            copies.push(action);
        }
    }
    plan.copies = copies;
    Ok(deferred)
}

/// Fail the run when the remote walk found link boundaries, or folders it may not list, almost
/// everywhere.
///
/// A link is recognised by DSM reporting, for a folder, a real path that is not where the listing
/// put it. Genuine links are rare. Most or all folders looking like links says instead that this
/// DSM's real paths do not have the shape the walk expects -- a different volume prefix, a lexical
/// rather than resolved path -- and acting on that would skip the whole tree folder by folder,
/// thousands of warnings and nothing synced. So once at least three folders, or three files, are
/// link boundaries and they are more than half of the folders, or files, the guard checked, the
/// run fails with that cause, by name.
///
/// The same holds for folders DSM refuses to list to this account. One is that folder's lasting
/// `remote-unreadable`, and the run syncs everything else. An account that lost its rights across
/// the share, but may still list the destination itself, would instead end every run amber
/// with nothing synced; so the same threshold fails the run, naming the folders.
pub fn check_link_boundaries(
    remote: &RemoteInventory,
    links: &crate::api::LinkCheckCounts,
) -> Result<()> {
    let mut folders = 0_usize;
    let mut unlistable = Vec::new();
    let mut linked_folders = Vec::new();
    let mut linked_files = Vec::new();
    for entry in remote.entries.values() {
        let is_dir = entry.kind == crate::local::EntryKind::Directory;
        folders += usize::from(is_dir);
        match entry.mount_point_type.as_deref() {
            Some(crate::api::LINKED_ENTRY_MOUNT_TYPE) if is_dir => {
                linked_folders.push(entry.relative.as_str());
            }
            Some(crate::api::LINKED_ENTRY_MOUNT_TYPE) => linked_files.push(entry.relative.as_str()),
            Some(crate::api::UNLISTABLE_ENTRY_MOUNT_TYPE) if is_dir => {
                unlistable.push(entry.relative.as_str());
            }
            _ => {}
        }
    }
    if unlistable.len() >= 3 && unlistable.len() * 2 > folders {
        return Err(Error::Message(format!(
            "the NAS refuses to list {} of the {folders} folders found to this account, so its \
             rights on the share rather than these folders are the likely problem; nothing was \
             synced. Folders: {}",
            unlistable.len(),
            shown_folders(&unlistable)
        )));
    }
    // Per kind, over what the guard actually evaluated: a destination of files whose real paths
    // all look wrong has few folders, or none, and judged by folders alone it withheld every
    // edited file, run after run, with exit 0 (G-3). Scoped-chain ancestors and excluded entries
    // are not evaluations and do not dilute either count.
    for (kind, linked, checked, sample) in [
        (
            "folders",
            links.folders_linked,
            links.folders_checked,
            &linked_folders,
        ),
        (
            "files",
            links.files_linked,
            links.files_checked,
            &linked_files,
        ),
    ] {
        if linked >= 3 && linked * 2 > checked {
            let shown: Vec<String> = sample
                .iter()
                .take(REFUSAL_SAMPLE)
                .map(|relative| (*relative).to_owned())
                .collect();
            return Err(Error::RemoteRealPathUnusable {
                kind,
                linked,
                checked,
                more: linked.saturating_sub(shown.len()),
                sample: shown,
            });
        }
    }
    Ok(())
}

/// The first ten of `folders`, comma-separated, with an ellipsis when there are more.
fn shown_folders(folders: &[&str]) -> String {
    let shown = folders
        .iter()
        .take(10)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if folders.len() > 10 {
        format!("{shown}, ...")
    } else {
        shown
    }
}

/// The named cause [`check_link_boundaries`] fails a run with.
pub const REMOTE_REAL_PATH_UNUSABLE: &str = "remote-real-path-unusable";

/// Withhold every mirror deletion a short directory listing could have misled.
///
/// A directory in `incomplete` was listed with fewer (or more) entries than DSM counted, so an
/// entry missing from its listing may simply be one DSM did not return. Its direct children, the
/// directory itself and every directory above it are kept, by name; deletions anywhere else are
/// unaffected, and so is the rest of the run.
pub fn withhold_deletions_in_incomplete_listings(
    plan: &mut SyncPlan,
    incomplete: &[String],
) -> Vec<DeferredPath> {
    if incomplete.is_empty() {
        return Vec::new();
    }
    let mut withheld = Vec::new();
    plan.post_deletes.retain(|action| {
        if crate::plan::listing_withholds(&action.relative, incomplete) {
            withheld.push(DeferredPath {
                relative: action.relative.clone(),
                reason: DeferReason::ListingIncomplete,
                detail: LISTING_INCOMPLETE_DETAIL.to_owned(),
            });
            false
        } else {
            true
        }
    });
    withheld
}

/// What a post-execution reconciliation pass established.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reconciliation {
    /// Entries the rebuilt plan holds that are explained by the tree changing while the run worked:
    /// a local file written after the run began, or a local file deleted while it was working.
    ///
    /// Converged as of the instant the run started, which is the only instant a run can be asked
    /// about. Zero on the ordinary path.
    pub changed_during_run: usize,
}

/// Plan options for the reconciliation pass, which observes and never acts.
///
/// The safety fuses are deliberately disarmed here, and only here. They exist to refuse a
/// *mutation*: `max_delete` caps how much one run may delete and `allow_empty_source` stops a
/// transiently empty source from mirroring a destination away. This plan is built after execution
/// and is only ever asked whether it is empty, so neither fuse can protect anything — but both can
/// still raise. When they did, remote-only entries a third party added *during* the run made
/// `build_plan` return `DeleteLimit`, and the operator saw a delete-limit refusal for a run that had
/// already completed, with reconciliation never reporting the real state. Refusing to answer a
/// question about convergence is not a safety property.
///
/// Everything that decides *what* the plan compares — the compare mode, server-copy availability,
/// the scope, and whether deletions are part of the comparison at all — is passed through
/// unchanged, because reconciliation has to ask the same question the run just answered. The one
/// exception is supplied by the caller: `compare`, since a forced run reconciles by comparing.
pub fn reconciliation_options(run: &PlanOptions, compare: CompareMode) -> PlanOptions {
    PlanOptions {
        delete: run.delete,
        allow_empty_source: true,
        max_delete: usize::MAX,
        compare,
        server_copy: run.server_copy,
        scope: run.scope.clone(),
    }
}

/// Decide whether the destination converged, distinguishing "the run did not do what it planned"
/// from "the tree changed while the run was working" and from "the run knowingly left this path".
///
/// Shared by the command line and the SDK, so an embedding application cannot fail a run the CLI
/// reports as a success, or the reverse.
///
/// Requiring the rebuilt plan to be **empty** made this the most reachable failure in the engine.
/// The rebuild is a second full local scan taken minutes to hours after the first, so any file
/// written locally in between appears in it. The question a run can answer is "is the destination
/// what the source was when I started", so an entry is excused when:
///
/// - it is, lies beneath, or holds a path this run **deferred** by name. It is already reported,
///   with its reason, and the next run plans it afresh.
/// - it is an upload, or a copy, of a local file whose modification time is after `started_at`:
///   this run was never going to carry those bytes.
/// - it is a create of a directory whose modification time is after `started_at`.
/// - it is a re-upload, for its modification time alone, of a file this run itself wrote and
///   verified by size and digest: DSM did not keep the time the run sent. That is reported as
///   `mtime_not_kept`, not as a failure to converge.
/// - it is a deletion of a remote entry that **this run did not schedule for deletion**, while
///   either the run itself uploaded or created at or beneath that path, or the remote entry
///   predates `started_at`. With deletion enabled, the run's own plan proves the path existed
///   locally when the run read the tree — otherwise the run would have deleted it — so it is a
///   local deletion made while the run was working.
///
/// Everything else is still a failure, and deliberately: a deletion this run scheduled that did
/// not take, an upload of a file older than `started_at` that the destination still disagrees
/// with, a type conflict, a missing destination root.
pub fn ensure_reconciled(
    source_root: &std::path::Path,
    executed: &SyncPlan,
    rebuilt: &SyncPlan,
    started_at: std::time::SystemTime,
    deferred: &DeferralIndex,
) -> Result<Reconciliation> {
    if rebuilt.is_empty() {
        return Ok(Reconciliation::default());
    }
    let pending = || reconciliation_pending(rebuilt);
    let mut changed_during_run = 0;
    // Indexed once. Asked per rebuilt action, a scan of the executed plan made reconciliation
    // quadratic in exactly the case it is most needed for: a DSM that does not keep upload times
    // rebuilds a plan holding every upload the run made.
    let written = ExecutedWrites::index(executed);
    // A rename the run could not make is named; one it did not plan is pending like anything else.
    for action in &rebuilt.renames {
        if !deferred.relates_to(&action.relative) {
            return Err(pending());
        }
    }
    for action in &rebuilt.pre_deletes {
        if !deferred.relates_to(&action.relative) {
            return Err(pending());
        }
    }
    for action in &rebuilt.uploads {
        if deferred.relates_to(&action.local.relative) {
            continue;
        }
        // This run wrote these bytes and proved them by size and digest; the destination differs
        // only in the modification time DSM chose to keep (a copy stamped with the time of the copy,
        // or an upload's milliseconds rounded). Counted as `mtime_not_kept` where it was observed,
        // and corrected by the next run's upload rather than failed here.
        if action.reason == crate::plan::ChangeReason::MtimeDiffers
            && written.exactly(&action.local.relative)
        {
            continue;
        }
        if !modified_after(action.local.mtime_ms, started_at) {
            return Err(pending());
        }
        changed_during_run += 1;
    }
    for action in &rebuilt.copies {
        if deferred.relates_to(&action.to_relative) {
            continue;
        }
        if !modified_after(action.local.mtime_ms, started_at) {
            return Err(pending());
        }
        changed_during_run += 1;
    }
    for action in &rebuilt.creates {
        if deferred.relates_to(&action.relative) {
            continue;
        }
        // The root itself is never dateable this way, and a missing destination root is a failure
        // regardless of when anything was touched.
        if action.relative.is_empty() {
            return Err(pending());
        }
        let metadata =
            std::fs::symlink_metadata(source_root.join(&action.relative)).map_err(|_| pending())?;
        let modified = metadata.modified().map_err(|_| pending())?;
        if modified <= started_at {
            return Err(pending());
        }
        changed_during_run += 1;
    }
    let started_seconds = started_at
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok());
    for action in &rebuilt.post_deletes {
        if deferred.relates_to(&action.relative) {
            continue;
        }
        if written.deleted(&action.relative) {
            return Err(pending());
        }
        let written_by_run = written.at_or_beneath(&action.relative);
        let predates_run =
            started_seconds.is_some_and(|started| action.snapshot.mtime_seconds <= started);
        if !written_by_run && !predates_run {
            return Err(pending());
        }
        changed_during_run += 1;
    }
    Ok(Reconciliation { changed_during_run })
}

/// What the executed plan wrote and deleted, indexed for the per-action questions reconciliation
/// asks.
struct ExecutedWrites<'plan> {
    /// Paths an upload or a copy wrote.
    exact: HashSet<&'plan str>,
    /// Paths an upload, a copy or a create wrote, and every folder above one.
    at_or_above: HashSet<&'plan str>,
    /// Paths a pre- or post-deletion was scheduled for.
    deleted: HashSet<&'plan str>,
}

impl<'plan> ExecutedWrites<'plan> {
    fn index(executed: &'plan SyncPlan) -> Self {
        let exact: HashSet<&str> = executed
            .uploads
            .iter()
            .map(|action| action.local.relative.as_str())
            .chain(
                executed
                    .copies
                    .iter()
                    .map(|action| action.to_relative.as_str()),
            )
            .collect();
        let mut at_or_above = HashSet::new();
        for written in exact.iter().copied().chain(
            executed
                .creates
                .iter()
                .map(|action| action.relative.as_str()),
        ) {
            let mut current = written;
            // Stops at the first folder already present: everything above it is present too.
            while at_or_above.insert(current) {
                match current.rfind('/') {
                    Some(slash) => current = &current[..slash],
                    None => break,
                }
            }
        }
        let deleted = executed
            .post_deletes
            .iter()
            .chain(&executed.pre_deletes)
            .map(|action| action.relative.as_str())
            .collect();
        Self {
            exact,
            at_or_above,
            deleted,
        }
    }

    fn exactly(&self, relative: &str) -> bool {
        self.exact.contains(relative)
    }

    /// Whether the run wrote `relative` or anything beneath it.
    fn at_or_beneath(&self, relative: &str) -> bool {
        self.at_or_above.contains(relative)
    }

    fn deleted(&self, relative: &str) -> bool {
        self.deleted.contains(relative)
    }
}

fn reconciliation_pending(plan: &SyncPlan) -> Error {
    Error::ReconciliationPending {
        operations: plan.pre_deletes.len()
            + plan.creates.len()
            + plan.copies.len()
            + plan.uploads.len()
            + plan.post_deletes.len()
            + plan.renames.len(),
    }
}

/// Whether a recorded millisecond modification time is strictly after `started_at`.
///
/// Compared in milliseconds against the same epoch the scan records, so the comparison cannot
/// disagree with the value the inventory holds. A time the clock cannot express relative to the
/// epoch is treated as *not* after the start, which fails the run: the safe direction for a
/// judgement about whether work was left undone.
pub fn modified_after(mtime_ms: i64, started_at: std::time::SystemTime) -> bool {
    started_at
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .is_some_and(|started_ms| mtime_ms > started_ms)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::api::UploadTransferEvent;
    use crate::local::{EntryKind, LocalEntry};
    use crate::plan::{ChangeReason, CreateAction, DeleteAction, DestinationGuard, UploadAction};

    use super::*;

    #[derive(Clone, Default)]
    struct MockOperations {
        events: Arc<Mutex<Vec<String>>>,
        fail_preflight: bool,
        fail_copy: bool,
        copy_not_started: bool,
        fail_verify_path: Option<String>,
        fail_snapshot_path: Option<String>,
        fail_live_snapshot_path: Option<String>,
        fail_upload: bool,
        fail_create: bool,
        /// When set, `upload` blocks until every worker has entered the call, guaranteeing
        /// concurrent workers fail together instead of one racing ahead and stopping the rest.
        upload_barrier: Option<Arc<std::sync::Barrier>>,
        /// When set, `upload` streams to its observer the way a real transfer does: the entry
        /// named [`FAILING_UPLOAD`] fails at once, and every other entry keeps feeding the
        /// observer until it declines. This is how an in-flight transfer is modelled.
        stream_to_observer: bool,
        /// Set once a streaming upload's observer declined and the transfer was abandoned.
        observer_declined: Arc<AtomicBool>,
        /// The scope each non-empty snapshot batch was taken with.
        ///
        /// Kept out of `events` so asserting on it does not disturb the phase-order sequences every
        /// other test in this module pins.
        batch_scopes: Arc<Mutex<Vec<String>>>,
        /// A destination that holds something when an upload's precondition is checked. Every
        /// other path reads as absent. Not recorded in `events`, for the reason above.
        remote_occupied: Option<(String, RemotePathState)>,
        /// Uploads that fail as a source changing under the transfer does.
        source_changes_during_upload: Option<String>,
        /// The batch verdict for a destination whose modification time DSM did not keep.
        mtime_not_kept_path: Option<String>,
        /// The DSM code a rename fails with.
        fail_rename: Option<i64>,
        /// A local path removed when the remote path is deleted: a source changing again between
        /// a deletion's presence check and the kept deletion's upload.
        remove_on_delete: Option<(String, PathBuf)>,
    }

    /// The one entry a streaming [`MockOperations`] fails immediately.
    const FAILING_UPLOAD: &str = "fails.txt";

    /// Streaming stops here rather than spinning forever, so a regression that ignores the stop
    /// flag fails the assertion instead of hanging the test run.
    const MAX_STREAMED_EVENTS: usize = 1_000_000;

    impl MockOperations {
        fn event(&self, value: impl Into<String>) {
            self.events.lock().unwrap().push(value.into());
        }
    }

    impl SyncOperations for MockOperations {
        fn preflight_upload_readable(&self, local: &LocalEntry) -> Result<()> {
            // Recorded under a different name from the digest form on purpose: the point of
            // splitting them is that the cheap one does not read the file, and an event sequence
            // that spelled both "preflight" could not show which one ran.
            self.event(format!("open:{}", local.relative));
            if self.fail_preflight {
                Err(Error::Message("preflight failed".to_owned()))
            } else {
                Ok(())
            }
        }

        fn preflight_upload_source(
            &self,
            local: &LocalEntry,
            _cancellation: &CancellationToken,
        ) -> Result<()> {
            self.event(format!("preflight:{}", local.relative));
            if self.fail_preflight {
                Err(Error::Message("preflight failed".to_owned()))
            } else {
                Ok(())
            }
        }

        fn delete_non_recursive(&self, _root: &RemoteRoot, remote_path: &str) -> Result<()> {
            self.event(format!("delete:{remote_path}"));
            if let Some((trigger, local)) = &self.remove_on_delete
                && trigger == remote_path
            {
                std::fs::remove_file(local).unwrap();
            }
            Ok(())
        }

        fn create_folder(&self, remote_path: &str) -> Result<()> {
            self.event(format!("create:{remote_path}"));
            if self.fail_create {
                Err(Error::Message("create failed".to_owned()))
            } else {
                Ok(())
            }
        }

        fn supports_rename(&self) -> bool {
            true
        }

        fn rename(&self, _root: &RemoteRoot, remote_path: &str, new_name: &str) -> Result<String> {
            self.event(format!("rename:{remote_path}->{new_name}"));
            match self.fail_rename {
                Some(code) => Err(Error::Api {
                    api: "SYNO.FileStation.Rename".to_owned(),
                    operation: "rename".to_owned(),
                    code,
                    description: String::new(),
                    details: Vec::new(),
                }),
                None => {
                    let (parent, _) = crate::path::parent_and_name(remote_path)?;
                    Ok(format!("{parent}/{new_name}"))
                }
            }
        }

        fn copy_file_verified(
            &self,
            _root: &RemoteRoot,
            action: &crate::plan::CopyAction,
            _cancellation: &CancellationToken,
        ) -> Result<()> {
            self.event(format!(
                "copy:{}->{}",
                action.from_remote_path, action.to_remote_path
            ));
            if self.fail_copy {
                Err(Error::Message("copy failed".to_owned()))
            } else if self.copy_not_started {
                Err(Error::ServerCopyNotStarted)
            } else {
                Ok(())
            }
        }

        fn verify_remote_snapshot_batch(
            &self,
            _root: &RemoteRoot,
            scope: &crate::plan::Scope,
            _rules: &crate::local::IgnoreRules,
            checks: &[SnapshotCheck],
            _cancellation: &CancellationToken,
        ) -> Result<Vec<SnapshotVerdict>> {
            if !checks.is_empty() {
                self.batch_scopes
                    .lock()
                    .unwrap()
                    .push(scope.as_str().to_owned());
            }
            Ok(checks
                .iter()
                .map(|check| {
                    self.event(format!("snapshot:{}", check.remote_path));
                    if self.fail_snapshot_path.as_deref() == Some(check.remote_path.as_str()) {
                        SnapshotVerdict::Changed
                    } else if self.mtime_not_kept_path.as_deref()
                        == Some(check.remote_path.as_str())
                    {
                        SnapshotVerdict::MtimeNotKept
                    } else {
                        SnapshotVerdict::Matches
                    }
                })
                .collect())
        }

        fn remote_path_state(&self, remote_path: &str) -> Result<RemotePathState> {
            Ok(match &self.remote_occupied {
                Some((path, state)) if path == remote_path => *state,
                _ => RemotePathState::Absent,
            })
        }

        fn verify_remote_metadata_snapshot(
            &self,
            remote_path: &str,
            _expected: &RemoteSnapshot,
            _cancellation: &CancellationToken,
        ) -> Result<()> {
            if self.fail_live_snapshot_path.as_deref() == Some(remote_path) {
                Err(remote_snapshot_changed(remote_path))
            } else {
                Ok(())
            }
        }

        fn verify_remote_content(
            &self,
            remote_path: &str,
            expected_size: u64,
            _expected_md5: crate::integrity::ContentMd5,
            _cancellation: &CancellationToken,
        ) -> Result<()> {
            self.event(format!("verify:{remote_path}:{expected_size}"));
            if self.fail_verify_path.as_deref() == Some(remote_path) {
                Err(Error::ContentVerificationFailed(remote_path.to_owned()))
            } else {
                Ok(())
            }
        }

        fn verify_uploaded_content(
            &self,
            remote_path: &str,
            expected_size: u64,
            _expected_md5: crate::integrity::ContentMd5,
            _cancellation: &CancellationToken,
        ) -> Result<()> {
            // A separate event from `verify:`, because the whole point of the distinction is that
            // this one transfers nothing. A test asserting `verify:` for an uploaded destination
            // would be asserting the bug.
            self.event(format!("verify-uploaded:{remote_path}:{expected_size}"));
            if self.fail_verify_path.as_deref() == Some(remote_path) {
                Err(Error::ContentVerificationFailed(remote_path.to_owned()))
            } else {
                Ok(())
            }
        }

        fn upload(
            &self,
            local: &LocalEntry,
            _remote_path: &str,
            observer: Option<UploadObserver>,
            _cancellation: &CancellationToken,
        ) -> Result<()> {
            if let Some(barrier) = &self.upload_barrier {
                barrier.wait();
            }
            self.event(format!("upload:{}", local.relative));
            if self.fail_upload {
                return Err(Error::Message("upload failed".to_owned()));
            }
            if self.source_changes_during_upload.as_deref() == Some(local.relative.as_str()) {
                return Err(Error::SourceChanged(local.full_path.clone()));
            }
            if !self.stream_to_observer {
                return Ok(());
            }
            if local.relative == FAILING_UPLOAD {
                return Err(Error::Message("upload failed".to_owned()));
            }
            let observer = observer.expect("a streaming upload always receives an observer");
            for _ in 0..MAX_STREAMED_EVENTS {
                if !observer(UploadTransferEvent::Advanced { bytes: 1 }) {
                    self.observer_declined.store(true, Ordering::Release);
                    // The real reader answers a declining observer with an interrupted read,
                    // which `ApiClient` reports as cancellation.
                    return Err(Error::Cancelled);
                }
            }
            Ok(())
        }
    }

    fn local(relative: &str) -> LocalEntry {
        LocalEntry {
            relative: relative.to_owned(),
            full_path: PathBuf::from(relative),
            kind: EntryKind::File,
            size: 1,
            mtime_ms: 1,
            identity: Default::default(),
            times: Default::default(),
            content_md5: None,
        }
    }

    /// The same entry with the full fingerprint every planned upload carries.
    ///
    /// `promote_upload_fingerprints` raises every upload to full MD5/CRC32/SHA-256 in every compare
    /// mode, so this — not the digest-less `local` — is the shape the mirror path actually sees.
    fn local_with_digest(relative: &str, digest: crate::integrity::ContentMd5) -> LocalEntry {
        LocalEntry {
            content_md5: Some(digest),
            ..local(relative)
        }
    }

    fn file_snapshot(
        mtime_seconds: i64,
        content_md5: Option<crate::integrity::ContentMd5>,
    ) -> RemoteSnapshot {
        RemoteSnapshot {
            kind: EntryKind::File,
            size: 1,
            mtime_seconds,
            content_md5,
            require_mtime: true,
        }
    }

    fn copy_action() -> crate::plan::CopyAction {
        let digest = crate::integrity::ContentMd5::from_bytes([1; 16]);
        crate::plan::CopyAction {
            from_relative: "old/file.txt".to_owned(),
            from_remote_path: "/share/root/old/file.txt".to_owned(),
            to_relative: "new/file.txt".to_owned(),
            to_remote_path: "/share/root/new/file.txt".to_owned(),
            local: local("new/file.txt"),
            expected_size: 1,
            content_md5: digest,
            source_snapshot: file_snapshot(0, Some(digest)),
        }
    }

    /// A source root that provably holds nothing.
    ///
    /// Mirror deletion re-stats the local path before it acts, so every plan in this module needs
    /// a root the deleted relatives are absent from. A path that does not exist answers `NotFound`
    /// for every child, which is exactly the precondition a real run establishes by scanning.
    fn absent_source_root() -> PathBuf {
        std::env::temp_dir().join(format!("sdsync-absent-source-{}", std::process::id()))
    }

    fn populated_plan() -> SyncPlan {
        SyncPlan {
            source_root: absent_source_root(),
            scope: crate::plan::Scope::root(),
            rules: crate::local::IgnoreRules::none(),
            pre_deletes: vec![DeleteAction {
                relative: "conflict".to_owned(),
                local_relative: "conflict".to_owned(),
                remote_path: "/share/root/conflict".to_owned(),
                kind: EntryKind::File,
                type_conflict: true,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            }],
            creates: vec![CreateAction {
                relative: "folder".to_owned(),
                remote_path: "/share/root/folder".to_owned(),
                reason: ChangeReason::MissingRemote,
            }],
            copies: Vec::new(),
            uploads: vec![UploadAction {
                local: local("file.txt"),
                remote_path: "/share/root/file.txt".to_owned(),
                reason: ChangeReason::MissingRemote,
                expected_remote: None,
            }],
            post_deletes: vec![DeleteAction {
                relative: "extra".to_owned(),
                local_relative: "extra".to_owned(),
                remote_path: "/share/root/extra".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            }],
            renames: Vec::new(),
            unchanged_files: 0,
            protected_entries: 0,
            upload_bytes: 1,
            skipped: Vec::new(),
            link_guard_unchecked: 0,
        }
    }

    #[test]
    fn dry_run_performs_no_operations() {
        let client = MockOperations::default();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &populated_plan(),
            ExecuteOptions {
                jobs: 1,
                dry_run: true,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(
            report.deleted + report.created + report.copied + report.uploaded,
            0
        );
        assert!(client.events.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_happens_before_remote_mutation() {
        let client = MockOperations::default();
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let result = execute_with_observer(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &populated_plan(),
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            cancellation,
            Arc::new(|_| None),
            &mut ExecutionReport::default(),
            |_| {},
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(client.events.lock().unwrap().is_empty());
    }

    #[test]
    fn source_preflight_happens_before_any_mutation() {
        let client = MockOperations {
            fail_preflight: true,
            ..MockOperations::default()
        };
        let result = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &populated_plan(),
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(*client.events.lock().unwrap(), ["open:file.txt"]);
    }

    #[test]
    fn upload_failure_prevents_remote_extra_deletion() {
        let client = MockOperations {
            fail_upload: true,
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        let result = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(
            *client.events.lock().unwrap(),
            ["open:file.txt", "upload:file.txt"]
        );
    }

    #[test]
    fn folder_creation_failure_prevents_upload_and_mirror_deletion() {
        let client = MockOperations {
            fail_create: true,
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();

        let result = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        );

        assert!(matches!(result, Err(Error::Message(message)) if message == "create failed"));
        assert_eq!(
            *client.events.lock().unwrap(),
            ["open:file.txt", "create:/share/root/folder"]
        );
    }

    #[test]
    fn successful_execution_reports_every_mutation_phase_and_upload_bytes() {
        let client = MockOperations::default();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &populated_plan(),
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();

        assert_eq!(report.deleted, 2);
        assert_eq!(report.created, 1);
        assert_eq!(report.copied, 0);
        assert_eq!(report.uploaded, 1);
        assert_eq!(report.uploaded_bytes, 1);
        assert_eq!(
            *client.events.lock().unwrap(),
            [
                // Both preflights are the cheap openability check: the file is opened and its size
                // and modification time compared, and it is never read twice.
                "open:file.txt",
                "snapshot:/share/root/conflict",
                "delete:/share/root/conflict",
                "create:/share/root/folder",
                "upload:file.txt",
                "open:file.txt",
                "snapshot:/share/root/extra",
                "snapshot:/share/root/file.txt",
                "delete:/share/root/extra",
            ]
        );
    }

    #[test]
    fn every_completed_mutation_reports_one_typed_event_in_phase_order() {
        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.copies.push(copy_action());

        let mut events = Vec::new();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(
            events,
            [
                ExecutionEvent::TypeConflictDeleted {
                    remote_path: "/share/root/conflict".to_owned(),
                },
                ExecutionEvent::DirectoryCreated {
                    remote_path: "/share/root/folder".to_owned(),
                },
                ExecutionEvent::RemoteContentCopied {
                    from_remote_path: "/share/root/old/file.txt".to_owned(),
                    to_remote_path: "/share/root/new/file.txt".to_owned(),
                    bytes: 1,
                },
                ExecutionEvent::Uploaded {
                    relative: "file.txt".to_owned(),
                    bytes: 1,
                },
                ExecutionEvent::RemoteExtraDeleted {
                    remote_path: "/share/root/extra".to_owned(),
                },
            ]
        );
        // One event per counted mutation keeps a progress caller's totals exact.
        assert_eq!(
            events.len() as u64,
            (report.deleted + report.created + report.copied + report.uploaded) as u64
        );
    }

    /// A mirror run must not download back the bytes it just uploaded.
    ///
    /// The post-upload verification work replaced a download-and-rehash of every uploaded file with
    /// the destination's own length plus File Station's server-side MD5 — and left the mirror path
    /// out, because the upload destination checks a `--delete` run adds were indistinguishable from
    /// a deletion guard: both carried a digest, and the digest's presence was what chose the
    /// evidence. So a `--compare metadata --delete` sync of 1 GB still moved 2 GB and still ran a
    /// full SHA-256 pass on the way back, on the platform the optimisation was measured for.
    ///
    /// No test could catch it, because no test had both an upload and a post-delete with assertions
    /// about which verification ran.
    #[test]
    fn an_uploaded_destination_on_a_delete_run_is_verified_without_transferring_it_back() {
        let digest = crate::integrity::ContentMd5::from_bytes([3; 16]);
        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.copies.clear();
        plan.uploads[0].local = local_with_digest("file.txt", digest);

        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();

        assert_eq!(report.uploaded, 1);
        assert_eq!(report.deleted, 1);
        let events = client.events.lock().unwrap().clone();
        assert!(
            events.contains(&"verify-uploaded:/share/root/file.txt:1".to_owned()),
            "{events:?}"
        );
        assert!(
            !events.contains(&"verify:/share/root/file.txt:1".to_owned()),
            "an uploaded file must not be transferred back to verify it: {events:?}"
        );
        // And the source is read exactly once more than the transfer itself: the opening preflight
        // and the pre-delete preflight are both the cheap openability check.
        assert_eq!(
            events
                .iter()
                .filter(|event| *event == "open:file.txt")
                .count(),
            2
        );
        assert!(
            !events.iter().any(|event| event.starts_with("preflight:")),
            "an upload-only plan must never pay the digest preflight: {events:?}"
        );
    }

    /// A deletion guard keeps the strong standard even on the same run as an upload.
    ///
    /// The distinction is where the bytes came from, not whether a digest is present. These bytes
    /// are about to be destroyed along with every revision DSM holds of them, and this process
    /// never sent them, so nothing short of reading them back can establish what they are.
    #[test]
    fn a_deletion_guard_still_reads_the_bytes_it_authorises_destroying() {
        let digest = crate::integrity::ContentMd5::from_bytes([4; 16]);
        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.copies.clear();
        plan.uploads[0].local = local_with_digest("file.txt", digest);
        plan.post_deletes[0].snapshot = file_snapshot(1, Some(digest));

        execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();

        let events = client.events.lock().unwrap().clone();
        assert!(
            events.contains(&"verify:/share/root/extra:1".to_owned()),
            "{events:?}"
        );
        assert!(
            !events.contains(&"verify-uploaded:/share/root/extra:1".to_owned()),
            "{events:?}"
        );
    }

    /// The atomic-replace race: absent when the tree was scanned, present when the delete runs.
    ///
    /// The remote snapshot is untouched and every remote guard passes, so nothing except a fresh
    /// local stat can tell this apart from a file the operator really deleted. Without one, the
    /// remote copy and every revision DSM holds of it are destroyed while a live local file sits
    /// next to it.
    #[test]
    fn a_local_file_that_reappeared_keeps_its_remote_copy_and_is_reported() {
        let root = std::env::temp_dir().join(format!(
            "sdsync-delete-race-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Written after the plan was built, exactly as an editor that unlinks and recreates does.
        std::fs::write(root.join("extra"), b"recreated").unwrap();

        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.source_root = root.clone();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.copies.clear();
        // A content-mode mirror delete, where the per-entry guard re-reads the remote file in full.
        plan.post_deletes[0].snapshot =
            file_snapshot(1, Some(crate::integrity::ContentMd5::from_bytes([7; 16])));

        let mut events = Vec::new();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |event| events.push(event),
        )
        .unwrap();

        // Not a failure, and not deferred either: the run uploads the file that came back, so the
        // mirror converges here rather than on some later run that may never be scheduled.
        assert_eq!(report.deleted, 0);
        assert_eq!(report.deletes_skipped, 1);
        assert_eq!(report.kept_deletes_uploaded, 1);
        assert_eq!(report.uploaded, 1);
        assert_eq!(report.uploaded_bytes, b"recreated".len() as u64);
        assert_eq!(
            events,
            [
                ExecutionEvent::RemoteExtraDeleteSkipped {
                    remote_path: "/share/root/extra".to_owned(),
                    relative: "extra".to_owned(),
                    reason: DeleteKeptReason::LocalPathExists,
                },
                ExecutionEvent::KeptDeleteUploaded {
                    relative: "extra".to_owned(),
                    bytes: b"recreated".len() as u64,
                },
            ]
        );
        let recorded = client.events.lock().unwrap().clone();
        assert!(
            !recorded.iter().any(|event| event.starts_with("delete:")),
            "{recorded:?}"
        );
        assert!(
            recorded.contains(&"upload:extra".to_owned()),
            "the reappeared file must be uploaded in this run: {recorded:?}"
        );
        // The refusal comes before the per-entry guard, so the whole-file content re-read that a
        // content-mode deletion pays for is not spent on an entry that is not going to be deleted.
        // The batch metadata check above it still ran: that is one phase inventory for every check
        // in the batch, so leaving one entry out of it would save nothing.
        assert!(
            !recorded.iter().any(|event| event.starts_with("verify:")),
            "{recorded:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every snapshot batch is verified against the scope the plan was built for.
    ///
    /// Taken at the root instead, a `--scope docs/q3` run paid a full unscoped recursive inventory
    /// of the destination per non-empty batch — up to four per run, roughly two hundred `List.list`
    /// requests each against a 100,000-entry destination, which is the exact expense `--scope` exists
    /// to avoid. And it was a correctness problem as well as a cost one: an out-of-scope subtree the
    /// sync account cannot list returns DSM code 105, which propagates, so a scoped run could be
    /// failed *after its uploads had landed* by a directory it was explicitly told to ignore.
    #[test]
    fn every_snapshot_batch_is_verified_against_the_plans_own_scope() {
        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.scope = crate::plan::Scope::parse("docs/q3").unwrap();
        plan.copies.push(copy_action());

        execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();

        let scopes = client.batch_scopes.lock().unwrap().clone();
        assert!(!scopes.is_empty(), "no snapshot batch ran");
        assert!(
            scopes.iter().all(|scope| scope == "docs/q3"),
            "every batch must use the run's scope: {scopes:?}"
        );
    }

    /// A kept child deletion must keep its parent too, or the parent's delete is certain to fail.
    ///
    /// `post_deletes` is ordered deepest-first and the phase batch's descendant guard approves a
    /// directory when every surviving descendant is also in the deleting set. The per-entry local
    /// re-check then removes a child from that set *after* the guard has already approved the
    /// parent — so a non-recursive delete went out against a directory that still had a child, DSM
    /// refused it with a file-operation error that is not retryable, and the run died with nothing
    /// connecting the failure to the deliberate refusal two entries earlier.
    #[test]
    fn a_kept_child_deletion_keeps_its_parent_directory_and_issues_no_delete() {
        let root = std::env::temp_dir().join(format!(
            "sdsync-delete-parent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("old")).unwrap();
        std::fs::write(root.join("old/report.md"), b"recreated").unwrap();

        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.source_root = root.clone();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.copies.clear();
        // Deepest-first, exactly as `sort_plan` leaves them.
        plan.post_deletes = vec![
            DeleteAction {
                relative: "old/report.md".to_owned(),
                local_relative: "old/report.md".to_owned(),
                remote_path: "/share/root/old/report.md".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
            DeleteAction {
                relative: "old".to_owned(),
                local_relative: "old".to_owned(),
                remote_path: "/share/root/old".to_owned(),
                kind: EntryKind::Directory,
                type_conflict: false,
                snapshot: RemoteSnapshot {
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_seconds: 0,
                    content_md5: None,
                    require_mtime: false,
                },
                destination_guard: None,
            },
        ];

        let mut events = Vec::new();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(report.deleted, 0);
        assert_eq!(report.deletes_skipped, 2);
        // The file is uploaded; the directory needs nothing, because the remote directory it was
        // scheduled against is exactly what the local directory calls for.
        assert_eq!(report.kept_deletes_uploaded, 1);
        assert_eq!(
            events,
            [
                ExecutionEvent::RemoteExtraDeleteSkipped {
                    remote_path: "/share/root/old/report.md".to_owned(),
                    relative: "old/report.md".to_owned(),
                    reason: DeleteKeptReason::LocalPathExists,
                },
                ExecutionEvent::RemoteExtraDeleteSkipped {
                    remote_path: "/share/root/old".to_owned(),
                    relative: "old".to_owned(),
                    reason: DeleteKeptReason::KeptDescendant,
                },
                ExecutionEvent::KeptDeleteUploaded {
                    relative: "old/report.md".to_owned(),
                    bytes: b"recreated".len() as u64,
                },
            ]
        );
        let recorded = client.events.lock().unwrap().clone();
        assert!(
            !recorded.iter().any(|event| event.starts_with("delete:")),
            "no delete may be issued at all: {recorded:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A kept deletion whose parent has no surviving child is still deleted.
    ///
    /// The ancestor rule must key off a kept *descendant*, not off any kept entry: a sibling's
    /// refusal says nothing about whether this directory is empty.
    #[test]
    fn a_kept_deletion_does_not_preserve_an_unrelated_directory() {
        let root = std::env::temp_dir().join(format!(
            "sdsync-delete-sibling-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("kept.txt"), b"back").unwrap();

        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.source_root = root.clone();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.copies.clear();
        plan.post_deletes = vec![
            DeleteAction {
                relative: "kept.txt".to_owned(),
                local_relative: "kept.txt".to_owned(),
                remote_path: "/share/root/kept.txt".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
            DeleteAction {
                relative: "empty".to_owned(),
                local_relative: "empty".to_owned(),
                remote_path: "/share/root/empty".to_owned(),
                kind: EntryKind::Directory,
                type_conflict: false,
                snapshot: RemoteSnapshot {
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_seconds: 0,
                    content_md5: None,
                    require_mtime: true,
                },
                destination_guard: None,
            },
        ];

        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();

        assert_eq!(report.deleted, 1);
        assert_eq!(report.deletes_skipped, 1);
        assert!(
            client
                .events
                .lock()
                .unwrap()
                .contains(&"delete:/share/root/empty".to_owned())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same plan against a root the entry really is absent from still deletes.
    #[test]
    fn a_local_file_that_stayed_deleted_still_loses_its_remote_copy() {
        let root = std::env::temp_dir().join(format!(
            "sdsync-delete-absent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.source_root = root.clone();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.copies.clear();

        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();

        assert_eq!(report.deleted, 1);
        assert_eq!(report.deletes_skipped, 0);
        assert!(
            client
                .events
                .lock()
                .unwrap()
                .contains(&"delete:/share/root/extra".to_owned())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A local stat that fails for any reason other than "not there" proves nothing, and a delete
    /// may not proceed on an unreadable precondition.
    ///
    /// Unix only, because the failure has to be provoked by a path whose parent is a file, and
    /// Windows reports a missing path *component* as `NotFound` rather than as `NotADirectory`.
    /// That is not a hole: if a parent component is not a directory then the child cannot exist,
    /// so "absent" is still the truthful answer there.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_local_path_refuses_the_delete_rather_than_assuming_absence() {
        let root = std::env::temp_dir().join(format!(
            "sdsync-delete-unreadable-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // A file where the walk expects a directory: stating `blocked/extra` cannot answer
        // "absent", and on every supported platform reports something other than `NotFound`.
        std::fs::write(root.join("blocked"), b"not a directory").unwrap();

        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.source_root = root.clone();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.copies.clear();
        plan.post_deletes = vec![DeleteAction {
            relative: "blocked/extra".to_owned(),
            local_relative: "blocked/extra".to_owned(),
            remote_path: "/share/root/blocked/extra".to_owned(),
            kind: EntryKind::File,
            type_conflict: false,
            snapshot: file_snapshot(1, None),
            destination_guard: None,
        }];

        let report = execute_once(&client, &plan).expect("the delete is kept, not the run failed");
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "blocked/extra");
        assert_eq!(deferred.reason, DeferReason::SourceUnreadable);
        assert!(
            !client
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("delete:"))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_fallback_reports_an_upload_event_carrying_the_copy_size() {
        let client = MockOperations {
            copy_not_started: true,
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes.clear();
        plan.copies.push(copy_action());

        let mut events = Vec::new();
        execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |event| events.push(event),
        )
        .unwrap();

        // The fallback upload runs without an observer, so this event is the only signal
        // a progress caller gets for those bytes.
        assert_eq!(
            events,
            [ExecutionEvent::CopyFallbackUploaded {
                relative: "new/file.txt".to_owned(),
                bytes: 1,
            }]
        );
    }

    #[test]
    fn execution_events_render_the_verbose_diagnostic_lines() {
        let lines = [
            ExecutionEvent::TypeConflictDeleted {
                remote_path: "/share/root/conflict".to_owned(),
            },
            ExecutionEvent::DirectoryCreated {
                remote_path: "/share/root/folder".to_owned(),
            },
            ExecutionEvent::RemoteContentCopied {
                from_remote_path: "/share/root/old.txt".to_owned(),
                to_remote_path: "/share/root/new.txt".to_owned(),
                bytes: 4,
            },
            ExecutionEvent::CopyFallbackUploaded {
                relative: "new.txt".to_owned(),
                bytes: 4,
            },
            ExecutionEvent::Uploaded {
                relative: "file.txt".to_owned(),
                bytes: 9,
            },
            ExecutionEvent::RemoteExtraDeleted {
                remote_path: "/share/root/extra".to_owned(),
            },
        ]
        .map(|event| event.to_string());

        assert_eq!(
            lines,
            [
                "deleted type conflict: /share/root/conflict",
                "created directory: /share/root/folder",
                "copied remote content: /share/root/old.txt -> /share/root/new.txt",
                "server copy unavailable; uploaded: new.txt",
                "uploaded: file.txt",
                "deleted remote extra: /share/root/extra",
            ]
        );
    }

    #[test]
    fn an_uploaded_destination_that_changed_blocks_remote_extra_deletion() {
        let client = MockOperations {
            fail_live_snapshot_path: Some("/share/root/file.txt".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();

        let error = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap_err();

        assert!(
            matches!(error, Error::RemoteSnapshotChanged(path) if path == "/share/root/file.txt")
        );
        let events = client.events.lock().unwrap();
        assert!(events.contains(&"snapshot:/share/root/file.txt".to_owned()));
        assert!(!events.contains(&"delete:/share/root/extra".to_owned()));
    }

    /// The single deferral a run reported, asserting there was exactly one.
    fn only_deferral(report: &ExecutionReport) -> &DeferredPath {
        assert_eq!(report.deferred.len(), 1, "{:?}", report.deferred);
        &report.deferred[0]
    }

    fn execute_once(client: &MockOperations, plan: &SyncPlan) -> Result<ExecutionReport> {
        execute_with(
            client,
            &RemoteRoot::parse("/share/root").unwrap(),
            plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
    }

    /// A remote-only entry someone replaced after planning is kept and named, and the run goes
    /// on. Before, it failed the run and took every other planned deletion with it.
    #[test]
    fn replacement_before_ordinary_mirror_delete_is_preserved() {
        let client = MockOperations {
            fail_snapshot_path: Some("/share/root/extra".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();

        let report = execute_once(&client, &plan).expect("one changed entry is that entry's");
        assert_eq!(report.deleted, 0);
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "extra");
        assert_eq!(deferred.reason, DeferReason::RemoteChanged);
        assert_eq!(
            *client.events.lock().unwrap(),
            ["snapshot:/share/root/extra"]
        );
    }

    #[test]
    fn replacement_before_type_conflict_delete_is_preserved() {
        let client = MockOperations {
            fail_snapshot_path: Some("/share/root/conflict".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes.clear();

        let report = execute_once(&client, &plan).expect("one changed entry is that entry's");
        assert_eq!(report.deleted, 0);
        assert_eq!(only_deferral(&report).reason, DeferReason::RemoteChanged);
        assert_eq!(
            *client.events.lock().unwrap(),
            ["snapshot:/share/root/conflict"]
        );
    }

    /// A kept type-conflict replacement keeps the upload that was to take its place.
    #[test]
    fn a_kept_type_conflict_replacement_withholds_the_upload_that_needed_it() {
        let client = MockOperations {
            fail_snapshot_path: Some("/share/root/file.txt".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.creates.clear();
        plan.post_deletes.clear();
        plan.pre_deletes[0].relative = "file.txt".to_owned();
        plan.pre_deletes[0].remote_path = "/share/root/file.txt".to_owned();

        let report = execute_once(&client, &plan).unwrap();
        assert_eq!(report.uploaded, 0);
        assert_eq!(only_deferral(&report).relative, "file.txt");
        let events = client.events.lock().unwrap();
        assert!(!events.iter().any(|event| event.starts_with("upload:")));
        assert!(!events.iter().any(|event| event.starts_with("delete:")));
    }

    #[test]
    fn content_replacement_after_phase_snapshot_is_preserved_before_delete() {
        let digest = crate::integrity::ContentMd5::from_bytes([8; 16]);
        let client = MockOperations {
            fail_verify_path: Some("/share/root/extra".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes[0].snapshot = file_snapshot(1, Some(digest));

        let report = execute_once(&client, &plan).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(only_deferral(&report).reason, DeferReason::RemoteChanged);
        assert_eq!(
            *client.events.lock().unwrap(),
            ["snapshot:/share/root/extra", "verify:/share/root/extra:1"]
        );
    }

    #[test]
    fn metadata_replacement_after_phase_snapshot_is_preserved_before_delete() {
        let client = MockOperations {
            fail_live_snapshot_path: Some("/share/root/extra".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();

        let report = execute_once(&client, &plan).unwrap();
        assert_eq!(report.deleted, 0);
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "extra");
        assert_eq!(deferred.reason, DeferReason::RemoteChanged);
        assert_eq!(
            *client.events.lock().unwrap(),
            ["snapshot:/share/root/extra"]
        );
    }

    /// N-3: one file that changes under its own upload defers that file, and only that file.
    #[test]
    fn a_source_changing_during_its_upload_defers_it_and_the_rest_of_the_run_proceeds() {
        let client = MockOperations {
            source_changes_during_upload: Some("busy.pst".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.push(UploadAction {
            local: local("busy.pst"),
            remote_path: "/share/root/busy.pst".to_owned(),
            reason: ChangeReason::MissingRemote,
            expected_remote: None,
        });

        let report = execute_once(&client, &plan).expect("a busy file is not a failed run");
        assert_eq!(report.uploaded, 1, "the unaffected upload still landed");
        assert_eq!(report.deleted, 1, "an unrelated mirror deletion still ran");
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "busy.pst");
        assert_eq!(deferred.reason, DeferReason::SourceChanged);
    }

    /// N-3 at the preflight: a source that changed since the scan is deferred before any
    /// mutation, without stopping the others.
    #[test]
    fn a_source_that_changed_before_preflight_is_deferred_not_fatal() {
        let root = std::env::temp_dir().join(format!(
            "sdsync-preflight-changed-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.post_deletes.clear();
        // A path that does not exist: the real preflight reports it vanished.
        let mut vanished = local("vanished.txt");
        vanished.full_path = root.join("vanished.txt");
        plan.uploads.push(UploadAction {
            local: vanished,
            remote_path: "/share/root/vanished.txt".to_owned(),
            reason: ChangeReason::MissingRemote,
            expected_remote: None,
        });
        let client = VanishingPreflight(MockOperations::default());

        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(report.uploaded, 1);
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "vanished.txt");
        assert_eq!(deferred.reason, DeferReason::SourceVanished);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// S1-4: a destination that appeared after planning is never overwritten.
    #[test]
    fn an_upload_whose_destination_appeared_after_planning_is_deferred_not_overwritten() {
        let client = MockOperations {
            remote_occupied: Some((
                "/share/root/file.txt".to_owned(),
                RemotePathState::File {
                    size: 9,
                    mtime_seconds: Some(5),
                },
            )),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.post_deletes.clear();

        let report = execute_once(&client, &plan).unwrap();
        assert_eq!(report.uploaded, 0);
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "file.txt");
        assert_eq!(deferred.reason, DeferReason::RemoteChanged);
        assert!(
            !client
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("upload:"))
        );
    }

    /// S1-4: a destination still exactly as planned is overwritten as planned.
    #[test]
    fn an_upload_over_the_planned_destination_proceeds() {
        let client = MockOperations {
            remote_occupied: Some((
                "/share/root/file.txt".to_owned(),
                RemotePathState::File {
                    size: 1,
                    mtime_seconds: Some(4),
                },
            )),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.post_deletes.clear();
        plan.uploads[0].reason = ChangeReason::MtimeDiffers;
        plan.uploads[0].expected_remote = Some(file_snapshot(4, None));

        let report = execute_once(&client, &plan).unwrap();
        assert_eq!(report.uploaded, 1);
        assert!(report.deferred.is_empty());
    }

    /// A destination whose modification time DSM did not keep is a warning, not a refusal: the
    /// bytes were proven by size and digest.
    #[test]
    fn an_upload_destination_whose_mtime_was_not_kept_still_permits_deletion() {
        let client = MockOperations {
            mtime_not_kept_path: Some("/share/root/file.txt".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();

        let report = execute_once(&client, &plan).unwrap();
        assert_eq!(report.deleted, 1);
        assert!(report.deferred.is_empty());
    }

    /// A server copy DSM stamped with the time of the copy is verified by its bytes and counted
    /// as a warning, rather than failing the run.
    #[test]
    fn a_server_copy_stamped_with_a_new_mtime_is_a_warning() {
        let client = MockOperations {
            mtime_not_kept_path: Some("/share/root/new/file.txt".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes.clear();
        plan.copies.push(copy_action());

        let report = execute_once(&client, &plan).unwrap();
        assert_eq!(report.copied, 1);
        assert_eq!(report.mtime_not_kept, 1);
        assert!(
            client
                .events
                .lock()
                .unwrap()
                .contains(&"verify:/share/root/new/file.txt:1".to_owned()),
            "the copy's bytes are still read back"
        );
    }

    /// G-4: a copy DSM reports as ended in failure is uploaded instead, in the same run, while the
    /// destination is still absent. Deferred instead, a copy that failed the same way every run
    /// left the run partial forever.
    #[test]
    fn a_server_copy_that_finished_with_an_error_falls_back_to_an_upload() {
        let client = FailingCopy(MockOperations::default());
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes.clear();
        plan.copies.push(copy_action());

        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(report.copied, 0);
        assert_eq!(report.copy_fallbacks, 1);
        assert_eq!(report.uploaded, 1);
        assert!(report.deferred.is_empty(), "{:?}", report.deferred);
        let events = client.0.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|event| event == "upload:new/file.txt"),
            "{events:?}"
        );
    }

    /// G-4: when the failed copy left an entry at the destination (or someone wrote there), nothing
    /// is uploaded over it in this run. It is the transient `server-copy-failed`, which converges
    /// next run: the destination now has a NAS counterpart, so that run uploads rather than copies.
    #[test]
    fn a_failed_server_copy_that_left_an_entry_is_deferred_for_one_run() {
        let client = FailingCopy(MockOperations {
            remote_occupied: Some((
                "/share/root/new/file.txt".to_owned(),
                RemotePathState::File {
                    size: 3,
                    mtime_seconds: Some(5),
                },
            )),
            ..MockOperations::default()
        });
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes.clear();
        plan.copies.push(copy_action());

        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(report.copied + report.uploaded, 0);
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "new/file.txt");
        assert_eq!(deferred.reason, DeferReason::ServerCopyFailed);
        assert!(!deferred.reason.is_lasting());
        assert!(
            deferred
                .detail
                .contains("the destination now holds an entry"),
            "{}",
            deferred.detail
        );
    }

    /// G-2 property: checked after every settlement, the breaker trips exactly when the end of the
    /// run would have, whatever the order of acceptances, refusals and exclusions.
    #[test]
    fn the_running_breaker_verdict_equals_the_end_of_run_verdict() {
        let refusal = |code: i64| Error::RemoteRefused {
            path: "/share/root/x".to_owned(),
            operation: "SYNO.FileStation.Upload.upload".to_owned(),
            code,
            description: "refused".to_owned(),
        };
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for _ in 0..2_000 {
            let items = 1 + next(40) as usize;
            let in_sync = next(10) as usize;
            let outcomes: Vec<u64> = (0..items).map(|_| next(5)).collect();
            let mut running = RefusalBreaker::new(items, in_sync);
            let mut tripped_at = None;
            for (index, outcome) in outcomes.iter().enumerate() {
                match outcome {
                    0 => running.accepted(),
                    1 => running.excluded(),
                    2 => running.settled_by(&refusal(407), "p"),
                    3 => running.settled_by(&refusal(418), "n"),
                    _ => running.settled_by(&Error::SourceChanged("x".into()), "local"),
                }
                if tripped_at.is_none() && running.check().is_err() {
                    tripped_at = Some(index);
                }
            }
            assert_eq!(running.pending, 0, "every item settled exactly once");
            let end = running.check();
            assert_eq!(
                tripped_at.is_some(),
                end.is_err(),
                "{outcomes:?} in_sync={in_sync}"
            );
            if let Some(index) = tripped_at {
                // Once tripped, it stays tripped: nothing after can lift it.
                let mut replay = RefusalBreaker::new(items, in_sync);
                for outcome in &outcomes[..=index] {
                    match outcome {
                        0 => replay.accepted(),
                        1 => replay.excluded(),
                        2 => replay.settled_by(&refusal(407), "p"),
                        3 => replay.settled_by(&refusal(418), "n"),
                        _ => replay.settled_by(&Error::SourceChanged("x".into()), "local"),
                    }
                }
                let Err(Error::RemoteRefusalsShareWide { not_attempted, .. }) = replay.check()
                else {
                    panic!("the replay tripped at the same point");
                };
                assert_eq!(not_attempted, items - index - 1);
            }
        }
    }

    /// BK-1: three lasting refusals among files already in sync are not a share-wide refusal; the
    /// same three on a run that found nothing in sync are.
    #[test]
    fn files_already_in_sync_count_towards_what_the_breaker_considered() {
        let refusal = Error::RemoteRefused {
            path: "/share/root/averylongname.txt".to_owned(),
            operation: "SYNO.FileStation.Upload.upload".to_owned(),
            code: 418,
            description: "refused".to_owned(),
        };
        let mut steady = RefusalBreaker::new(3, 30);
        let mut first = RefusalBreaker::new(3, 0);
        for index in 0..3 {
            steady.settled_by(&refusal, &format!("averylongname{index}.txt"));
            first.settled_by(&refusal, &format!("averylongname{index}.txt"));
        }
        assert!(steady.check().is_ok());
        let error = first.check().unwrap_err().to_string();
        assert!(
            error.starts_with("remote-refused-share-wide: name"),
            "{error}"
        );
        assert!(error.contains("for one cause"), "{error}");
        assert!(
            error.contains("SYNO.FileStation.Upload.upload:418 x3"),
            "{error}"
        );
    }

    /// A DSM refusal of one deletion keeps that entry, and its ancestors, and names it.
    #[test]
    fn a_refused_deletion_is_named_and_the_others_proceed() {
        let client = RefusingDelete {
            refused: "/share/root/a/refused".to_owned(),
            ..RefusingDelete::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes = vec![
            DeleteAction {
                relative: "a/refused".to_owned(),
                local_relative: "a/refused".to_owned(),
                remote_path: "/share/root/a/refused".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
            DeleteAction {
                relative: "extra".to_owned(),
                local_relative: "extra".to_owned(),
                remote_path: "/share/root/extra".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
            DeleteAction {
                relative: "a".to_owned(),
                local_relative: "a".to_owned(),
                remote_path: "/share/root/a".to_owned(),
                kind: EntryKind::Directory,
                type_conflict: false,
                snapshot: RemoteSnapshot {
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_seconds: 0,
                    content_md5: None,
                    require_mtime: false,
                },
                destination_guard: None,
            },
        ];

        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(report.deleted, 1, "the unrelated deletion ran");
        assert_eq!(
            report.deletes_skipped, 1,
            "the parent of a kept entry stays"
        );
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "a/refused");
        assert_eq!(deferred.reason, DeferReason::DeleteFailed);
        let events = client.inner.events.lock().unwrap();
        assert!(!events.contains(&"delete:/share/root/a".to_owned()));
    }

    /// Wraps the mock so the preflight answers the way the real one does for a vanished file.
    #[derive(Clone, Default)]
    struct VanishingPreflight(MockOperations);

    /// Wraps the mock so every server copy finishes with an error.
    #[derive(Clone, Default)]
    struct FailingCopy(MockOperations);

    /// Wraps the mock so DSM refuses one deletion.
    #[derive(Clone, Default)]
    struct RefusingDelete {
        inner: MockOperations,
        /// The remote path refused, or `*` for every one.
        refused: String,
        /// The API and code of the refusal; `SYNO.FileStation.Delete` 900 when unset.
        refused_with: Option<(&'static str, i64)>,
        /// Every deletion attempted, refused or not.
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    }

    macro_rules! delegate_operations {
        ($wrapper:ty, $inner:tt, { $($override:item)* }) => {
            impl SyncOperations for $wrapper {
                $($override)*

                fn preflight_upload_source(
                    &self,
                    local: &LocalEntry,
                    cancellation: &CancellationToken,
                ) -> Result<()> {
                    self.$inner.preflight_upload_source(local, cancellation)
                }

                fn create_folder(&self, remote_path: &str) -> Result<()> {
                    self.$inner.create_folder(remote_path)
                }

                fn supports_rename(&self) -> bool {
                    self.$inner.supports_rename()
                }

                fn rename(
                    &self,
                    root: &RemoteRoot,
                    remote_path: &str,
                    new_name: &str,
                ) -> Result<String> {
                    self.$inner.rename(root, remote_path, new_name)
                }

                fn verify_remote_snapshot_batch(
                    &self,
                    root: &RemoteRoot,
                    scope: &crate::plan::Scope,
                    rules: &crate::local::IgnoreRules,
                    checks: &[SnapshotCheck],
                    cancellation: &CancellationToken,
                ) -> Result<Vec<SnapshotVerdict>> {
                    self.$inner
                        .verify_remote_snapshot_batch(root, scope, rules, checks, cancellation)
                }

                fn verify_remote_metadata_snapshot(
                    &self,
                    remote_path: &str,
                    expected: &RemoteSnapshot,
                    cancellation: &CancellationToken,
                ) -> Result<()> {
                    self.$inner
                        .verify_remote_metadata_snapshot(remote_path, expected, cancellation)
                }

                fn verify_remote_content(
                    &self,
                    remote_path: &str,
                    expected_size: u64,
                    expected_md5: crate::integrity::ContentMd5,
                    cancellation: &CancellationToken,
                ) -> Result<()> {
                    self.$inner.verify_remote_content(
                        remote_path,
                        expected_size,
                        expected_md5,
                        cancellation,
                    )
                }

                fn verify_uploaded_content(
                    &self,
                    remote_path: &str,
                    expected_size: u64,
                    expected_md5: crate::integrity::ContentMd5,
                    cancellation: &CancellationToken,
                ) -> Result<()> {
                    self.$inner.verify_uploaded_content(
                        remote_path,
                        expected_size,
                        expected_md5,
                        cancellation,
                    )
                }

                fn remote_path_state(&self, remote_path: &str) -> Result<RemotePathState> {
                    self.$inner.remote_path_state(remote_path)
                }

                fn upload(
                    &self,
                    local: &LocalEntry,
                    remote_path: &str,
                    observer: Option<UploadObserver>,
                    cancellation: &CancellationToken,
                ) -> Result<()> {
                    self.$inner.upload(local, remote_path, observer, cancellation)
                }
            }
        };
    }

    delegate_operations!(VanishingPreflight, 0, {
        fn preflight_upload_readable(&self, local: &LocalEntry) -> Result<()> {
            if local.relative == "vanished.txt" {
                return Err(Error::FileIo {
                    path: local.full_path.clone(),
                    source: std::io::Error::from(std::io::ErrorKind::NotFound),
                });
            }
            self.0.preflight_upload_readable(local)
        }

        fn delete_non_recursive(&self, root: &RemoteRoot, remote_path: &str) -> Result<()> {
            self.0.delete_non_recursive(root, remote_path)
        }

        fn copy_file_verified(
            &self,
            root: &RemoteRoot,
            action: &crate::plan::CopyAction,
            cancellation: &CancellationToken,
        ) -> Result<()> {
            self.0.copy_file_verified(root, action, cancellation)
        }
    });

    delegate_operations!(FailingCopy, 0, {
        fn preflight_upload_readable(&self, local: &LocalEntry) -> Result<()> {
            self.0.preflight_upload_readable(local)
        }

        fn delete_non_recursive(&self, root: &RemoteRoot, remote_path: &str) -> Result<()> {
            self.0.delete_non_recursive(root, remote_path)
        }

        fn copy_file_verified(
            &self,
            _root: &RemoteRoot,
            action: &crate::plan::CopyAction,
            _cancellation: &CancellationToken,
        ) -> Result<()> {
            Err(Error::ServerCopyFailed {
                destination: action.to_remote_path.clone(),
                detail: "{\"code\":1000}".to_owned(),
            })
        }
    });

    delegate_operations!(RefusingDelete, inner, {
        fn preflight_upload_readable(&self, local: &LocalEntry) -> Result<()> {
            self.inner.preflight_upload_readable(local)
        }

        fn delete_non_recursive(&self, root: &RemoteRoot, remote_path: &str) -> Result<()> {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            if self.refused == "*" || remote_path == self.refused {
                let (api, code) = self
                    .refused_with
                    .unwrap_or(("SYNO.FileStation.Delete", 900));
                return Err(Error::Api {
                    api: api.to_owned(),
                    operation: "delete".to_owned(),
                    code,
                    description: String::new(),
                    details: Vec::new(),
                });
            }
            self.inner.delete_non_recursive(root, remote_path)
        }

        fn copy_file_verified(
            &self,
            root: &RemoteRoot,
            action: &crate::plan::CopyAction,
            cancellation: &CancellationToken,
        ) -> Result<()> {
            self.inner.copy_file_verified(root, action, cancellation)
        }
    });

    #[test]
    fn verified_copy_runs_before_remote_extra_deletion() {
        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.copies.push(copy_action());
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(report.copied, 1);
        assert_eq!(
            *client.events.lock().unwrap(),
            [
                "open:new/file.txt",
                "snapshot:/share/root/old/file.txt",
                // A copy keeps the digest re-check on both sides of the copy: it duplicates remote
                // bytes and verifies the destination against a digest taken from this local file.
                "preflight:new/file.txt",
                "verify:/share/root/old/file.txt:1",
                "copy:/share/root/old/file.txt->/share/root/new/file.txt",
                "preflight:new/file.txt",
                "snapshot:/share/root/new/file.txt",
                "verify:/share/root/new/file.txt:1",
                "snapshot:/share/root/extra",
                "delete:/share/root/extra"
            ]
        );
    }

    #[test]
    fn copy_failure_preserves_planned_remote_extra() {
        let client = MockOperations {
            fail_copy: true,
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.copies.push(copy_action());
        assert!(
            execute_with(
                &client,
                &RemoteRoot::parse("/share/root").unwrap(),
                &plan,
                ExecuteOptions {
                    jobs: 1,
                    dry_run: false,
                },
                |_| {},
            )
            .is_err()
        );
        assert_eq!(
            *client.events.lock().unwrap(),
            [
                "open:new/file.txt",
                "snapshot:/share/root/old/file.txt",
                "preflight:new/file.txt",
                "verify:/share/root/old/file.txt:1",
                "copy:/share/root/old/file.txt->/share/root/new/file.txt"
            ]
        );
    }

    #[test]
    fn a_copied_destination_that_changed_fails_before_success() {
        let client = MockOperations {
            fail_snapshot_path: Some("/share/root/new/file.txt".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes.clear();
        plan.copies.push(copy_action());

        assert!(
            execute_with(
                &client,
                &RemoteRoot::parse("/share/root").unwrap(),
                &plan,
                ExecuteOptions {
                    jobs: 1,
                    dry_run: false,
                },
                |_| {},
            )
            .is_err()
        );
        let events = client.events.lock().unwrap();
        assert!(events.contains(&"snapshot:/share/root/new/file.txt".to_owned()));
        assert!(!events.iter().any(|event| event.starts_with("delete:")));
    }

    #[test]
    fn guarded_copy_source_delete_revalidates_and_preserves_source_on_failure() {
        let client = MockOperations {
            fail_verify_path: Some("/share/root/new/file.txt".to_owned()),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        let snapshot = local("new/file.txt");
        let digest = crate::integrity::ContentMd5::from_bytes([1; 16]);
        plan.copies.clear();
        plan.post_deletes = vec![DeleteAction {
            relative: "old/file.txt".to_owned(),
            local_relative: "old/file.txt".to_owned(),
            remote_path: "/share/root/old/file.txt".to_owned(),
            kind: EntryKind::File,
            type_conflict: false,
            snapshot: file_snapshot(0, Some(digest)),
            destination_guard: Some(DestinationGuard {
                remote_path: "/share/root/new/file.txt".to_owned(),
                local: snapshot,
                expected_size: 1,
                expected_mtime_seconds: 0,
                content_md5: digest,
            }),
        }];

        let report = execute_once(&client, &plan).expect("the source is kept, not the run failed");
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "old/file.txt");
        assert_eq!(deferred.reason, DeferReason::RemoteChanged);
        let events = client.events.lock().unwrap();
        assert!(events.contains(&"verify:/share/root/new/file.txt:1".to_owned()));
        assert!(!events.contains(&"delete:/share/root/old/file.txt".to_owned()));
        let verify_index = events
            .iter()
            .position(|event| event == "verify:/share/root/new/file.txt:1")
            .unwrap();
        assert_eq!(events[verify_index - 1], "preflight:new/file.txt");
    }

    #[test]
    fn deterministic_copy_start_rejection_uses_verified_upload_fallback() {
        let client = MockOperations {
            copy_not_started: true,
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes.clear();
        plan.copies.push(copy_action());

        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(report.copied, 0);
        assert_eq!(report.uploaded, 1);
        assert!(
            client
                .events
                .lock()
                .unwrap()
                .contains(&"upload:new/file.txt".to_owned())
        );
    }

    #[test]
    fn a_second_concurrent_upload_failure_does_not_override_the_first_reported_error() {
        let client = MockOperations {
            fail_upload: true,
            upload_barrier: Some(Arc::new(std::sync::Barrier::new(2))),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.post_deletes.clear();
        plan.uploads = vec![
            UploadAction {
                local: local("one.txt"),
                remote_path: "/share/root/one.txt".to_owned(),
                reason: ChangeReason::MissingRemote,
                expected_remote: None,
            },
            UploadAction {
                local: local("two.txt"),
                remote_path: "/share/root/two.txt".to_owned(),
                reason: ChangeReason::MissingRemote,
                expected_remote: None,
            },
        ];

        let error = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 2,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap_err();

        // The barrier forces both workers into `upload` before either can observe the other's
        // failure, so both fail and only the first failure is surfaced to the caller.
        assert!(matches!(error, Error::Message(message) if message == "upload failed"));
        let events = client.events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("upload:"))
                .count(),
            2
        );
    }

    /// A peer worker's failure must abandon transfers that are already streaming. Without that,
    /// `thread::scope` cannot return until every in-flight upload has run out its own `--timeout`,
    /// which is what made a failed or cancelled run look hung for up to two hours.
    #[test]
    fn an_in_flight_upload_is_abandoned_once_a_peer_worker_fails() {
        let client = MockOperations {
            stream_to_observer: true,
            upload_barrier: Some(Arc::new(std::sync::Barrier::new(2))),
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.post_deletes.clear();
        plan.uploads = vec![
            UploadAction {
                local: local(FAILING_UPLOAD),
                remote_path: format!("/share/root/{FAILING_UPLOAD}"),
                reason: ChangeReason::MissingRemote,
                expected_remote: None,
            },
            UploadAction {
                local: local("streams.txt"),
                remote_path: "/share/root/streams.txt".to_owned(),
                reason: ChangeReason::MissingRemote,
                expected_remote: None,
            },
        ];

        let error = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 2,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap_err();

        assert!(client.observer_declined.load(Ordering::Acquire));
        // The abandoned transfer reports cancellation, but the failure that caused it is what
        // the operator needs to see, so it must not be masked by the abort it triggered.
        assert!(matches!(error, Error::Message(message) if message == "upload failed"));
    }

    /// Cancellation must reach a transfer that is already streaming, not just the queue.
    #[test]
    fn a_cancelled_run_abandons_an_upload_that_is_already_streaming() {
        let client = MockOperations {
            stream_to_observer: true,
            ..MockOperations::default()
        };
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.post_deletes.clear();
        plan.uploads = vec![UploadAction {
            local: local("streams.txt"),
            remote_path: "/share/root/streams.txt".to_owned(),
            reason: ChangeReason::MissingRemote,
            expected_remote: None,
        }];

        let cancellation = CancellationToken::default();
        let observer_cancellation = cancellation.clone();
        let result = execute_with_observer(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            cancellation,
            // Stand in for the operator's Ctrl-C: the token is set while the transfer is in
            // flight, exactly as the signal handler would set it.
            Arc::new(move |_| {
                let cancellation = observer_cancellation.clone();
                let observer: UploadObserver = Arc::new(move |_| {
                    cancellation.cancel();
                    true
                });
                Some(observer)
            }),
            &mut ExecutionReport::default(),
            |_| {},
        );

        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(client.observer_declined.load(Ordering::Acquire));
    }

    #[test]
    fn a_substantive_upload_failure_outranks_the_cancellations_it_causes() {
        let mut first = None;
        record_worker_error(&mut first, Error::Cancelled);
        assert!(matches!(first, Some(Error::Cancelled)));

        record_worker_error(&mut first, Error::Message("upload failed".to_owned()));
        assert!(matches!(&first, Some(Error::Message(message)) if message == "upload failed"));

        // The substantive failure is kept: neither a later cancellation nor a later failure
        // may displace the error that is already recorded.
        record_worker_error(&mut first, Error::Cancelled);
        record_worker_error(&mut first, Error::Message("second failure".to_owned()));
        assert!(matches!(&first, Some(Error::Message(message)) if message == "upload failed"));

        // A run that only ever reports cancellation still reports cancellation.
        let mut cancelled_only = None;
        record_worker_error(&mut cancelled_only, Error::Cancelled);
        record_worker_error(&mut cancelled_only, Error::Cancelled);
        assert!(matches!(cancelled_only, Some(Error::Cancelled)));
    }

    /// A minimal hand-rolled HTTP/1.1 server answering exactly one `SYNO.API.Info` discovery
    /// request, enough for `ApiClient::connect` to succeed without a full DSM handshake.
    fn spawn_discovery_only_server() -> (String, thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();

            let mut received = Vec::new();
            let mut buffer = [0_u8; 8192];
            let header_end = loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "connection closed before request headers");
                received.extend_from_slice(&buffer[..count]);
                if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    break position + 4;
                }
            };
            let header_text = String::from_utf8(received[..header_end].to_vec()).unwrap();
            let mut content_length = 0usize;
            for line in header_text.split("\r\n") {
                if let Some((name, value)) = line.split_once(':')
                    && name.trim().eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            while received.len() - header_end < content_length {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "connection closed before request body");
                received.extend_from_slice(&buffer[..count]);
            }

            let body = serde_json::json!({
                "success": true,
                "data": {
                    "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                    "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                    "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                    "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                    "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3},
                }
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            stream.flush().unwrap();
        });
        (format!("http://{address}/prefix/"), handle)
    }

    #[test]
    fn execute_wrapper_delegates_to_execute_with_using_a_real_api_client() {
        let (base_url, server) = spawn_discovery_only_server();
        let client = ApiClient::connect(&crate::api::ClientOptions {
            base_url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: std::time::Duration::from_secs(2),
            request_timeout: std::time::Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        server.join().unwrap();

        let empty_plan = SyncPlan {
            source_root: absent_source_root(),
            scope: crate::plan::Scope::root(),
            rules: crate::local::IgnoreRules::none(),
            pre_deletes: Vec::new(),
            creates: Vec::new(),
            copies: Vec::new(),
            uploads: Vec::new(),
            post_deletes: Vec::new(),
            renames: Vec::new(),
            unchanged_files: 0,
            protected_entries: 0,
            upload_bytes: 0,
            skipped: Vec::new(),
            link_guard_unchecked: 0,
        };
        let mut reported = Vec::new();
        // `execute` (unlike `execute_with`, used by every other test in this module) is the
        // real public entry point over a concrete `ApiClient`; an empty plan never reaches the
        // network beyond the discovery handshake above, since every snapshot-batch check list
        // it builds is empty and short-circuits before any request.
        let report = execute(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &empty_plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |line| reported.push(line),
        )
        .unwrap();

        assert_eq!(report.deleted, 0);
        assert_eq!(report.created, 0);
        assert_eq!(report.copied, 0);
        assert_eq!(report.uploaded, 0);
        assert_eq!(report.uploaded_bytes, 0);
        assert!(reported.is_empty());
    }

    fn scratch_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sdsync-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// N1/N2: the delete-keep lookup never resolves a name by case. A stat that succeeds through
    /// case folding (Windows, macOS) found a *different* file, kept the deletion and uploaded that
    /// file under the remote spelling, every run.
    ///
    /// F-5: it does resolve a name by Unicode normalisation, as the planner pairs names, and says
    /// which bytes it found. The NFC spelling of a local NFD name is that file.
    #[test]
    fn local_presence_matches_by_normalisation_and_never_by_case() {
        let root = scratch_root("presence");
        std::fs::create_dir_all(root.join("Docs")).unwrap();
        std::fs::write(root.join("Docs/report.pdf"), b"r").unwrap();
        std::fs::write(root.join("caf\u{0065}\u{0301}.txt"), b"nfd").unwrap();

        assert_eq!(
            local_presence(&root, "Docs/report.pdf").unwrap(),
            LocalPresence::Present {
                spelling: "Docs/report.pdf".to_owned()
            }
        );
        assert_eq!(
            local_presence(&root, "Docs/Report.pdf").unwrap(),
            LocalPresence::Absent
        );
        assert_eq!(
            local_presence(&root, "docs/report.pdf").unwrap(),
            LocalPresence::Absent
        );
        assert_eq!(
            local_presence(&root, "caf\u{00e9}.txt").unwrap(),
            LocalPresence::Present {
                spelling: "caf\u{0065}\u{0301}.txt".to_owned()
            },
            "the NFC spelling of a local NFD name is that file, at its own bytes"
        );
        assert_eq!(
            local_presence(&root, "missing.txt").unwrap(),
            LocalPresence::Absent
        );
        assert_eq!(
            local_presence(&root, "Docs/missing-caf\u{00e9}.txt").unwrap(),
            LocalPresence::Absent
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// F-5: two local names equal under normalisation cannot both be the remote entry. Present, so
    /// the deletion is kept, but which one is cannot be told, so nothing is uploaded. An ASCII name a
    /// compatibility singleton composes to is still looked for under its other spellings.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn local_presence_names_an_ambiguous_normalisation() {
        let root = scratch_root("presence-ambiguous");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/caf\u{00e9}.txt"), b"nfc").unwrap();
        std::fs::write(root.join("d/caf\u{0065}\u{0301}.txt"), b"nfd").unwrap();
        std::fs::write(root.join("\u{212a}elvin.txt"), b"kelvin sign").unwrap();
        assert_eq!(
            local_presence(&root, "d/caf\u{00e9}.txt").unwrap(),
            LocalPresence::Ambiguous {
                path: "d/caf\u{00e9}.txt".to_owned()
            }
        );
        assert_eq!(
            local_presence(&root, "Kelvin.txt").unwrap(),
            LocalPresence::Present {
                spelling: "\u{212a}elvin.txt".to_owned()
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// N2: a remote name Windows cannot hold is never resolved to its alias. Win32 strips the
    /// trailing dot or space, so a stat of `notes.` found `notes`, and the kept-delete upload then
    /// failed on the name after the run's other deletions had already happened.
    #[cfg(windows)]
    #[test]
    fn a_name_windows_cannot_hold_is_unrepresentable_not_present() {
        let root = scratch_root("unrepresentable");
        std::fs::write(root.join("notes"), b"n").unwrap();
        for name in ["notes.", "notes ", "dir./notes", "CON", "aux.txt", "a?b"] {
            assert_eq!(
                local_presence(&root, name).unwrap(),
                LocalPresence::Unrepresentable,
                "{name:?}"
            );
        }
        assert_eq!(
            local_presence(&root, "notes").unwrap(),
            LocalPresence::Present {
                spelling: "notes".to_owned()
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// N2 end to end through the executor: an unrepresentable remote-only name is kept and named,
    /// and the run's other deletions still happen.
    #[cfg(windows)]
    #[test]
    fn an_unrepresentable_remote_extra_is_kept_and_named_while_others_are_deleted() {
        let root = scratch_root("unrepresentable-delete");
        std::fs::write(root.join("notes"), b"n").unwrap();
        let client = MockOperations::default();
        let mut plan = populated_plan();
        plan.source_root = root.clone();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes.insert(
            0,
            DeleteAction {
                relative: "notes.".to_owned(),
                local_relative: "notes.".to_owned(),
                remote_path: "/share/root/notes.".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
        );

        let report = execute_once(&client, &plan).unwrap();
        assert_eq!(report.deleted, 1);
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "notes.");
        assert_eq!(deferred.reason, DeferReason::UnrepresentableName);
        let events = client.events.lock().unwrap();
        assert!(!events.contains(&"delete:/share/root/notes.".to_owned()));
        assert!(!events.iter().any(|event| event.starts_with("upload:")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An entry that can be listed but never opened (an app execution alias) is lasting, not a
    /// lock that clears: deferring it as unreadable made every run partial forever.
    #[cfg(windows)]
    #[test]
    fn an_entry_without_data_is_a_lasting_deferral() {
        let error = Error::FileIo {
            path: PathBuf::from("WindowsApps/python.exe"),
            source: std::io::Error::from_raw_os_error(1920),
        };
        assert_eq!(
            DeferReason::for_local_error(&error),
            Some(DeferReason::UnsupportedLocalEntry)
        );
        assert!(DeferReason::UnsupportedLocalEntry.is_lasting());
        let locked = Error::FileIo {
            path: PathBuf::from("mail.pst"),
            source: std::io::Error::from_raw_os_error(32),
        };
        assert_eq!(
            DeferReason::for_local_error(&locked),
            Some(DeferReason::SourceUnreadable)
        );
    }

    /// A plan without a result says whether a deferral is lasting from its own tag.
    #[test]
    fn a_plan_skip_tells_a_lasting_deferral_from_a_transient_one() {
        let skip = |detail: &str| crate::local::SkippedEntry {
            relative: "a".to_owned(),
            reason: crate::local::SkipReason::Deferred {
                detail: detail.to_owned(),
            },
        };
        assert!(skip_is_transient_deferral(&skip(
            "source-changed: source file changed"
        )));
        assert!(!skip_is_transient_deferral(&skip(
            "remote-unreadable: code 105"
        )));
        assert!(!skip_is_transient_deferral(&skip(
            "unsupported-local-entry: cannot access"
        )));
        assert!(!skip_is_transient_deferral(&crate::local::SkippedEntry {
            relative: "b".to_owned(),
            reason: crate::local::SkipReason::LinkOrReparsePoint,
        }));
    }

    #[test]
    fn the_deferral_index_answers_for_ancestors_and_descendants() {
        let mut index = DeferralIndex::default();
        assert!(index.insert("a/b", DeferReason::SourceChanged));
        assert!(
            !index.insert("a/b", DeferReason::RemoteChanged),
            "first reason stays"
        );
        assert_eq!(index.covering("a/b"), Some(DeferReason::SourceChanged));
        assert_eq!(index.covering("a/b/c"), Some(DeferReason::SourceChanged));
        assert_eq!(index.covering("a"), None);
        assert_eq!(index.covering("a/bc"), None);
        assert!(index.has_descendant("a"));
        assert!(!index.has_descendant("a/b"));
        assert!(!index.has_descendant("a-b"));
        assert!(index.relates_to("a"));
        assert!(index.relates_to("a/b/c"));
        assert!(!index.relates_to("z"));
    }

    /// A deferred upload takes its type-conflict replacement and any deletion it was to prove
    /// with it, and the plan's byte total follows.
    #[test]
    fn removing_a_deferred_path_removes_everything_decided_on_its_evidence() {
        let digest = crate::integrity::ContentMd5::from_bytes([3; 16]);
        let mut plan = populated_plan();
        plan.pre_deletes[0].relative = "file.txt".to_owned();
        plan.post_deletes.push(DeleteAction {
            relative: "old/file.txt".to_owned(),
            local_relative: "old/file.txt".to_owned(),
            remote_path: "/share/root/old/file.txt".to_owned(),
            kind: EntryKind::File,
            type_conflict: false,
            snapshot: file_snapshot(1, Some(digest)),
            destination_guard: Some(DestinationGuard {
                remote_path: "/share/root/file.txt".to_owned(),
                local: local("file.txt"),
                expected_size: 1,
                expected_mtime_seconds: 0,
                content_md5: digest,
            }),
        });
        let mut index = DeferralIndex::default();
        index.insert("file.txt", DeferReason::SourceChanged);

        remove_deferred_from_plan(&mut plan, &index);

        assert!(plan.uploads.is_empty());
        assert!(plan.pre_deletes.is_empty());
        assert_eq!(plan.upload_bytes, 0);
        assert_eq!(
            plan.post_deletes
                .iter()
                .map(|action| action.relative.as_str())
                .collect::<Vec<_>>(),
            ["extra"],
            "an unrelated deletion stays; the guarded one goes"
        );
    }

    fn case_rename_plan() -> SyncPlan {
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads = vec![UploadAction {
            local: local("report.pdf"),
            remote_path: "/share/root/report.pdf".to_owned(),
            reason: ChangeReason::MissingRemote,
            expected_remote: None,
        }];
        plan.post_deletes = vec![
            DeleteAction {
                relative: "Report.pdf".to_owned(),
                local_relative: "Report.pdf".to_owned(),
                remote_path: "/share/root/Report.pdf".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
            DeleteAction {
                relative: "extra".to_owned(),
                local_relative: "extra".to_owned(),
                remote_path: "/share/root/extra".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
        ];
        plan
    }

    /// S1-1: the old spelling of a case-only rename is never deleted when the new spelling's
    /// upload was deferred during execution. Before, the NAS was left with neither spelling.
    #[test]
    fn a_rename_whose_upload_is_deferred_keeps_the_old_spelling() {
        let client = MockOperations {
            source_changes_during_upload: Some("report.pdf".to_owned()),
            ..MockOperations::default()
        };

        let report = execute_once(&client, &case_rename_plan()).unwrap();

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.deleted, 1, "the unrelated deletion still runs");
        let events = client.events.lock().unwrap();
        assert!(!events.contains(&"delete:/share/root/Report.pdf".to_owned()));
        assert!(events.contains(&"delete:/share/root/extra".to_owned()));
        assert_eq!(
            report
                .deferred
                .iter()
                .map(|record| (record.relative.as_str(), record.reason))
                .collect::<Vec<_>>(),
            [
                ("report.pdf", DeferReason::SourceChanged),
                ("Report.pdf", DeferReason::SourceChanged),
            ]
        );
    }

    /// S1-1 at plan time: a deferred upload takes the other spelling's deletion out of the plan,
    /// and a directory rename's deletion with it, named.
    #[test]
    fn removing_a_deferred_upload_withholds_every_spelling_related_deletion() {
        let mut plan = case_rename_plan();
        plan.post_deletes.push(DeleteAction {
            relative: "Photos".to_owned(),
            local_relative: "Photos".to_owned(),
            remote_path: "/share/root/Photos".to_owned(),
            kind: EntryKind::Directory,
            type_conflict: false,
            snapshot: file_snapshot(0, None),
            destination_guard: None,
        });
        let mut index = DeferralIndex::default();
        index.insert("report.pdf", DeferReason::SourceChanged);
        index.insert("photos/caf\u{00e9}.jpg", DeferReason::SourceUnreadable);

        let withheld = remove_deferred_from_plan(&mut plan, &index);

        assert_eq!(
            withheld
                .iter()
                .map(|record| record.relative.as_str())
                .collect::<Vec<_>>(),
            ["Report.pdf", "Photos"]
        );
        assert_eq!(
            plan.post_deletes
                .iter()
                .map(|action| action.relative.as_str())
                .collect::<Vec<_>>(),
            ["extra"]
        );
        assert!(index.spelling_related("PHOTOS/CAFE\u{0301}.JPG").is_some());
        assert!(index.spelling_related("photos-old").is_none());
    }

    /// N6: deletions directly inside a short listing, the listed directory itself and its
    /// ancestors are withheld by name; deletions elsewhere are untouched.
    #[test]
    fn deletions_in_an_incomplete_listing_are_withheld_by_name() {
        let delete = |relative: &str, kind: EntryKind| DeleteAction {
            relative: relative.to_owned(),
            local_relative: relative.to_owned(),
            remote_path: format!("/share/root/{relative}"),
            kind,
            type_conflict: false,
            snapshot: file_snapshot(1, None),
            destination_guard: None,
        };
        let mut plan = populated_plan();
        plan.post_deletes = vec![
            delete("top/abe/old.txt", EntryKind::File),
            delete("top/abe/sub/deep.txt", EntryKind::File),
            delete("top/abe", EntryKind::Directory),
            delete("top", EntryKind::Directory),
            delete("elsewhere.txt", EntryKind::File),
        ];

        let withheld =
            withhold_deletions_in_incomplete_listings(&mut plan, &["top/abe".to_owned()]);

        assert_eq!(
            withheld
                .iter()
                .map(|record| record.relative.as_str())
                .collect::<Vec<_>>(),
            ["top/abe/old.txt", "top/abe", "top"]
        );
        assert!(
            withheld
                .iter()
                .all(|record| record.reason == DeferReason::ListingIncomplete)
        );
        assert_eq!(
            plan.post_deletes
                .iter()
                .map(|action| action.relative.as_str())
                .collect::<Vec<_>>(),
            ["top/abe/sub/deep.txt", "elsewhere.txt"],
            "a subdirectory listed on its own, and everything elsewhere, are mirrored as usual"
        );
    }

    /// A re-upload the rebuilt plan wants only for its modification time, of a file this run wrote
    /// and verified, is DSM declining the time and not a failure to converge.
    #[test]
    fn reconciliation_excuses_an_mtime_only_difference_on_a_file_the_run_wrote() {
        let mut executed = populated_plan();
        executed.pre_deletes.clear();
        executed.post_deletes.clear();
        executed.creates.clear();
        let mut rebuilt = executed.clone();
        rebuilt.uploads[0].reason = ChangeReason::MtimeDiffers;

        let started_at = std::time::SystemTime::now();
        assert_eq!(
            ensure_reconciled(
                std::path::Path::new("."),
                &executed,
                &rebuilt,
                started_at,
                &DeferralIndex::default()
            )
            .unwrap(),
            Reconciliation::default()
        );
        rebuilt.uploads[0].reason = ChangeReason::ContentDiffers;
        assert!(matches!(
            ensure_reconciled(
                std::path::Path::new("."),
                &executed,
                &rebuilt,
                started_at,
                &DeferralIndex::default()
            ),
            Err(Error::ReconciliationPending { operations: 1 })
        ));
    }

    fn deletions(relatives: &[&str]) -> Vec<DeleteAction> {
        relatives
            .iter()
            .map(|relative| DeleteAction {
                relative: (*relative).to_owned(),
                local_relative: (*relative).to_owned(),
                remote_path: format!("/share/root/{relative}"),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            })
            .collect()
    }

    fn deletion_only_plan(relatives: &[&str]) -> SyncPlan {
        let mut plan = populated_plan();
        plan.pre_deletes.clear();
        plan.creates.clear();
        plan.uploads.clear();
        plan.post_deletes = deletions(relatives);
        plan
    }

    /// ADV-E-9 / ADV-R-8: a session DSM will not give back (a re-login refused) is not a refusal
    /// of one entry. The run stops at the first deletion, once, with that cause; it used to attempt
    /// every remaining deletion on the dead session and name each `delete-failed`.
    #[test]
    fn a_dead_session_in_the_delete_phase_stops_at_the_first_deletion() {
        let client = RefusingDelete {
            refused: "*".to_owned(),
            refused_with: Some(("SYNO.API.Auth", 400)),
            ..RefusingDelete::default()
        };
        let plan = deletion_only_plan(&["a.txt", "b.txt", "c.txt"]);
        let error = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap_err();
        assert!(error.is_session_failure(), "{error}");
        assert_eq!(client.attempts.load(Ordering::Acquire), 1);
    }

    /// ADV-E-6: DSM refusing a deletion for permission recurs on every run, so it is lasting and
    /// does not make every run partial. A transient refusal (900) stays `delete-failed`.
    #[test]
    fn a_permission_refused_deletion_is_lasting() {
        for (code, expected) in [
            (407, DeferReason::DeleteNotPermitted),
            (105, DeferReason::DeleteNotPermitted),
            (900, DeferReason::DeleteFailed),
        ] {
            let client = RefusingDelete {
                refused: "/share/root/a.txt".to_owned(),
                refused_with: Some(("SYNO.FileStation.Delete", code)),
                ..RefusingDelete::default()
            };
            let report = execute_with(
                &client,
                &RemoteRoot::parse("/share/root").unwrap(),
                &deletion_only_plan(&["a.txt", "b.txt"]),
                ExecuteOptions {
                    jobs: 1,
                    dry_run: false,
                },
                |_| {},
            )
            .unwrap();
            assert_eq!(report.deleted, 1);
            assert_eq!(only_deferral(&report).reason, expected, "code {code}");
            assert_eq!(expected.is_lasting(), code != 900);
        }
    }

    /// ADV-E-6: a local file this account may not read is lasting; a lock is not.
    #[test]
    fn permission_denied_is_lasting_and_a_lock_is_not() {
        let denied = Error::FileIo {
            path: PathBuf::from("secret.txt"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert_eq!(
            DeferReason::for_local_error(&denied),
            Some(DeferReason::LocalPermissionDenied)
        );
        assert!(DeferReason::LocalPermissionDenied.is_lasting());
        let locked = Error::FileIo {
            path: PathBuf::from("mailbox"),
            source: std::io::Error::other("sharing violation"),
        };
        assert_eq!(
            DeferReason::for_local_error(&locked),
            Some(DeferReason::SourceUnreadable)
        );
        assert!(!DeferReason::SourceUnreadable.is_lasting());
    }

    fn rename_plan() -> SyncPlan {
        let mut plan = deletion_only_plan(&["Photos/gone.jpg"]);
        // As the planner records it: the rename pairs `Photos` with the local `photos`.
        plan.post_deletes[0].local_relative = "photos/gone.jpg".to_owned();
        plan.renames.push(crate::plan::RenameAction {
            relative: "photos".to_owned(),
            from_relative: "Photos".to_owned(),
            from_remote_path: "/share/root/Photos".to_owned(),
            new_name: "photos".to_owned(),
            to_remote_path: "/share/root/photos".to_owned(),
            kind: EntryKind::Directory,
        });
        plan
    }

    /// ADV-E-1: a case-only rename runs last, after every deletion, and is counted.
    #[test]
    fn a_case_rename_runs_last() {
        let client = MockOperations::default();
        let mut events = Vec::new();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &rename_plan(),
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |event| events.push(event),
        )
        .unwrap();
        assert_eq!(report.renamed, 1);
        assert!(report.deferred.is_empty());
        let recorded = client.events.lock().unwrap().clone();
        let delete = recorded
            .iter()
            .position(|event| event == "delete:/share/root/Photos/gone.jpg")
            .expect("the deletion ran");
        let rename = recorded
            .iter()
            .position(|event| event == "rename:/share/root/Photos->photos")
            .expect("the rename ran");
        assert!(delete < rename, "{recorded:?}");
        assert!(events.contains(&ExecutionEvent::Renamed {
            from_remote_path: "/share/root/Photos".to_owned(),
            to_remote_path: "/share/root/photos".to_owned(),
        }));
    }

    /// ADV-E-1, mandatory: a rename DSM refuses deletes nothing on its account, is named
    /// `case-rename-pending` (a lasting warning; the next run tries the rename again), and the
    /// run still completes everything else.
    #[test]
    fn a_refused_case_rename_is_pending_and_deletes_nothing() {
        let client = MockOperations {
            fail_rename: Some(900),
            ..MockOperations::default()
        };
        let mut plan = rename_plan();
        plan.post_deletes.clear();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(report.renamed, 0);
        assert_eq!(report.deleted, 0);
        let deferred = only_deferral(&report);
        assert_eq!(deferred.relative, "photos");
        assert_eq!(deferred.reason, DeferReason::CaseRenamePending);
        // Lasting: only renaming it in File Station settles it, and the rename is tried again on
        // every run where it can be.
        assert!(deferred.reason.is_lasting());
        assert!(
            deferred.detail.contains("/share/root/Photos"),
            "{}",
            deferred.detail
        );
        assert!(
            !client
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("delete:"))
        );
    }

    /// A rename the rebuilt plan still holds is pending unless the run named it.
    #[test]
    fn reconciliation_holds_a_pending_rename_to_its_deferral() {
        let executed = rename_plan();
        let mut rebuilt = rename_plan();
        rebuilt.post_deletes.clear();
        let started_at = std::time::SystemTime::now();
        assert!(matches!(
            ensure_reconciled(
                std::path::Path::new("."),
                &executed,
                &rebuilt,
                started_at,
                &DeferralIndex::default()
            ),
            Err(Error::ReconciliationPending { operations: 1 })
        ));
        let mut named = DeferralIndex::default();
        named.insert("photos", DeferReason::CaseRenamePending);
        assert!(
            ensure_reconciled(
                std::path::Path::new("."),
                &executed,
                &rebuilt,
                started_at,
                &named
            )
            .is_ok()
        );
    }

    /// Deferrals the plan names among its skips (a folder the walk could not read, a rename
    /// planning could not finish) become records, once.
    #[test]
    fn deferrals_named_in_plan_skips_become_records() {
        let mut plan = populated_plan();
        plan.skipped = vec![
            crate::local::SkippedEntry {
                relative: "locked".to_owned(),
                reason: crate::local::SkipReason::Deferred {
                    detail: "local-permission-denied: failed to read \"locked\": denied".to_owned(),
                },
            },
            crate::local::SkippedEntry {
                relative: "Photos".to_owned(),
                reason: crate::local::SkipReason::Deferred {
                    detail: "case-rename-pending: the NAS holds both".to_owned(),
                },
            },
            crate::local::SkippedEntry {
                relative: "link".to_owned(),
                reason: crate::local::SkipReason::LinkOrReparsePoint,
            },
        ];
        let known = [DeferredPath {
            relative: "Photos".to_owned(),
            reason: DeferReason::CaseRenamePending,
            detail: "already named".to_owned(),
        }];
        let records = deferrals_in_plan_skips(&plan, &known);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].relative, "locked");
        assert_eq!(records[0].reason, DeferReason::LocalPermissionDenied);
        assert_eq!(records[0].detail, "failed to read \"locked\": denied");
    }

    /// C-R3: a folder holding a NAS entry the walk would not carry is never deleted, nor is any
    /// folder above it, and a type replacement that needed one is withheld with what depended on it.
    #[test]
    fn folders_holding_an_uncarried_remote_entry_are_never_deleted() {
        let mut plan = deletion_only_plan(&["top/odd", "top", "elsewhere.txt"]);
        plan.pre_deletes = deletions(&["swap"]);
        plan.creates.push(CreateAction {
            relative: "swap".to_owned(),
            remote_path: "/share/root/swap".to_owned(),
            reason: ChangeReason::TypeReplaced,
        });
        let skipped = [
            crate::api::SkippedRemoteEntry {
                parent: "top/odd".to_owned(),
                name: "Icon\r".to_owned(),
                remote_path: "/share/root/top/odd/Icon\r".to_owned(),
                kind: EntryKind::File,
                reason: crate::api::RemoteSkipReason::UnrepresentableName,
                excluded: false,
                cause: "control character".to_owned(),
            },
            crate::api::SkippedRemoteEntry {
                parent: "swap".to_owned(),
                name: "a\\b".to_owned(),
                remote_path: "/share/root/swap/a\\b".to_owned(),
                kind: EntryKind::File,
                reason: crate::api::RemoteSkipReason::UnrepresentableName,
                excluded: true,
                cause: "backslash".to_owned(),
            },
        ];
        let deferred = protect_skipped_remote_entries(&mut plan, &skipped);
        assert_eq!(
            plan.post_deletes
                .iter()
                .map(|action| action.relative.as_str())
                .collect::<Vec<_>>(),
            ["elsewhere.txt"]
        );
        assert!(plan.pre_deletes.is_empty());
        assert!(plan.creates.is_empty(), "the replacement cannot land");
        let named = deferred
            .iter()
            .map(|record| (record.relative.as_str(), record.reason))
            .collect::<Vec<_>>();
        assert!(
            named.contains(&("top/odd/Icon\\r", DeferReason::RemoteSkipped)),
            "{named:?}"
        );
        assert!(
            named.contains(&("top", DeferReason::RemoteSkipped)),
            "{named:?}"
        );
        assert!(
            named.contains(&("swap", DeferReason::RemoteSkipped)),
            "{named:?}"
        );
        assert!(
            !named
                .iter()
                .any(|(relative, _)| relative.ends_with("a\\\\b")),
            "an excluded entry is protected but not reported: {named:?}"
        );
        // Reported, by name, on every run until someone renames or removes it on the NAS: a
        // lasting warning, never a partial run that would re-trigger forever.
        assert!(DeferReason::RemoteSkipped.is_lasting());
    }

    /// ADV-E-1 delete safety: a remote-only file beneath a folder a case rename pairs is checked
    /// for local presence under its *local* spelling. A file recreated there since the scan keeps
    /// its NAS copy, as it would anywhere else.
    #[test]
    fn a_deletion_beneath_a_renamed_folder_checks_the_local_spelling() {
        let source = std::env::temp_dir().join(format!(
            "sdsync-rename-presence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(source.join("photos")).unwrap();
        std::fs::write(source.join("photos/gone.jpg"), b"back again").unwrap();
        let mut plan = rename_plan();
        plan.source_root = source.clone();
        let client = MockOperations::default();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        );
        std::fs::remove_dir_all(&source).unwrap();
        let report = report.unwrap();
        assert_eq!(report.deleted, 0, "{report:?}");
        assert_eq!(report.deletes_skipped, 1);
        assert!(
            !client
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("delete:"))
        );
    }

    /// D3-6: a deletion beneath a normalisation-paired folder is checked at its recorded local
    /// spelling, and a recreated leaf is found under either normalisation of its own name. The NAS
    /// copy is kept, and the kept deletion uploads the file found, under the NAS spelling.
    #[test]
    fn a_deletion_beneath_a_normalisation_paired_folder_checks_the_recorded_spelling() {
        let source = scratch_root("nfd-presence");
        std::fs::create_dir_all(source.join("cafe\u{301}")).unwrap();
        std::fs::write(source.join("cafe\u{301}/nai\u{308}ve.txt"), b"back again").unwrap();
        let mut plan = deletion_only_plan(&["caf\u{e9}/na\u{ef}ve.txt"]);
        plan.post_deletes[0].local_relative = "cafe\u{301}/na\u{ef}ve.txt".to_owned();
        plan.source_root = source.clone();
        let client = MockOperations::default();
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        );
        std::fs::remove_dir_all(&source).unwrap();
        let report = report.unwrap();
        assert_eq!(report.deleted, 0, "{report:?}");
        assert_eq!(report.deletes_skipped, 1);
        assert_eq!(report.kept_deletes_uploaded, 1, "{report:?}");
        let events = client.events.lock().unwrap().clone();
        assert!(
            !events.iter().any(|event| event.starts_with("delete:")),
            "{events:?}"
        );
        // The file found, at its own bytes.
        assert!(
            events
                .iter()
                .any(|event| event == "upload:cafe\u{301}/nai\u{308}ve.txt"),
            "{events:?}"
        );
    }

    /// D3-7 / no silent failures: a file found back at its deletion's presence check and gone
    /// again before the kept deletion's upload is named `source-vanished`, its NAS copy kept, and
    /// the run is not complete. It used to be skipped with nothing recorded.
    #[test]
    fn a_reappeared_file_gone_again_before_its_upload_is_named() {
        let source = scratch_root("present-then-gone");
        std::fs::create_dir_all(source.join("a")).unwrap();
        std::fs::write(source.join("a/gone.txt"), b"back for a moment").unwrap();
        let mut plan = deletion_only_plan(&["a/gone.txt", "z.txt"]);
        plan.source_root = source.clone();
        let client = MockOperations {
            remove_on_delete: Some(("/share/root/z.txt".to_owned(), source.join("a/gone.txt"))),
            ..MockOperations::default()
        };
        let report = execute_with(
            &client,
            &RemoteRoot::parse("/share/root").unwrap(),
            &plan,
            ExecuteOptions {
                jobs: 1,
                dry_run: false,
            },
            |_| {},
        );
        std::fs::remove_dir_all(&source).unwrap();
        let report = report.unwrap();
        assert_eq!(report.deleted, 1, "only z.txt: {report:?}");
        assert_eq!(report.kept_deletes_uploaded, 0);
        let deferred: Vec<(&str, DeferReason)> = report
            .deferred
            .iter()
            .map(|record| (record.relative.as_str(), record.reason))
            .collect();
        assert_eq!(deferred, [("a/gone.txt", DeferReason::SourceVanished)]);
        assert!(
            !DeferReason::SourceVanished.is_lasting(),
            "the run is partial"
        );
        let events = client.events.lock().unwrap().clone();
        assert!(
            !events
                .iter()
                .any(|event| event == "delete:/share/root/a/gone.txt"),
            "{events:?}"
        );
    }

    /// Answers the comma guard's reads from a fixed set, and counts them.
    #[derive(Default)]
    struct KnownEntries {
        existing: Vec<&'static str>,
        asked: Mutex<Vec<String>>,
    }

    impl RemoteEntryLookup for KnownEntries {
        fn remote_entry_exists(&self, remote_path: &str) -> Result<bool> {
            self.asked.lock().unwrap().push(remote_path.to_owned());
            Ok(self.existing.contains(&remote_path))
        }
    }

    fn named(deferred: &[DeferredPath]) -> Vec<(&str, DeferReason)> {
        deferred
            .iter()
            .map(|record| (record.relative.as_str(), record.reason))
            .collect()
    }

    /// N1: a deletion of a comma path whose part before the comma is an entry the inventory holds
    /// is withheld, by name, as the lasting `remote-skipped`, with every folder above it; one whose
    /// prefix is nothing is carried. Nothing is asked of the NAS for a prefix the inventory holds.
    #[test]
    fn a_deletion_that_could_address_another_entry_is_withheld_with_its_folders() {
        let mut plan = deletion_only_plan(&["d/a,b.txt", "d", "x,y.txt", "plain.txt"]);
        plan.post_deletes[1].kind = EntryKind::Directory;
        let remote = scoped_remote(&["d", "d/a"]);
        let lookup = KnownEntries::default();
        let deferred = withhold_comma_ambiguous_mutations(&lookup, &mut plan, &remote).unwrap();
        assert_eq!(
            named(&deferred),
            [
                ("d/a,b.txt", DeferReason::RemoteSkipped),
                ("d", DeferReason::RemoteSkipped)
            ]
        );
        assert!(DeferReason::RemoteSkipped.is_lasting());
        assert!(
            deferred[0].detail.contains("\"/share/root/d/a\""),
            "{deferred:?}"
        );
        assert_eq!(
            plan.post_deletes
                .iter()
                .map(|action| action.relative.as_str())
                .collect::<Vec<_>>(),
            ["x,y.txt", "plain.txt"]
        );
        // `/share/root/x` is not in the inventory, so the NAS was asked, once.
        assert_eq!(*lookup.asked.lock().unwrap(), ["/share/root/x"]);
    }

    /// N1: a prefix outside the inventory is read from the NAS; an existing one withholds the
    /// mutation. A withheld type replacement keeps the content that was to replace it, a withheld
    /// rename stays pending, and a withheld server copy is uploaded instead.
    #[test]
    fn a_comma_path_whose_prefix_the_nas_holds_is_not_mutated() {
        let mut plan = populated_plan();
        plan.pre_deletes[0].relative = "n,1".to_owned();
        plan.pre_deletes[0].remote_path = "/share/root/n,1".to_owned();
        plan.uploads[0].local = local("n,1");
        plan.uploads[0].remote_path = "/share/root/n,1".to_owned();
        plan.renames.push(crate::plan::RenameAction {
            relative: "r,1".to_owned(),
            from_relative: "R,1".to_owned(),
            from_remote_path: "/share/root/R,1".to_owned(),
            new_name: "r,1".to_owned(),
            to_remote_path: "/share/root/r,1".to_owned(),
            kind: EntryKind::File,
        });
        let digest = crate::integrity::ContentMd5::from_digests([7; 16], 7, [7; 32]);
        plan.copies.push(crate::plan::CopyAction {
            from_relative: "old/c,1".to_owned(),
            from_remote_path: "/share/root/old/c,1".to_owned(),
            to_relative: "new/c,1".to_owned(),
            to_remote_path: "/share/root/new/c,1".to_owned(),
            local: local("new/c,1"),
            expected_size: 1,
            content_md5: digest,
            source_snapshot: file_snapshot(1, Some(digest)),
        });
        let lookup = KnownEntries {
            existing: vec!["/share/root/n", "/share/root/R", "/share/root/old/c"],
            ..KnownEntries::default()
        };
        let deferred =
            withhold_comma_ambiguous_mutations(&lookup, &mut plan, &scoped_remote(&[])).unwrap();
        assert_eq!(
            named(&deferred),
            [
                ("n,1", DeferReason::RemoteSkipped),
                ("r,1", DeferReason::RemoteSkipped)
            ]
        );
        assert!(plan.pre_deletes.is_empty());
        assert!(plan.renames.is_empty());
        assert!(plan.copies.is_empty());
        assert_eq!(
            plan.uploads
                .iter()
                .map(|action| action.remote_path.as_str())
                .collect::<Vec<_>>(),
            ["/share/root/new/c,1"],
            "the replacement is withheld and the copy is uploaded instead"
        );
        assert_eq!(plan.upload_bytes, 1);
        // The plain deletion is untouched.
        assert_eq!(plan.post_deletes.len(), 1);
    }

    /// P-7: a folder deleted while the walk listed it is a transient `remote-changed`, by name.
    #[test]
    fn a_vanished_remote_folder_is_a_transient_named_deferral() {
        let deferred = vanished_remote_folder_deferrals(&["tmp-123".to_owned()]);
        assert_eq!(named(&deferred), [("tmp-123", DeferReason::RemoteChanged)]);
        assert!(!DeferReason::RemoteChanged.is_lasting());
        assert!(vanished_remote_folder_deferrals(&[]).is_empty());
    }

    fn scoped_local(relatives: &[(&str, EntryKind)]) -> LocalInventory {
        LocalInventory {
            root: absent_source_root(),
            entries: relatives
                .iter()
                .map(|(relative, kind)| {
                    (
                        (*relative).to_owned(),
                        LocalEntry {
                            relative: (*relative).to_owned(),
                            full_path: PathBuf::from(relative),
                            kind: *kind,
                            size: 1,
                            mtime_ms: 1_000,
                            identity: Default::default(),
                            times: Default::default(),
                            content_md5: None,
                        },
                    )
                })
                .collect(),
            skipped: Vec::new(),
        }
    }

    fn scoped_remote(relatives: &[&str]) -> RemoteInventory {
        RemoteInventory {
            root_exists: true,
            entries: relatives
                .iter()
                .map(|relative| {
                    (
                        (*relative).to_owned(),
                        crate::api::RemoteEntry {
                            relative: (*relative).to_owned(),
                            remote_path: format!("/share/root/{relative}"),
                            kind: EntryKind::Directory,
                            size: 0,
                            mtime_seconds: 0,
                            ctime_seconds: None,
                            crtime_seconds: None,
                            digest_nas_time: None,
                            mount_point_type: None,
                            content_md5: None,
                        },
                    )
                })
                .collect(),
        }
    }

    /// ADV-E-1: a scoped run naming the new spelling of a renamed folder never creates it beside
    /// the old one; the scope is withheld as a remote case collision, by name.
    #[test]
    fn a_scope_the_nas_holds_under_another_spelling_is_withheld() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let mut listed = Vec::new();
        let mut local = scoped_local(&[
            ("docs", EntryKind::Directory),
            ("docs/photos", EntryKind::Directory),
            ("docs/photos/a.jpg", EntryKind::File),
        ]);
        withhold_scope_spelling_conflicts(
            &root,
            &crate::plan::Scope::parse("docs/photos").unwrap(),
            &mut local,
            &scoped_remote(&["docs"]),
            |path| {
                listed.push(path.to_owned());
                Ok((vec!["Photos".to_owned(), "other".to_owned()], true))
            },
        )
        .unwrap();
        assert_eq!(listed, ["/share/root/docs"]);
        assert_eq!(local.entries.keys().collect::<Vec<_>>(), ["docs"]);
        assert_eq!(
            local.skipped,
            [crate::local::SkippedEntry {
                relative: "docs/photos".to_owned(),
                reason: crate::local::SkipReason::RemoteCaseCollision {
                    other: "docs/Photos".to_owned(),
                },
            }]
        );

        // No other spelling: nothing is withheld, and an existing scope is never listed.
        let mut local = scoped_local(&[("photos", EntryKind::Directory)]);
        withhold_scope_spelling_conflicts(
            &root,
            &crate::plan::Scope::parse("photos").unwrap(),
            &mut local,
            &scoped_remote(&[]),
            |_| Ok((vec!["photos-old".to_owned()], true)),
        )
        .unwrap();
        assert!(local.skipped.is_empty());
        // A short listing cannot rule the other spelling out.
        let mut short = scoped_local(&[("photos", EntryKind::Directory)]);
        withhold_scope_spelling_conflicts(
            &root,
            &crate::plan::Scope::parse("photos").unwrap(),
            &mut short,
            &scoped_remote(&[]),
            |_| Ok((Vec::new(), false)),
        )
        .unwrap();
        assert!(short.entries.is_empty());
        assert!(matches!(
            short.skipped.as_slice(),
            [crate::local::SkippedEntry { reason: crate::local::SkipReason::Deferred { detail }, .. }]
                if detail.starts_with("listing-incomplete: ")
        ));
        withhold_scope_spelling_conflicts(
            &root,
            &crate::plan::Scope::parse("photos").unwrap(),
            &mut local,
            &scoped_remote(&["photos"]),
            |_| panic!("an existing scope needs no listing"),
        )
        .unwrap();
        assert!(local.skipped.is_empty());
    }

    /// Link boundaries almost everywhere are an API-shape mismatch, not links: the run fails with
    /// the named cause. A few real links among many folders are per-path skips.
    #[test]
    fn link_boundaries_almost_everywhere_fail_the_run() {
        let counts = |folders_checked, folders_linked| crate::api::LinkCheckCounts {
            folders_checked,
            folders_linked,
            ..Default::default()
        };
        let mut remote = scoped_remote(&["a", "b", "c", "d"]);
        for entry in remote.entries.values_mut().take(3) {
            entry.mount_point_type = Some(crate::api::LINKED_ENTRY_MOUNT_TYPE.to_owned());
        }
        let error = check_link_boundaries(&remote, &counts(4, 3)).unwrap_err();
        assert!(
            error.to_string().starts_with(REMOTE_REAL_PATH_UNUSABLE),
            "{error}"
        );
        assert!(
            error.to_string().contains("Exclude the links by name"),
            "{error}"
        );

        let mut mostly_folders = scoped_remote(&["a", "b", "c", "d", "e", "f", "g"]);
        for entry in mostly_folders.entries.values_mut().take(3) {
            entry.mount_point_type = Some(crate::api::LINKED_ENTRY_MOUNT_TYPE.to_owned());
        }
        assert!(check_link_boundaries(&mostly_folders, &counts(7, 3)).is_ok());
        // A real mount point is not a link.
        let mut mounts = scoped_remote(&["a", "b", "c"]);
        for entry in mounts.entries.values_mut() {
            entry.mount_point_type = Some("cifs".to_owned());
        }
        assert!(check_link_boundaries(&mounts, &counts(3, 0)).is_ok());
    }

    /// G-3: the shape breaker counts files on their own, so a destination of files whose real
    /// paths all look wrong fails by name instead of withholding every edit with exit 0. Only what
    /// the guard evaluated counts: entries it could not check, or excluded, never dilute it.
    #[test]
    fn link_boundaries_count_files_and_folders_separately() {
        let remote = scoped_remote(&["a", "b"]);
        let files = crate::api::LinkCheckCounts {
            folders_checked: 2,
            folders_linked: 0,
            files_checked: 7,
            files_linked: 5,
        };
        let error = check_link_boundaries(&remote, &files)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with(REMOTE_REAL_PATH_UNUSABLE), "{error}");
        assert!(error.contains("5 of the 7 NAS files"), "{error}");
        // Two of a kind are named skips, never a failed run.
        let two = crate::api::LinkCheckCounts {
            files_checked: 2,
            files_linked: 2,
            folders_checked: 2,
            folders_linked: 2,
        };
        assert!(check_link_boundaries(&remote, &two).is_ok());
        // Three genuine links among many files are their own skips.
        let few = crate::api::LinkCheckCounts {
            files_checked: 40,
            files_linked: 3,
            ..Default::default()
        };
        assert!(check_link_boundaries(&remote, &few).is_ok());
    }

    /// ADV3-P-3 breaker: one folder the account may not list is that folder's lasting warning,
    /// but most of the share refused (at least three, more than half the folders) is the
    /// account's rights, and fails the run naming them rather than ending amber with nothing done.
    #[test]
    fn unlistable_folders_almost_everywhere_fail_the_run() {
        let mark = |remote: &mut RemoteInventory, count: usize| {
            for entry in remote.entries.values_mut().take(count) {
                entry.mount_point_type = Some(crate::api::UNLISTABLE_ENTRY_MOUNT_TYPE.to_owned());
            }
        };
        let mut refused = scoped_remote(&["a", "b", "c", "d"]);
        mark(&mut refused, 3);
        let error = check_link_boundaries(&refused, &Default::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("3 of the 4 folders"), "{error}");
        assert!(error.contains("a, b, c"), "{error}");

        let mut some = scoped_remote(&["a", "b", "c", "d", "e", "f", "g"]);
        mark(&mut some, 3);
        assert!(check_link_boundaries(&some, &Default::default()).is_ok());
        // Two are never enough, however few folders there are.
        let mut two = scoped_remote(&["a", "b"]);
        mark(&mut two, 2);
        assert!(check_link_boundaries(&two, &Default::default()).is_ok());
    }
}
