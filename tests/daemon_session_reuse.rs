//! fix9 P7: the daemon keeps the DSM session between runs and lends it to the next one.
//!
//! The real daemon runs the real core against a DSM-shaped tree and the in-tree File Station mock.
//! The core is reached through a wrapper that records its argument list and environment, so a
//! test can prove the session never travels there. Every test ends by searching every file the
//! daemon, the core and the wrapper wrote, and the daemon's own status reply, for the mock's SID
//! and `SynoToken`.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "support/dsm_package.rs"]
mod dsm_package;
use dsm_package::DsmPackage;

#[path = "support/file_station_mock.rs"]
#[allow(dead_code)]
mod file_station_mock;
use file_station_mock::{CapturedRequest, MockFileStation};

#[path = "../src/daemon/testkit.rs"]
#[allow(dead_code)]
mod testkit;

const DAEMON: &str = env!("CARGO_BIN_EXE_sdsync-dsm-api");
const CORE: &str = env!("CARGO_BIN_EXE_synology-drive-sync");
const REMOTE: &str = "/home/Drive/alpha";
const PASSWORD: &[u8] = b"correct horse battery staple\n";
/// What the mock issues (every SID it hands out starts with it) and its `SynoToken`.
const SECRETS: [&str; 2] = ["e2e-session-secret", "e2e-syno-token-secret"];

fn count(requests: &[CapturedRequest], operation: &str) -> usize {
    requests
        .iter()
        .filter(|request| request.operation() == operation)
        .count()
}

fn script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    testkit::settle_for_exec(path);
    path.to_path_buf()
}

struct Daemon {
    root: PathBuf,
    child: Option<Child>,
    server: MockFileStation,
}

impl Daemon {
    /// A daemon over profile `alpha` (three files, already on the NAS), `env` added to its own.
    fn start(label: &str, env: &[(&str, &str)]) -> Self {
        let root =
            std::env::temp_dir().join(format!("sdsyncd-session-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dsm = DsmPackage::build(&root, Path::new(CORE));
        let server = MockFileStation::start();
        for folder in ["/home", "/home/Drive", REMOTE] {
            server.add_directory(folder);
        }
        dsm.configure_profile(
            "alpha",
            &[("--url", server.base_url()), ("--username", "e2e-user")],
        );
        fs::write(root.join("home/secrets/alpha.password"), PASSWORD).unwrap();
        let source = dsm.source_dir("alpha");
        for index in 0..3 {
            let name = format!("file-{index}.dat");
            let content = format!("{index:064}");
            fs::write(source.join(&name), &content).unwrap();
            server.add_file(
                &format!("{REMOTE}/{name}"),
                content.as_bytes(),
                1_700_000_000,
            );
        }
        // The wrapper records exactly what the core was given: its arguments and environment.
        let wrapper = script(
            &root.join("core-wrapper"),
            &format!(
                "printf 'argv: %s\\n' \"$*\" >> {log}\nenv >> {log}\n{CORE} \"$@\"",
                log = root.join("core-argv.log").display()
            ),
        );
        let manager = script(&root.join("manager"), "exit 0");
        let stderr = fs::File::create(root.join("daemon.stderr")).unwrap();
        let mut command = Command::new(DAEMON);
        command
            .arg("--daemon")
            .args(["--home".as_ref(), root.join("home").as_os_str()])
            .args(["--var".as_ref(), root.join("var").as_os_str()])
            .args(["--core".as_ref(), wrapper.as_os_str()])
            .args(["--manager".as_ref(), manager.as_os_str()])
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        for (key, value) in env {
            command.env(key, value);
        }
        let daemon = Self {
            root: root.clone(),
            child: Some(command.spawn().unwrap()),
            server,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while !daemon.p("var/run/daemon.ready").exists() {
            assert!(
                Instant::now() < deadline,
                "the daemon never became ready: {}",
                daemon.stderr()
            );
            thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn ask(&self, request: &Value) -> Value {
        let mut stream = UnixStream::connect(self.p("var/run/ctl.sock")).expect("ctl socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(90)))
            .unwrap();
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap_or_else(|_| panic!("reply to {request}: {reply}"))
    }

    fn stderr(&self) -> String {
        fs::read_to_string(self.p("daemon.stderr")).unwrap_or_default()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.p("var/log/controller.log")).unwrap_or_default()
    }

    fn run_stderr(&self) -> String {
        fs::read_to_string(self.p("var/log/run-stderr.log")).unwrap_or_default()
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.server.requests()
    }

    /// Run profile `alpha` once; it must succeed.
    fn run_once(&self, label: &str) {
        let reply = self.ask(&json!({"verb": "run", "scope": "alpha"}));
        assert_eq!(reply["ok"], true, "{reply}");
        let id = reply["id"].clone();
        let deadline = Instant::now() + Duration::from_secs(120);
        let finished = loop {
            let status = self.ask(&json!({"verb": "status"}));
            if let Some(finished) = status["events"].as_array().and_then(|events| {
                events
                    .iter()
                    .find(|event| event["event"] == "run_finished" && event["id"] == id)
            }) {
                break finished.clone();
            }
            assert!(
                Instant::now() < deadline,
                "{label}: the run never finished: {status}\n{}",
                self.stderr()
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            finished["exit"],
            0,
            "{label}: {finished}\nrun-stderr: {}\ncontroller.log: {}",
            self.run_stderr(),
            self.log()
        );
    }

    fn wait_for(&self, what: &str, within: Duration, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + within;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "{what}\ncontroller.log: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn logouts(&self) -> usize {
        count(&self.requests(), "SYNO.API.Auth.logout")
    }

    /// Stop the daemon the way the package does, and wait for it to exit.
    fn stop(&mut self) {
        let reply = self.ask(&json!({"verb": "stop", "wait_s": 60}));
        assert_eq!(reply["state"], "stopped", "{reply}");
        let mut child = self.child.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the daemon did not exit");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "{status}: {}", self.stderr());
    }

    /// No file under the test's root, and nothing the daemon answers, holds a session secret:
    /// the controller and run logs, every state record, the result and progress records, the
    /// core's argv and environment (the wrapper's record) and the daemon's own stderr.
    fn assert_no_secret_anywhere(&self) {
        let (files, leaks) = self.secret_leaks();
        assert!(files > 10, "the search found only {files} files");
        assert!(leaks.is_empty(), "{leaks:#?}");
        let argv = fs::read_to_string(self.p("core-argv.log")).unwrap();
        assert!(argv.contains("--session-stdin"), "{argv}");
        if let Some(child) = &self.child {
            let status = self.ask(&json!({"verb": "status"})).to_string();
            for secret in SECRETS {
                assert!(!status.contains(secret), "status names {secret}");
            }
            for file in ["cmdline", "environ"] {
                let bytes = fs::read(format!("/proc/{}/{file}", child.id())).unwrap();
                let text = String::from_utf8_lossy(&bytes);
                for secret in SECRETS {
                    assert!(!text.contains(secret), "the daemon's {file} names {secret}");
                }
            }
        }
    }

    /// How many files the search read, and which of them hold a secret.
    fn secret_leaks(&self) -> (usize, Vec<String>) {
        let mut leaks = Vec::new();
        let mut files = 0;
        let mut pending = vec![self.root.clone()];
        while let Some(path) = pending.pop() {
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                for entry in fs::read_dir(&path).unwrap().flatten() {
                    pending.push(entry.path());
                }
            } else if metadata.is_file() {
                files += 1;
                let bytes = fs::read(&path).unwrap_or_default();
                let text = String::from_utf8_lossy(&bytes);
                for secret in SECRETS {
                    if text.contains(secret) {
                        leaks.push(format!("{} holds {secret}", path.display()));
                    }
                }
            }
        }
        (files, leaks)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn every_run_borrows_one_login_and_the_stop_logs_it_out() {
    let mut daemon = Daemon::start("borrow", &[]);
    for label in ["first", "second", "third"] {
        daemon.run_once(label);
    }
    let requests = daemon.requests();
    // One discovery and one login for three runs; nobody logged out yet: the daemon holds it.
    assert_eq!(daemon.server.logins_seen(), 1, "{}", daemon.log());
    assert_eq!(count(&requests, "SYNO.API.Info.query"), 1);
    assert_eq!(daemon.logouts(), 0, "a kept session is not ended");
    let log = daemon.log();
    for event in ["session_kept", "session_lent"] {
        assert!(
            log.contains(&format!("\"event\":\"{event}\"")),
            "{event}: {log}"
        );
    }
    // The search is live: a file that does hold the SID is found. Named by its real path: `var` is
    // a DSM-shaped alias, and the search does not follow links into the tree it already walks.
    let canary = fs::canonicalize(daemon.p("var/state"))
        .unwrap()
        .join("canary");
    fs::write(&canary, format!("sid={}-0", SECRETS[0])).unwrap();
    let (_, leaks) = daemon.secret_leaks();
    assert_eq!(
        leaks,
        [format!("{} holds {}", canary.display(), SECRETS[0])]
    );
    fs::remove_file(&canary).unwrap();
    daemon.assert_no_secret_anywhere();
    daemon.stop();
    assert_eq!(daemon.logouts(), 1, "the stop ended the kept session");
    assert!(
        daemon.log().contains("why=service_stop"),
        "{}",
        daemon.log()
    );
    daemon.assert_no_secret_anywhere();
}

#[test]
fn a_refused_lent_session_costs_one_fresh_login_named_in_the_log() {
    let mut daemon = Daemon::start("refused", &[]);
    daemon.run_once("first");
    assert_eq!(daemon.server.logins_seen(), 1);
    // DSM ends the session while the daemon holds it: the next request on it is refused.
    daemon.server.expire_session_after(0);
    daemon.run_once("refused");
    assert_eq!(
        daemon.server.logins_seen(),
        2,
        "exactly one fresh login: {}",
        daemon.run_stderr()
    );
    assert!(
        daemon.run_stderr().contains("session.borrowed_rejected"),
        "{}",
        daemon.run_stderr()
    );
    assert!(
        daemon.log().contains("\"event\":\"session_ttl_shortened\"")
            || daemon.log().contains("\"event\":\"session_rejected\""),
        "{}",
        daemon.log()
    );
    // The fresh session came back and serves the next run.
    daemon.run_once("after");
    assert_eq!(daemon.server.logins_seen(), 2);
    assert_eq!(daemon.logouts(), 0, "DSM ended the refused one itself");
    daemon.assert_no_secret_anywhere();
    daemon.stop();
    assert_eq!(daemon.logouts(), 1);
}

#[test]
fn a_session_idle_past_its_ttl_is_logged_out_and_the_next_run_logs_in() {
    // Debug builds read the TTL override; this suite always runs one.
    let mut daemon = Daemon::start("ttl", &[("SDSYNC_TEST_SESSION_TTL_SECONDS", "1")]);
    daemon.run_once("first");
    assert_eq!(daemon.logouts(), 0);
    thread::sleep(Duration::from_millis(1_500));
    // A wake makes the control thread turn now rather than at its 30-second cap.
    assert_eq!(daemon.ask(&json!({"verb": "wake"}))["ok"], true);
    daemon.wait_for(
        "the lapsed session's logout",
        Duration::from_secs(20),
        |daemon| daemon.logouts() == 1,
    );
    // The logout's note reaches the log on the next turn.
    assert_eq!(daemon.ask(&json!({"verb": "wake"}))["ok"], true);
    daemon.wait_for("the expiry note", Duration::from_secs(10), |daemon| {
        daemon.log().contains("why=ttl_expired")
    });
    daemon.run_once("second");
    assert_eq!(
        daemon.server.logins_seen(),
        2,
        "nothing fresh was left to lend"
    );
    daemon.assert_no_secret_anywhere();
    daemon.stop();
    assert_eq!(daemon.logouts(), 2);
}

#[test]
fn a_changed_password_file_ends_the_reuse() {
    let mut daemon = Daemon::start("binding", &[]);
    daemon.run_once("first");
    // The same password, rewritten: the file's identity changed, so the binding did.
    thread::sleep(Duration::from_millis(20));
    let password = daemon.p("home/secrets/alpha.password");
    let replacement = daemon.p("home/secrets/alpha.password.new");
    fs::write(&replacement, PASSWORD).unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&replacement, &password).unwrap();
    daemon.run_once("rebound");
    assert_eq!(daemon.server.logins_seen(), 2, "{}", daemon.run_stderr());
    assert!(
        daemon.run_stderr().contains("session.handover_unused"),
        "{}",
        daemon.run_stderr()
    );
    assert!(
        daemon.log().contains("borrowed=ignored"),
        "the run said it did not use what it was lent: {}",
        daemon.log()
    );
    // The mock issues one SID per session generation, so the fresh login answered the very SID
    // that was lent and nothing is left to end; that the daemon ends an unused session with a
    // different SID is session.rs's unit test to prove.
    daemon.assert_no_secret_anywhere();
    daemon.stop();
    assert_eq!(daemon.logouts(), 1);
}
