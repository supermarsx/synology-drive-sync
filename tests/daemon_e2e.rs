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

#[path = "support/dsm_package.rs"]
mod dsm_package;
use dsm_package::DsmPackage;

const DAEMON: &str = env!("CARGO_BIN_EXE_sdsync-dsm-api");
const CORE: &str = env!("CARGO_BIN_EXE_synology-drive-sync");

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Package {
    root: PathBuf,
    dsm: DsmPackage,
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
        let dsm = DsmPackage::build(&root, Path::new(CORE));
        dsm.configure_profile("alpha", &[]);
        Self {
            root,
            dsm,
            daemon: None,
        }
    }

    /// Save profile `name` through the real manager (`overrides` replace or add
    /// `configure-profile` options, e.g. `("--timeout", "2")`), returning the
    /// fragment it wrote. Call before [`Package::start`].
    fn configure(&self, name: &str, overrides: &[(&str, &str)]) -> PathBuf {
        self.dsm.configure_profile(name, overrides)
    }

    /// The directory profile `name` syncs from (under the DSM volume root).
    fn source_dir(&self, name: &str) -> PathBuf {
        self.dsm.source_dir(name)
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
        // The socket answers from the moment it listens, a beat before the daemon publishes
        // daemon.ready (listen, then the record): wait for the record, and say what is there.
        let ready = self.p("var/run/daemon.ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "the daemon answered ping but never published {}: {:?}
{}",
                ready.display(),
                fs::read_dir(self.p("var/run"))
                    .map(|entries| entries
                        .flatten()
                        .map(|entry| entry.file_name())
                        .collect::<Vec<_>>())
                    .unwrap_or_default(),
                self.stderr()
            );
            thread::sleep(Duration::from_millis(10));
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
    let source = package.source_dir("alpha");
    fs::write(source.join("file.txt"), b"data").unwrap();
    package.configure(
        "alpha",
        &[
            ("--url", &format!("http://127.0.0.1:{port}")),
            ("--timeout", "2"),
            ("--connect-timeout", "2"),
            ("--retries", "0"),
        ],
    );
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
    // daemon.pid: pid, start tick, boot id (line 1 is what PID readers take).
    let first_line = || {
        fs::read_to_string(package.p("var/run/daemon.pid"))
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let pid: u32 = first_line().parse().unwrap();
    assert!(launch().status.success(), "start is idempotent");
    assert_eq!(first_line(), pid.to_string());
    // A crash is restarted by the supervisor and named.
    crash(pid);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let now_pid = first_line();
        if !now_pid.is_empty()
            && now_pid != pid.to_string()
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

// ---------------------------------------------------------------- fix5: records and the supervisor

fn launch_command(package: &Package, core: &Path) -> Command {
    let mut command = Command::new(DAEMON);
    command
        .arg("--launch-daemon")
        .args(["--home".as_ref(), package.p("home").as_os_str()])
        .args(["--var".as_ref(), package.p("var").as_os_str()])
        .args(["--core".as_ref(), core.as_os_str()])
        .env("PATH", "/usr/bin:/bin");
    command
}

/// The parent of `pid` from /proc, when it is a `--supervise` process.
fn supervisor_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ppid: u32 = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let cmdline = fs::read(format!("/proc/{ppid}/cmdline")).ok()?;
    cmdline
        .split(|b| *b == 0)
        .any(|arg| arg == b"--supervise")
        .then_some(ppid)
}

/// Live and not a zombie.
fn process_live(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| !matches!(state, "Z" | "X" | "x"))
    })
}

fn identity_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Every live `--supervise` process whose arguments name `var`.
fn supervisors_of_tree(var: &Path) -> Vec<u32> {
    let var = var.display().to_string();
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            process_live(*pid)
                && fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                    let text = String::from_utf8_lossy(&c);
                    text.contains("--supervise") && text.contains(&var)
                })
        })
        .collect()
}

/// S1-c2: daemon.pid names the daemon by its whole identity (pid, start tick,
/// boot id), as daemon.ready does, so a reused PID is never mistaken for it.
#[test]
fn daemon_pid_carries_the_start_and_boot_identity() {
    let mut package = Package::new("pid-identity");
    let core = script(&package.p("core"), "exit 0");
    package.start(&core, &[]);
    let pid = identity_lines(&package.p("var/run/daemon.pid"));
    let ready = identity_lines(&package.p("var/run/daemon.ready"));
    assert_eq!(pid.len(), 3, "{pid:?}");
    assert_eq!(pid, ready, "daemon.pid and daemon.ready name one process");
    assert_eq!(
        pid[0],
        package.daemon.as_ref().unwrap().id().to_string(),
        "{pid:?}"
    );
    package.stop(Duration::from_secs(5));
}

/// S1-c2: a daemon.ready that cannot be written is fatal: the daemon says so,
/// removes its records and exits, rather than serving a daemon that every stop
/// must refuse (it has no readiness identity).
#[test]
fn a_failed_ready_write_stops_the_daemon_and_removes_its_records() {
    let package = Package::new("ready-unwritable");
    let core = script(&package.p("core"), "exit 0");
    private_dir(&package.p("@appdata/run"));
    // A directory where daemon.ready goes: the atomic write refuses it.
    private_dir(&package.p("@appdata/run/daemon.ready"));
    let manager = script(&package.p("manager"), "exit 0");
    let stderr = fs::File::create(package.p("daemon.stderr")).unwrap();
    let mut child = Command::new(DAEMON)
        .arg("--daemon")
        .args(["--home".as_ref(), package.p("home").as_os_str()])
        .args(["--var".as_ref(), package.p("var").as_os_str()])
        .args(["--core".as_ref(), core.as_os_str()])
        .args(["--manager".as_ref(), manager.as_os_str()])
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the daemon kept serving without daemon.ready: {}",
                package.stderr()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(1), "{}", package.stderr());
    assert!(
        package.stderr().contains("daemon.ready"),
        "{}",
        package.stderr()
    );
    for gone in ["var/run/ctl.sock", "var/run/daemon.pid"] {
        assert!(!package.p(gone).exists(), "{gone} left behind");
    }
    let log = package.log();
    assert!(
        log.contains("\"event\":\"service_start_failed\"") && log.contains("step=daemon.ready"),
        "{log}"
    );
}

/// S1-c2 trigger A: a supervisor whose restart budget is spent removes the
/// records of the daemon that died, and its own, so start and stop never
/// refuse over a dead daemon whose PID was reused.
#[test]
fn a_supervisor_that_gives_up_removes_the_daemon_records() {
    let package = Package::new("gives-up");
    let core = script(&package.p("core"), "exit 0");
    let launched = launch_command(&package, &core).output().unwrap();
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let read_pid = || -> Option<u32> {
        fs::read_to_string(package.p("var/run/daemon.pid"))
            .ok()?
            .lines()
            .next()?
            .parse()
            .ok()
    };
    let first = read_pid().unwrap();
    let supervisor = supervisor_of(first).expect("the daemon runs under --supervise");
    assert_eq!(
        identity_lines(&package.p("var/run/supervisor.pid"))
            .first()
            .map(String::as_str),
        Some(supervisor.to_string().as_str()),
        "supervisor.pid names the supervisor"
    );
    let mut current = first;
    // Five restarts are allowed in ten minutes; the sixth crash ends it.
    for round in 0..6 {
        crash(current);
        if round == 5 {
            break;
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(next) = read_pid()
                && next != current
                && package.try_ask(&json!({"verb": "ping"})).is_some()
            {
                current = next;
                break;
            }
            assert!(
                Instant::now() < deadline,
                "round {round}: no restart: {}",
                package.log()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while process_live(supervisor) {
        assert!(Instant::now() < deadline, "the supervisor did not give up");
        thread::sleep(Duration::from_millis(50));
    }
    for gone in [
        "var/run/daemon.pid",
        "var/run/daemon.ready",
        "var/run/ctl.sock",
        "var/run/supervisor.pid",
    ] {
        assert!(!package.p(gone).exists(), "{gone} left behind");
    }
    let log = package.log();
    assert!(
        log.contains("restarts_exhausted=true")
            && log.contains("\"event\":\"service_records_removed\""),
        "{log}"
    );
}

/// Hold the flock on `lock` for `seconds` the way the shell's probe does, and return only once it
/// is held (a `flock -n` of it reports it taken), so a test never races the holder's start.
// The caller waits for the holder (the test ends with `wait()` on it); clippy cannot see that
// across the return.
#[allow(clippy::zombie_processes)]
fn hold_lock(lock: &Path, seconds: u32) -> Child {
    private_dir(lock.parent().unwrap());
    fs::write(lock, b"").unwrap();
    fs::set_permissions(lock, fs::Permissions::from_mode(0o600)).unwrap();
    let holder = Command::new("flock")
        .arg("-x")
        .arg(lock)
        .args(["sleep", &seconds.to_string()])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let probe = Command::new("flock")
            .arg("-n")
            .arg(lock)
            .arg("true")
            .status()
            .unwrap();
        if probe.code() == Some(1) {
            return holder;
        }
        assert!(
            Instant::now() < deadline,
            "the holder never took {}: the probe exited {probe}",
            lock.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// A `supervisor.lock` held for a moment (the shell's probe) must not make a starting supervisor
/// leave with exit 0 and no daemon: it rides the lock out like the daemon does `daemon.lock`.
#[test]
fn a_supervisor_lock_held_for_a_moment_does_not_make_the_supervisor_leave() {
    let package = Package::new("supervisor-lock-moment");
    let core = script(&package.p("core"), "exit 0");
    let holder = hold_lock(&package.p("@appdata/run/supervisor.lock"), 1);
    let mut supervise = Command::new(DAEMON)
        .arg("--supervise")
        .args(["--home".as_ref(), package.p("home").as_os_str()])
        .args(["--var".as_ref(), package.p("var").as_os_str()])
        .args(["--core".as_ref(), core.as_os_str()])
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if package.try_ask(&json!({"verb": "ping"})).is_some() {
            break;
        }
        if let Some(status) = supervise.try_wait().unwrap() {
            panic!(
                "the supervisor left ({status}) with no daemon: {}",
                package.log()
            );
        }
        assert!(
            Instant::now() < deadline,
            "no daemon answered: {}",
            package.log()
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !package.log().contains("\"event\":\"service_superseded\""),
        "a lock held for a moment is not a second supervisor: {}",
        package.log()
    );
    // Stop it as the lifecycle does: TERM the supervisor, which stops its daemon.
    Command::new("kill")
        .args(["-TERM", &supervise.id().to_string()])
        .status()
        .unwrap();
    let status = supervise.wait().unwrap();
    assert!(status.success(), "{status}: {}", package.log());
    let mut holder = holder;
    let _ = holder.wait();
}

/// A program a forked child still holds open for writing is "Text file busy" until the child execs;
/// the fixture's run of the manager waits that out instead of failing the test (seen as
/// `run the manager: Text file busy` on slow, instrumented runners).
#[test]
fn a_script_that_is_briefly_busy_for_writing_is_run_once_it_is_free() {
    let dir = std::env::temp_dir().join(format!("sdsyncd-etxtbsy-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let program = script(&dir.join("prog"), "echo ran");
    let writer = fs::OpenOptions::new().write(true).open(&program).unwrap();
    // Busy: exec fails now, as it would with a forked child's copy of the descriptor.
    let plain = Command::new(&program).output();
    assert_eq!(
        plain.as_ref().err().and_then(|error| error.raw_os_error()),
        Some(26),
        "{plain:?}"
    );
    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        drop(writer);
    });
    let output = dsm_package::output_retrying(&mut Command::new(&program)).unwrap();
    releaser.join().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ran\n");
    let _ = fs::remove_dir_all(&dir);
}

/// F4: the shell's `flock -n daemon.lock true` probe holds the lock for a
/// moment; a daemon starting then waits it out instead of exiting 75.
#[test]
fn a_lock_held_for_a_moment_does_not_refuse_the_daemon() {
    let mut package = Package::new("lock-moment");
    let core = script(&package.p("core"), "exit 0");
    // Held before the daemon starts, and known to be held: not "a moment after
    // spawning flock", which under load could be after the daemon had the lock.
    let mut holder = hold_lock(&package.p("@appdata/run/daemon.lock"), 1);
    package.start(&core, &[]);
    let _ = holder.wait();
    package.stop(Duration::from_secs(5));
}

/// F4: a supervisor whose daemon finds another daemon serving says so and
/// leaves (exit 0), never silently.
#[test]
fn a_supervisor_beside_a_serving_daemon_says_it_is_superseded() {
    let mut package = Package::new("superseded");
    let core = script(&package.p("core"), "exit 0");
    package.start(&core, &[]);
    let started = Instant::now();
    let supervise = Command::new(DAEMON)
        .arg("--supervise")
        .args(["--home".as_ref(), package.p("home").as_os_str()])
        .args(["--var".as_ref(), package.p("var").as_os_str()])
        .args(["--core".as_ref(), core.as_os_str()])
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(supervise.status.success(), "{supervise:?}");
    let log = package.log();
    assert!(log.contains("\"event\":\"service_superseded\""), "{log}");
    assert!(
        !package.p("var/run/supervisor.pid").exists(),
        "the superseded supervisor removed its record"
    );
    package.stop(Duration::from_secs(5));
}

/// F3: a launch that times out stops the supervisor it started and says why
/// the daemon never answered, with the daemon's own last words.
#[test]
fn a_launch_that_times_out_stops_its_supervisor_and_names_the_cause() {
    let package = Package::new("launch-timeout");
    let core = script(&package.p("core"), "exit 0");
    private_dir(&package.p("@appdata/run"));
    // ctl.sock cannot be bound: every daemon exits 1 naming it.
    private_dir(&package.p("@appdata/run/ctl.sock"));
    fs::write(package.p("@appdata/run/ctl.sock/keep"), b"x").unwrap();
    let started = Instant::now();
    let launched = launch_command(&package, &core)
        .args(["--ready-within-seconds", "3"])
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(launched.status.code(), Some(1));
    let said = String::from_utf8_lossy(&launched.stderr);
    assert!(said.contains("ctl.sock"), "{said}");
    // Nothing of this tree keeps running.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = supervisors_of_tree(&package.p("var"));
        if left.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the supervisor outlived the launch: {left:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!package.p("var/run/supervisor.pid").exists());
}

/// The unit harness (`src/daemon/control_tests.rs`) runs on every CI platform, so
/// it cannot run the shell manager. It renders its profile fragments from a
/// template (`src/daemon/testdata/manager-profile.toml`); this test is what keeps
/// that template honest: it is the real manager's output with the package root,
/// the source directory and the profile name replaced by `@ROOT@`, `@SOURCE@`
/// and `@NAME@`, for two profile names. Set `SDSYNC_REGEN_MANAGER_TEMPLATE=1` to
/// rewrite the template after a deliberate manager change.
#[test]
fn the_unit_harness_template_is_what_the_real_manager_renders() {
    const TEMPLATE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/daemon/testdata/manager-profile.toml"
    );
    let package = Package::new("template");
    let render = |name: &str| {
        let fragment = package.configure(name, &[]);
        let text = fs::read_to_string(&fragment)
            .unwrap_or_else(|e| panic!("read {}: {e}", fragment.display()));
        (text, package.source_dir(name))
    };
    let (alpha, alpha_source) = render("alpha");
    let normalized = alpha
        .replace(alpha_source.to_str().unwrap(), "@SOURCE@")
        .replace(package.root.to_str().unwrap(), "@ROOT@")
        .replace("alpha", "@NAME@");
    if std::env::var_os("SDSYNC_REGEN_MANAGER_TEMPLATE").is_some() {
        fs::write(TEMPLATE, &normalized).unwrap_or_else(|e| panic!("write {TEMPLATE}: {e}"));
    }
    let template = fs::read_to_string(TEMPLATE).unwrap_or_else(|e| panic!("read {TEMPLATE}: {e}"));
    assert_eq!(
        normalized, template,
        "the manager renders a profile differently from the unit-harness template; \
         re-run with SDSYNC_REGEN_MANAGER_TEMPLATE=1 and review the diff"
    );
    assert!(template.starts_with("[profiles.@NAME@]\n"), "{template}");
    let (other, other_source) = render("bravo-2");
    let expected = template
        .replace("@SOURCE@", other_source.to_str().unwrap())
        .replace("@ROOT@", package.root.to_str().unwrap())
        .replace("@NAME@", "bravo-2");
    assert_eq!(
        other, expected,
        "the template does not generalise over the name"
    );
}
