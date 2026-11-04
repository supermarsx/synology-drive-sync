//! SC-3: post-run reconciliation verifies live only what the run touched, plus a canary, and
//! carries the plan pass's digests for everything else -- while still catching what
//! reconciliation exists to catch.
//!
//! Counted in requests, never in wall time: the in-tree mock skews timings (scale-harness
//! lesson), and the request count is the cost a real NAS pays.
//!
//! Carrying needs the local status-change time and filesystem identity, which only Unix exposes
//! through a path stat, so the counting tests are Unix-only. Elsewhere reconciliation stays fully
//! live, which `windows_reconciliation_stays_fully_live` pins.

#[allow(dead_code)]
mod support;

use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::{CapturedRequest, MockFileStation};

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const ROOT: &str = "/team/carry";

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("carry-environment");
    for directory in ["home", "appdata", "local-appdata", "xdg"] {
        fs::create_dir_all(environment.child(directory)).expect("create environment directory");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command
        .args(arguments)
        .env("HOME", environment.child("home"))
        .env("USERPROFILE", environment.child("home"))
        .env("APPDATA", environment.child("appdata"))
        .env("LOCALAPPDATA", environment.child("local-appdata"))
        .env("XDG_CONFIG_HOME", environment.child("xdg"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SDSYNC_") {
            command.env_remove(name);
        }
    }
    // A pair the plan read within the racy window of its last change is not carried (D2). The
    // fixtures settle for 2.2 s, not the minute the writeback bound asks of a real tree; the
    // debug-only override shortens the windows to match.
    command.env("SDSYNC_TEST_PAIR_INDEX_RACY_WINDOW_MS", "1000");
    command.output().expect("run synology-drive-sync")
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is one JSON document ({error}); stderr: {}; stdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn count(requests: &[CapturedRequest], operation: &str) -> usize {
    requests
        .iter()
        .filter(|request| request.operation() == operation)
        .count()
}

/// The fixture: `files` files of 32 bytes in ten folders, identical on both sides, except the
/// `missing` first files, which the NAS does not have.
struct Tree {
    _fixture: TestDir,
    source: std::path::PathBuf,
    password: std::path::PathBuf,
    server: MockFileStation,
}

fn name(index: usize) -> String {
    format!("d{}/f{index:05}.bin", index % 10)
}

fn content(index: usize) -> Vec<u8> {
    format!("{index:032}").into_bytes()
}

fn tree(files: usize, missing: usize) -> Tree {
    let fixture = TestDir::new("carry");
    let source = fixture.child("source");
    let server = MockFileStation::start();
    server.add_directory(ROOT);
    for folder in 0..10 {
        fs::create_dir_all(source.join(format!("d{folder}"))).expect("create source folder");
        server.add_directory(&format!("{ROOT}/d{folder}"));
    }
    for index in 0..files {
        fs::write(source.join(name(index)), content(index)).expect("write source file");
        if index >= missing {
            server.add_file(
                &format!("{ROOT}/{}", name(index)),
                &content(index),
                1_700_000_000,
            );
        }
    }
    let password = fixture.write("password", PASSWORD);
    // Outside the (shortened) racy window before the plan reads them, or nothing is carried.
    std::thread::sleep(std::time::Duration::from_millis(2_200));
    Tree {
        _fixture: fixture,
        source,
        password,
        server,
    }
}

impl Tree {
    fn sync(&self, extra: &[&str]) -> Output {
        let mut arguments = vec![
            "--quiet",
            "--output",
            "json",
            "sync",
            self.source.to_str().expect("UTF-8 source"),
            ROOT,
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            self.password.to_str().expect("UTF-8 password path"),
            "--no-vault",
            "--allow-http",
            "--compare",
            "content",
            "--jobs",
            "1",
        ];
        arguments.extend_from_slice(extra);
        run(&arguments)
    }
}

#[cfg(unix)]
#[test]
fn reconciliation_verifies_the_touched_pairs_and_the_canary_and_carries_the_rest() {
    let tree = tree(1_000, 2);
    let output = tree.sync(&[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = stdout_json(&output);
    assert_eq!(result["result"]["uploaded"], 2);
    let requests = tree.server.requests();
    // Plan: 998 comparison pairs. Execution: one verification digest per upload. Reconciliation:
    // the 2 uploads (now pairs, touched) plus the 25-pair canary -- not all 1,000 again.
    assert_eq!(
        count(&requests, "SYNO.FileStation.MD5.start"),
        998 + 2 + (2 + 25)
    );
    let reconciliation = &result["result"]["reconciliation"];
    assert_eq!(reconciliation["mode"], "carry");
    assert_eq!(reconciliation["carried"], 998 - 25);
    assert_eq!(reconciliation["verified_live"], 2 + 25);
    assert_eq!(reconciliation["canary_checked"], 25);
    assert_eq!(reconciliation["canary_inconclusive"], 0);
    assert_eq!(reconciliation["canary_mismatch"], Value::Null);
    assert_eq!(reconciliation["fell_back_to_full"], false);
}

#[test]
fn reconcile_full_re_verifies_every_pair_live() {
    let tree = tree(200, 2);
    let output = tree.sync(&["--reconcile", "full"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = stdout_json(&output);
    let requests = tree.server.requests();
    assert_eq!(
        count(&requests, "SYNO.FileStation.MD5.start"),
        198 + 2 + 200
    );
    assert_eq!(result["result"]["reconciliation"]["mode"], "full");
    assert_eq!(result["result"]["reconciliation"]["carried"], 0);
}

#[cfg(not(unix))]
#[test]
fn windows_reconciliation_stays_fully_live() {
    // No status-change time or file identity from a path stat: nothing is carried.
    let tree = tree(50, 2);
    let output = tree.sync(&[]);
    assert_eq!(output.status.code(), Some(0));
    let result = stdout_json(&output);
    assert_eq!(result["result"]["reconciliation"]["carried"], 0);
    assert_eq!(
        count(&tree.server.requests(), "SYNO.FileStation.MD5.start"),
        48 + 2 + 50
    );
}

/// A third party rewrites an untouched NAS file during the run, keeping its size and mtime. Its
/// change time moves, so reconciliation verifies it live and the run does not report a
/// convergence it does not have.
#[cfg(unix)]
#[test]
fn a_sibling_rewritten_on_the_nas_during_the_run_is_verified_live_and_caught() {
    let tree = tree(100, 1);
    let sibling = format!("{ROOT}/{}", name(50));
    let mut altered = content(50);
    altered[0] = b'X';
    tree.server
        .edit_file_on_next("SYNO.FileStation.Upload.upload", &sibling, &altered, false);
    let output = tree.sync(&[]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "a NAS file that no longer matches its source must not reconcile"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("reconciliation") || stderr.contains("converge"),
        "named as a reconciliation failure: {stderr}"
    );
    let starts = tree
        .server
        .requests()
        .into_iter()
        .filter(|request| {
            request.operation() == "SYNO.FileStation.MD5.start"
                && request
                    .fields
                    .get("file_path")
                    .is_some_and(|path| path.contains("f00050"))
        })
        .count();
    assert_eq!(starts, 2, "planned once and re-verified live once");
}

/// A canary read live disagrees with the plan pass while every fact matched: the pass
/// re-verifies every carried pair live, and the disagreement is named in the result.
#[cfg(unix)]
#[test]
fn a_canary_disagreement_falls_back_to_a_full_live_pass_and_is_named() {
    // 20 files, 1 uploaded: 19 eligible pairs. A canary of 18 at stride 1 draws the first 18 and
    // serves the last one, so the fallback has a carried pair to re-verify.
    let tree = tree(20, 1);
    let target = format!("{ROOT}/{}", name(1));
    let mut altered = content(1);
    altered[0] = b'X';
    tree.server
        .edit_file_on_next("SYNO.FileStation.Upload.upload", &target, &altered, true);
    let output = tree.sync(&["--reconcile-canary", "18"]);
    // The NAS really does hold different bytes for f00001 now, so the run cannot reconcile.
    assert_ne!(output.status.code(), Some(0));
    let result = stdout_json(&output);
    let reconciliation = &result["result"]["reconciliation"];
    assert_eq!(reconciliation["fell_back_to_full"], true, "{result}");
    assert_eq!(reconciliation["carried"], 0);
    assert_eq!(
        reconciliation["canary_mismatch"]["code"],
        "reconcile-carry-canary-mismatch"
    );
    assert_eq!(reconciliation["canary_mismatch"]["path"], name(1));
    // Plan 19 + upload 1 + reconciliation (1 touched + 18 canaries + 1 fallback).
    assert_eq!(
        count(&tree.server.requests(), "SYNO.FileStation.MD5.start"),
        19 + 1 + 1 + 18 + 1
    );
}
