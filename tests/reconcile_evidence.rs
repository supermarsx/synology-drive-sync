//! What a run verifies live instead of taking from evidence, end to end: by the NAS clock that
//! proved a pair, and by the run's scope.
//!
//! The clock tests move the mock's clock for the whole process
//! (`MockFileStation::set_process_clock_offset`), which is why they live in a binary of their own
//! and take [`CLOCK`] in turn. The index and the carry need the local status-change time, which
//! only Unix exposes, so the file is Unix-only.
#![cfg(unix)]

#[allow(dead_code)]
mod support;

use std::fs;
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const FILES: usize = 40;
/// Read by debug builds only (`pair_index::RacyWindows::for_host`).
const RACY_WINDOW_OVERRIDE: &str = "SDSYNC_TEST_PAIR_INDEX_RACY_WINDOW_MS";

/// Held for a test's whole life by every test that moves the mock's clock.
static CLOCK: Mutex<()> = Mutex::new(());

/// The mock's clock, put back however the test ends.
struct ClockOffset;

impl ClockOffset {
    fn set(seconds: i64) -> Self {
        MockFileStation::set_process_clock_offset(seconds);
        Self
    }
}

impl Drop for ClockOffset {
    fn drop(&mut self) {
        MockFileStation::set_process_clock_offset(0);
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn name(index: usize) -> String {
    format!("folder-{}/file-{index:03}.dat", index % 4)
}

fn content(index: usize) -> Vec<u8> {
    format!("{index:064}").into_bytes()
}

struct Tree {
    _directory: TestDir,
    source: std::path::PathBuf,
    index: std::path::PathBuf,
    password: std::path::PathBuf,
    server: MockFileStation,
    root: &'static str,
}

/// `FILES` files in four folders, already on the NAS with old change times, settled locally.
fn tree(label: &str, root: &'static str) -> Tree {
    let directory = TestDir::new(label);
    let source = directory.child("source");
    let index = directory.child("index");
    let server = MockFileStation::start();
    server.add_directory(root);
    for folder in 0..4 {
        fs::create_dir_all(source.join(format!("folder-{folder}"))).unwrap();
        server.add_directory(&format!("{root}/folder-{folder}"));
    }
    for file in 0..FILES {
        fs::write(source.join(name(file)), content(file)).unwrap();
        server.add_file(
            &format!("{root}/{}", name(file)),
            &content(file),
            1_700_000_000,
        );
    }
    let password = directory.write("password", PASSWORD);
    // Out of the local racy window, so the first run's evidence is stored settled.
    std::thread::sleep(Duration::from_millis(2_200));
    Tree {
        _directory: directory,
        source,
        index,
        password,
        server,
        root,
    }
}

impl Tree {
    fn sync(&self, extra: &[&str]) -> (Value, String) {
        let environment = TestDir::new("evidence-environment");
        fs::create_dir_all(environment.child("home")).unwrap();
        let mut arguments = vec![
            "--output",
            "json",
            "sync",
            self.source.to_str().unwrap(),
            self.root,
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            self.password.to_str().unwrap(),
            "--no-vault",
            "--allow-http",
            "--jobs",
            "1",
            "--pair-index",
            self.index.to_str().unwrap(),
        ];
        arguments.extend_from_slice(extra);
        let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
        command
            .args(&arguments)
            .env("HOME", environment.child("home"))
            .env("XDG_CONFIG_HOME", environment.child("home"));
        for (variable, _) in std::env::vars_os() {
            if variable.to_string_lossy().starts_with("SDSYNC_") {
                command.env_remove(variable);
            }
        }
        command.env(RACY_WINDOW_OVERRIDE, "1000");
        let output: Output = command.output().expect("run synology-drive-sync");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.code(), Some(0), "{stderr}");
        let document = serde_json::from_slice(&output.stdout).expect("one JSON document");
        (document, stderr)
    }

    fn edit(&self, file: usize) {
        let path = self.source.join(name(file));
        let mut changed = content(file);
        changed.reverse();
        fs::write(&path, changed).unwrap();
    }
}

fn issue_codes(document: &Value) -> Vec<String> {
    document["plan"]["pair_index"]["issues"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|issue| issue["code"].as_str().map(str::to_owned))
        .collect()
}

/// A pair the index served carries the NAS clock of the run that proved it. That clock is
/// behind this run's start by however long ago that was, which only ever makes the racy rule
/// stricter, so reconciliation carries the pair instead of reading both sides of it again, and
/// nothing tells the operator to check the NAS's time.
#[test]
fn a_served_pair_proved_by_an_earlier_run_is_carried_not_verified_live() {
    let _clock = CLOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    // A NAS clock 15 s behind: inside the 20 s the index's harvest allows, so the first run's
    // proofs are stored settled, with that clock.
    let _offset = ClockOffset::set(-15);
    let tree = tree("evidence-old-clock", "/team/old-clock");
    let proved_at = now_seconds();
    let canaries = ["--pair-index-canary", "2", "--reconcile-canary", "2"];
    let (first, _) = tree.sync(&canaries);
    assert_eq!(first["plan"]["pair_index"]["stored"], FILES, "{first}");

    // The stored clock is now more than 20 s behind this host's: the bound the carry used to
    // demand of every clock, as if each had been read in this run.
    while now_seconds() <= proved_at + 6 {
        std::thread::sleep(Duration::from_millis(200));
    }
    tree.edit(7);
    let (second, stderr) = tree.sync(&canaries);
    assert_eq!(second["result"]["uploaded"], 1, "{second}");
    assert_eq!(
        second["plan"]["pair_index"]["served"],
        FILES - 1 - 2,
        "every pair but the edited one and the canary: {second}"
    );
    let reconciliation = &second["result"]["reconciliation"];
    assert_eq!(
        reconciliation["plan_clock_unbounded"], 0,
        "{reconciliation}"
    );
    assert!(
        reconciliation["verified_live"].as_u64().unwrap() <= 1 + 2,
        "the upload and the canary only: {reconciliation}"
    );
    assert!(
        reconciliation["carried"].as_u64().unwrap() >= (FILES - 1 - 2) as u64,
        "{reconciliation}"
    );
    assert!(
        !issue_codes(&second).contains(&"pair-index-plan-clock-unbounded".to_owned()),
        "{second}"
    );
    assert!(!stderr.contains("plan-clock-unbounded"), "{stderr}");
}

/// A NAS clock ahead of this host's could make any change time look settled, so no pair it proved
/// is carried: each is verified live, counted and named.
#[test]
fn a_pair_proved_by_a_nas_clock_ahead_of_this_host_is_verified_live() {
    let _clock = CLOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let _offset = ClockOffset::set(120);
    let tree = tree("evidence-ahead", "/team/ahead");
    tree.edit(3);
    let (document, stderr) = tree.sync(&["--reconcile-canary", "2"]);
    assert_eq!(document["result"]["uploaded"], 1, "{document}");
    let reconciliation = &document["result"]["reconciliation"];
    assert_eq!(reconciliation["carried"], 0, "{reconciliation}");
    assert!(
        reconciliation["plan_clock_unbounded"].as_u64().unwrap() >= (FILES - 1 - 2) as u64,
        "every untouched pair outside the canary: {reconciliation}"
    );
    assert!(
        reconciliation["verified_live"].as_u64().unwrap() >= (FILES - 1) as u64,
        "{reconciliation}"
    );
    assert!(
        issue_codes(&document).contains(&"pair-index-plan-clock-unbounded".to_owned()),
        "{document}"
    );
    assert!(
        stderr.contains("pair-index-plan-clock-unbounded"),
        "named on standard error too: {stderr}"
    );
}

/// A run scoped below the source root draws 2 canaries by default, in the plan pass and in
/// reconciliation; an explicit count still wins.
#[test]
fn a_scoped_run_draws_two_canaries_unless_told_otherwise() {
    let _clock = CLOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let tree = tree("evidence-scoped", "/team/scoped");
    let (first, _) = tree.sync(&[]);
    assert_eq!(first["plan"]["pair_index"]["stored"], FILES, "{first}");
    std::thread::sleep(Duration::from_millis(1_200));

    tree.edit(1);
    let (scoped, _) = tree.sync(&["--scope", "folder-1"]);
    assert_eq!(scoped["result"]["uploaded"], 1, "{scoped}");
    assert_eq!(
        scoped["plan"]["pair_index"]["canary_checked"], 2,
        "{scoped}"
    );
    assert_eq!(
        scoped["result"]["reconciliation"]["canary_checked"], 2,
        "{scoped}"
    );

    tree.edit(5);
    let (explicit, _) = tree.sync(&[
        "--scope",
        "folder-1",
        "--pair-index-canary",
        "5",
        "--reconcile-canary",
        "4",
    ]);
    assert_eq!(
        explicit["plan"]["pair_index"]["canary_checked"], 5,
        "{explicit}"
    );
    assert_eq!(
        explicit["result"]["reconciliation"]["canary_checked"], 4,
        "{explicit}"
    );

    // The whole tree keeps 25.
    let (whole, _) = tree.sync(&[]);
    assert_eq!(whole["plan"]["pair_index"]["canary_checked"], 25, "{whole}");
}
