//! Crash-atomic deposit under process death: `abort()` at every step of the protocol (the
//! `fault-inject` feature's `SDSYNC_FAULT_ABORT=deposit:<step>`), and SIGKILL at arbitrary
//! moments. After each, the final name holds the old version or the new one -- never a partial
//! file, never anything else -- or, killed between a replacement's two renames, nothing, with the
//! old version whole under its reserved name; the next run settles what was left, names it, and
//! converges.
//!
//! `cargo test --features fault-inject --test atomic_deposit_faults`
#![cfg(all(unix, feature = "fault-inject"))]

#[allow(dead_code)]
mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const ROOT: &str = "/team/faults";
const PASSWORD: &[u8] = b"correct horse battery staple\n";

struct Fixture {
    dir: TestDir,
    server: MockFileStation,
}

impl Fixture {
    fn new() -> Self {
        let dir = TestDir::new("deposit-faults");
        for directory in ["source", "home"] {
            fs::create_dir_all(dir.child(directory)).unwrap();
        }
        dir.write("password", PASSWORD);
        let server = MockFileStation::start();
        server.add_directory(ROOT);
        Self { dir, server }
    }

    fn command(&self, fault: Option<&str>) -> Command {
        let source = self.dir.child("source");
        let password = self.dir.child("password");
        let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
        command
            .args([
                "--output",
                "json",
                "sync",
                source.to_str().unwrap(),
                ROOT,
                "--url",
                self.server.base_url(),
                "--username",
                "e2e-user",
                "--password-file",
                password.to_str().unwrap(),
                "--no-vault",
                "--allow-http",
                "--jobs",
                "1",
                "--retries",
                "0",
            ])
            .env("HOME", self.dir.child("home"))
            .env("XDG_CONFIG_HOME", self.dir.child("home"))
            .env_remove("XDG_STATE_HOME");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SDSYNC_") {
                command.env_remove(name);
            }
        }
        if let Some(fault) = fault {
            command.env("SDSYNC_FAULT_ABORT", fault);
        }
        command
    }

    fn sync(&self, fault: Option<&str>) -> Output {
        self.command(fault).output().unwrap()
    }

    fn reserved_on_nas(&self) -> Vec<String> {
        self.server
            .file_paths()
            .into_iter()
            .filter(|path| path.contains("/~sdsync-"))
            .collect()
    }

    fn journals(&self) -> usize {
        let dir = self
            .dir
            .child("home")
            .join(".local/state/synology-drive-sync/deposit");
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".journal"))
                    .count()
            })
            .unwrap_or(0)
    }
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn issue_codes(output: &Output) -> Vec<String> {
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    document["result"]["deposit"]["issues"]
        .as_array()
        .map(|issues| {
            issues
                .iter()
                .filter_map(|issue| issue["code"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn mtime(path: &Path) -> i64 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Everything a reader of `path` could have seen was one of `allowed` (`None`: absent).
fn assert_only(server: &MockFileStation, path: &str, allowed: &[Option<&[u8]>], context: &str) {
    for (operation, content) in server.file_history(path) {
        assert!(
            allowed
                .iter()
                .any(|candidate| candidate.map(<[u8]>::to_vec) == content),
            "{context}: after {operation}, {path} held {:?}",
            content.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        );
    }
}

/// One overwrite, aborted at `step`; then a clean run.
fn overwrite_aborted_at(step: &str, recovery: &[&str]) {
    let fixture = Fixture::new();
    let source = fixture.dir.write("source/a.txt", b"the new version");
    let path = format!("{ROOT}/a.txt");
    fixture.server.add_file(&path, b"old", mtime(&source) - 100);
    fixture.server.watch_file(&path);

    let aborted = fixture.sync(Some(&format!("deposit:{step}")));
    assert_eq!(
        aborted.status.code(),
        None,
        "{step}: {}",
        describe(&aborted)
    );
    // Absent only in the one gap between moving the old version aside and publishing the new.
    // Absent only from the moment the old version was moved aside, for the one request until the
    // new one took the name: any step from `backup-renamed` on has passed through that gap.
    let mut allowed: Vec<Option<&[u8]>> = vec![Some(b"old"), Some(b"the new version")];
    if matches!(
        step,
        "backup-renamed" | "published" | "backup-deleted" | "before-journal-close"
    ) {
        allowed.push(None);
    }
    assert_only(&fixture.server, &path, &allowed, step);
    assert_eq!(
        fixture.journals(),
        1,
        "{step}: a killed run leaves its journal"
    );

    let recovered = fixture.sync(None);
    assert_eq!(
        recovered.status.code(),
        Some(0),
        "{step}: {}",
        describe(&recovered)
    );
    assert_eq!(
        fixture.server.file_contents(&path),
        Some(b"the new version".to_vec()),
        "{step}"
    );
    assert!(
        fixture.reserved_on_nas().is_empty(),
        "{step}: {:?}",
        fixture.server.file_paths()
    );
    assert_eq!(fixture.journals(), 0, "{step}");
    let codes = issue_codes(&recovered);
    for code in recovery {
        assert!(codes.iter().any(|c| c == code), "{step}: {codes:?}");
    }
    assert_only(
        &fixture.server,
        &path,
        &[Some(b"old"), Some(b"the new version"), None],
        step,
    );
}

#[test]
fn killed_after_declaring() {
    overwrite_aborted_at("journal-declared", &[]);
}

#[test]
fn killed_while_streaming_the_temporary_copy() {
    overwrite_aborted_at("temp-streaming", &[]);
}

#[test]
fn killed_with_a_verified_temporary_copy() {
    overwrite_aborted_at("temp-verified", &["deposit-temp-removed"]);
}

#[test]
fn killed_between_the_two_renames() {
    overwrite_aborted_at("backup-renamed", &["deposit-rolled-back"]);
}

#[test]
fn killed_after_publishing_before_deleting_the_previous_version() {
    overwrite_aborted_at("published", &["deposit-completed"]);
}

#[test]
fn killed_after_deleting_the_previous_version() {
    overwrite_aborted_at("backup-deleted", &[]);
}

#[test]
fn killed_before_closing_the_journal() {
    overwrite_aborted_at("before-journal-close", &[]);
}

#[test]
fn killed_while_recovery_restores_the_previous_version() {
    let fixture = Fixture::new();
    let source = fixture.dir.write("source/a.txt", b"the new version");
    let path = format!("{ROOT}/a.txt");
    fixture.server.add_file(&path, b"old", mtime(&source) - 100);

    let first = fixture.sync(Some("deposit:backup-renamed"));
    assert_eq!(first.status.code(), None, "{}", describe(&first));
    // Recovery itself dies right after putting the old version back: the run after it finds
    // nothing left to settle and converges.
    let second = fixture.sync(Some("deposit:recovery-rolled-back"));
    assert_eq!(second.status.code(), None, "{}", describe(&second));
    assert_eq!(fixture.server.file_contents(&path), Some(b"old".to_vec()));

    let third = fixture.sync(None);
    assert_eq!(third.status.code(), Some(0), "{}", describe(&third));
    assert_eq!(
        fixture.server.file_contents(&path),
        Some(b"the new version".to_vec())
    );
    assert!(fixture.reserved_on_nas().is_empty());
}

#[test]
fn a_new_file_killed_at_any_step_never_appears_partial() {
    for step in [
        "temp-streaming",
        "temp-verified",
        "published",
        "before-journal-close",
    ] {
        let fixture = Fixture::new();
        fixture.dir.write("source/new.txt", b"a whole new file");
        let path = format!("{ROOT}/new.txt");
        fixture.server.watch_file(&path);

        let aborted = fixture.sync(Some(&format!("deposit:{step}")));
        assert_eq!(
            aborted.status.code(),
            None,
            "{step}: {}",
            describe(&aborted)
        );
        assert_only(
            &fixture.server,
            &path,
            &[None, Some(b"a whole new file")],
            step,
        );

        let recovered = fixture.sync(None);
        assert_eq!(
            recovered.status.code(),
            Some(0),
            "{step}: {}",
            describe(&recovered)
        );
        assert_eq!(
            fixture.server.file_contents(&path),
            Some(b"a whole new file".to_vec()),
            "{step}"
        );
        assert!(fixture.reserved_on_nas().is_empty(), "{step}");
    }
}

/// SIGKILL at moments spread across whole runs, each run carrying a new version of every file;
/// after every kill the final names hold only whole versions, and a last clean run converges with
/// nothing left behind.
#[test]
fn sigkill_at_arbitrary_moments_never_leaves_a_partial_file_and_the_next_run_converges() {
    let fixture = Fixture::new();
    let names = ["a.txt", "b.txt", "dir/c.txt"];
    let mut versions: Vec<Vec<u8>> = Vec::new();
    for name in names {
        fixture.server.watch_file(&format!("{ROOT}/{name}"));
    }
    // A cheap deterministic sequence of kill delays, 0..120 ms.
    let mut seed: u64 = 0x5eed_1234;
    let mut killed = 0_u32;
    for iteration in 0..24_u32 {
        let content = format!(
            "version {iteration:04} {}",
            "x".repeat(iteration as usize * 97)
        );
        versions.push(content.clone().into_bytes());
        for name in names {
            fixture
                .dir
                .write(&format!("source/{name}"), content.as_bytes());
        }
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let delay = Duration::from_millis((seed >> 33) % 120);
        let mut child = fixture
            .command(None)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while started.elapsed() < delay {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = child.kill();
        if child.wait().unwrap().code().is_none() {
            killed += 1;
        }
        for name in names {
            let path = format!("{ROOT}/{name}");
            for (operation, content) in fixture.server.file_history(&path) {
                if let Some(content) = content {
                    assert!(
                        versions.contains(&content),
                        "iteration {iteration}: after {operation}, {path} held a partial file"
                    );
                }
            }
        }
    }

    // The delays land inside runs often enough that most iterations really were killed mid-way.
    assert!(
        killed >= 8,
        "only {killed} of 24 runs were killed before they finished"
    );
    let last = fixture.sync(None);
    assert_eq!(last.status.code(), Some(0), "{}", describe(&last));
    for name in names {
        assert_eq!(
            fixture
                .server
                .file_contents(&format!("{ROOT}/{name}"))
                .as_ref(),
            versions.last(),
            "{name}"
        );
    }
    assert!(
        fixture.reserved_on_nas().is_empty(),
        "{:?}",
        fixture.server.file_paths()
    );
    assert_eq!(fixture.journals(), 0);
}
