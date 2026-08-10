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

#[derive(Clone, Debug)]
struct SnapshotCheck {
    relative: String,
    remote_path: String,
    expected: RemoteSnapshot,
    deleting: bool,
}

trait SyncOperations: Clone + Send + Sync {
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
    fn upload(
        &self,
        local: &crate::local::LocalEntry,
        remote_path: &str,
        observer: Option<UploadObserver>,
        cancellation: &CancellationToken,
    ) -> Result<()>;
}

impl SyncOperations for ApiClient {
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
        checks: &[SnapshotCheck],
        cancellation: &CancellationToken,
    ) -> Result<()> {
        verify_api_remote_snapshot_batch(self, root, checks, cancellation)
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

fn verify_api_remote_snapshot_batch(
    client: &ApiClient,
    root: &RemoteRoot,
    checks: &[SnapshotCheck],
    cancellation: &CancellationToken,
) -> Result<()> {
    if checks.is_empty() {
        return Ok(());
    }
    cancellation.check()?;
    let current_inventory = client.remote_inventory(root)?;
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

fn delete_snapshot_check(action: &crate::plan::DeleteAction) -> SnapshotCheck {
    SnapshotCheck {
        relative: action.relative.clone(),
        remote_path: action.remote_path.clone(),
        expected: action.snapshot.clone(),
        deleting: true,
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
        client.verify_remote_content(
            &check.remote_path,
            check.expected.size,
            expected_md5,
            cancellation,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub struct ExecuteOptions {
    pub jobs: usize,
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ExecutionReport {
    pub deleted: usize,
    pub created: usize,
    pub copied: usize,
    pub uploaded: usize,
    pub uploaded_bytes: u64,
}

pub fn execute(
    client: &ApiClient,
    root: &RemoteRoot,
    plan: &SyncPlan,
    options: ExecuteOptions,
    report: impl FnMut(String),
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
    report: impl FnMut(String),
) -> Result<ExecutionReport> {
    execute_with_observer(
        client,
        root,
        plan,
        options,
        cancellation,
        observer_factory,
        report,
    )
}

fn execute_with<O: SyncOperations>(
    client: &O,
    root: &RemoteRoot,
    plan: &SyncPlan,
    options: ExecuteOptions,
    report: impl FnMut(String),
) -> Result<ExecutionReport> {
    execute_with_observer(
        client,
        root,
        plan,
        options,
        CancellationToken::default(),
        Arc::new(|_| None),
        report,
    )
}

fn execute_with_observer<O: SyncOperations>(
    client: &O,
    root: &RemoteRoot,
    plan: &SyncPlan,
    options: ExecuteOptions,
    cancellation: CancellationToken,
    observer_factory: UploadObserverFactory,
    mut report: impl FnMut(String),
) -> Result<ExecutionReport> {
    if options.dry_run {
        return Ok(ExecutionReport::default());
    }

    // Open every scheduled source before a type-conflict deletion can remove remote data.
    for action in &plan.uploads {
        cancellation.check()?;
        client.preflight_upload_source(&action.local, &cancellation)?;
    }
    for action in &plan.copies {
        cancellation.check()?;
        client.preflight_upload_source(&action.local, &cancellation)?;
    }

    let mut completed = ExecutionReport::default();
    let pre_delete_checks = plan
        .pre_deletes
        .iter()
        .map(delete_snapshot_check)
        .collect::<Vec<_>>();
    client.verify_remote_snapshot_batch(root, &pre_delete_checks, &cancellation)?;
    for (action, check) in plan.pre_deletes.iter().zip(&pre_delete_checks) {
        cancellation.check()?;
        verify_live_snapshot(client, check, &cancellation)?;
        client.delete_non_recursive(root, &action.remote_path)?;
        completed.deleted += 1;
        report(format!("deleted type conflict: {}", action.remote_path));
    }
    for action in &plan.creates {
        cancellation.check()?;
        client.create_folder(&action.remote_path)?;
        completed.created += 1;
        report(format!("created directory: {}", action.remote_path));
    }
    let copy_source_checks = plan
        .copies
        .iter()
        .map(|action| SnapshotCheck {
            relative: action.from_relative.clone(),
            remote_path: action.from_remote_path.clone(),
            expected: action.source_snapshot.clone(),
            deleting: false,
        })
        .collect::<Vec<_>>();
    client.verify_remote_snapshot_batch(root, &copy_source_checks, &cancellation)?;
    for (action, check) in plan.copies.iter().zip(&copy_source_checks) {
        cancellation.check()?;
        client.preflight_upload_source(&action.local, &cancellation)?;
        verify_live_snapshot(client, check, &cancellation)?;
        match client.copy_file_verified(root, action, &cancellation) {
            Ok(()) => {
                client.preflight_upload_source(&action.local, &cancellation)?;
                completed.copied += 1;
                report(format!(
                    "copied remote content: {} -> {}",
                    action.from_remote_path, action.to_remote_path
                ));
            }
            Err(Error::ServerCopyNotStarted) => {
                client.upload(&action.local, &action.to_remote_path, None, &cancellation)?;
                completed.uploaded += 1;
                completed.uploaded_bytes = completed
                    .uploaded_bytes
                    .saturating_add(action.expected_size);
                report(format!(
                    "server copy unavailable; uploaded: {}",
                    action.local.relative
                ));
            }
            Err(error) => return Err(error),
        }
    }
    let copy_destination_checks = plan
        .copies
        .iter()
        .map(destination_snapshot_check)
        .collect::<Vec<_>>();
    client.verify_remote_snapshot_batch(root, &copy_destination_checks, &cancellation)?;
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
                        let observer: Option<UploadObserver> = Some(Arc::new(move |event| {
                            let user_continues = user_observer
                                .as_ref()
                                .is_none_or(|observer| observer(event));
                            user_continues && !cancellation_for_observer.is_cancelled()
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
                        report(format!("uploaded: {relative}"));
                    }
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
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
        for action in &plan.uploads {
            cancellation.check()?;
            client.preflight_upload_source(&action.local, &cancellation)?;
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
            })
    }));
    post_delete_checks.extend(upload_destination_checks.iter().cloned());
    client.verify_remote_snapshot_batch(root, &post_delete_checks, &cancellation)?;
    for check in &upload_destination_checks {
        verify_live_snapshot(client, check, &cancellation)?;
    }
    for action in &plan.post_deletes {
        cancellation.check()?;
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
            };
            verify_live_snapshot(client, &guard_check, &cancellation)?;
        }
        verify_live_snapshot(client, &delete_snapshot_check(action), &cancellation)?;
        client.delete_non_recursive(root, &action.remote_path)?;
        completed.deleted += 1;
        report(format!("deleted remote extra: {}", action.remote_path));
    }

    Ok(completed)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::local::{EntryKind, LocalEntry};
    use crate::plan::{CreateAction, DeleteAction, DestinationGuard, UploadAction};

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
    }

    impl MockOperations {
        fn event(&self, value: impl Into<String>) {
            self.events.lock().unwrap().push(value.into());
        }
    }

    impl SyncOperations for MockOperations {
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
            checks: &[SnapshotCheck],
            _cancellation: &CancellationToken,
        ) -> Result<()> {
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

        fn upload(
            &self,
            local: &LocalEntry,
            _remote_path: &str,
            _observer: Option<UploadObserver>,
            _cancellation: &CancellationToken,
        ) -> Result<()> {
            if let Some(barrier) = &self.upload_barrier {
                barrier.wait();
            }
            self.event(format!("upload:{}", local.relative));
            if self.fail_upload {
                Err(Error::Message("upload failed".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    fn local(relative: &str) -> LocalEntry {
        LocalEntry {
            relative: relative.to_owned(),
            full_path: PathBuf::from(relative),
            kind: EntryKind::File,
            size: 1,
            mtime_ms: 1,
            content_md5: None,
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

    fn populated_plan() -> SyncPlan {
        SyncPlan {
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
            }],
            copies: Vec::new(),
            uploads: vec![UploadAction {
                local: local("file.txt"),
                remote_path: "/share/root/file.txt".to_owned(),
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
        assert_eq!(*client.events.lock().unwrap(), ["preflight:file.txt"]);
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
            ["preflight:file.txt", "upload:file.txt"]
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
            ["preflight:file.txt", "create:/share/root/folder"]
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
                "preflight:file.txt",
                "snapshot:/share/root/conflict",
                "delete:/share/root/conflict",
                "create:/share/root/folder",
                "upload:file.txt",
                "preflight:file.txt",
                "snapshot:/share/root/extra",
                "snapshot:/share/root/file.txt",
                "delete:/share/root/extra",
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
                "preflight:new/file.txt",
                "snapshot:/share/root/old/file.txt",
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
                "preflight:new/file.txt",
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
            },
            UploadAction {
                local: local("two.txt"),
                remote_path: "/share/root/two.txt".to_owned(),
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
