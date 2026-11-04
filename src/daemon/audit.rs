//! The mandatory audit of a run (I-7), as `sdsync-run` kept it: the outbox is
//! reconciled before a run is admitted, a transaction is created and begun
//! (`prepared`) once it is admitted, moved to `executing` just before the core
//! starts, and completed with the run's final state. A run whose audit cannot
//! be opened does not start (`audit_unavailable`): the package never changes
//! the NAS without a durable record that it began to.
//!
//! The steps are the bridge's own (`sdsync-dsm-api --audit-transaction …`),
//! run as bounded children of the daemon. The daemon is therefore the outbox
//! record's owner, and a daemon that dies mid-run leaves a record the next
//! reconciliation completes as `outcome_unknown`, never as success.

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
