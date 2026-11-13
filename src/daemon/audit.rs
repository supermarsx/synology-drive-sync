//! The mandatory audit of a run (I-7), as `sdsync-run` kept it: the outbox is
//! reconciled before a run is admitted, a transaction is created and begun
//! (`prepared`) once it is admitted, moved to `executing` just before the core
//! starts, and completed with the run's final state. A run whose audit cannot
//! be opened does not start (`audit_unavailable`): the package never changes
//! the NAS without a durable record that it began to.
//!
//! The steps are the bridge's own (`sdsync-dsm-api --audit-transaction ...`).
//! [`InProcessAudit`] calls the same functions on a bounded helper thread of the
//! daemon, which saves five process starts per run on a small NAS;
//! [`BridgeAudit`] runs them as bounded children and is what the daemon falls
//! back to when it cannot vouch for its own identity. Either way the daemon is
//! the outbox record's owner, and a daemon that dies mid-run leaves a record the
//! next reconciliation completes as `outcome_unknown`, never as success.

#![forbid(unsafe_code)]

use std::time::Duration;

/// Who asked for a run, for the audit record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuditOrigin {
    /// A routine or the all-profiles schedule (`package-scheduler`).
    Scheduler,
    /// The dashboard or the CLI through the daemon (`package-runner`).
    Manager,
}

impl AuditOrigin {
    pub(crate) fn origin(self) -> &'static str {
        match self {
            Self::Scheduler => "scheduler",
            Self::Manager => "manager",
        }
    }

    pub(crate) fn actor(self) -> &'static str {
        match self {
            Self::Scheduler => "package-scheduler",
            Self::Manager => "package-runner",
        }
    }
}

/// The audit steps. Production runs the bridge; tests record calls.
pub(crate) trait RunAudit: Send + Sync {
    fn reconcile(&self) -> Result<(), String>;
    /// Create and begin a transaction; its id.
    fn begin(&self, operation: &str, scope: &str, origin: AuditOrigin) -> Result<String, String>;
    fn execute(&self, transaction: &str) -> Result<(), String>;
    /// `state`: `succeeded`, `failed` or `outcome_unknown`.
    fn complete(&self, transaction: &str, state: &str) -> Result<(), String>;
}

/// An audit that records nothing (tests that are not about the audit).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct NoAudit;

impl RunAudit for NoAudit {
    fn reconcile(&self) -> Result<(), String> {
        Ok(())
    }
    fn begin(&self, _: &str, _: &str, _: AuditOrigin) -> Result<String, String> {
        Ok("none".into())
    }
    fn execute(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn complete(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
}

/// How long one audit step may take.
pub(crate) const AUDIT_STEP_DEADLINE: Duration = Duration::from_secs(30);

#[cfg(target_os = "linux")]
pub(crate) use bridge::BridgeAudit;
#[cfg(target_os = "linux")]
pub(crate) use in_process::InProcessAudit;

#[cfg(target_os = "linux")]
mod bridge {
    use std::ffi::OsString;
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Instant;

    use super::{AUDIT_STEP_DEADLINE, AuditOrigin, RunAudit};

    /// `sdsync-dsm-api --audit-transaction …`, bounded.
    pub(crate) struct BridgeAudit {
        pub(crate) bridge: PathBuf,
        pub(crate) env: Vec<(OsString, OsString)>,
        pub(crate) uid: u32,
    }

    impl BridgeAudit {
        fn step(&self, arguments: &[&str]) -> Result<String, String> {
            let mut child = Command::new(&self.bridge)
                .arg("--audit-transaction")
                .args(arguments)
                .env_clear()
                .envs(self.env.iter().map(|(k, v)| (k, v)))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("{}: {error}", self.bridge.display()))?;
            let started = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut out = String::new();
                        if let Some(mut stdout) = child.stdout.take() {
                            let _ = (&mut stdout).take(4096).read_to_string(&mut out);
                        }
                        let mut err = String::new();
                        if let Some(mut stderr) = child.stderr.take() {
                            let _ = (&mut stderr).take(4096).read_to_string(&mut err);
                        }
                        return if status.success() {
                            Ok(out.trim().to_owned())
                        } else {
                            Err(format!(
                                "--audit-transaction {} exited {}{}",
                                arguments[0],
                                status.code().unwrap_or(-1),
                                err.lines()
                                    .rev()
                                    .find(|line| !line.trim().is_empty())
                                    .map(|line| format!(": {}", line.trim()))
                                    .unwrap_or_default()
                            ))
                        };
                    }
                    Ok(None) => {}
                    Err(error) => return Err(format!("wait failed: {error}")),
                }
                if started.elapsed() >= AUDIT_STEP_DEADLINE {
                    let _ = child.kill();
                    // H3: the wait after KILL is bounded too; a child in
                    // uninterruptible sleep goes to a reaper, named.
                    let pid = child.id();
                    let handed = if super::super::sys::reap_within(
                        &mut child,
                        std::time::Duration::from_secs(5),
                    ) {
                        "reaped"
                    } else {
                        super::super::sys::abandon_to_reaper(child)
                    };
                    return Err(format!(
                        "--audit-transaction {} gave no answer within {} s (pid={pid} {handed})",
                        arguments[0],
                        AUDIT_STEP_DEADLINE.as_secs()
                    ));
                }
                thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }

    impl RunAudit for BridgeAudit {
        fn reconcile(&self) -> Result<(), String> {
            self.step(&["reconcile"]).map(drop)
        }

        fn begin(
            &self,
            operation: &str,
            scope: &str,
            origin: AuditOrigin,
        ) -> Result<String, String> {
            let transaction = self.step(&["create", origin.origin()])?;
            if transaction.is_empty()
                || transaction.len() > 96
                || !transaction
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(format!(
                    "the audit transaction id {transaction:?} is not one"
                ));
            }
            self.step(&[
                "begin",
                operation,
                scope,
                origin.actor(),
                &self.uid.to_string(),
                origin.origin(),
                &transaction,
            ])?;
            Ok(transaction)
        }

        fn execute(&self, transaction: &str) -> Result<(), String> {
            self.step(&["execute", transaction]).map(drop)
        }

        fn complete(&self, transaction: &str, state: &str) -> Result<(), String> {
            self.step(&["complete", transaction, state]).map(drop)
        }
    }
}

/// The bridge's audit steps, called in the daemon's own process.
///
/// The bridge child did four things the daemon does differently: it cleared its
/// environment (the daemon's own is never read by these functions, and the
/// manager they run is built from an explicit one), it took its parent's process
/// identity as the record's owner (the daemon is that parent, so it takes its
/// own), it was killed after [`AUDIT_STEP_DEADLINE`] (a helper thread cannot be
/// killed, so a step that does not return is abandoned, counted, and refused
/// past `MAX_STUCK`), and it checked that it was the package's uid (done once,
/// when the audit is built).
#[cfg(target_os = "linux")]
mod in_process {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;

    use crate::dsm_api::{
        AuditOutboxPaths, AuditOutboxPhase, AuditOutboxRecord, BridgeResult, linux_files,
        linux_runtime, new_audit_transaction_id, record_audit_event, validate_package_identity,
    };

    use super::{AUDIT_STEP_DEADLINE, AuditOrigin, RunAudit};

    /// Steps that never returned before the audit refuses to start another.
    const MAX_STUCK: usize = 3;

    /// Writes the durable audit event for a record in a state (`requested`,
    /// `succeeded`, ...): the shell manager's `audit-event`.
    type Appender = Arc<dyn Fn(&AuditOutboxRecord, &str) -> BridgeResult<()> + Send + Sync>;

    pub(crate) struct InProcessAudit {
        paths: AuditOutboxPaths<'static>,
        package_uid: u32,
        /// The uid recorded as the actor (the daemon's own).
        actor_uid: u32,
        append: Appender,
        /// Steps abandoned after their deadline that have not returned yet.
        stuck: Arc<AtomicUsize>,
    }

    impl InProcessAudit {
        /// The package's audit outbox, written as the package user. `Err` names
        /// why this process is not fit to do it (the daemon then runs the bridge).
        pub(crate) fn production(actor_uid: u32) -> Result<Self, String> {
            let identity = linux_runtime::identity_state()
                .map_err(|error| format!("the daemon's identity could not be read: {error:?}"))?;
            let package_uid = validate_package_identity(&identity)
                .map_err(|error| format!("the daemon is not the package's user: {error:?}"))?;
            Ok(Self {
                paths: AuditOutboxPaths::production(),
                package_uid,
                actor_uid,
                append: Arc::new(record_audit_event),
                stuck: Arc::new(AtomicUsize::new(0)),
            })
        }

        #[cfg(test)]
        pub(crate) fn with(
            paths: AuditOutboxPaths<'static>,
            package_uid: u32,
            append: Appender,
        ) -> Self {
            Self {
                paths,
                package_uid,
                actor_uid: package_uid.max(1),
                append,
                stuck: Arc::new(AtomicUsize::new(0)),
            }
        }

        #[cfg(test)]
        pub(crate) fn stuck(&self) -> usize {
            self.stuck.load(Ordering::SeqCst)
        }

        /// `work` on a helper thread, waited for at most [`AUDIT_STEP_DEADLINE`]
        /// (`deadline` in tests).
        fn step<T: Send + 'static>(
            &self,
            name: &'static str,
            work: impl FnOnce() -> BridgeResult<T> + Send + 'static,
        ) -> Result<T, String> {
            self.step_within(name, AUDIT_STEP_DEADLINE, work)
        }

        fn step_within<T: Send + 'static>(
            &self,
            name: &'static str,
            deadline: std::time::Duration,
            work: impl FnOnce() -> BridgeResult<T> + Send + 'static,
        ) -> Result<T, String> {
            let stuck = self.stuck.load(Ordering::SeqCst);
            if stuck >= MAX_STUCK {
                return Err(format!(
                    "audit {name} not started: {stuck} earlier audit steps never returned"
                ));
            }
            // 0 running, 1 returned, 2 abandoned by the waiter: whichever of the
            // two moves it first decides who counts the step as stuck.
            let state = Arc::new(AtomicU8::new(0));
            let (sender, receiver) = mpsc::channel();
            let spawned = thread::Builder::new()
                .name("sdsyncd-audit".to_owned())
                .spawn({
                    let (state, stuck) = (Arc::clone(&state), Arc::clone(&self.stuck));
                    move || {
                        let _ = sender.send(work());
                        if state.swap(1, Ordering::SeqCst) == 2 {
                            stuck.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                });
            if let Err(error) = spawned {
                return Err(format!("audit {name} not started: {error}"));
            }
            let answer = match receiver.recv_timeout(deadline) {
                Err(RecvTimeoutError::Timeout)
                    if state
                        .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok() =>
                {
                    self.stuck.fetch_add(1, Ordering::SeqCst);
                    return Err(format!(
                        "audit {name} gave no answer within {} s (the step was abandoned)",
                        deadline.as_secs()
                    ));
                }
                // It returned as the deadline passed: take what it sent.
                Err(RecvTimeoutError::Timeout) => receiver.try_recv().ok(),
                Err(RecvTimeoutError::Disconnected) => None,
                Ok(result) => Some(result),
            };
            match answer {
                Some(Ok(value)) => Ok(value),
                Some(Err(error)) => Err(format!("audit {name} failed: {error:?}")),
                None => Err(format!("audit {name} ended without an answer")),
            }
        }
    }

    impl RunAudit for InProcessAudit {
        fn reconcile(&self) -> Result<(), String> {
            let (paths, uid, append) = (self.paths, self.package_uid, Arc::clone(&self.append));
            self.step("reconcile", move || {
                linux_files::reconcile_audit_transactions(&paths, uid, |record, state| {
                    append(record, state)
                })
                .map(drop)
            })
        }

        fn begin(
            &self,
            operation: &str,
            scope: &str,
            origin: AuditOrigin,
        ) -> Result<String, String> {
            let (paths, uid, actor_uid, append) = (
                self.paths,
                self.package_uid,
                self.actor_uid,
                Arc::clone(&self.append),
            );
            let (operation, scope) = (operation.to_owned(), scope.to_owned());
            self.step("begin", move || {
                let owner = linux_files::current_process_identity()?;
                let transaction = new_audit_transaction_id(origin.origin(), &owner)?;
                let (owner_pid, owner_start, owner_boot) = owner;
                let record = AuditOutboxRecord {
                    schema: "sdsync.dsm-audit-outbox.v1".to_owned(),
                    transaction: transaction.clone(),
                    operation,
                    profile: scope,
                    actor: origin.actor().to_owned(),
                    actor_uid,
                    origin: origin.origin().to_owned(),
                    client_request_id: None,
                    job_id: None,
                    owner_pid,
                    owner_start,
                    owner_boot,
                    phase: AuditOutboxPhase::Prepared,
                };
                linux_files::audit_transaction_begin(
                    &paths,
                    uid,
                    record,
                    AuditOutboxPhase::Prepared,
                    |record, state| append(record, state),
                )?;
                Ok(transaction)
            })
        }

        fn execute(&self, transaction: &str) -> Result<(), String> {
            let (paths, uid) = (self.paths, self.package_uid);
            let transaction = transaction.to_owned();
            self.step("execute", move || {
                linux_files::mark_audit_transaction_executing_owned_by(
                    &paths,
                    uid,
                    &transaction,
                    linux_files::current_process_identity()?,
                )
            })
        }

        fn complete(&self, transaction: &str, state: &str) -> Result<(), String> {
            let terminal = match state {
                "succeeded" => AuditOutboxPhase::Succeeded,
                "failed" => AuditOutboxPhase::Failed,
                "outcome_unknown" => AuditOutboxPhase::OutcomeUnknown,
                other => return Err(format!("audit complete: {other:?} is not a final state")),
            };
            let (paths, uid, append) = (self.paths, self.package_uid, Arc::clone(&self.append));
            let transaction = transaction.to_owned();
            let pending = self.step("complete", move || {
                linux_files::audit_transaction_complete(
                    &paths,
                    uid,
                    &transaction,
                    terminal,
                    |record, state| append(record, state),
                )
            })?;
            if pending {
                // The record is terminal and durable but its event could not be
                // written; the next reconciliation writes it (the bridge's exit 75).
                return Err(
                    "audit complete: the event could not be written yet; reconciliation will write it"
                        .to_owned(),
                );
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::path::PathBuf;
        use std::sync::Mutex;
        use std::time::Duration;

        use super::*;

        struct Outbox {
            root: PathBuf,
            paths: AuditOutboxPaths<'static>,
            uid: u32,
        }

        impl Outbox {
            fn new(label: &str) -> Self {
                let root = std::env::temp_dir().join(format!(
                    "sdsyncd-audit-{label}-{}-{:?}",
                    std::process::id(),
                    thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&root);
                std::fs::create_dir_all(&root).unwrap();
                let directory = root.join("audit-outbox");
                std::fs::create_dir(&directory).unwrap();
                for dir in [&root, &directory] {
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
                }
                let uid = std::fs::metadata(&root).unwrap().uid();
                let directory: &'static std::path::Path = Box::leak(directory.into_boxed_path());
                let lock: &'static std::path::Path =
                    Box::leak(root.join("audit-outbox.flock").into_boxed_path());
                Self {
                    root,
                    paths: AuditOutboxPaths {
                        directory,
                        lock,
                        requests: None,
                        processing: None,
                        responses: None,
                        lock_wait: Duration::from_secs(2),
                    },
                    uid,
                }
            }

            fn events(&self) -> Vec<String> {
                let mut names: Vec<String> = std::fs::read_dir(self.paths.directory)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect();
                names.sort();
                names
            }
        }

        impl Drop for Outbox {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        fn recording() -> (Appender, Arc<Mutex<Vec<String>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&seen);
            let append: Appender = Arc::new(move |record, state| {
                log.lock()
                    .unwrap()
                    .push(format!("{} {} {}", record.operation, record.profile, state));
                Ok(())
            });
            (append, seen)
        }

        /// The whole life of a run's audit, in process: the record is created
        /// prepared, owned by this process, marked executing, completed with the
        /// run's state and removed, and the event log saw `requested` then the
        /// final state.
        #[test]
        fn a_run_is_audited_in_process_from_prepared_to_removed() {
            let outbox = Outbox::new("life");
            let (append, seen) = recording();
            let audit = InProcessAudit::with(outbox.paths, outbox.uid, append);
            audit.reconcile().unwrap();
            let transaction = audit.begin("run", "docs", AuditOrigin::Scheduler).unwrap();
            assert!(transaction.starts_with("scheduler-"), "{transaction}");
            assert_eq!(outbox.events(), [format!("{transaction}.event")]);
            audit.execute(&transaction).unwrap();
            // A live owner is not reconciled away.
            audit.reconcile().unwrap();
            assert_eq!(outbox.events().len(), 1);
            audit.complete(&transaction, "succeeded").unwrap();
            assert!(outbox.events().is_empty(), "{:?}", outbox.events());
            assert_eq!(
                *seen.lock().unwrap(),
                [
                    "run docs requested",
                    "run docs requested",
                    "run docs succeeded"
                ]
            );
        }

        /// A transaction that is not begun cannot be executed or completed
        /// (the same refusals the bridge child gave), and a state that is not
        /// final is refused before anything is touched.
        #[test]
        fn steps_out_of_order_or_with_a_bad_state_are_refused_and_named() {
            let outbox = Outbox::new("refused");
            let (append, _) = recording();
            let audit = InProcessAudit::with(outbox.paths, outbox.uid, append);
            let phantom = format!("scheduler-{}", "0".repeat(64));
            assert!(
                audit
                    .execute(&phantom)
                    .unwrap_err()
                    .contains("audit execute failed")
            );
            let error = audit.complete(&phantom, "running").unwrap_err();
            assert!(error.contains("not a final state"), "{error}");
            assert!(audit.complete(&phantom, "succeeded").is_err());
            assert!(outbox.events().is_empty());
        }

        /// The event log failing leaves the durable record behind for the next
        /// reconciliation and says so, rather than reporting the audit done.
        #[test]
        fn a_failed_event_write_leaves_the_record_and_is_reported() {
            let outbox = Outbox::new("append-fails");
            let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = Arc::clone(&fail);
            let append: Appender = Arc::new(move |_, _| {
                if flag.load(Ordering::SeqCst) {
                    Err(crate::dsm_api::BridgeError::new(
                        crate::dsm_api::ErrorKind::Unavailable,
                    ))
                } else {
                    Ok(())
                }
            });
            let audit = InProcessAudit::with(outbox.paths, outbox.uid, append);
            let transaction = audit.begin("run", "docs", AuditOrigin::Manager).unwrap();
            audit.execute(&transaction).unwrap();
            fail.store(true, Ordering::SeqCst);
            let error = audit.complete(&transaction, "failed").unwrap_err();
            assert!(error.contains("reconciliation will write it"), "{error}");
            assert_eq!(outbox.events(), [format!("{transaction}.event")]);
            // Reconciled once the log works again.
            fail.store(false, Ordering::SeqCst);
            audit.reconcile().unwrap();
            assert!(outbox.events().is_empty(), "{:?}", outbox.events());
        }

        /// A step that does not return is abandoned at its deadline and named;
        /// after MAX_STUCK of them no further step is started, and one that
        /// finally returns is no longer counted.
        #[test]
        fn a_step_that_never_returns_is_abandoned_counted_and_bounded() {
            let outbox = Outbox::new("stuck");
            let (append, _) = recording();
            let audit = InProcessAudit::with(outbox.paths, outbox.uid, append);
            let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
            for n in 1..=MAX_STUCK {
                let gate = Arc::clone(&release);
                let error = audit
                    .step_within("probe", Duration::from_millis(50), move || {
                        while !gate.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Ok(())
                    })
                    .unwrap_err();
                assert!(error.contains("abandoned"), "{error}");
                assert_eq!(audit.stuck(), n);
            }
            let error = audit.reconcile().unwrap_err();
            assert!(error.contains("never returned"), "{error}");
            release.store(true, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while audit.stuck() > 0 {
                assert!(std::time::Instant::now() < deadline, "{}", audit.stuck());
                thread::sleep(Duration::from_millis(10));
            }
            audit.reconcile().unwrap();
        }
    }
}
