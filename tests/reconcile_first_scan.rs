//! fix8 R1-R3: post-run reconciliation judged against the run's first scan, end to end.
//!
//! The field failure: "final reconciliation found 1 pending in-scope operations" on a NAS whose
//! users moved files into the source while a run worked. A move keeps an old modification time,
//! so the run could not tell the file from one it had failed to upload; it failed, named no path,
//! and threw away every digest it had just proven. Here, a path the first scan did not hold is a
//! named transient deferral (exit 3) that still commits the run's evidence, and a path the first
//! scan held that still differs is a failure that names it, and keeps the evidence it can.
//!
//! The pair index harvests a pair only where the walk reports a file identity and a change time,
//! so these are Unix-only, as the index's own suites are.
#![cfg(unix)]

#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const ROOT: &str = "/team/first-scan";

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("first-scan-environment");
    for directory in ["home", "xdg"] {
        fs::create_dir_all(environment.child(directory)).expect("create environment directory");
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
    // The fixtures settle for 2.2 s, not the minute the writeback bound asks of a real tree; the
    // debug-only override shortens the racy windows to match.
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

/// A source and a NAS folder that agree on `same`, plus `new.txt`, which only the source has, so
/// the run has an upload to make and reconciles afterwards.
struct Fixture {
    _dir: TestDir,
    source: PathBuf,
    index: PathBuf,
    record: PathBuf,
    password: PathBuf,
    server: MockFileStation,
}

impl Fixture {
    fn new(label: &str, same: &[&str]) -> Self {
        let dir = TestDir::new(label);
        let source = dir.child("source");
        fs::create_dir_all(&source).expect("create source");
        let server = MockFileStation::start();
        server.add_directory(ROOT);
        for name in same {
            let content = format!("{name} content").into_bytes();
            fs::write(source.join(name), &content).expect("write source file");
            server.add_file(&format!("{ROOT}/{name}"), &content, 1_700_000_000);
        }
        fs::write(source.join("new.txt"), b"only here").expect("write new file");
        let password = dir.write("password", PASSWORD);
        // Outside the (shortened) racy window before the plan reads them, or nothing is stored.
        std::thread::sleep(Duration::from_millis(2_200));
        Self {
            index: dir.child("index"),
            record: dir.child("result.ndjson"),
            _dir: dir,
            source,
            password,
            server,
        }
    }

    fn sync(&self) -> Output {
        run(&[
            "--quiet",
            "--output",
            "json",
            "--result-record",
            self.record.to_str().unwrap(),
            "sync",
            self.source.to_str().unwrap(),
            ROOT,
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            self.password.to_str().unwrap(),
            "--no-vault",
            "--allow-http",
            "--compare",
            "content",
            "--jobs",
            "1",
            "--pair-index",
            self.index.to_str().unwrap(),
        ])
    }

    fn result_record(&self) -> Value {
        let text = fs::read_to_string(&self.record).expect("the run wrote its result record");
        serde_json::from_str(text.lines().last().expect("one record line")).unwrap()
    }
}

fn set_old_mtime(path: &Path) {
    fs::File::options()
        .write(true)
        .open(path)
        .expect("open moved file")
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000))
        .expect("keep the old modification time a move keeps");
}

/// R2: a file moved into the source while the run uploads, with the old modification time a move
/// keeps, is named as a transient `source-changed` deferral: the run is partial (exit 3), not
/// failed, its evidence is committed, and the next run carries the file.
#[test]
fn a_file_moved_in_during_the_run_is_a_named_transient_deferral_and_the_index_is_kept() {
    let fixture = Fixture::new("first-scan-moved-in", &["steady.txt"]);
    let moved = fixture.source.join("moved.pdf");
    let created = moved.clone();
    fixture
        .server
        .on_next_request("SYNO.FileStation.Upload.upload", move || {
            fs::write(&created, b"moved in from elsewhere").expect("move a file in");
            set_old_mtime(&created);
        });

    let output = fixture.sync();
    assert_eq!(
        output.status.code(),
        Some(3),
        "a change during the run is transient, never a failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = stdout_json(&output);
    let result = &document["result"];
    assert_eq!(result["outcome"], "partial");
    assert_eq!(result["failed"], false);
    assert_eq!(result["uploaded"], 1);
    assert_eq!(result["changed_during_run"], 1);
    let deferred = result["deferred"].as_array().expect("named deferrals");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    assert_eq!(deferred[0]["path"], "moved.pdf");
    assert_eq!(deferred[0]["reason"], "source-changed");
    assert!(
        deferred[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("not in the run's first scan")),
        "{deferred:?}"
    );
    // The evidence the run proved is written, even though the run is partial.
    let index = &result["pair_index"];
    assert!(
        index["stored"].as_u64().is_some_and(|stored| stored >= 1),
        "{index}"
    );
    assert!(
        index["entries"]
            .as_u64()
            .is_some_and(|entries| entries >= 1)
    );
    let record = fixture.result_record();
    assert_eq!(record["error"], Value::Null, "{record}");

    // The next run carries it.
    let next = fixture.sync();
    assert_eq!(
        next.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    assert_eq!(
        fixture.server.file_contents(&format!("{ROOT}/moved.pdf")),
        Some(b"moved in from elsewhere".to_vec())
    );
}
