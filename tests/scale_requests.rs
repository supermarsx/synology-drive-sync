//! Scale regression for "reconciliation takes ages": request counts, never wall time.
//!
//! The in-tree mock answers in microseconds and skews timings 6-50x against a NAS (scale-harness
//! lesson), but every request it counts is one a NAS would serve, and every `MD5.start` is a
//! whole-file read on the NAS. A no-change content run used to cost two requests per file and a
//! run that changed two files twice that; with the verified-pair index and the SC-3 carry, both
//! cost about the listing. 3,000 files in 173 folders; the 100k measurement stays a manual
//! benchmark (perf-design §1, §5).
#![cfg(unix)]

#[allow(dead_code)]
mod support;

use std::fs;
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::{CapturedRequest, MockFileStation};

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const ROOT: &str = "/team/scale";
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

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("scale-environment");
    fs::create_dir_all(environment.child("home")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command
        .args(arguments)
        .env("HOME", environment.child("home"))
        .env("XDG_CONFIG_HOME", environment.child("home"));
    for (variable, _) in std::env::vars_os() {
        if variable.to_string_lossy().starts_with("SDSYNC_") {
            command.env_remove(variable);
        }
    }
    command.output().expect("run synology-drive-sync")
}

#[test]
fn no_change_and_two_change_runs_cost_about_the_listing() {
    let fixture = TestDir::new("scale");
    let source = fixture.child("source");
    let index = fixture.child("index");
    let server = MockFileStation::start();
    server.add_directory(ROOT);
    for folder in 0..FOLDERS {
        fs::create_dir_all(source.join(format!("folder-{folder:03}"))).unwrap();
        server.add_directory(&format!("{ROOT}/folder-{folder:03}"));
    }
    for file in 0..FILES {
        fs::write(source.join(name(file)), content(file)).unwrap();
        server.add_file(
            &format!("{ROOT}/{}", name(file)),
            &content(file),
            1_700_000_000,
        );
    }
    let password = fixture.write("password", PASSWORD);
    // Out of the racy window, so the first run's evidence can be served.
    std::thread::sleep(Duration::from_millis(2_200));
    let sync = |label: &str| -> (Value, Vec<CapturedRequest>) {
        let before = server.requests().len();
        let output = run(&[
            "--quiet",
            "--output",
            "json",
            "sync",
            source.to_str().unwrap(),
            ROOT,
            "--url",
            server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            password.to_str().unwrap(),
            "--no-vault",
            "--allow-http",
            "--jobs",
            "1",
            "--pair-index",
            index.to_str().unwrap(),
        ]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = server.requests()[before..].to_vec();
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        let mut others = std::collections::BTreeMap::new();
        for request in &requests {
            let operation = request.operation();
            if operation != "SYNO.FileStation.List.list"
                && !operation.starts_with("SYNO.FileStation.MD5")
            {
                *others.entry(operation).or_insert(0) += 1;
            }
        }
        println!("{label}: other requests {others:?}");
        println!(
            "{label}: {} requests, {} List.list, {} MD5.start",
            requests.len(),
            count(&requests, "SYNO.FileStation.List.list"),
            count(&requests, "SYNO.FileStation.MD5.start")
        );
        (document, requests)
    };

    // Run 1, cold: every pair proven live, once, and stored.
    let (_, cold) = sync("run 1 (cold)");
    assert_eq!(count(&cold, "SYNO.FileStation.MD5.start"), FILES);
    let database = fs::read_dir(&index)
        .unwrap()
        .flatten()
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "redb")
        })
        .expect("the index file");
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        database.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Run 2, no change: the listing, the canary, and the session.
    let (document, warm) = sync("run 2 (no change)");
    let lists = count(&warm, "SYNO.FileStation.List.list");
    assert_eq!(lists, FOLDERS + 1, "one listing per folder");
    assert!(count(&warm, "SYNO.FileStation.MD5.start") <= K);
    // Besides the listings and the canary's start and status: API discovery, login, the
    // destination's write check and four `getinfo` of the root, File Station's host for the
    // index binding, and logout -- nine, whatever the tree's size.
    let session = 9;
    assert!(
        warm.len() <= lists + session + 2 * K,
        "{} requests for {lists} listings",
        warm.len()
    );
    assert_eq!(document["plan"]["pair_index"]["served"], FILES - K);

    // Run 3, two files missing on the NAS: two uploads, and a reconciliation that verifies the
    // two touched pairs plus the canary, not the tree.
    for file in [17, 2_222] {
        server.remove_file(&format!("{ROOT}/{}", name(file)));
    }
    let (document, changed) = sync("run 3 (two missing)");
    assert_eq!(document["result"]["uploaded"], 2);
    let reconciliation = &document["result"]["reconciliation"];
    assert!(reconciliation["verified_live"].as_u64().unwrap() <= (2 + K) as u64);
    assert!(
        count(&changed, "SYNO.FileStation.MD5.start") <= K + 2 + (2 + K),
        "plan canary, two upload verifications, reconciliation"
    );
    assert!(
        changed.len() <= 2 * lists + 2 * session + 11 + 4 * K + 4,
        "{} requests for 2 x {lists} listings",
        changed.len()
    );
}
