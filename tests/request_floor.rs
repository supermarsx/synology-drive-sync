//! The request floor of a sync run: what a run asks a NAS before it does any work of its own.
//!
//! Request counts, never wall time, are asserted: the in-tree mock answers in microseconds. Set
//! `SDSYNC_MOCK_LATENCY_MS` (and `--nocapture`) to put a NAS-shaped round trip in every request and
//! read the wall time of each run off the output; that is how the before/after numbers of the
//! request-floor work were taken. 144 files in 54 folders, two levels deep.
#![cfg(unix)]

#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::{CapturedRequest, MockFileStation};

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const ROOT: &str = "/team/floor";
const TOP: usize = 6;
const SUB: usize = 8;
const PER_FOLDER: usize = 3;
const FILES: usize = TOP * SUB * PER_FOLDER;
/// Every folder below the root, plus the root itself.
const LISTED_FOLDERS: usize = TOP + TOP * SUB + 1;
const RACY_WINDOW_OVERRIDE: &str = "SDSYNC_TEST_PAIR_INDEX_RACY_WINDOW_MS";

fn name(file: usize) -> String {
    let folder = file / PER_FOLDER;
    format!(
        "top-{:02}/sub-{:02}/file-{file:05}.dat",
        folder / SUB,
        folder % SUB
    )
}

fn content(file: usize) -> Vec<u8> {
    format!("{file:064}").into_bytes()
}

fn tally(requests: &[CapturedRequest]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for request in requests {
        *counts.entry(request.operation()).or_insert(0) += 1;
    }
    counts
}

fn count(counts: &BTreeMap<String, usize>, operation: &str) -> usize {
    counts.get(operation).copied().unwrap_or(0)
}

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("floor-environment");
    fs::create_dir_all(environment.child("home")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command
        .args(arguments)
        .env("HOME", environment.child("home"))
        .env("XDG_CONFIG_HOME", environment.child("home"));
    for (variable, _) in std::env::vars_os() {
        let variable_name = variable.to_string_lossy();
        if variable_name.starts_with("SDSYNC_") && variable_name != "SDSYNC_MOCK_LATENCY_MS" {
            command.env_remove(variable);
        }
    }
    command.env(RACY_WINDOW_OVERRIDE, "1000");
    command.output().expect("run synology-drive-sync")
}

#[test]
fn a_run_asks_for_its_ancestors_once_and_does_not_touch_a_session_it_just_used() {
    let fixture = TestDir::new("request-floor");
    let source = fixture.child("source");
    let index = fixture.child("index");
    let server = MockFileStation::start();
    server.add_directory(ROOT);
    for top in 0..TOP {
        server.add_directory(&format!("{ROOT}/top-{top:02}"));
        for sub in 0..SUB {
            fs::create_dir_all(source.join(format!("top-{top:02}/sub-{sub:02}"))).unwrap();
            server.add_directory(&format!("{ROOT}/top-{top:02}/sub-{sub:02}"));
        }
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
    std::thread::sleep(Duration::from_millis(2_200));
    let sync = |label: &str| -> (Value, BTreeMap<String, usize>, Duration) {
        let before = server.requests().len();
        let started = Instant::now();
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
        let wall = started.elapsed();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{label}: {}
{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let requests = server.requests()[before..].to_vec();
        let counts = tally(&requests);
        println!(
            "{label}: {:.2}s, {} requests {counts:?}",
            wall.as_secs_f64(),
            requests.len()
        );
        (
            serde_json::from_slice(&output.stdout).unwrap(),
            counts,
            wall,
        )
    };

    // Cold: no index, every pair proven live on the NAS.
    let (_, cold, _) = sync("run 1 (cold)");
    assert_eq!(count(&cold, "SYNO.FileStation.MD5.start"), FILES);
    assert_eq!(count(&cold, "SYNO.FileStation.List.list"), LISTED_FOLDERS);

    // Nothing changed: the listing, the canary, and the session. The ancestors of the destination
    // are read by one request that the walk and the write check share, so the count of `getinfo`
    // does not grow with the depth of the destination.
    let (document, warm, _) = sync("run 2 (no change)");
    assert_eq!(count(&warm, "SYNO.FileStation.List.list"), LISTED_FOLDERS);
    assert_eq!(
        count(&warm, "SYNO.FileStation.List.getinfo"),
        1,
        "the destination's ancestors are one request: {warm:?}"
    );
    assert_eq!(count(&warm, "SYNO.FileStation.List.list_share"), 0);
    assert!(document["plan"]["pair_index"]["served"].as_u64().unwrap() > 0);

    // Two files missing on the NAS: two uploads. The session was answered a moment before the
    // first mutation, so it is not touched again.
    for file in [17, 101] {
        server.remove_file(&format!("{ROOT}/{}", name(file)));
    }
    let (document, changed, _) = sync("run 3 (two missing)");
    assert_eq!(document["result"]["uploaded"], 2);
    assert_eq!(
        count(&changed, "SYNO.FileStation.List.list_share"),
        0,
        "a touch right after a request is a wasted round trip"
    );
    // The plan pass and the reconciliation each name the ancestors once, and each upload costs
    // its precondition and its size check.
    assert_eq!(
        count(&changed, "SYNO.FileStation.List.getinfo"),
        2 + 2 * 2,
        "{changed:?}"
    );
}
