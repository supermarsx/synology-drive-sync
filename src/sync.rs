use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use crate::api::{ApiClient, UploadObserver};
use crate::cancel::CancellationToken;
use crate::local::LocalEntry;
use crate::path::RemoteRoot;
use crate::plan::{RemoteSnapshot, SyncPlan};
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
    relative: String,
    remote_path: String,
    expected: RemoteSnapshot,
    deleting: bool,
    evidence: ContentEvidence,
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
    ) -> Result<()>;
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
    ) -> Result<()> {
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

/// Re-read the destination once and check every snapshot in the batch against it.
///
/// Scoped, and pruned by the run's own rules, because it has to answer the same question the
/// planner's inventory answered. An unscoped walk here was both a cost and a correctness problem: a
/// `--scope docs/q3` run paid a full recursive inventory of the whole destination per batch, up to
/// four times per run, and a subtree outside the scope that the sync account cannot list returns DSM
/// code 105, which propagates -- failing a scoped run *after its uploads had landed* for a directory
/// it was explicitly told to ignore, and reporting it as a snapshot problem.
fn verify_api_remote_snapshot_batch(
    client: &ApiClient,
    root: &RemoteRoot,
    scope: &crate::plan::Scope,
    rules: &crate::local::IgnoreRules,
    checks: &[SnapshotCheck],
    cancellation: &CancellationToken,
) -> Result<()> {
    if checks.is_empty() {
        return Ok(());
    }
    cancellation.check()?;
    let current_inventory = client
        .remote_inventory_scoped(root, scope, rules, usize::MAX, cancellation)?
        .inventory;
    cancellation.check()?;
    let deleting: std::collections::BTreeSet<&str> = checks
        .iter()
        .filter(|check| check.deleting)
        .map(|check| check.relative.as_str())
        .collect();
    for check in checks {
        let Some(current) = current_inventory.entries.get(&check.relative) else {
            return Err(remote_snapshot_changed(&check.remote_path));
        };
        if current.remote_path != check.remote_path
            || current.kind != check.expected.kind
            || current.size != check.expected.size
            || (check.expected.require_mtime
                && current.mtime_seconds != check.expected.mtime_seconds)
        {
            return Err(remote_snapshot_changed(&check.remote_path));
        }
        if check.deleting && check.expected.kind == crate::local::EntryKind::Directory {
            let prefix = format!("{}/", check.relative);
            if current_inventory.entries.keys().any(|candidate| {
                candidate.starts_with(&prefix) && !deleting.contains(candidate.as_str())
            }) {
                return Err(remote_snapshot_changed(&check.remote_path));
            }
        }
    }
    cancellation.check()?;
    Ok(())
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
/// Reported as absent only on a definite `NotFound`. Any other error means absence was not
/// established, and a delete may not proceed on a precondition nobody could read: the caller sees
/// the filesystem error instead. That is the direction to fail in, because the alternative is
/// deleting remote data on the strength of a failed local stat.
fn local_path_is_absent(source_root: &std::path::Path, relative: &str) -> Result<bool> {
    let path = source_root.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(source) => Err(Error::FileIo { path, source }),
    }
}

fn delete_snapshot_check(action: &crate::plan::DeleteAction) -> SnapshotCheck {
    SnapshotCheck {
        relative: action.relative.clone(),
        remote_path: action.remote_path.clone(),
        expected: action.snapshot.clone(),
        deleting: true,
        // This digest authorises destroying remote data and every revision DSM holds of it. It is
        // required to carry full proof, and full proof is not something File Station can compute.
        evidence: ContentEvidence::ServerProduced,
    }
}

fn destination_snapshot_check(action: &crate::plan::CopyAction) -> SnapshotCheck {
    SnapshotCheck {
        relative: action.to_relative.clone(),
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
    }
}

fn upload_destination_snapshot_check(action: &crate::plan::UploadAction) -> SnapshotCheck {
    SnapshotCheck {
        relative: action.local.relative.clone(),
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
    client.verify_remote_metadata_snapshot(&check.remote_path, &check.expected, cancellation)?;
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

/// One remote mutation, reported as it happens.
///
/// Every variant but [`Self::RemoteExtraDeleteSkipped`] corresponds to exactly one successful
/// operation, so a caller can count progress by matching on the variant instead of parsing the
/// rendered text. `Display` produces the diagnostic line shown under `-v`.
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
    /// A planned server-side copy the server refused to start, uploaded instead. Unlike
    /// `Uploaded`, this transfer runs without an upload observer, so it reaches a progress
    /// caller only through this event.
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
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
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

    // Open every scheduled source before a type-conflict deletion can remove remote data. The
    // question here is whether the transfer can still happen at all, not whether the content is
    // still what was planned -- the upload itself fingerprints the bytes it sends and fails on a
    // change, and a copy re-checks its own source against its digest immediately before copying.
    // Asking for the digest here read and hashed every uploaded file a second time, for an answer
    // the transfer establishes more precisely a few seconds later.
    for action in &plan.uploads {
        cancellation.check()?;
        client.preflight_upload_readable(&action.local)?;
    }
    for action in &plan.copies {
        cancellation.check()?;
        client.preflight_upload_readable(&action.local)?;
    }

    let pre_delete_checks = plan
        .pre_deletes
        .iter()
        .map(delete_snapshot_check)
        .collect::<Vec<_>>();
    client.verify_remote_snapshot_batch(
        root,
        &plan.scope,
        &plan.rules,
        &pre_delete_checks,
        &cancellation,
    )?;
    for (action, check) in plan.pre_deletes.iter().zip(&pre_delete_checks) {
        cancellation.check()?;
        verify_live_snapshot(client, check, &cancellation)?;
        client.delete_non_recursive(root, &action.remote_path)?;
        completed.deleted += 1;
        report(ExecutionEvent::TypeConflictDeleted {
            remote_path: action.remote_path.clone(),
        });
    }
    for action in &plan.creates {
        cancellation.check()?;
        client.create_folder(&action.remote_path)?;
        completed.created += 1;
        report(ExecutionEvent::DirectoryCreated {
            remote_path: action.remote_path.clone(),
        });
    }
    let copy_source_checks = plan
        .copies
        .iter()
        .map(|action| SnapshotCheck {
            relative: action.from_relative.clone(),
            remote_path: action.from_remote_path.clone(),
            expected: action.source_snapshot.clone(),
            deleting: false,
            // The bytes about to be duplicated on the NAS. This process never sent them, so the
            // only way to know they are the content the plan matched is to read them.
            evidence: ContentEvidence::ServerProduced,
        })
        .collect::<Vec<_>>();
    client.verify_remote_snapshot_batch(
        root,
        &plan.scope,
        &plan.rules,
        &copy_source_checks,
        &cancellation,
    )?;
    for (action, check) in plan.copies.iter().zip(&copy_source_checks) {
        cancellation.check()?;
        client.preflight_upload_source(&action.local, &cancellation)?;
        verify_live_snapshot(client, check, &cancellation)?;
        match client.copy_file_verified(root, action, &cancellation) {
            Ok(()) => {
                client.preflight_upload_source(&action.local, &cancellation)?;
                completed.copied += 1;
                cancellation.tick();
                report(ExecutionEvent::RemoteContentCopied {
                    from_remote_path: action.from_remote_path.clone(),
                    to_remote_path: action.to_remote_path.clone(),
                    bytes: action.expected_size,
                });
            }
            Err(Error::ServerCopyNotStarted) => {
                // Observed like any other upload. A DSM without `CopyMove` v3, or one that rejects
                // every copy deterministically, turns every planned copy into this transfer, and
                // with no observer a multi-gigabyte fallback reported no progress at all.
                // Cancellation no longer depends on the observer — `ObservedReader` consults the
                // token itself — but the bytes still have to be visible.
                client.upload(
                    &action.local,
                    &action.to_remote_path,
                    observer_factory(&action.local),
                    &cancellation,
                )?;
                completed.uploaded += 1;
                completed.uploaded_bytes = completed
                    .uploaded_bytes
                    .saturating_add(action.expected_size);
                cancellation.tick();
                report(ExecutionEvent::CopyFallbackUploaded {
                    relative: action.local.relative.clone(),
                    bytes: action.expected_size,
                });
            }
            Err(error) => return Err(error),
        }
    }
    let copy_destination_checks = plan
        .copies
        .iter()
        .map(destination_snapshot_check)
        .collect::<Vec<_>>();
    client.verify_remote_snapshot_batch(
        root,
        &plan.scope,
        &plan.rules,
        &copy_destination_checks,
        &cancellation,
    )?;
    for check in &copy_destination_checks {
        verify_live_snapshot(client, check, &cancellation)?;
    }

    if !plan.uploads.is_empty() {
        let worker_count = options.jobs.clamp(1, 16).min(plan.uploads.len());
        let queue = Arc::new(Mutex::new(VecDeque::from(plan.uploads.clone())));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();

        thread::scope(|scope| {
            for _ in 0..worker_count {
                let queue = Arc::clone(&queue);
                let stop = Arc::clone(&stop);
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
                                    Err(Error::Message(
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
                        let user_observer = observer_factory(&action.local);
                        let cancellation_for_observer = cancellation.clone();
                        // `thread::scope` cannot return until every in-flight upload finishes, so
                        // the observer also watches `stop`: without it a cancelled run, or a run
                        // whose peer worker already failed, would keep streaming this file until
                        // the full `--timeout` elapsed. Returning `false` makes the body reader
                        // fail with `Interrupted`, which aborts the request instead of waiting.
                        let stop_for_observer = Arc::clone(&stop);
                        let observer: Option<UploadObserver> = Some(Arc::new(move |event| {
                            let user_continues = user_observer
                                .as_ref()
                                .is_none_or(|observer| observer(event));
                            user_continues
                                && !cancellation_for_observer.is_cancelled()
                                && !stop_for_observer.load(Ordering::Acquire)
                        }));
                        let result = worker_client.upload(
                            &action.local,
                            &action.remote_path,
                            observer,
                            &cancellation,
                        );
                        if result.is_err() {
                            stop.store(true, Ordering::Release);
                        }
                        if sender.send((relative, size, result)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(sender);

            let mut first_error = None;
            for (relative, size, result) in receiver {
                match result {
                    Ok(()) => {
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
                    Err(error) => record_worker_error(&mut first_error, error),
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            Ok(())
        })?;
    }

    cancellation.check()?;

    // Remote-only deletion is deliberately last. Any earlier failure returns before this point.
    // File Station has no conditional-delete primitive, so metadata is checked in one fresh
    // phase inventory and content-mode files are rehashed immediately before each delete. The
    // nonrecursive delete remains the final directory race guard.
    let upload_destination_checks = if plan.post_deletes.is_empty() {
        Vec::new()
    } else {
        // Openable and unchanged, not re-hashed. What a mirror deletion has to be sure of is that
        // every upload landed, and the destination checks below prove that from the remote side
        // against a fingerprint of the bytes the upload sent. Whether the local file has changed
        // *since* then does not bear on deleting a different, remote-only path -- and asking cost a
        // third and fourth full read and MD5/CRC32/SHA-256 pass over every uploaded file.
        for action in &plan.uploads {
            cancellation.check()?;
            client.preflight_upload_readable(&action.local)?;
        }
        plan.uploads
            .iter()
            .map(upload_destination_snapshot_check)
            .collect::<Vec<_>>()
    };
    let mut post_delete_checks = plan
        .post_deletes
        .iter()
        .map(delete_snapshot_check)
        .collect::<Vec<_>>();
    post_delete_checks.extend(plan.post_deletes.iter().filter_map(|action| {
        action
            .destination_guard
            .as_ref()
            .map(|guard| SnapshotCheck {
                relative: guard.local.relative.clone(),
                remote_path: guard.remote_path.clone(),
                expected: RemoteSnapshot {
                    kind: crate::local::EntryKind::File,
                    size: guard.expected_size,
                    mtime_seconds: guard.expected_mtime_seconds,
                    content_md5: Some(guard.content_md5),
                    require_mtime: true,
                },
                deleting: false,
                // A server-side copy's destination, and the proof that authorises deleting its
                // source. Both halves of that call for the strong standard.
                evidence: ContentEvidence::ServerProduced,
            })
    }));
    post_delete_checks.extend(upload_destination_checks.iter().cloned());
    client.verify_remote_snapshot_batch(
        root,
        &plan.scope,
        &plan.rules,
        &post_delete_checks,
        &cancellation,
    )?;
    for check in &upload_destination_checks {
        verify_live_snapshot(client, check, &cancellation)?;
    }
    // Relatives whose deletion was kept. `sort_plan` orders post-deletes deepest-first, so every
    // descendant is decided before the directory that holds it and this set is complete by the time
    // an ancestor is considered.
    let mut kept_relatives: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for action in &plan.post_deletes {
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
        if !local_path_is_absent(&plan.source_root, &action.relative)? {
            kept_relatives.insert(action.relative.clone());
            completed.deletes_skipped += 1;
            report(ExecutionEvent::RemoteExtraDeleteSkipped {
                remote_path: action.remote_path.clone(),
                relative: action.relative.clone(),
                reason: DeleteKeptReason::LocalPathExists,
            });
            continue;
        }
        if let Some(guard) = &action.destination_guard {
            client.preflight_upload_source(&guard.local, &cancellation)?;
            let guard_check = SnapshotCheck {
                relative: guard.local.relative.clone(),
                remote_path: guard.remote_path.clone(),
                expected: RemoteSnapshot {
                    kind: crate::local::EntryKind::File,
                    size: guard.expected_size,
                    mtime_seconds: guard.expected_mtime_seconds,
                    content_md5: Some(guard.content_md5),
                    require_mtime: true,
                },
                deleting: false,
                evidence: ContentEvidence::ServerProduced,
            };
            verify_live_snapshot(client, &guard_check, &cancellation)?;
        }
        verify_live_snapshot(client, &delete_snapshot_check(action), &cancellation)?;
        client.delete_non_recursive(root, &action.remote_path)?;
        completed.deleted += 1;
        report(ExecutionEvent::RemoteExtraDeleted {
            remote_path: action.remote_path.clone(),
        });
    }

    // A kept deletion means the local file is back, and the remote copy it was scheduled against is
    // still there and stale. Uploading it here is what makes the run converge. The alternative was a
    // summary line promising that "the next run uploads it" — true of the next run, and no answer at
    // all for this one, which then reported a reconciliation failure it had itself created.
    //
    // Same preflight, same transfer, same post-condition as any other upload. Only the plan's
    // knowledge of it differs, which is why it is a separate event.
    for relative in &kept_relatives {
        cancellation.check()?;
        let Some(mut local) = crate::local::entry_at(&plan.source_root, relative)? else {
            // Gone again, or DSM-managed after all. Either way there is nothing to upload, and the
            // next run's plan is the right place to decide what the destination should hold.
            continue;
        };
        // A directory that reappeared needs nothing: the remote entry that was going to be deleted
        // is a directory too, so the two already agree. A kind flip between them is a genuine
        // divergence and is left for reconciliation to report rather than resolved by guesswork.
        if local.kind != crate::local::EntryKind::File {
            continue;
        }
        let Some(remote_path) = kept_upload_target(plan, relative) else {
            continue;
        };
        crate::local::promote_to_full_fingerprint(&mut local, &cancellation)?;
        client.preflight_upload_readable(&local)?;
        let bytes = local.size;
        client.upload(
            &local,
            &remote_path,
            observer_factory(&local),
            &cancellation,
        )?;
        completed.uploaded += 1;
        completed.kept_deletes_uploaded += 1;
        completed.uploaded_bytes = completed.uploaded_bytes.saturating_add(bytes);
        cancellation.tick();
        report(ExecutionEvent::KeptDeleteUploaded {
            relative: relative.clone(),
            bytes,
        });
    }

    Ok(())
}

/// True when a kept deletion lies beneath `relative`.
///
/// A range query rather than a scan: the set is ordered, so every candidate starts at the prefix.
fn kept_descendant(kept: &std::collections::BTreeSet<String>, relative: &str) -> bool {
    let prefix = format!("{relative}/");
    kept.range(prefix.clone()..)
        .next()
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

/// The remote path a kept deletion's local file should be uploaded to.
///
/// Taken from the delete action itself: it is the path the planner mapped for this relative, under
/// the same root and through the same validation, so the follow-up upload cannot reach anywhere the
/// plan could not.
fn kept_upload_target(plan: &SyncPlan, relative: &str) -> Option<String> {
    plan.post_deletes
        .iter()
        .find(|action| action.relative == relative && action.kind == crate::local::EntryKind::File)
        .map(|action| action.remote_path.clone())
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
        ) -> Result<()> {
            if !checks.is_empty() {
                self.batch_scopes
                    .lock()
                    .unwrap()
                    .push(scope.as_str().to_owned());
            }
            for check in checks {
                self.event(format!("snapshot:{}", check.remote_path));
                if self.fail_snapshot_path.as_deref() == Some(check.remote_path.as_str()) {
                    return Err(remote_snapshot_changed(&check.remote_path));
                }
            }
            Ok(())
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
            }],
            post_deletes: vec![DeleteAction {
                relative: "extra".to_owned(),
                remote_path: "/share/root/extra".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            }],
            unchanged_files: 0,
            protected_entries: 0,
            upload_bytes: 1,
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
                remote_path: "/share/root/old/report.md".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
            DeleteAction {
                relative: "old".to_owned(),
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
                remote_path: "/share/root/kept.txt".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot(1, None),
                destination_guard: None,
            },
            DeleteAction {
                relative: "empty".to_owned(),
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
            remote_path: "/share/root/blocked/extra".to_owned(),
            kind: EntryKind::File,
            type_conflict: false,
            snapshot: file_snapshot(1, None),
            destination_guard: None,
        }];

        let failure = execute_with(
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

        assert!(matches!(failure, Error::FileIo { .. }), "{failure:?}");
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
    fn uploaded_destination_with_wrong_mtime_blocks_remote_extra_deletion() {
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
            ["snapshot:/share/root/conflict"]
        );
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
        assert!(matches!(error, Error::RemoteSnapshotChanged(path) if path == "/share/root/extra"));
        assert_eq!(
            *client.events.lock().unwrap(),
            ["snapshot:/share/root/extra"]
        );
    }

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
    fn copied_destination_with_wrong_mtime_fails_before_success() {
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
            },
            UploadAction {
                local: local("two.txt"),
                remote_path: "/share/root/two.txt".to_owned(),
                reason: ChangeReason::MissingRemote,
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
            },
            UploadAction {
                local: local("streams.txt"),
                remote_path: "/share/root/streams.txt".to_owned(),
                reason: ChangeReason::MissingRemote,
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
            unchanged_files: 0,
            protected_entries: 0,
            upload_bytes: 0,
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
}
