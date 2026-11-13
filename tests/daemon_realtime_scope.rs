//! fix9 integration: a realtime routine's run is scoped to the folders that changed, end to end.
//!
//! The real daemon (a realtime routine saved through the shipped manager, watched by inotify)
//! runs the real core against a DSM-shaped tree and the in-tree File Station mock. Nothing is
//! captured by a fake core: a wrapper records the argument list and the result record and then
//! `exec`s nothing else than the real binary. Two files are made in two separate subfolders; the
//! dispatched run must carry one `--scope-shallow` per folder and the two canaries, must succeed,
//! must write both files, and must not have listed anything else on the NAS. The session the
//! catch-up run opened is lent to it, so the whole sequence is one login.

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
use dsm_package::{DsmPackage, output_retrying};

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
const FOLDERS: [&str; 3] = ["a", "b", "c"];

fn script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    testkit::settle_for_exec(path);
    path.to_path_buf()
}

fn folder_of(request: &CapturedRequest) -> Option<String> {
    request
        .fields
        .get("folder_path")
        .map(|value| value.trim_matches('"').to_owned())
}

struct Daemon {
    root: PathBuf,
    child: Option<Child>,
    server: MockFileStation,
    source: PathBuf,
}

impl Daemon {
    fn start(label: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("sdsyncd-realtime-{label}-{}", std::process::id()));
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
        // Three folders, each holding two files that are already on the NAS, and one at the root.
        let mut files = vec!["top.dat".to_owned()];
        for folder in FOLDERS {
            fs::create_dir_all(source.join(folder)).unwrap();
            server.add_directory(&format!("{REMOTE}/{folder}"));
            for index in 0..2 {
                files.push(format!("{folder}/file-{index}.dat"));
            }
        }
        for name in &files {
            let content = format!("{name:0>64}");
            fs::write(source.join(name), &content).unwrap();
            server.add_file(
                &format!("{REMOTE}/{name}"),
                content.as_bytes(),
                1_700_000_000,
            );
        }
        // The realtime routine, saved by the shipped manager as the dashboard saves it.
        let mut routine = dsm.manager();
        routine.args([
            "configure-routine",
            "--profile",
            "alpha",
            "--mode",
            "realtime",
            "--debounce-seconds",
            "1",
            "--poll-seconds",
            "5",
        ]);
        let output = output_retrying(&mut routine).expect("run the manager");
        assert!(
            output.status.success(),
            "configure-routine: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The wrapper records what the real core was given and what its result record said.
        let log = root.join("core-runs.log");
        let wrapper = script(
            &root.join("core-wrapper"),
            &format!(
                "printf 'argv: %s\\n' \"$*\" >> {log}\n\
                 record=; previous=\n\
                 for argument in \"$@\"; do [ \"$previous\" = --result-record ] && record=$argument; previous=$argument; done\n\
                 {CORE} \"$@\"\n\
                 rc=$?\n\
                 printf 'exit: %s\\n' \"$rc\" >> {log}\n\
                 [ -n \"$record\" ] && [ -f \"$record\" ] && {{ printf 'record: ' >> {log}; cat \"$record\" >> {log}; }}\n\
                 exit $rc",
                log = log.display()
            ),
        );
        let manager = script(&root.join("manager"), "exit 0");
        let stderr = fs::File::create(root.join("daemon.stderr")).unwrap();
        let child = Command::new(DAEMON)
            .arg("--daemon")
            .args(["--home".as_ref(), root.join("home").as_os_str()])
            .args(["--var".as_ref(), root.join("var").as_os_str()])
            .args(["--core".as_ref(), wrapper.as_os_str()])
            .args(["--manager".as_ref(), manager.as_os_str()])
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let daemon = Self {
            root: root.clone(),
            child: Some(child),
            server,
            source,
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

    fn controller_log(&self) -> String {
        fs::read_to_string(self.p("var/log/controller.log")).unwrap_or_default()
    }

    fn run_stderr(&self) -> String {
        fs::read_to_string(self.p("var/log/run-stderr.log")).unwrap_or_default()
    }

    fn core_runs(&self) -> String {
        fs::read_to_string(self.p("core-runs.log")).unwrap_or_default()
    }

    /// The argument lists of the core runs so far, and how many have ended.
    fn argvs(&self) -> Vec<String> {
        self.core_runs()
            .lines()
            .filter_map(|line| line.strip_prefix("argv: ").map(str::to_owned))
            .collect()
    }

    fn finished_runs(&self) -> usize {
        self.core_runs()
            .lines()
            .filter(|line| line.starts_with("exit: "))
            .count()
    }

    fn wait_for(&self, what: &str, within: Duration, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + within;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "{what}\ncore runs: {}\ncontroller.log: {}\nrun-stderr: {}\ndaemon: {}",
                self.core_runs(),
                self.controller_log(),
                self.run_stderr(),
                self.stderr()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The routine's status entry: watched natively, with nothing pending and no run active.
    fn quiet(&self) -> bool {
        let status = self.ask(&json!({"verb": "status"}));
        let routine = &status["routines"][0];
        status["active"].is_null()
            && routine["mode"] == "realtime"
            && routine["backend"] == "inotify"
            && routine["pending_change"] == false
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.server.requests()
    }

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

/// The `--scope`/`--scope-shallow` elements of one argument list, in order, as `(flag, path)`.
fn scope_elements(argv: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = argv.split(' ').collect();
    words
        .windows(2)
        .filter(|pair| matches!(pair[0], "--scope" | "--scope-shallow"))
        .map(|pair| (pair[0].to_owned(), pair[1].to_owned()))
        .collect()
}

fn flag_value<'a>(argv: &'a str, flag: &str) -> Option<&'a str> {
    let words: Vec<&str> = argv.split(' ').collect();
    words
        .windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1])
}

#[test]
fn two_edits_in_two_folders_run_as_two_shallow_scopes_with_two_canaries() {
    let mut daemon = Daemon::start("scope");

    // The routine has never succeeded, so arming it syncs the whole tree once: no scope, and the
    // default canary. It opens the DSM session the daemon then keeps.
    daemon.wait_for("the catch-up run", Duration::from_secs(120), |daemon| {
        daemon.finished_runs() == 1 && daemon.quiet()
    });
    let first = daemon.argvs().remove(0);
    assert!(
        scope_elements(&first).is_empty(),
        "a whole-tree catch-up carries no scope: {first}"
    );
    assert!(
        flag_value(&first, "--reconcile-canary").is_none(),
        "a whole-tree run keeps the core's default canary: {first}"
    );
    assert_eq!(
        daemon.server.logins_seen(),
        1,
        "{}",
        daemon.controller_log()
    );
    // The routine ran with the debounce the manager saved, and the tree is quiet.
    let before = daemon.requests().len();

    // Two edits, in two folders that are not each other's ancestors, with `b` untouched.
    let (a, c) = ("a/new.dat", "c/new.dat");
    fs::write(daemon.source.join(a), "edit in a").unwrap();
    fs::write(daemon.source.join(c), "edit in c").unwrap();

    daemon.wait_for("the scoped run", Duration::from_secs(120), |daemon| {
        daemon.finished_runs() == 2
    });
    let argvs = daemon.argvs();
    assert_eq!(argvs.len(), 2, "exactly one more run: {argvs:?}");
    let scoped = &argvs[1];

    // One shallow element per changed folder, nothing recursive, nothing for `b` or the root.
    assert_eq!(
        scope_elements(scoped),
        [
            ("--scope-shallow".to_owned(), "a".to_owned()),
            ("--scope-shallow".to_owned(), "c".to_owned()),
        ],
        "{scoped}"
    );
    // Two canaries on the realtime run, for the pair index and for reconciliation alike.
    assert_eq!(
        flag_value(scoped, "--pair-index-canary"),
        Some("2"),
        "{scoped}"
    );
    assert_eq!(
        flag_value(scoped, "--reconcile-canary"),
        Some("2"),
        "{scoped}"
    );
    // The session never travels in the argument list; a lent one is named by the flag alone.
    assert!(scoped.contains("--session-stdin"), "{scoped}");

    // The run succeeded, and both edits landed on the NAS.
    let record = daemon
        .core_runs()
        .lines()
        .filter_map(|line| line.strip_prefix("record: "))
        .next_back()
        .map(|line| serde_json::from_str::<Value>(line.trim()).expect("the result record"))
        .expect("the scoped run wrote a result record");
    assert_eq!(record["status"], "complete", "{record}");
    assert_eq!(record["result"]["uploaded"], 2, "{record}");
    assert_eq!(
        daemon.server.file_contents(&format!("{REMOTE}/{a}")),
        Some(b"edit in a".to_vec())
    );
    assert_eq!(
        daemon.server.file_contents(&format!("{REMOTE}/{c}")),
        Some(b"edit in c".to_vec())
    );
    // At most two pairs were re-verified as the canary, and the request-floor counters ride on
    // the record the daemon reads.
    assert!(
        record["result"]["reconciliation"]["canary_checked"]
            .as_u64()
            .is_some_and(|checked| checked <= 2),
        "{record}"
    );
    assert_eq!(
        record["result"]["request_floor"]["getinfo_batch_fallbacks"], 0,
        "{record}"
    );

    // Nothing outside the two folders was listed: not `b`, not the root, not anything below.
    let mut listed: Vec<String> = daemon.requests()[before..]
        .iter()
        .filter(|request| request.operation() == "SYNO.FileStation.List.list")
        .filter_map(folder_of)
        .collect();
    listed.sort();
    listed.dedup();
    assert_eq!(
        listed,
        [format!("{REMOTE}/a"), format!("{REMOTE}/c")],
        "{listed:?}"
    );
    // Nor was anything outside them written: every mutation names a path in `a` or `c`.
    let mut writes = 0;
    for request in &daemon.requests()[before..] {
        if matches!(
            request.operation().as_str(),
            "SYNO.FileStation.Upload.upload"
                | "SYNO.FileStation.CopyMove.start"
                | "SYNO.FileStation.Delete.start"
                | "SYNO.FileStation.CreateFolder.create"
        ) {
            writes += 1;
            let fields = format!("{:?}", request.fields);
            assert!(
                fields.contains(&format!("{REMOTE}/a")) || fields.contains(&format!("{REMOTE}/c")),
                "{} wrote outside the scope: {request:?}",
                request.operation()
            );
            assert!(
                !fields.contains(&format!("{REMOTE}/b")) && !fields.contains("top.dat"),
                "{} reached `b` or the root: {request:?}",
                request.operation()
            );
        }
    }
    assert!(writes >= 2, "the two edits were written: {writes}");

    // The session the catch-up run opened was lent to this one: still one login, named by the
    // core and by the daemon. If it had not been, the run would have logged in itself.
    assert_eq!(
        daemon.server.logins_seen(),
        1,
        "the realtime run borrowed the daemon's session: {}",
        daemon.run_stderr()
    );
    // The daemon copies a run's stderr and notes the lend once the run has been reaped, which
    // is a moment after the wrapper's own "exit" line.
    daemon.wait_for(
        "the borrowed session named by the core and by the daemon",
        Duration::from_secs(20),
        |daemon| {
            daemon.run_stderr().contains("session.borrowed")
                && daemon
                    .controller_log()
                    .contains("\"event\":\"session_lent\"")
        },
    );
    daemon.stop();
}

/// A folder made and removed inside the quiet period is on neither side when the run starts. That
/// must not fail the run (`scope_not_found`): the set still names a folder that exists, or the
/// dispatch is skipped, and either way nothing is left failed or retried.
#[test]
fn a_folder_made_and_removed_within_the_quiet_period_fails_nothing() {
    let mut daemon = Daemon::start("transient");
    daemon.wait_for("the catch-up run", Duration::from_secs(120), |daemon| {
        daemon.finished_runs() == 1 && daemon.quiet()
    });
    // A real edit keeps the dispatch from being skipped as a change that changes nothing.
    fs::write(daemon.source.join("b/kept.dat"), "kept").unwrap();
    fs::create_dir(daemon.source.join("a/scratch")).unwrap();
    fs::remove_dir(daemon.source.join("a/scratch")).unwrap();
    daemon.wait_for("the run for the edit", Duration::from_secs(120), |daemon| {
        daemon.finished_runs() == 2
    });
    let argvs = daemon.argvs();
    let scoped = &argvs[1];
    assert!(
        !scope_elements(scoped)
            .iter()
            .all(|(_, path)| path == "a/scratch"),
        "the set names more than the folder that is gone: {scoped}"
    );
    daemon.wait_for("the run's record", Duration::from_secs(20), |daemon| {
        daemon.core_runs().contains("record: ")
    });
    let exits: Vec<String> = daemon
        .core_runs()
        .lines()
        .filter(|line| line.starts_with("exit: "))
        .map(str::to_owned)
        .collect();
    assert_eq!(exits, ["exit: 0", "exit: 0"], "{}", daemon.core_runs());
    assert_eq!(
        daemon.server.file_contents(&format!("{REMOTE}/b/kept.dat")),
        Some(b"kept".to_vec())
    );
    daemon.stop();
}

/// The same folder, alone: for a routine that cannot delete there is nothing to run for. (With the
/// removal dropped as delete-only, a run scoped to the folder would have failed with
/// `scope_not_found` and been retried.)
#[test]
fn a_folder_made_and_removed_alone_starts_no_run() {
    let mut daemon = Daemon::start("transient-alone");
    daemon.wait_for("the catch-up run", Duration::from_secs(120), |daemon| {
        daemon.finished_runs() == 1 && daemon.quiet()
    });
    fs::create_dir(daemon.source.join("a/scratch")).unwrap();
    fs::remove_dir(daemon.source.join("a/scratch")).unwrap();
    // Past the debounce (1 s) and the check that follows it.
    thread::sleep(Duration::from_secs(4));
    assert_eq!(daemon.argvs().len(), 1, "{}", daemon.core_runs());
    let status = daemon.ask(&json!({"verb": "status"}));
    let routine = &status["routines"][0];
    assert_eq!(routine["pending_change"], false, "{status}");
    assert_eq!(routine["state"], "succeeded", "{status}");
    daemon.stop();
}
