//! The verified-pair index end to end: a real CLI run against the File Station mock, twice.
//!
//! The first run proves every pair live and stores the evidence; the second takes it, except for
//! the canary. Each adversarial case then changes the tree the way an attack would, keeping every
//! fact it can, and asserts the change is still synced: the index can only ever suppress an
//! upload, and never one whose facts moved. Counted in requests, not wall time.
#![cfg(unix)]

#[allow(dead_code)]
mod support;

use std::fs;
use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::{CapturedRequest, MockFileStation};

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const ROOT: &str = "/team/indexed";

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("index-environment");
    for directory in ["home", "xdg"] {
        fs::create_dir_all(environment.child(directory)).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command
        .args(arguments)
        .env("HOME", environment.child("home"))
        .env("XDG_CONFIG_HOME", environment.child("xdg"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SDSYNC_") {
            command.env_remove(name);
        }
    }
    command.output().expect("run synology-drive-sync")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "one JSON document ({error}); stderr: {}; stdout: {}",
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

fn name(index: usize) -> String {
    format!("d{}/f{index:04}.bin", index % 6)
}

fn content(index: usize) -> Vec<u8> {
    format!("{index:040}").into_bytes()
}

struct Tree {
    fixture: TestDir,
    source: PathBuf,
    index: PathBuf,
    password: PathBuf,
    server: MockFileStation,
}

/// `files` identical pairs, old enough on both sides to be outside the racy window.
fn tree(files: usize) -> Tree {
    let fixture = TestDir::new("index");
    let source = fixture.child("source");
    let server = MockFileStation::start();
    server.add_directory(ROOT);
    for folder in 0..6 {
        fs::create_dir_all(source.join(format!("d{folder}"))).unwrap();
        server.add_directory(&format!("{ROOT}/d{folder}"));
    }
    for index in 0..files {
        fs::write(source.join(name(index)), content(index)).unwrap();
        server.add_file(
            &format!("{ROOT}/{}", name(index)),
            &content(index),
            1_700_000_000,
        );
    }
    let password = fixture.write("password", PASSWORD);
    let index = fixture.child("index");
    // A local change time within two seconds of the read that proves a pair makes it racy:
    // stored, never served. Fixtures written just now must age out of that window first.
    std::thread::sleep(Duration::from_millis(2_200));
    Tree {
        fixture,
        source,
        index,
        password,
        server,
    }
}

impl Tree {
    fn run_with(
        &self,
        verb: &str,
        source: &Path,
        extra: &[&str],
    ) -> (Output, Vec<CapturedRequest>) {
        let before = self.server.requests().len();
        let mut arguments = vec![
            "--quiet",
            "--output",
            "json",
            verb,
            source.to_str().unwrap(),
            ROOT,
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
        let output = run(&arguments);
        let requests = self.server.requests()[before..].to_vec();
        (output, requests)
    }

    fn sync(&self, extra: &[&str]) -> (Output, Vec<CapturedRequest>) {
        self.run_with("sync", &self.source, extra)
    }
}

/// The one index file the runs wrote, whatever the run's profile is called.
fn database_file(tree: &Tree) -> PathBuf {
    let files: Vec<PathBuf> = fs::read_dir(&tree.index)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "redb")
        })
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    files.into_iter().next().unwrap()
}

fn assert_ok(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn index_of(document: &Value) -> &Value {
    if document["result"]["pair_index"].is_object() {
        &document["result"]["pair_index"]
    } else {
        &document["plan"]["pair_index"]
    }
}

#[test]
fn a_second_run_takes_stored_evidence_and_reads_only_the_canary() {
    let tree = tree(60);
    let (first, requests) = tree.sync(&["--pair-index-canary", "2"]);
    assert_ok(&first);
    let document = json(&first);
    assert_eq!(count(&requests, "SYNO.FileStation.MD5.start"), 60);
    assert_eq!(index_of(&document)["state"], "cold");
    assert_eq!(index_of(&document)["stored"], 60);
    // What the redb file holds after the commit, for the package's status line.
    assert_eq!(index_of(&document)["entries"], 60);
    let database = database_file(&tree);
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let (second, requests) = tree.sync(&["--pair-index-canary", "2"]);
    assert_ok(&second);
    let document = json(&second);
    assert_eq!(
        count(&requests, "SYNO.FileStation.MD5.start"),
        2,
        "only the canary"
    );
    let index = index_of(&document);
    assert_eq!(index["state"], "warm");
    assert_eq!(index["served"], 58);
    assert_eq!(index["canary_checked"], 2);
    assert_eq!(index["issues"], Value::Array(Vec::new()));
}

/// L1: one byte rewritten in place and the modification time put back.
#[test]
fn an_in_place_edit_with_its_mtime_restored_is_uploaded() {
    let tree = tree(30);
    assert_ok(&tree.sync(&["--pair-index-canary", "0"]).0);
    let path = tree.source.join(name(7));
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(3)).unwrap();
    file.write_all(b"X").unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    let (output, _) = tree.sync(&["--pair-index-canary", "0"]);
    assert_ok(&output);
    assert_eq!(json(&output)["result"]["uploaded"], 1);
    assert_eq!(
        tree.server
            .file_contents(&format!("{ROOT}/{}", name(7)))
            .unwrap(),
        fs::read(&path).unwrap()
    );
}

/// R1: another writer replaces the NAS copy keeping its size and modification time.
#[test]
fn a_remote_replacement_keeping_size_and_mtime_is_overwritten() {
    let tree = tree(30);
    assert_ok(&tree.sync(&["--pair-index-canary", "0"]).0);
    let remote = format!("{ROOT}/{}", name(4));
    let mut forged = content(4);
    forged[0] = b'Z';
    tree.server.rewrite_file_keeping_mtime(&remote, &forged);
    let (output, _) = tree.sync(&["--pair-index-canary", "0"]);
    assert_ok(&output);
    assert_eq!(json(&output)["result"]["uploaded"], 1);
    assert_eq!(tree.server.file_contents(&remote).unwrap(), content(4));
}

/// P5/L8: a change no fact shows. Only the canary can see it: the index is discarded, the run
/// verifies everything live, uploads the file, and names the disagreement. Never exit 3.
#[test]
fn a_canary_disagreement_discards_the_index_names_it_and_still_syncs() {
    let tree = tree(30);
    assert_ok(&tree.sync(&[]).0);
    let remote = format!("{ROOT}/{}", name(9));
    let mut forged = content(9);
    forged[1] = b'Q';
    tree.server
        .corrupt_file_keeping_every_fact(&remote, &forged);
    // Every stored pair is a canary, so the corrupted one is among them.
    let (output, _) = tree.sync(&["--pair-index-canary", "1000"]);
    assert_ok(&output);
    let document = json(&output);
    assert_eq!(document["result"]["uploaded"], 1);
    let index = index_of(&document);
    assert_eq!(index["canary_mismatches"], 1);
    let issue = index["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|issue| issue["code"] == "pair-index-canary-mismatch")
        .expect("named");
    assert_eq!(issue["severity"], "warning");
    assert_eq!(issue["path"], name(9));
    assert_eq!(tree.server.file_contents(&remote).unwrap(), content(9));
}

/// A metadata or forced run neither reads nor writes the index.
#[test]
fn metadata_and_forced_runs_never_touch_the_index() {
    let tree = tree(10);
    assert_ok(&tree.sync(&[]).0);
    let database = database_file(&tree);
    let before = fs::read(&database).unwrap();
    for compare in ["metadata", "size-only"] {
        let (output, _) = tree.sync(&["--compare", compare]);
        assert_ok(&output);
        let document = json(&output);
        let index = index_of(&document);
        assert_eq!(index["state"], "off", "{compare}");
        assert_eq!(index["issues"][0]["code"], "pair-index-not-applicable");
    }
    assert_eq!(fs::read(&database).unwrap(), before, "byte-identical");
}

/// P1: the same index directory pointed at another source retires the evidence.
#[test]
fn an_index_from_another_source_is_retired_not_served() {
    let tree = tree(10);
    assert_ok(&tree.sync(&[]).0);
    let other = tree.fixture.child("other");
    fs::create_dir_all(&other).unwrap();
    for folder in 0..6 {
        fs::create_dir_all(other.join(format!("d{folder}"))).unwrap();
    }
    for index in 0..10 {
        fs::copy(tree.source.join(name(index)), other.join(name(index))).unwrap();
    }
    std::thread::sleep(Duration::from_millis(2_200));
    let (output, requests) = tree.run_with("sync", &other, &[]);
    assert_ok(&output);
    assert_eq!(
        count(&requests, "SYNO.FileStation.MD5.start"),
        10,
        "nothing served"
    );
    let document = json(&output);
    let index = index_of(&document);
    assert_eq!(index["issues"][0]["code"], "pair-index-retired");
    assert!(
        index["issues"][0]["cause"]
            .as_str()
            .unwrap()
            .contains("source"),
        "{index}"
    );
}

/// A plan reads and writes evidence too, so it warms the next sync.
#[test]
fn a_plan_warms_the_next_sync() {
    let tree = tree(20);
    let (plan, requests) =
        tree.run_with("plan", &tree.source.clone(), &["--pair-index-canary", "0"]);
    assert_ok(&plan);
    assert_eq!(count(&requests, "SYNO.FileStation.MD5.start"), 20);
    assert_eq!(index_of(&json(&plan))["stored"], 20);
    let (sync, requests) = tree.sync(&["--pair-index-canary", "0"]);
    assert_ok(&sync);
    assert_eq!(count(&requests, "SYNO.FileStation.MD5.start"), 0);
}

/// A directory the index does not own is refused by name; the run is live and succeeds.
#[test]
fn an_unusable_index_directory_is_named_and_the_run_goes_on_live() {
    let tree = tree(10);
    fs::create_dir_all(&tree.index).unwrap();
    fs::write(tree.index.join("unrelated.txt"), b"keep me").unwrap();
    let (output, requests) = tree.sync(&[]);
    assert_ok(&output);
    assert_eq!(count(&requests, "SYNO.FileStation.MD5.start"), 10);
    let document = json(&output);
    let index = index_of(&document);
    assert_eq!(index["state"], "unusable");
    assert_eq!(index["issues"][0]["code"], "pair-index-directory-unusable");
    assert_eq!(
        fs::read(tree.index.join("unrelated.txt")).unwrap(),
        b"keep me"
    );
}
