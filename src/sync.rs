use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use crate::api::{ApiClient, UploadObserver};
use crate::local::LocalEntry;
use crate::path::RemoteRoot;
use crate::plan::SyncPlan;
use crate::{Error, Result};

trait SyncOperations: Clone + Send + Sync {
    fn preflight_upload_source(&self, local: &crate::local::LocalEntry) -> Result<()>;
    fn delete_non_recursive(&self, root: &RemoteRoot, remote_path: &str) -> Result<()>;
    fn create_folder(&self, remote_path: &str) -> Result<()>;
    fn upload(
        &self,
        local: &crate::local::LocalEntry,
        remote_path: &str,
        observer: Option<UploadObserver>,
    ) -> Result<()>;
}

impl SyncOperations for ApiClient {
    fn preflight_upload_source(&self, local: &crate::local::LocalEntry) -> Result<()> {
        self.preflight_upload_source(local)
    }

    fn delete_non_recursive(&self, root: &RemoteRoot, remote_path: &str) -> Result<()> {
        self.delete_non_recursive(root, remote_path)
    }

    fn create_folder(&self, remote_path: &str) -> Result<()> {
        self.create_folder(remote_path)
    }

    fn upload(
        &self,
        local: &crate::local::LocalEntry,
        remote_path: &str,
        observer: Option<UploadObserver>,
    ) -> Result<()> {
        self.upload_observed(local, remote_path, observer)
    }
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
    pub uploaded: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
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
        client.preflight_upload_source(&action.local)?;
    }

    let mut completed = ExecutionReport::default();
    for action in &plan.pre_deletes {
        cancellation.check()?;
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
                        let user_observer = observer_factory(&action.local);
                        let cancellation_for_observer = cancellation.clone();
                        let observer: Option<UploadObserver> = Some(Arc::new(move |event| {
                            let user_continues = user_observer
                                .as_ref()
                                .is_none_or(|observer| observer(event));
                            user_continues && !cancellation_for_observer.is_cancelled()
                        }));
                        let result =
                            worker_client.upload(&action.local, &action.remote_path, observer);
                        if result.is_err() {
                            stop.store(true, Ordering::Release);
                        }
                        if sender.send((relative, result)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(sender);

            let mut first_error = None;
            for (relative, result) in receiver {
                match result {
                    Ok(()) => {
                        completed.uploaded += 1;
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
    for action in &plan.post_deletes {
        cancellation.check()?;
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
    use crate::plan::{CreateAction, DeleteAction, UploadAction};

    use super::*;

    #[derive(Clone, Default)]
    struct MockOperations {
        events: Arc<Mutex<Vec<String>>>,
        fail_preflight: bool,
        fail_upload: bool,
    }

    impl MockOperations {
        fn event(&self, value: impl Into<String>) {
            self.events.lock().unwrap().push(value.into());
        }
    }

    impl SyncOperations for MockOperations {
        fn preflight_upload_source(&self, local: &LocalEntry) -> Result<()> {
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
            Ok(())
        }

        fn upload(
            &self,
            local: &LocalEntry,
            _remote_path: &str,
            _observer: Option<UploadObserver>,
        ) -> Result<()> {
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
        }
    }

    fn populated_plan() -> SyncPlan {
        SyncPlan {
            pre_deletes: vec![DeleteAction {
                relative: "conflict".to_owned(),
                remote_path: "/share/root/conflict".to_owned(),
                kind: EntryKind::File,
                type_conflict: true,
            }],
            creates: vec![CreateAction {
                relative: "folder".to_owned(),
                remote_path: "/share/root/folder".to_owned(),
            }],
            uploads: vec![UploadAction {
                local: local("file.txt"),
                remote_path: "/share/root/file.txt".to_owned(),
            }],
            post_deletes: vec![DeleteAction {
                relative: "extra".to_owned(),
                remote_path: "/share/root/extra".to_owned(),
                kind: EntryKind::File,
                type_conflict: false,
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
        assert_eq!(report.deleted + report.created + report.uploaded, 0);
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
}
