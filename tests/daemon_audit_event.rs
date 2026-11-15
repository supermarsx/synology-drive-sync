//! fix11: the manager's private `api audit-event` endpoint, reached from under
//! the real daemon, as the daemon's in-process audit reaches it on a NAS.
//!
//! 26.65 moved the run audit into the daemon (`InProcessAudit`), whose event
//! writer runs the shipped manager's `api audit-event`. The manager accepted
//! that endpoint only from the bridge (`--serve`, `--consume-job`,
//! `--audit-transaction …` as argv[1]), so on DSM every event was refused
//! (exit 77) and every run was refused `audit_unavailable`, every 30 s, for
//! good. No test ran the endpoint with the daemon as its parent: the daemon's
//! unit tests wrote events through a recording stand-in, and the package suite
//! only checked that a stranger is refused.
//!
//! Here the parent is the real thing: the built `sdsync-dsm-api` installed as
//! the package's bridge in a DSM-shaped tree (symlinked `target`, `home`,
//! `var`), started with `--daemon`, running a "core" that `exec`s the shipped
//! manager -- so the manager's parent is the daemon process itself, with the
//! daemon's exe and argv. The manager gets exactly the argv and environment
//! `record_audit_event` gives it (pinned by the unit test
//! `the_audit_event_invocation_is_the_one_the_daemon_e2e_replays`), relocated
//! to the fixture's roots as everything in these fixtures is.
//!
//! Two bridge calls of the manager's audit writer read audit.log and
//! activity.log at the fixed `/var/packages` path in the real binary
//! (`repair-log-tail`, `verify`); in this tree they go to a stand-in, the same
//! relocation the fixture already makes for the framework alias. The identity
//! check under test, `require_internal_*_parent`, is not touched.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "support/dsm_package.rs"]
mod dsm_package;
use dsm_package::DsmPackage;

#[path = "../src/daemon/testkit.rs"]
#[allow(dead_code)]
mod testkit;

const DAEMON: &str = env!("CARGO_BIN_EXE_sdsync-dsm-api");
const CORE: &str = env!("CARGO_BIN_EXE_synology-drive-sync");

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The two fixed-path bridge calls of `append_audit_event`, relocated.
const FIXED_PATH_STAND_IN: &str = r#"#!/bin/sh
case "$1 $2" in
    "--audit-transaction repair-log-tail") echo clean ;;
    "--audit-transaction verify") ;;
    *) echo "fixed-path stand-in: unexpected call: $*" >&2; exit 64 ;;
esac
exit 0
"#;

struct Tree {
    dsm: DsmPackage,
    daemon: Option<Child>,
    transaction: String,
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    testkit::settle_for_exec(path);
}

impl Tree {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sdsyncd-audit-event-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let dsm = DsmPackage::build(&root, Path::new(CORE));
        // Saved through the shipped manager while the fixture's bridge mock is
        // still in place (it answers the transition and admission probes).
        dsm.configure_profile("alpha", &[]);
        // The real binary is the package's bridge from here on, and so the
        // daemon's executable: one file, mode 0755, one link, the package user's.
        let store = root.join("@appstore");
        let bridge = store.join("bin/sdsync-dsm-api");
        fs::remove_file(&bridge).unwrap();
        fs::copy(DAEMON, &bridge).unwrap();
        fs::set_permissions(&bridge, fs::Permissions::from_mode(0o755)).unwrap();
        testkit::settle_for_exec(&bridge);
        assert_eq!(fs::metadata(&bridge).unwrap().nlink(), 1);
        let stand_in = store.join("bin/fixed-path-bridge");
        executable(&stand_in, FIXED_PATH_STAND_IN);
        let common = store.join("libexec/sdsync-common");
        let mut text = fs::read_to_string(&common).unwrap();
        text.push_str(&format!(
            "\n# fixture: the real bridge reads the logs at /var/packages only\n\
             verify_durable_audit_event() {{ {0} --audit-transaction verify \"$@\"; }}\n\
             repair_durable_log_tail() {{ {0} --audit-transaction repair-log-tail \"$@\"; }}\n",
            stand_in.display()
        ));
        fs::write(&common, text).unwrap();
        let transaction = format!("scheduler-{}", "5e".repeat(32));
        let tree = Self {
            dsm,
            daemon: None,
            transaction,
        };
        tree.write_probes();
        tree
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dsm.p(rel)
    }

    fn uid(&self) -> u32 {
        fs::metadata(self.p("home")).unwrap().uid()
    }

    /// The environment `record_audit_event` gives the manager
    /// (`manager_command_environment` and the audit attribution), at this
    /// tree's roots, plus the two framework variables every fixture call sets.
    fn manager_env(&self) -> String {
        format!(
            "PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C LC_ALL=C HOME={home} \
             SYNOPKG_PKGDEST={target} SYNOPKG_PKGHOME={home} SYNOPKG_PKGVAR={var} \
             SYNOPKG_PKGNAME=synology-drive-sync SYNOPKG_DSM_VERSION_MAJOR=7 \
             SDSYNC_DSM_AUDIT_ACTOR=package-scheduler SDSYNC_DSM_AUDIT_ACTOR_UID={uid} \
             SDSYNC_DSM_AUDIT_ORIGIN=scheduler SDSYNC_DSM_AUDIT_TRANSACTION={tx}",
            home = self.p("home").display(),
            target = self.p("target").display(),
            var = self.p("var").display(),
            uid = self.uid(),
            tx = self.transaction,
        )
    }

    /// The core the daemon runs: it `exec`s the probe named in `probe.mode`,
    /// so the probe's parent is the daemon. Its output lands in
    /// `probe.<mode>.out`; its exit is the run's.
    fn write_probes(&self) {
        let manager = self.p("target/bin/sdsync-dsm");
        let env = self.manager_env();
        let parent_check = self.p("parent-check");
        executable(
            &parent_check,
            "#!/bin/sh\n\
             . \"$SYNOPKG_PKGDEST/libexec/sdsync-common\"\n\
             [ \"${PROBE_OWNER:-}\" != other ] || path_owner=$((path_owner + 1))\n\
             status=0; require_internal_daemon_audit_parent || status=$?\n\
             echo \"daemon_parent=$status\"\n\
             status=0; require_internal_api_bridge_parent || status=$?\n\
             echo \"bridge_parent=$status\"\n",
        );
        executable(
            &self.p("core"),
            &format!(
                "#!/bin/sh\n\
                 mode=$(cat {root}/probe.mode)\n\
                 out={root}/probe.$mode.out\n\
                 case $mode in\n\
                 event) exec /usr/bin/env -i {env} {manager} api audit-event --operation run --state requested --profile alpha >\"$out\" 2>&1 ;;\n\
                 rejected) exec /usr/bin/env -i {env} SDSYNC_DSM_AUDIT_REASON=unclassified {manager} api audit-rejected >\"$out\" 2>&1 ;;\n\
                 wake) exec /usr/bin/env -i {env} {manager} api controller-wake >\"$out\" 2>&1 ;;\n\
                 parent) exec /usr/bin/env -i {env} {check} >\"$out\" 2>&1 ;;\n\
                 other-owner) exec /usr/bin/env -i {env} PROBE_OWNER=other {check} >\"$out\" 2>&1 ;;\n\
                 esac\n\
                 echo \"unknown probe $mode\" >\"$out\"; exit 64\n",
                root = self.dsm.root().display(),
                manager = manager.display(),
                check = parent_check.display(),
            ),
        );
    }

    fn start(&mut self) {
        let stderr = fs::File::create(self.p("daemon.stderr")).unwrap();
        // Started from the package's own path, as the supervisor starts it.
        let child = Command::new(self.p("target/bin/sdsync-dsm-api"))
            .arg("--daemon")
            .args(["--home".as_ref(), self.p("home").as_os_str()])
            .args(["--var".as_ref(), self.p("var").as_os_str()])
            .args(["--core".as_ref(), self.p("core").as_os_str()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.try_ask(&json!({"verb": "ping"})).is_none() {
            assert!(
                Instant::now() < deadline,
                "the daemon never answered: {}",
                self.stderr()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn stderr(&self) -> String {
        fs::read_to_string(self.p("daemon.stderr")).unwrap_or_default()
    }

    fn try_ask(&self, request: &Value) -> Option<Value> {
        let mut stream = UnixStream::connect(self.p("var/run/ctl.sock")).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .ok()?;
        let mut line = request.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).ok()?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).ok()?;
        serde_json::from_str(&reply).ok()
    }

    /// Run the probe `mode` as the daemon's core; its output and the run's exit.
    fn probe(&self, mode: &str) -> (String, i64) {
        fs::write(self.p("probe.mode"), mode).unwrap();
        let asked = self
            .try_ask(&json!({"verb": "run", "scope": "alpha"}))
            .unwrap_or_else(|| panic!("no reply to run: {}", self.stderr()));
        assert_eq!(asked["ok"], true, "{asked}");
        let id = asked["id"].as_u64().expect("a run id");
        let done = self
            .try_ask(&json!({"verb": "wait", "id": id, "wait_s": 60}))
            .unwrap_or_else(|| panic!("no reply to wait: {}", self.stderr()));
        assert_eq!(done["finished"], true, "{done}");
        let out = fs::read_to_string(self.p(&format!("probe.{mode}.out"))).unwrap_or_default();
        (out, done["exit"].as_i64().unwrap_or(-2))
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.p("var/log").join(name)).unwrap_or_default()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = self.try_ask(&json!({"verb": "stop", "wait_s": 10}));
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(self.dsm.root());
    }
}

/// The daemon reaches `api audit-event` and the event is durable in audit.log
/// and Activity; the daemon's identity, under the package uid, is what the
/// manager accepts -- for this endpoint only. Before fix11 the event was refused
/// `forbidden` "private bridge endpoint" (exit 77): the field failure.
#[test]
fn the_daemon_reaches_the_managers_audit_event_and_nothing_else_private() {
    let mut tree = Tree::new("daemon-parent");
    tree.start();

    let (out, exit) = tree.probe("event");
    assert_eq!(
        exit, 0,
        "the manager refused the daemon's audit event: {out}"
    );
    let audit = tree.log("audit.log");
    let record = audit
        .lines()
        .find(|line| line.contains(&tree.transaction))
        .unwrap_or_else(|| panic!("no audit.log record for the event: {audit}"));
    let record: Value = serde_json::from_str(record).unwrap();
    assert_eq!(record["operation"], "run");
    assert_eq!(record["state"], "requested");
    assert_eq!(record["origin"], "scheduler");
    assert_eq!(record["actor"], "package-scheduler");
    assert_eq!(record["profile"], "alpha");
    assert_eq!(record["mandatory"], true);
    let activity = tree.log("activity.log");
    assert!(
        activity.lines().any(
            |line| line.contains("|audit.requested|alpha|") && line.contains(&tree.transaction)
        ),
        "{activity}"
    );

    // The same daemon is not a caller of the other private endpoints.
    for mode in ["rejected", "wake"] {
        let (out, exit) = tree.probe(mode);
        assert_eq!(exit, 77, "{mode}: {out}");
        assert!(out.contains("private bridge endpoint"), "{mode}: {out}");
    }

    // The identity check itself, from under the daemon: the daemon's shape is
    // accepted for the audit endpoint and not as a bridge; a parent under
    // another uid than the package's is refused.
    let (out, _) = tree.probe("parent");
    assert_eq!(out.trim(), "daemon_parent=0\nbridge_parent=77", "{out}");
    let (out, _) = tree.probe("other-owner");
    assert_eq!(out.trim(), "daemon_parent=77\nbridge_parent=77", "{out}");
}
