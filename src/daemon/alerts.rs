//! Failure alerts for daemon runs: the failure counter, the DSM system log
//! (Log Center) and the desktop notification, as the shell runner and
//! controller delivered them.
//!
//! The delivery is the package's own shell code, unchanged
//! (`handle_alert_result`, `dsm_log_event`, `safe_notify` in `sdsync-common`):
//! it owns the alert policy (`alerts.conf`), the cooldown ledgers and the
//! notifiers, and porting it would be a second implementation to keep in step.
//! The daemon calls it in a child shell, off the control thread, with a 10 s
//! deadline, and names every delivery that fails (`alert_unavailable`, with
//! the channel and cause). An escalation's own cooldown (per routine, the
//! alert policy's `cooldown_seconds`) is the control thread's, kept in the
//! routine's `escalated_epoch` as the shell kept it.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use super::routines::Level;

/// Where an alert's outcome is logged: level, event, detail.
pub(crate) type AlertLog = Arc<dyn Fn(Level, &'static str, String) + Send + Sync>;

/// The alert channels. Every call returns at once; delivery is asynchronous
/// and its failures are logged by the implementation.
pub(crate) trait Alerts: Send + Sync {
    /// A finished sync of `scope` (`all` or a profile): the failure counter,
    /// and the notification and system log when the policy asks for them.
    fn run_result(&self, scope: &str, failed: bool, exit: i32);
    /// A routine whose retries are exhausted (its cooldown already checked).
    fn escalate(&self, profile: &str, exit: i32);
}

/// No delivery: every alert is logged as unavailable, with why.
pub(crate) struct NoAlerts {
    pub(crate) cause: String,
    pub(crate) log: AlertLog,
}

impl Alerts for NoAlerts {
    fn run_result(&self, scope: &str, failed: bool, exit: i32) {
        if failed {
            (self.log)(
                Level::Error,
                "alert_unavailable",
                format!(
                    "scope={scope} exit={exit} channel=counter,desktop,system_log cause={}",
                    self.cause
                ),
            );
        }
    }

    fn escalate(&self, profile: &str, exit: i32) {
        (self.log)(
            Level::Error,
            "alert_unavailable",
            format!(
                "profile={profile} exit={exit} channel=desktop,system_log cause={}",
                self.cause
            ),
        );
    }
}

/// How long one delivery may take.
pub(crate) const ALERT_DEADLINE: Duration = Duration::from_secs(10);

/// The alert policy's `cooldown_seconds` (60..2592000), else the shell's 3600.
pub(crate) fn policy_cooldown(alerts_conf: Option<&[u8]>) -> u64 {
    alerts_conf
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("cooldown_seconds="))
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .filter(|seconds| (60..=2_592_000).contains(seconds))
        .unwrap_or(3600)
}

#[cfg(target_os = "linux")]
pub(crate) use shell::ShellAlerts;

#[cfg(target_os = "linux")]
mod shell {
    use std::ffi::OsString;
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Instant;

    use super::{ALERT_DEADLINE, AlertLog, Alerts};
    use crate::dsm_api::daemon::routines::Level;

    /// Delivery through the package's `sdsync-common`.
    pub(crate) struct ShellAlerts {
        pub(crate) common: PathBuf,
        pub(crate) env: Vec<(OsString, OsString)>,
        pub(crate) log: AlertLog,
    }

    /// `handle_alert_result SCOPE RESULT EXIT`, as the runner called it.
    const RUN_RESULT: &str = r#". "$1" || exit 70; shift; handle_alert_result "$@""#;

    /// The controller's routine_escalate_alert without its cooldown (the
    /// daemon's): the system log always, the desktop when the policy wants
    /// failures. Exit 0 only when every wanted channel delivered.
    const ESCALATE: &str = r#". "$1" || exit 70
status=0
dsm_log_event sync_failed "$(epoch_now)" || { echo "channel=system_log status=$?" >&2; status=71; }
enabled=$(setting "$alert_policy_file" enabled true) || enabled=true
failure=$(setting "$alert_policy_file" on_failure true) || failure=true
if [ "$enabled" = true ] && [ "$failure" = true ]; then
    safe_notify sync_failed "$2" "$3" || { echo "channel=desktop status=$?" >&2; status=71; }
fi
exit "$status""#;

    impl ShellAlerts {
        fn deliver(&self, what: String, script: &'static str, arguments: Vec<String>) {
            let common = self.common.clone();
            let env = self.env.clone();
            let log = self.log.clone();
            let spawned = thread::Builder::new()
                .name("sdsyncd-alert".into())
                .spawn(move || {
                    let outcome = run(&common, &env, script, &arguments);
                    if let Err(cause) = outcome {
                        log(
                            Level::Error,
                            "alert_unavailable",
                            format!("{what} cause={cause}"),
                        );
                    }
                });
            if let Err(error) = spawned {
                (self.log)(
                    Level::Error,
                    "alert_unavailable",
                    format!("cause=the alert thread could not start: {error}"),
                );
            }
        }
    }

    fn run(
        common: &PathBuf,
        env: &[(OsString, OsString)],
        script: &str,
        arguments: &[String],
    ) -> Result<(), String> {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .arg("sdsync-alert")
            .arg(common)
            .args(arguments)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("/bin/sh: {error}"))?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        return Ok(());
                    }
                    let mut err = String::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        let _ = (&mut stderr).take(4096).read_to_string(&mut err);
                    }
                    let last = err
                        .lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    return Err(format!(
                        "status {}{}",
                        status.code().unwrap_or(-1),
                        if last.is_empty() {
                            String::new()
                        } else {
                            format!(": {last}")
                        }
                    ));
                }
                Ok(None) => {}
                Err(error) => return Err(format!("wait failed: {error}")),
            }
            if started.elapsed() >= ALERT_DEADLINE {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {} s", ALERT_DEADLINE.as_secs()));
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    impl Alerts for ShellAlerts {
        fn run_result(&self, scope: &str, failed: bool, exit: i32) {
            self.deliver(
                format!("scope={scope} exit={exit} channel=counter,desktop,system_log"),
                RUN_RESULT,
                vec![
                    scope.to_owned(),
                    if failed { "failed" } else { "succeeded" }.to_owned(),
                    exit.max(0).to_string(),
                ],
            );
        }

        fn escalate(&self, profile: &str, exit: i32) {
            self.deliver(
                format!("profile={profile} exit={exit} channel=escalation"),
                ESCALATE,
                vec![profile.to_owned(), exit.max(1).to_string()],
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        fn common(dir: &std::path::Path, body: &str) -> PathBuf {
            let path = dir.join("sdsync-common");
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            path
        }

        /// The shell functions receive the runner's arguments; a delivery that
        /// fails, or never answers, is named with its cause.
        #[test]
        fn deliveries_reach_the_shell_and_failures_are_named() {
            let dir = std::env::temp_dir().join(format!("sdsyncd-alerts-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let calls = dir.join("calls");
            let path = common(
                &dir,
                &format!(
                    "handle_alert_result() {{ echo \"result $*\" >> {calls}; [ \"$1\" != broken ] || {{ echo 'error: alert processing refused: alerts.conf is a symlink' >&2; return 73; }}; }}\n\
                     epoch_now() {{ echo 1; }}\n\
                     dsm_log_event() {{ echo \"syslog $*\" >> {calls}; }}\n\
                     setting() {{ echo true; }}\n\
                     alert_policy_file=/dev/null\n\
                     safe_notify() {{ echo \"notify $*\" >> {calls}; [ \"$2\" != slow ] || sleep 30; }}\n",
                    calls = calls.display()
                ),
            );
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let alerts = ShellAlerts {
                common: path,
                env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                log: Arc::new(move |_, event, detail| {
                    sink.lock().unwrap().push(format!("{event} {detail}"));
                }),
            };
            alerts.run_result("photos", true, 1);
            alerts.escalate("photos", 1);
            alerts.run_result("broken", true, 1);
            alerts.escalate("slow", 1);
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                let seen = events.lock().unwrap().clone();
                let text = std::fs::read_to_string(&calls).unwrap_or_default();
                if text.contains("result photos failed 1")
                    && text.contains("syslog sync_failed 1")
                    && text.contains("notify sync_failed photos 1")
                    && seen
                        .iter()
                        .any(|e| e.contains("scope=broken") && e.contains("symlink"))
                    && seen
                        .iter()
                        .any(|e| e.contains("profile=slow") && e.contains("within 10 s"))
                {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "{seen:?}\n{text}");
                thread::sleep(Duration::from_millis(50));
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_cooldown_follows_the_policy_within_its_bounds() {
        assert_eq!(super::policy_cooldown(None), 3600);
        assert_eq!(
            super::policy_cooldown(Some(b"enabled=true\ncooldown_seconds=600\n")),
            600
        );
        assert_eq!(super::policy_cooldown(Some(b"cooldown_seconds=5\n")), 3600);
        assert_eq!(super::policy_cooldown(Some(b"cooldown_seconds=x\n")), 3600);
    }
}
