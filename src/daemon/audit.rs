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
//! back to when it cannot vouch for its own identity, or ([`FallbackAudit`])
//! once the package manager refuses it as a caller of `api audit-event` -- the
//! endpoint that writes the events, which accepted only the bridge until
//! fix11 and so refused every run on 26.65. Either way the daemon is
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
    /// Why this transport cannot work here at all, once that is known (the
    /// manager refused this process as a caller): [`FallbackAudit`] then hands
    /// every later step to the bridge. `None` for a transport that works or
    /// fails step by step.
    fn transport_refused(&self) -> Option<String> {
        None
    }
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

/// The in-process audit, and the bridge children it hands over to for the rest
/// of the service's life once the manager refuses the daemon as a caller of its
/// private `api audit-event` endpoint (26.65 on DSM: every run was refused
/// `audit_unavailable`, because the manager accepted only the bridge). Runs stay
/// audited either way: the step that met the refusal is done again through the
/// bridge, and the switch is announced once, by name.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct FallbackAudit {
    primary: std::sync::Arc<dyn RunAudit>,
    fallback: std::sync::Arc<dyn RunAudit>,
    switched: std::sync::atomic::AtomicBool,
    /// Called once, at the switch, with the refusal and the step it met.
    announce: Announce,
}

/// [`FallbackAudit`]'s announcement of its switch: `(cause, step)`.
pub(crate) type Announce = Box<dyn Fn(&str, &str) + Send + Sync>;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl FallbackAudit {
    pub(crate) fn new(
        primary: std::sync::Arc<dyn RunAudit>,
        fallback: std::sync::Arc<dyn RunAudit>,
        announce: Announce,
    ) -> Self {
        Self {
            primary,
            fallback,
            switched: std::sync::atomic::AtomicBool::new(false),
            announce,
        }
    }

    fn on_fallback(&self) -> bool {
        self.switched.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The primary failed with `error`: `Some(cause)` when that was the
    /// transport's refusal, and the switch is made (announced only by the
    /// caller that made it).
    fn switch_after(&self, step: &str, error: &str) -> Option<String> {
        let cause = self.primary.transport_refused()?;
        if !self
            .switched
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            (self.announce)(&cause, &format!("{step}: {error}"));
        }
        Some(cause)
    }
}

impl RunAudit for FallbackAudit {
    fn reconcile(&self) -> Result<(), String> {
        if self.on_fallback() {
            return self.fallback.reconcile();
        }
        match self.primary.reconcile() {
            Err(error) if self.switch_after("reconcile", &error).is_some() => self
                .fallback
                .reconcile()
                .map_err(|again| format!("{error}; through the bridge too: {again}")),
            other => other,
        }
    }

    fn begin(&self, operation: &str, scope: &str, origin: AuditOrigin) -> Result<String, String> {
        if self.on_fallback() {
            return self.fallback.begin(operation, scope, origin);
        }
        match self.primary.begin(operation, scope, origin) {
            Err(error) if self.switch_after("begin", &error).is_some() => {
                // The refused begin left its record terminal (failed) in the
                // outbox; the bridge writes its events and retires it first.
                self.fallback
                    .reconcile()
                    .and_then(|()| self.fallback.begin(operation, scope, origin))
                    .map_err(|again| format!("{error}; through the bridge too: {again}"))
            }
            other => other,
        }
    }

    fn execute(&self, transaction: &str) -> Result<(), String> {
        if self.on_fallback() {
            return self.fallback.execute(transaction);
        }
        match self.primary.execute(transaction) {
            Err(error) if self.switch_after("execute", &error).is_some() => self
                .fallback
                .execute(transaction)
                .map_err(|again| format!("{error}; through the bridge too: {again}")),
            other => other,
        }
    }

    fn complete(&self, transaction: &str, state: &str) -> Result<(), String> {
        if self.on_fallback() {
            return self.fallback.complete(transaction, state);
        }
        match self.primary.complete(transaction, state) {
            // The record is terminal and durable; the bridge writes its events.
            Err(error) if self.switch_after("complete", &error).is_some() => self
                .fallback
                .complete(transaction, state)
                .map_err(|again| format!("{error}; through the bridge too: {again}")),
            other => other,
        }
    }
}

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
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use crate::dsm_api::{
        AuditOutboxPaths, AuditOutboxPhase, AuditOutboxRecord, BridgeError, BridgeResult,
        ErrorKind, MANAGER_PATH, linux_files, linux_runtime, new_audit_transaction_id,
        record_audit_event_named, validate_package_identity,
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
        /// Set once the manager refused this process as a caller of its
        /// private audit endpoint: why (see `transport_refused`).
        refused: Arc<Mutex<Option<String>>>,
    }

    /// The manager's code for "this process is not a caller I accept".
    const CALLER_REFUSED: &str = "audit_event_caller_refused";

    /// A bridge error as one line: its kind, its code, and what the code means
    /// where the daemon knows (`code=none` says the failing site named none).
    pub(crate) fn describe(error: &BridgeError) -> String {
        let kind = match error.kind {
            ErrorKind::BadRequest => "bad_request",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::Forbidden => "forbidden",
            ErrorKind::CsrfRejected => "csrf_rejected",
            ErrorKind::MethodNotAllowed => "method_not_allowed",
            ErrorKind::UnsupportedMediaType => "unsupported_media_type",
            ErrorKind::PayloadTooLarge => "payload_too_large",
            ErrorKind::Conflict => "conflict",
            ErrorKind::UnsafeRuntime => "unsafe_runtime",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::Internal => "internal",
        };
        let meaning = match error.code {
            Some(CALLER_REFUSED) => {
                " (the package manager refused this process as a caller of its private api audit-event endpoint)"
            }
            Some("audit_event_not_written") => {
                " (the package manager could not write audit.log or activity.log)"
            }
            Some("audit_event_unsafe_state") => {
                " (the package manager found its private state unsafe)"
            }
            Some("audit_event_invalid") => " (the package manager refused the event as invalid)",
            Some("audit_event_failed") => " (the package manager failed without naming why)",
            Some("manager_spawn_failed") => " (the package manager could not be started)",
            _ => "",
        };
        let manager = if error
            .code
            .is_some_and(|code| code.starts_with("audit_event_") || code == "manager_spawn_failed")
        {
            format!(" manager={MANAGER_PATH}")
        } else {
            String::new()
        };
        format!(
            "kind={kind} code={}{manager}{meaning}",
            error.code.unwrap_or("none")
        )
    }

    /// `append`, noting a refusal of the caller in `refused`.
    fn noting_refusal(append: Appender, refused: Arc<Mutex<Option<String>>>) -> Appender {
        Arc::new(move |record, state| {
            let result = append(record, state);
            if let Err(error) = &result
                && error.code == Some(CALLER_REFUSED)
                && let Ok(mut slot) = refused.lock()
                && slot.is_none()
            {
                *slot = Some(describe(error));
            }
            result
        })
    }

    impl InProcessAudit {
        /// The package's audit outbox, written as the package user. `Err` names
        /// why this process is not fit to do it (the daemon then runs the bridge).
        pub(crate) fn production(actor_uid: u32) -> Result<Self, String> {
            let identity = linux_runtime::identity_state().map_err(|error| {
                format!(
                    "the daemon's identity could not be read: {}",
                    describe(&error)
                )
            })?;
            let package_uid = validate_package_identity(&identity).map_err(|error| {
                format!("the daemon is not the package's user: {}", describe(&error))
            })?;
            let refused = Arc::new(Mutex::new(None));
            Ok(Self {
                paths: AuditOutboxPaths::production(),
                package_uid,
                actor_uid,
                append: noting_refusal(Arc::new(record_audit_event_named), Arc::clone(&refused)),
                stuck: Arc::new(AtomicUsize::new(0)),
                refused,
            })
        }

        #[cfg(test)]
        pub(crate) fn with(
            paths: AuditOutboxPaths<'static>,
            package_uid: u32,
            append: Appender,
        ) -> Self {
            let refused = Arc::new(Mutex::new(None));
            Self {
                paths,
                package_uid,
                actor_uid: package_uid.max(1),
                append: noting_refusal(append, Arc::clone(&refused)),
                stuck: Arc::new(AtomicUsize::new(0)),
                refused,
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
                Some(Err(error)) => Err(format!("audit {name} failed: {}", describe(&error))),
                None => Err(format!("audit {name} ended without an answer")),
            }
        }
    }

    impl RunAudit for InProcessAudit {
        fn transport_refused(&self) -> Option<String> {
            self.refused.lock().ok().and_then(|slot| slot.clone())
        }

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

        /// What the 26.65 manager answered the daemon: not a caller it accepts.
        fn caller_refused(calls: Arc<AtomicUsize>) -> Appender {
            Arc::new(move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(BridgeError::unavailable_because(CALLER_REFUSED))
            })
        }

        fn record_json(outbox: &Outbox, name: &str) -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(outbox.paths.directory.join(name)).unwrap())
                .unwrap()
        }

        /// fix11, the field failure: the manager refuses the daemon's events.
        /// The begin fails naming the code and the manager (not `code: None`),
        /// the transport reports itself refused, and the record left behind is
        /// the one found on the NAS: terminal `failed`, owned by the live
        /// daemon, no client request id. With the same manager every
        /// reconciliation fails on it (the 30 s loop); once the manager takes
        /// the daemon's events it is written as requested + failed and retired.
        #[test]
        fn a_refused_caller_is_named_and_its_record_is_reconciled_once_events_are_taken() {
            let outbox = Outbox::new("refused-caller");
            let calls = Arc::new(AtomicUsize::new(0));
            let audit =
                InProcessAudit::with(outbox.paths, outbox.uid, caller_refused(Arc::clone(&calls)));
            assert_eq!(audit.transport_refused(), None);
            let error = audit
                .begin("run", "checkoffice_nas", AuditOrigin::Scheduler)
                .unwrap_err();
            assert!(
                error.starts_with("audit begin failed: kind=unavailable code=audit_event_caller_refused manager=/var/packages/synology-drive-sync/target/bin/sdsync-dsm (the package manager refused this process as a caller"),
                "{error}"
            );
            assert!(
                audit
                    .transport_refused()
                    .is_some_and(|cause| cause.contains("code=audit_event_caller_refused")),
                "{:?}",
                audit.transport_refused()
            );
            let names = outbox.events();
            assert_eq!(names.len(), 1, "{names:?}");
            let left = record_json(&outbox, &names[0]);
            assert_eq!(left["phase"], "failed");
            assert_eq!(left["operation"], "run");
            assert_eq!(left["profile"], "checkoffice_nas");
            assert_eq!(left["actor"], "package-scheduler");
            assert_eq!(left["origin"], "scheduler");
            assert_eq!(left["owner_pid"], std::process::id());
            assert!(left["job_id"].is_null());
            assert!(left.get("client_request_id").is_none(), "{left}");
            let error = audit.reconcile().unwrap_err();
            assert!(
                error.contains(
                    "audit reconcile failed: kind=unavailable code=audit_event_caller_refused"
                ),
                "{error}"
            );
            assert_eq!(outbox.events(), names, "kept for the next reconciliation");
            let (append, seen) = recording();
            let fixed = InProcessAudit::with(outbox.paths, outbox.uid, append);
            fixed.reconcile().unwrap();
            assert!(outbox.events().is_empty(), "{:?}", outbox.events());
            assert_eq!(
                *seen.lock().unwrap(),
                [
                    "run checkoffice_nas requested",
                    "run checkoffice_nas failed"
                ]
            );
        }

        /// The boot id the kernel gives this boot, as an owner records it.
        fn boot_id() -> String {
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim()
                .to_owned()
        }

        /// A pid that names no live process: a child that has been reaped.
        fn dead_pid() -> u32 {
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        }

        /// Write the outbox record found on the NAS after 26.65's first run,
        /// byte for byte in shape, with `phase` and the owner given.
        fn plant_nas_record(outbox: &Outbox, phase: &str, owner: (u32, u64, &str)) -> String {
            let transaction = format!(
                "scheduler-f16d{}",
                "0123456789abcdef".repeat(4)[..60].to_owned()
            );
            let text = format!(
                "{{\"schema\":\"sdsync.dsm-audit-outbox.v1\",\"transaction\":\"{transaction}\",\
                 \"operation\":\"run\",\"profile\":\"checkoffice_nas\",\"actor\":\"package-scheduler\",\
                 \"actor_uid\":190227,\"origin\":\"scheduler\",\"job_id\":null,\"owner_pid\":{},\
                 \"owner_start\":{},\"owner_boot\":\"{}\",\"phase\":\"{phase}\"}}",
                owner.0, owner.1, owner.2
            );
            let path = outbox.paths.directory.join(format!("{transaction}.event"));
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            transaction
        }

        /// fix11, the upgrade: the record 26.65 left (terminal `failed`) is
        /// reconciled by the fixed daemon whether its owner is the daemon that
        /// is still running or the one the package restart ended, and a record
        /// still `prepared` is completed as failed once its owner is gone but
        /// left alone while it lives.
        #[test]
        fn the_record_left_on_the_nas_is_reconciled_with_a_live_or_a_dead_owner() {
            let boot = boot_id();
            let live = linux_files::current_process_identity().unwrap();
            let dead = dead_pid();
            for (label, phase, owner, retired) in [
                (
                    "failed-live",
                    "failed",
                    (live.0, live.1, boot.as_str()),
                    true,
                ),
                (
                    "failed-dead",
                    "failed",
                    (dead, 36_524_948, boot.as_str()),
                    true,
                ),
                (
                    "prepared-dead",
                    "prepared",
                    (dead, 36_524_948, boot.as_str()),
                    true,
                ),
                (
                    "prepared-live",
                    "prepared",
                    (live.0, live.1, boot.as_str()),
                    false,
                ),
            ] {
                let outbox = Outbox::new(label);
                let transaction = plant_nas_record(&outbox, phase, owner);
                let (append, seen) = recording();
                let audit = InProcessAudit::with(outbox.paths, outbox.uid, append);
                audit.reconcile().unwrap_or_else(|e| panic!("{label}: {e}"));
                if retired {
                    assert!(outbox.events().is_empty(), "{label}: {:?}", outbox.events());
                    assert_eq!(
                        *seen.lock().unwrap(),
                        [
                            "run checkoffice_nas requested",
                            "run checkoffice_nas failed"
                        ],
                        "{label}"
                    );
                } else {
                    assert_eq!(outbox.events(), [format!("{transaction}.event")], "{label}");
                    assert!(seen.lock().unwrap().is_empty(), "{label}");
                }
            }
        }

        /// fix11, the fallback over a real outbox: the in-process transport is
        /// refused at its first begin; the run is begun through the other
        /// transport (here an in-process audit whose events are taken, standing
        /// in for the bridge children), which first retires the refused begin's
        /// record. Every later step goes there directly, the refused transport
        /// is not asked again, and the switch is announced once.
        #[test]
        fn a_refused_in_process_audit_hands_the_run_to_the_bridge_once() {
            let outbox = Outbox::new("fallback");
            let refusals = Arc::new(AtomicUsize::new(0));
            let primary: Arc<dyn RunAudit> = Arc::new(InProcessAudit::with(
                outbox.paths,
                outbox.uid,
                caller_refused(Arc::clone(&refusals)),
            ));
            let (append, seen) = recording();
            let bridge: Arc<dyn RunAudit> =
                Arc::new(InProcessAudit::with(outbox.paths, outbox.uid, append));
            let announced = Arc::new(Mutex::new(Vec::new()));
            let audit = super::super::FallbackAudit::new(primary, bridge, {
                let announced = Arc::clone(&announced);
                Box::new(move |cause, step| {
                    announced.lock().unwrap().push(format!("{cause} | {step}"));
                })
            });
            // Nothing in the outbox: the reconcile writes no event and is not refused.
            audit.reconcile().unwrap();
            assert_eq!(refusals.load(Ordering::SeqCst), 0);
            let transaction = audit.begin("run", "docs", AuditOrigin::Scheduler).unwrap();
            let refused_so_far = refusals.load(Ordering::SeqCst);
            assert!(refused_so_far > 0);
            assert_eq!(outbox.events(), [format!("{transaction}.event")]);
            audit.execute(&transaction).unwrap();
            audit.complete(&transaction, "succeeded").unwrap();
            assert!(outbox.events().is_empty(), "{:?}", outbox.events());
            // A second run never meets the refused transport.
            audit.reconcile().unwrap();
            let second = audit.begin("run", "docs", AuditOrigin::Scheduler).unwrap();
            audit.execute(&second).unwrap();
            audit.complete(&second, "failed").unwrap();
            assert_eq!(refusals.load(Ordering::SeqCst), refused_so_far);
            assert_eq!(
                *seen.lock().unwrap(),
                [
                    // The refused begin's record, retired by the bridge.
                    "run docs requested",
                    "run docs failed",
                    "run docs requested",
                    "run docs requested",
                    "run docs succeeded",
                    "run docs requested",
                    "run docs requested",
                    "run docs failed",
                ]
            );
            let announced = announced.lock().unwrap();
            assert_eq!(announced.len(), 1, "{announced:?}");
            assert!(
                announced[0].contains("code=audit_event_caller_refused")
                    && announced[0].contains("begin: audit begin failed"),
                "{announced:?}"
            );
        }
    }
}

#[cfg(test)]
mod fallback_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A transport whose every step fails with `error`; `refused` is what it
    /// reports as its transport refusal.
    struct Failing {
        error: &'static str,
        refused: Option<&'static str>,
        calls: AtomicUsize,
    }

    impl RunAudit for Failing {
        fn reconcile(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.error.into())
        }
        fn begin(&self, _: &str, _: &str, _: AuditOrigin) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.error.into())
        }
        fn execute(&self, _: &str) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.error.into())
        }
        fn complete(&self, _: &str, _: &str) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.error.into())
        }
        fn transport_refused(&self) -> Option<String> {
            self.refused.map(str::to_owned)
        }
    }

    /// Records the steps it is asked for.
    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);

    impl RunAudit for Recording {
        fn reconcile(&self) -> Result<(), String> {
            self.0.lock().unwrap().push("reconcile".into());
            Ok(())
        }
        fn begin(&self, operation: &str, scope: &str, _: AuditOrigin) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("begin {operation} {scope}"));
            Ok("tx".into())
        }
        fn execute(&self, transaction: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("execute {transaction}"));
            Ok(())
        }
        fn complete(&self, transaction: &str, state: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("complete {transaction} {state}"));
            Ok(())
        }
    }

    fn counted(
        primary: Arc<dyn RunAudit>,
        bridge: Arc<dyn RunAudit>,
    ) -> (FallbackAudit, Arc<AtomicUsize>) {
        let announced = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&announced);
        let audit = FallbackAudit::new(
            primary,
            bridge,
            Box::new(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );
        (audit, announced)
    }

    /// A failure that is not the transport's refusal (the outbox unsafe, the
    /// log full) is the run's answer: it is returned as it is and nothing
    /// switches, so a real audit failure is never papered over by a retry.
    #[test]
    fn a_failure_that_is_not_a_refusal_is_returned_and_does_not_switch() {
        let primary = Arc::new(Failing {
            error: "audit reconcile failed: kind=unsafe_runtime code=none",
            refused: None,
            calls: AtomicUsize::new(0),
        });
        let bridge = Arc::new(Recording::default());
        let (audit, announced) = counted(primary.clone(), bridge.clone());
        for _ in 0..2 {
            assert_eq!(
                audit.reconcile().unwrap_err(),
                "audit reconcile failed: kind=unsafe_runtime code=none"
            );
            assert!(audit.begin("run", "docs", AuditOrigin::Scheduler).is_err());
        }
        assert_eq!(primary.calls.load(Ordering::SeqCst), 4);
        assert!(bridge.0.lock().unwrap().is_empty());
        assert_eq!(announced.load(Ordering::SeqCst), 0);
    }

    /// A refusal met at any step switches every later step, once and for good;
    /// the step that met it is done again through the bridge.
    #[test]
    fn a_refusal_at_complete_finishes_the_run_through_the_bridge() {
        let primary = Arc::new(Failing {
            error: "audit complete: the event could not be written yet",
            refused: Some("code=audit_event_caller_refused"),
            calls: AtomicUsize::new(0),
        });
        let bridge = Arc::new(Recording::default());
        let (audit, announced) = counted(primary.clone(), bridge.clone());
        audit.complete("tx", "succeeded").unwrap();
        audit.reconcile().unwrap();
        audit.begin("plan", "all", AuditOrigin::Manager).unwrap();
        audit.execute("tx").unwrap();
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *bridge.0.lock().unwrap(),
            [
                "complete tx succeeded",
                "reconcile",
                "begin plan all",
                "execute tx"
            ]
        );
        assert_eq!(announced.load(Ordering::SeqCst), 1);
    }

    /// The bridge failing too is named with both causes.
    #[test]
    fn a_bridge_that_fails_too_is_named_with_both_causes() {
        let primary = Arc::new(Failing {
            error: "audit begin failed: kind=unavailable code=audit_event_caller_refused",
            refused: Some("code=audit_event_caller_refused"),
            calls: AtomicUsize::new(0),
        });
        let bridge = Arc::new(Failing {
            error: "--audit-transaction reconcile exited 73",
            refused: None,
            calls: AtomicUsize::new(0),
        });
        let (audit, announced) = counted(primary, bridge);
        let error = audit
            .begin("run", "docs", AuditOrigin::Scheduler)
            .unwrap_err();
        assert_eq!(
            error,
            "audit begin failed: kind=unavailable code=audit_event_caller_refused; through the \
             bridge too: --audit-transaction reconcile exited 73"
        );
        assert_eq!(announced.load(Ordering::SeqCst), 1);
    }
}
