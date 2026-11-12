//! fix8 R5: the warm path as the package runs it.
//!
//! `tests/scale_requests.rs` proves the verified-pair index short-circuits a no-change run, but it
//! drives the command line with `--pair-index` by hand. The package's daemon builds its own core
//! argument list (`control.rs`, `core_args`) against the manager-rendered configuration, and only
//! an argv unit test pinned that it passes the index directory. Here the real daemon runs the real
//! core against a DSM-shaped tree (symlinked roots, a `[profiles.<name>]` table written by the
//! shipped manager) and the in-tree File Station mock, twice, and the second run must not read
//! every file again on either side.
//!
//! The core is reached through a two-line wrapper that records the argument list the daemon chose
//! and the core's wall time, and sets only the debug build's racy-window override (the daemon
//! passes an allow-listed environment, so the variable cannot come from the test). The arguments
//! are the daemon's, untouched.

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
/// Where `configure_profile` points profile `alpha`.
const REMOTE: &str = "/home/Drive/alpha";
const FILES: usize = 3_000;
const FOLDERS: usize = 172;
/// The default canary.
const K: usize = 25;

fn name(index: usize) -> String {
    format!("folder-{:03}/file-{index:05}.dat", index % FOLDERS)
}

fn content(index: usize) -> Vec<u8> {
    format!("{index:064}").into_bytes()
}

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
}

impl Daemon {
    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn ask(&self, request: &Value) -> Value {
        let mut stream = UnixStream::connect(self.p("var/run/ctl.sock")).expect("ctl socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap_or_else(|_| panic!("reply to {request}: {reply}"))
    }

    fn stderr(&self) -> String {
        fs::read_to_string(self.p("daemon.stderr")).unwrap_or_default()
    }

    fn state(&self, rel: &str) -> String {
        fs::read_to_string(self.p("var/state").join(rel)).unwrap_or_default()
    }

    /// Run profile `alpha` once and wait for its `run_finished` event.
    fn run_once(&self) -> Value {
        let reply = self.ask(&json!({"verb": "run", "scope": "alpha"}));
        assert_eq!(reply["ok"], true, "{reply}");
        let id = reply["id"].clone();
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let status = self.ask(&json!({"verb": "status"}));
            if let Some(finished) = status["events"].as_array().and_then(|events| {
                events
                    .iter()
                    .find(|event| event["event"] == "run_finished" && event["id"] == id)
            }) {
                return finished.clone();
            }
            assert!(
                Instant::now() < deadline,
                "the run never finished: {status}\n{}",
                self.stderr()
            );
            thread::sleep(Duration::from_millis(20));
        }
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

/// The value of `key=` in a `key=value` state record.
fn field<'a>(record: &'a str, key: &str) -> Option<&'a str> {
    record
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

#[test]
fn a_second_unchanged_daemon_run_reads_no_file_on_either_side() {
    let root = std::env::temp_dir().join(format!("sdsyncd-warm-{}", std::process::id()));
    let dsm = DsmPackage::build(&root, Path::new(CORE));
    let server = MockFileStation::start();
    for folder in ["/home", "/home/Drive", REMOTE] {
        server.add_directory(folder);
    }
    dsm.configure_profile(
        "alpha",
        &[("--url", server.base_url()), ("--username", "e2e-user")],
    );
    // The mock's account, under the name the manager gave the profile's password file.
    fs::write(
        root.join("home/secrets/alpha.password"),
        b"correct horse battery staple\n",
    )
    .unwrap();
    let source = dsm.source_dir("alpha");
    for folder in 0..FOLDERS {
        fs::create_dir_all(source.join(format!("folder-{folder:03}"))).unwrap();
        server.add_directory(&format!("{REMOTE}/folder-{folder:03}"));
    }
    for file in 0..FILES {
        fs::write(source.join(name(file)), content(file)).unwrap();
        server.add_file(
            &format!("{REMOTE}/{}", name(file)),
            &content(file),
            1_700_000_000,
        );
    }
    let argv_log = root.join("core-argv.log");
    let wrapper = script(
        &root.join("core-wrapper"),
        &format!(
            "printf '%s\\n' \"$*\" >> {log}\n\
             record=; previous=\n\
             for argument in \"$@\"; do [ \"$previous\" = --result-record ] && record=$argument; previous=$argument; done\n\
             start=$(date +%s%N)\n\
             SDSYNC_TEST_PAIR_INDEX_RACY_WINDOW_MS=1000 {CORE} \"$@\"\n\
             rc=$?\n\
             echo \"core_ns=$(( $(date +%s%N) - start )) exit=$rc\" >> {log}\n\
             cp \"$record\" {last}\n\
             exit $rc",
            log = argv_log.display(),
            last = root.join("last-result.ndjson").display()
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
    let daemon = Daemon {
        root: root.clone(),
        child: Some(child),
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
    // Out of the (shortened) racy window before the first run reads them.
    thread::sleep(Duration::from_millis(2_200));

    let mut measured = Vec::new();
    for label in ["cold", "warm", "warm again"] {
        let before = server.requests().len();
        let started = Instant::now();
        let finished = daemon.run_once();
        let wall = started.elapsed();
        let requests = server.requests()[before..].to_vec();
        assert_eq!(
            finished["exit"],
            0,
            "{label}: {finished}\n{}\n{}",
            daemon.state("run.state"),
            fs::read_to_string(daemon.p("var/log/run-stderr.log")).unwrap_or_default()
        );
        let profile = daemon.state("profiles/alpha.state");
        let hits: usize = field(&profile, "index_hits")
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| {
                panic!(
                    "{label}: no index line: {profile}\nargv: {}\nresult: {}\ncontroller.log: {}",
                    fs::read_to_string(&argv_log).unwrap_or_default(),
                    fs::read_to_string(root.join("last-result.ndjson")).unwrap_or_default(),
                    fs::read_to_string(daemon.p("var/log/controller.log")).unwrap_or_default()
                )
            });
        let misses: usize = field(&profile, "index_misses")
            .and_then(|value| value.parse().ok())
            .unwrap();
        let core_ms = fs::read_to_string(&argv_log)
            .unwrap()
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("core_ns="))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .map(|ns| ns / 1_000_000)
            .unwrap();
        let mut others = std::collections::BTreeMap::new();
        for request in &requests {
            let operation = request.operation();
            if operation != "SYNO.FileStation.List.list"
                && !operation.starts_with("SYNO.FileStation.MD5")
            {
                *others.entry(operation).or_insert(0) += 1;
            }
        }
        println!(
            "{label}: {} requests ({} List.list, {} MD5.start, {} MD5.status, other {others:?}), \
             index {} ({} served, {} live), core {core_ms} ms, daemon run {} ms",
            requests.len(),
            count(&requests, "SYNO.FileStation.List.list"),
            count(&requests, "SYNO.FileStation.MD5.start"),
            count(&requests, "SYNO.FileStation.MD5.status"),
            index_state(&profile),
            hits,
            misses,
            wall.as_millis()
        );
        measured.push((requests, hits, misses));
    }

    // The daemon chose the index: the argument list it built names the profile's directory.
    let argv = fs::read_to_string(&argv_log).unwrap();
    let first = argv.lines().next().unwrap();
    assert!(
        first.contains(" sync --profile alpha ")
            && first.contains(&format!(
                "--pair-index {}",
                root.join("var/state/cache/pairs").display()
            )),
        "{first}"
    );

    let (cold, cold_hits, cold_misses) = &measured[0];
    assert_eq!(count(cold, "SYNO.FileStation.MD5.start"), FILES);
    assert_eq!((*cold_hits, *cold_misses), (0, FILES));
    for (label, (warm, hits, misses)) in [("warm", &measured[1]), ("warm again", &measured[2])] {
        let lists = count(warm, "SYNO.FileStation.List.list");
        assert_eq!(lists, FOLDERS + 1, "{label}: one listing per folder");
        // No per-file hash on either side: every pair but the canary is served from the index,
        // and only the canary costs an MD5 task.
        assert_eq!(*hits, FILES - K, "{label}");
        assert_eq!(*misses, K, "{label}");
        assert!(
            count(warm, "SYNO.FileStation.MD5.start") <= K,
            "{label}: {} MD5 tasks",
            count(warm, "SYNO.FileStation.MD5.start")
        );
        // Discovery, login, the destination's write check, a `getinfo` per folder of the
        // destination path (six for `/home/Drive/alpha`), the index binding's File Station host
        // and logout: eleven, whatever the tree's size.
        let session = warm
            .iter()
            .filter(|request| {
                let operation = request.operation();
                operation != "SYNO.FileStation.List.list"
                    && !operation.starts_with("SYNO.FileStation.MD5")
            })
            .count();
        assert!(session <= 11, "{label}: {session} session requests");
        assert!(
            warm.len() <= lists + session + 2 * K,
            "{label}: {} requests for {lists} listings",
            warm.len()
        );
    }
}

fn index_state(profile: &str) -> &str {
    field(profile, "index_state").unwrap_or("?")
}
