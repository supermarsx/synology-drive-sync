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
        self.sync_with(&[])
    }

    fn sync_with(&self, extra: &[&str]) -> Output {
        let mut arguments = vec![
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
        ];
        arguments.extend_from_slice(extra);
        run(&arguments)
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

/// R1 and R3: a path in the first scan, unchanged locally, whose NAS copy a third party rewrote
/// keeping its size and modification time, is not converged: exit 1, with the path, the action and
/// the cause on standard error and in the result record. The failed run still stores what it
/// proved about the other pairs, and names what it dropped.
#[test]
fn a_first_scan_path_that_still_differs_fails_by_name_and_keeps_the_other_evidence() {
    // More pairs than the canary (25) samples, so the next run has some to serve.
    let mut names = (0..40)
        .map(|index| format!("kept-{index:02}.txt"))
        .collect::<Vec<_>>();
    names.push("rewritten.txt".to_owned());
    let names = names.iter().map(String::as_str).collect::<Vec<_>>();
    let fixture = Fixture::new("first-scan-differs", &names);
    let mut altered = b"rewritten.txt content".to_vec();
    altered[0] = b'X';
    fixture.server.edit_file_on_next(
        "SYNO.FileStation.Upload.upload",
        &format!("{ROOT}/rewritten.txt"),
        &altered,
        false,
    );

    let output = fixture.sync();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "upload rewritten.txt (in the first scan, unchanged since, and the NAS still differs)"
        ),
        "the failure names the path, the action and the cause: {stderr}"
    );
    let record = fixture.result_record();
    assert_eq!(
        record["error"]["code"], "reconciliation_pending",
        "{record}"
    );
    assert_eq!(record["error"]["path"], "rewritten.txt", "{record}");
    assert!(
        record["error"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("did not converge on 1 in-scope path(s)")),
        "{record}"
    );

    let document = stdout_json(&output);
    let index = &document["result"]["pair_index"];
    assert!(
        index["stored"].as_u64().is_some_and(|stored| stored >= 1),
        "the pairs the run proved are kept: {index}"
    );
    let issues = index["issues"].as_array().expect("index issues");
    let dropped = issues
        .iter()
        .find(|issue| issue["code"] == "pair-index-failed-run")
        .unwrap_or_else(|| panic!("what was dropped is named: {issues:?}"));
    assert_eq!(dropped["severity"], "info");
    assert!(
        dropped["cause"]
            .as_str()
            .is_some_and(|cause| cause.contains("reconciliation_pending")
                && cause.contains(" not, because the run planned")
                && !cause.contains("  ")),
        "{dropped}"
    );

    // The next run is warm for the pairs the failed run proved, and repairs the other.
    let next = fixture.sync();
    assert_eq!(
        next.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
    let document = stdout_json(&next);
    assert!(
        document["plan"]["pair_index"]["served"]
            .as_u64()
            .is_some_and(|served| served >= 1),
        "{}",
        document["plan"]["pair_index"]
    );
    assert_eq!(
        fixture
            .server
            .file_contents(&format!("{ROOT}/rewritten.txt")),
        Some(b"rewritten.txt content".to_vec())
    );
}

/// R6: a file that is rewritten while every run works (a log, an open mailbox, an Office lock
/// file) is a transient deferral twice, then the lasting `file-keeps-changing`: the third run
/// exits 0, so a realtime routine stops re-running for it, and still names it. The name clears the
/// first run the file holds still long enough to sync.
#[test]
fn a_file_rewritten_during_every_run_is_named_lasting_on_the_third_run() {
    let fixture = Fixture::new("first-scan-keeps-changing", &["steady.txt"]);
    let busy = fixture.source.join("busy.log");
    fs::write(&busy, b"line 0\n").expect("write busy file");
    for round in 1..=3_u32 {
        let appended = busy.clone();
        fixture
            .server
            .on_next_request("SYNO.FileStation.List.list", move || {
                let mut text = fs::read(&appended).expect("read busy file");
                text.extend_from_slice(format!("line {round}\n").as_bytes());
                fs::write(&appended, text).expect("append to busy file");
            });
        let output = fixture.sync();
        let document = stdout_json(&output);
        let deferred = document["result"]["deferred"]
            .as_array()
            .expect("named deferrals")
            .iter()
            .find(|record| record["path"] == "busy.log")
            .cloned()
            .unwrap_or_else(|| panic!("run {round} names busy.log: {document}"));
        if round < 3 {
            assert_eq!(
                output.status.code(),
                Some(3),
                "run {round}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(deferred["reason"], "source-changed", "run {round}");
            assert_eq!(document["result"]["outcome"], "partial");
        } else {
            assert_eq!(
                output.status.code(),
                Some(0),
                "a path that keeps changing no longer makes the run partial: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(document["result"]["outcome"], "complete");
            assert_eq!(deferred["reason"], "file-keeps-changing");
            assert!(
                deferred["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.starts_with(
                        "file_keeps_changing path=busy.log runs=3; last cause source-changed: "
                    )),
                "{deferred}"
            );
        }
    }
    assert_eq!(
        fixture.server.file_contents(&format!("{ROOT}/busy.log")),
        None,
        "nothing torn was written"
    );

    // It settles: the next run syncs it, names nothing, and the count is gone.
    let settled = fixture.sync();
    assert_eq!(
        settled.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&settled.stderr)
    );
    let document = stdout_json(&settled);
    assert_eq!(document["result"]["deferred"], serde_json::json!([]));
    assert_eq!(
        fixture.server.file_contents(&format!("{ROOT}/busy.log")),
        Some(fs::read(&busy).unwrap())
    );
    let leftovers = fs::read_dir(&fixture.index)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".unsettled"))
        .count();
    assert_eq!(leftovers, 0, "a record that counts nothing is removed");
}

/// Run `fixture` four times, `fault` arming one failure of `path` before each of the first three:
/// a transient `transient` twice, the lasting `lasting` on the third (exit 0, the cause named),
/// and nothing named once the path succeeds.
fn loops_until_named(
    fixture: &Fixture,
    extra: &[&str],
    path: &str,
    fault: impl Fn(&MockFileStation),
    transient: &str,
    lasting: &str,
) {
    for round in 1..=4_u32 {
        if round <= 3 {
            fault(&fixture.server);
        }
        let output = fixture.sync_with(extra);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        // The result record, which is what the package's daemon reads.
        let document = fixture.result_record();
        let named = document["result"]["deferred"]
            .as_array()
            .expect("named deferrals")
            .iter()
            .find(|record| record["path"] == path)
            .cloned();
        match round {
            1 | 2 => {
                assert_eq!(output.status.code(), Some(3), "run {round}: {stderr}");
                let named = named.unwrap_or_else(|| panic!("run {round}: {document}"));
                assert_eq!(named["reason"], transient, "run {round}");
            }
            3 => {
                assert_eq!(output.status.code(), Some(0), "run 3: {stderr}");
                assert_eq!(document["result"]["outcome"], "complete");
                let named = named.unwrap_or_else(|| panic!("run 3: {document}"));
                assert_eq!(named["reason"], lasting);
                assert!(
                    named["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.starts_with(&format!(
                            "keeps_failing path={path} runs=3 cause={transient}; "
                        ))),
                    "{named}"
                );
            }
            _ => {
                assert_eq!(output.status.code(), Some(0), "run 4: {stderr}");
                assert!(named.is_none(), "cleared once it succeeds: {document}");
            }
        }
    }
}

/// R7: a deletion DSM keeps refusing for a reason other than permission (code 900) is
/// `delete-failed` twice, then the lasting `delete-keeps-failing`; it clears when it takes.
#[test]
fn a_deletion_that_keeps_failing_is_named_lasting_on_the_third_run() {
    let fixture = Fixture::new("first-scan-delete-loop", &["steady.txt"]);
    fixture.server.add_file(
        &format!("{ROOT}/gone.txt"),
        b"only on the NAS",
        1_700_000_000,
    );
    loops_until_named(
        &fixture,
        &["--delete"],
        "gone.txt",
        |server| server.fail_next_api_operation("SYNO.FileStation.Delete.delete", 900),
        "delete-failed",
        "delete-keeps-failing",
    );
    assert_eq!(
        fixture.server.file_contents(&format!("{ROOT}/gone.txt")),
        None
    );
}

/// R7: a file the NAS keeps failing to checksum is `remote-digest-unavailable` twice, then the
/// lasting `remote-digest-keeps-failing`; it clears when it can be compared.
#[test]
fn a_checksum_that_keeps_failing_is_named_lasting_on_the_third_run() {
    let fixture = Fixture::new("first-scan-digest-loop", &["steady.txt"]);
    // Only the one pair is compared, so each armed failure is its own.
    fs::remove_file(fixture.source.join("new.txt")).expect("remove the upload");
    loops_until_named(
        &fixture,
        &[],
        "steady.txt",
        |server| server.fail_next_api_operation("SYNO.FileStation.MD5.start", 599),
        "remote-digest-unavailable",
        "remote-digest-keeps-failing",
    );
}
