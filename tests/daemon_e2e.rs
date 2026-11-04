//! End-to-end hang tests for `sdsync-dsm-api --daemon` (Phase 2, design §5(c)).
//!
//! The real daemon binary runs against a private package tree with a fake or
//! real core. Throughout every hang: `ctl status` answers in under a second, a
//! mutation completes in under two, and stop finishes within its budget.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const DAEMON: &str = env!("CARGO_BIN_EXE_sdsync-dsm-api");
const CORE: &str = env!("CARGO_BIN_EXE_synology-drive-sync");

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Package {
    root: PathBuf,
    daemon: Option<Child>,
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

impl Package {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sdsyncd-e2e-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        // Shaped like DSM: home and var are the framework's symlinks to
        // @apphome and @appdata, which is what the scripts pass the daemon.
        for dir in [
            "",
            "@apphome",
            "@apphome/config",
            "@apphome/config/profiles.d",
            "@apphome/config/routines.d",
            "@apphome/secrets",
            "@appdata",
        ] {
            private_dir(&root.join(dir));
        }
        std::os::unix::fs::symlink(root.join("@apphome"), root.join("home")).unwrap();
        std::os::unix::fs::symlink(root.join("@appdata"), root.join("var")).unwrap();
        // As the package writes them: the daemon holds every run to the
        // runner's pre-core checks (the url, the policy, the password file).
        fs::write(
            root.join("home/config/profiles.d/alpha.toml"),
            "[profiles.alpha]\nsource = \"/volume1/alpha\"\nremote = \"/home/Drive/alpha\"\n\
             url = \"http://127.0.0.1:9\"\nusername = \"operator\"\nexcludes = []\n\
             delete = false\nallow-empty-source = false\nmax-delete = 100\n\
             allow-http = true\ndanger-accept-invalid-certs = false\n",
        )
        .unwrap();
        let password = root.join("home/secrets/alpha.password");
        fs::write(&password, b"secret\n").unwrap();
        fs::set_permissions(&password, fs::Permissions::from_mode(0o600)).unwrap();
        Self { root, daemon: None }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn start(&mut self, core: &Path, extra: &[&str]) {
        let manager = script(
            &self.p("manager"),
            "sleep 0.1; echo \"saved $1\" >&2; exit 0",
        );
        let stderr = fs::File::create(self.p("daemon.stderr")).unwrap();
        let child = Command::new(DAEMON)
            .arg("--daemon")
            .args(["--home".as_ref(), self.p("home").as_os_str()])
            .args(["--var".as_ref(), self.p("var").as_os_str()])
            .args(["--core".as_ref(), core.as_os_str()])
            .args(["--manager".as_ref(), manager.as_os_str()])
            .args(extra)
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
        assert!(self.p("var/run/daemon.ready").exists());
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

    fn ask(&self, request: Value) -> Value {
        self.try_ask(&request)
            .unwrap_or_else(|| panic!("no reply to {request}: {}", self.stderr()))
    }

    /// `ctl status` under a second, every time.
    fn status_fast(&self) -> Value {
        let start = Instant::now();
        let status = self.ask(json!({"verb": "status"}));
        let took = start.elapsed();
        assert!(took < Duration::from_secs(1), "ctl status took {took:?}");
        status
    }

    fn wait_until(&self, what: &str, within: Duration, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + within;
        loop {
            let status = self.status_fast();
            if done(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {status}\n{}",
                self.stderr()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn state(&self, rel: &str) -> String {
        fs::read_to_string(self.p("var/state").join(rel)).unwrap_or_default()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.p("var/log/controller.log")).unwrap_or_default()
    }

    /// A mutation beside the run completes in under two seconds.
    fn mutation_fast(&self) {
        let start = Instant::now();
        let reply =
            self.ask(json!({"verb": "mutate", "op": "configure-profile", "profile": "alpha"}));
        let took = start.elapsed();
        assert_eq!(reply["ok"], true, "{reply}");
        assert!(took < Duration::from_secs(2), "mutation took {took:?}");
        let refused =
            self.ask(json!({"verb": "mutate", "op": "remove-profile", "profile": "alpha"}));
        assert_eq!(refused["code"], "profile_running", "{refused}");
    }

    /// Stop within the budget, leaving nothing an older pre-hook checks for.
    fn stop(&mut self, budget: Duration) {
        let start = Instant::now();
        let reply = self.ask(json!({"verb": "stop", "wait_s": 60}));
        assert_eq!(reply["state"], "stopped", "{reply}");
        let mut child = self.daemon.take().unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < budget,
                "the daemon did not exit within {budget:?}"
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "{status}: {}", self.stderr());
        for gone in [
            "var/run/ctl.sock",
            "var/run/daemon.ready",
            "var/run/daemon.pid",
            "var/run/run.lock",
        ] {
            assert!(!self.p(gone).exists(), "{gone} left behind");
        }
        assert!(
            self.state("controller.state")
                .starts_with("state=stopped\n")
        );
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn active_pid(status: &Value) -> bool {
    status["active"]["pid"].is_u64()
}

fn idle(status: &Value) -> bool {
    status["active"].is_null()
}

/// Hang test: a fake core that ignores TERM is KILLed after the grace and read
/// as `cancelled`; the daemon stays responsive and stops within its budget.
#[test]
fn a_core_that_ignores_term_is_killed_after_grace_and_reads_cancelled() {
    let mut package = Package::new("ignore-term");
    let core = script(
        &package.p("core"),
        "trap '' TERM; while :; do sleep 0.1; done",
    );
    package.start(&core, &["--cancel-grace-seconds", "1"]);
    let reply = package.ask(json!({"verb": "run", "scope": "alpha"}));
    assert_eq!(reply["ok"], true, "{reply}");
    package.wait_until("the core", Duration::from_secs(10), active_pid);
    assert!(
        package.p("var/run/run.lock/pid").exists(),
        "run.lock is published while it runs"
    );
    package.mutation_fast();
    let start = Instant::now();
    assert_eq!(package.ask(json!({"verb": "cancel"}))["state"], "stopping");
    package.wait_until("the cancel", Duration::from_secs(10), idle);
    assert!(
        start.elapsed() < Duration::from_secs(1 + 2 + 2),
        "{:?}",
        start.elapsed()
    );
    let run = package.state("run.state");
    assert!(run.contains("state=failed\n"), "{run}");
    assert!(run.contains("exit_code=137\n"), "{run}");
    assert!(run.contains("reason=cancelled\n"), "{run}");
    assert!(run.contains("outcome=cancelled\n"), "{run}");
    assert!(!package.p("var/run/run.lock").exists());
    // Stop during a second, equally deaf run.
    package.ask(json!({"verb": "run", "scope": "alpha"}));
    package.wait_until("the second core", Duration::from_secs(10), active_pid);
    package.stop(Duration::from_secs(1 + 2 + 5));
    let run = package.state("run.state");
    assert!(
        run.contains("reason=cancelled\n")
            && run.contains("detail=the package stopped during the run"),
        "{run}"
    );
}

/// Hang test: a stuck child (no I/O) is warned about, then ended by the
/// watchdog, and the outcome names it.
#[test]
fn a_stuck_core_is_named_by_the_watchdog() {
    let mut package = Package::new("stuck");
    let core = script(&package.p("core"), "exec sleep 600");
    package.start(
        &core,
        &[
            "--stall-warn-seconds",
            "1",
            "--stall-kill-seconds",
            "2",
            "--cancel-grace-seconds",
            "1",
        ],
    );
    package.ask(json!({"verb": "run", "scope": "alpha"}));
    package.wait_until("the core", Duration::from_secs(10), active_pid);
    package.mutation_fast();
    package.wait_until("the watchdog", Duration::from_secs(15), idle);
    let run = package.state("run.state");
    assert!(run.contains("reason=run_stalled\n"), "{run}");
    assert!(run.contains("detail=no progress for 1 min"), "{run}");
    let profile = package.state("profiles/alpha.state");
    assert!(profile.contains("reason=run_stalled\n"), "{profile}");
    let log = package.log();
    assert!(log.contains("\"event\":\"run_stalled\""), "{log}");
    package.stop(Duration::from_secs(5));
}

/// Hang test: the real core against a server that accepts and never answers
/// ends with a named timeout, and the daemon stays responsive meanwhile.
#[test]
fn a_black_hole_server_ends_in_a_named_timeout() {
    let mut package = Package::new("blackhole");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Accept and hold every connection, answering nothing.
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    let source = package.p("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("file.txt"), b"data").unwrap();
    let password = package.p("home/secrets/alpha.password");
    fs::write(&password, b"secret\n").unwrap();
    fs::set_permissions(&password, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        package.p("home/config/config.toml"),
        format!(
            "default-profile = \"alpha\"\n\n[profiles.alpha]\nsource = \"{}\"\nremote = \"/home/Drive/alpha\"\n\
             url = \"http://127.0.0.1:{port}\"\nusername = \"operator\"\npassword-file = \"{}\"\n\
             no-vault = true\nallow-http = true\ntimeout = 2\nconnect-timeout = 2\nretries = 0\n",
            source.display(),
            password.display()
        ),
    )
    .unwrap();
    package.start(Path::new(CORE), &[]);
    package.ask(json!({"verb": "run", "scope": "alpha"}));
    package.wait_until("the core", Duration::from_secs(10), |s| !idle(s));
    let finished = package.wait_until("the timeout", Duration::from_secs(90), idle);
    let run = package.state("run.state");
    assert!(run.contains("state=failed\n"), "{run}\n{finished}");
    let profile = package.state("profiles/alpha.state");
    let stderr_log = fs::read_to_string(package.p("var/log/run-stderr.log")).unwrap_or_default();
    // The cause is named where the dashboard shows it: the profile's detail
    // (240 bytes) and the run's stderr tail, under this run's header.
    assert!(
        profile.contains("operation timed out"),
        "a named timeout: {run}\n{profile}\n{stderr_log}"
    );
    assert!(
        stderr_log.starts_with("# epoch=") && stderr_log.contains("operation timed out"),
        "{stderr_log}"
    );
    assert!(
        run.contains(&format!("127.0.0.1:{port}"))
            || package
                .state("profiles/alpha.state")
                .contains(&format!("127.0.0.1:{port}")),
        "the endpoint is named: {run}\n{}",
        package.state("profiles/alpha.state")
    );
    package.stop(Duration::from_secs(5));
}

/// A second daemon refuses to start while the first holds the lock (exit 75),
/// and a client for a daemon that is not there gets a named, prompt failure.
#[test]
fn the_daemon_is_a_singleton_and_clients_fail_by_name() {
    let mut package = Package::new("single");
    let core = script(&package.p("core"), "exit 0");
    package.start(&core, &[]);
    let second = Command::new(DAEMON)
        .arg("--daemon")
        .args(["--home".as_ref(), package.p("home").as_os_str()])
        .args(["--var".as_ref(), package.p("var").as_os_str()])
        .output()
        .unwrap();
    assert_eq!(
        second.status.code(),
        Some(75),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let ctl = Command::new(DAEMON)
        .args(["--ctl", "status", "--var"])
        .arg(package.p("var"))
        .output()
        .unwrap();
    assert!(ctl.status.success());
    let status: Value = serde_json::from_slice(&ctl.stdout).unwrap();
    assert_eq!(status["schema"], "sdsyncd.status.v1");
    package.stop(Duration::from_secs(5));
    let start = Instant::now();
    let gone = Command::new(DAEMON)
        .args(["--ctl", "ping", "--var"])
        .arg(package.p("var"))
        .output()
        .unwrap();
    assert_eq!(gone.status.code(), Some(69));
    assert!(String::from_utf8_lossy(&gone.stdout).contains("daemon_unreachable"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

/// `--launch-daemon` returns once the daemon answers, start is idempotent, and
/// a clean `ctl stop` ends the supervisor too (it restarts only after a crash).
#[test]
fn launch_returns_when_ready_and_a_clean_stop_ends_the_supervisor() {
    let package = Package::new("launch");
    let core = script(&package.p("core"), "exit 0");
    let launch = || {
        Command::new(DAEMON)
            .arg("--launch-daemon")
            .args(["--home".as_ref(), package.p("home").as_os_str()])
            .args(["--var".as_ref(), package.p("var").as_os_str()])
            .args(["--core".as_ref(), core.as_os_str()])
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap()
    };
    let start = Instant::now();
    let first = launch();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(start.elapsed() < Duration::from_secs(30));
    assert_eq!(package.ask(json!({"verb": "ping"}))["ok"], true);
    let pid: u32 = fs::read_to_string(package.p("var/run/daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(launch().status.success(), "start is idempotent");
    assert_eq!(
        fs::read_to_string(package.p("var/run/daemon.pid"))
            .unwrap()
            .trim(),
        pid.to_string()
    );
    // A crash is restarted by the supervisor and named.
    crash(pid);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let now_pid = fs::read_to_string(package.p("var/run/daemon.pid")).unwrap_or_default();
        if !now_pid.trim().is_empty()
            && now_pid.trim() != pid.to_string()
            && package.try_ask(&json!({"verb": "ping"})).is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the supervisor did not restart the daemon"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let log = package.log();
    assert!(log.contains("\"event\":\"service_restarted\""), "{log}");
    assert_eq!(
        package.ask(json!({"verb": "stop", "wait_s": 30}))["state"],
        "stopped"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while package.p("var/run/ctl.sock").exists() {
        assert!(Instant::now() < deadline, "ctl.sock left behind");
        thread::sleep(Duration::from_millis(50));
    }
    // The supervisor exits after a clean stop rather than restarting.
    thread::sleep(Duration::from_millis(1500));
    assert!(package.try_ask(&json!({"verb": "ping"})).is_none());
}

/// SIGKILL a process by pid (the test's stand-in for a crash).
fn crash(pid: u32) {
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// A plain run that succeeds records the shell's documents and the bridge's
/// run.lock contract is released.
#[test]
fn a_clean_run_is_recorded_like_the_shell_runner_did() {
    let mut package = Package::new("clean");
    let core = script(
        &package.p("core"),
        "for a in \"$@\"; do case $prev in --result-record) out=$a ;; esac; prev=$a; done\n\
         printf '%s\\n' '{\"schema\":\"sdsync.result-record.v1\",\"profile\":\"alpha\",\"status\":\"complete\",\"error\":null,\"result\":{\"outcome\":\"complete\",\"deferred\":[],\"skipped\":0}}' > \"$out\"\n\
         exit 0",
    );
    package.start(&core, &[]);
    package.ask(json!({"verb": "run", "scope": "alpha"}));
    package.wait_until("the run", Duration::from_secs(10), |s| {
        idle(s)
            && s["events"]
                .as_array()
                .is_some_and(|e| e.iter().any(|v| v["event"] == "run_finished"))
    });
    let run = package.state("run.state");
    assert!(run.starts_with("state=succeeded\n"), "{run}");
    assert!(run.contains("operation=sync\nreason=ok\n"), "{run}");
    assert!(run.contains("deferred=0\nskipped=0\n"), "{run}");
    let profile = package.state("profiles/alpha.state");
    assert!(profile.starts_with("state=succeeded\n"), "{profile}");
    let mut activity = String::new();
    fs::File::open(package.p("var/log/activity.log"))
        .unwrap()
        .read_to_string(&mut activity)
        .unwrap();
    assert!(
        activity.contains("|run.started|alpha|running|")
            && activity.contains("|run.succeeded|alpha|succeeded|"),
        "{activity}"
    );
    package.stop(Duration::from_secs(5));
}
