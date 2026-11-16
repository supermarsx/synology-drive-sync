//! Crash-atomic deposit, end to end: the command line against the in-tree File Station mock.
//!
//! The mock records what a reader of the NAS could have seen under a name after every request
//! (`watch_file`), so "never a partial file under the final name" is asserted request by request,
//! not inferred from the end state. Kill and abort tests live in `atomic_deposit_faults.rs`.

#[allow(dead_code)]
mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const ROOT: &str = "/team/atomic";
const PASSWORD: &[u8] = b"correct horse battery staple\n";

/// One isolated user: the same HOME across runs, so the deposit journal persists between them.
struct User {
    home: TestDir,
}

impl User {
    fn new() -> Self {
        let home = TestDir::new("atomic-user");
        for directory in ["home", "appdata", "local-appdata", "xdg"] {
            fs::create_dir_all(home.child(directory)).unwrap();
        }
        Self { home }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
        command
            .args(arguments)
            .env("HOME", self.home.child("home"))
            .env("USERPROFILE", self.home.child("home"))
            .env("APPDATA", self.home.child("appdata"))
            .env("LOCALAPPDATA", self.home.child("local-appdata"))
            .env("XDG_CONFIG_HOME", self.home.child("xdg"))
            .env_remove("XDG_STATE_HOME");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SDSYNC_") {
                command.env_remove(name);
            }
        }
        command.output().unwrap()
    }

    /// Where the journal lives by default for this user.
    fn journal_dir(&self) -> std::path::PathBuf {
        if cfg!(windows) {
            self.home
                .child("local-appdata")
                .join("synology-drive-sync")
                .join("deposit")
        } else if cfg!(target_os = "macos") {
            self.home
                .child("home")
                .join("Library/Application Support/synology-drive-sync/deposit")
        } else {
            self.home
                .child("home")
                .join(".local/state/synology-drive-sync/deposit")
        }
    }
}

struct Fixture {
    dir: TestDir,
    server: MockFileStation,
}

impl Fixture {
    fn new() -> Self {
        let dir = TestDir::new("atomic");
        fs::create_dir_all(dir.child("source")).unwrap();
        dir.write("password", PASSWORD);
        let server = MockFileStation::start();
        server.add_directory(ROOT);
        Self { dir, server }
    }

    fn sync(&self, user: &User, extra: &[&str]) -> Output {
        let source = self.dir.child("source");
        let password = self.dir.child("password");
        let mut arguments = vec![
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
            "2",
        ];
        arguments.extend_from_slice(extra);
        user.run(&arguments)
    }

    fn reserved_on_nas(&self) -> Vec<String> {
        self.server
            .file_paths()
            .into_iter()
            .filter(|path| path.contains("/~sdsync-"))
            .collect()
    }

    fn count(&self, operation: &str) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|request| request.operation() == operation)
            .count()
    }
}

fn describe(output: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout\n{}\n--- stderr\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn result(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
}

fn deposit(document: &Value) -> &Value {
    &document["result"]["deposit"]
}

fn issue_codes(document: &Value) -> Vec<String> {
    deposit(document)["issues"]
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

#[test]
fn new_files_and_overwrites_reach_their_names_whole_and_leave_nothing_behind() {
    let fixture = Fixture::new();
    let user = User::new();
    fixture.dir.write("source/kept.txt", b"unchanged");
    fixture.dir.write("source/new.txt", b"a brand new file");
    let changed = fixture
        .dir
        .write("source/sub/changed.txt", b"the new version");
    fixture.server.add_file(
        &format!("{ROOT}/kept.txt"),
        b"unchanged",
        mtime(&fixture.dir.child("source/kept.txt")),
    );
    fixture.server.add_directory(&format!("{ROOT}/sub"));
    fixture
        .server
        .add_file(&format!("{ROOT}/sub/changed.txt"), b"old", mtime(&changed));
    for path in ["new.txt", "sub/changed.txt"] {
        fixture.server.watch_file(&format!("{ROOT}/{path}"));
    }

    let output = fixture.sync(&user, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        fixture.server.file_history(&format!("{ROOT}/new.txt")),
        vec![
            ("start".to_owned(), None),
            (
                "SYNO.FileStation.Rename.rename".to_owned(),
                Some(b"a brand new file".to_vec())
            ),
        ],
        "a new file appears whole, by rename"
    );
    let changed_history = fixture
        .server
        .file_history(&format!("{ROOT}/sub/changed.txt"))
        .into_iter()
        .map(|(_, content)| content)
        .collect::<Vec<_>>();
    assert_eq!(
        changed_history,
        vec![
            Some(b"old".to_vec()),
            None,
            Some(b"the new version".to_vec())
        ],
        "an overwrite shows the old version, then (for one request) nothing, then the new one"
    );
    assert!(
        fixture.reserved_on_nas().is_empty(),
        "{:?}",
        fixture.server.file_paths()
    );
    let document = result(&output);
    assert_eq!(deposit(&document)["mode"], "atomic", "{document}");
    assert_eq!(deposit(&document)["published"], 1, "{document}");
    assert_eq!(deposit(&document)["replaced"], 1, "{document}");
    assert_eq!(deposit(&document)["unsettled"], 0, "{document}");
    assert!(issue_codes(&document).is_empty(), "{document}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("deposit"),
        "{}",
        describe(&output)
    );
    // Settled runs leave no journal; the lock file stays for the next run to claim.
    let journals = fs::read_dir(user.journal_dir())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".journal"))
        .count();
    assert_eq!(journals, 0);
    // Three renames: the new file's, and the overwrite's two. One delete: the previous version.
    assert_eq!(fixture.count("SYNO.FileStation.Rename.rename"), 3);
    assert_eq!(fixture.count("SYNO.FileStation.Delete.delete"), 1);

    // Converged: the next run writes nothing.
    let again = fixture.sync(&user, &[]);
    assert_eq!(again.status.code(), Some(0), "{}", describe(&again));
    assert_eq!(fixture.count("SYNO.FileStation.Upload.upload"), 2);
}

#[test]
fn direct_deposit_writes_the_final_name_and_says_so_on_every_run() {
    let fixture = Fixture::new();
    let user = User::new();
    fixture.dir.write("source/a.txt", b"content");

    let output = fixture.sync(&user, &["--deposit", "direct"]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fixture.count("SYNO.FileStation.Rename.rename"), 0);
    assert_eq!(
        fixture.server.file_contents(&format!("{ROOT}/a.txt")),
        Some(b"content".to_vec())
    );
    let document = result(&output);
    assert_eq!(deposit(&document)["mode"], "direct", "{document}");
    assert_eq!(issue_codes(&document), vec!["deposit-not-atomic"]);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("warning: deposit-not-atomic"),
        "{}",
        describe(&output)
    );
}

#[test]
fn a_dsm_without_rename_gets_nothing_written_and_every_path_named() {
    let fixture = Fixture::new();
    let user = User::new();
    fixture.server.withhold_api("SYNO.FileStation.Rename");
    fixture.dir.write("source/a.txt", b"content");
    fixture.dir.write("source/dir/b.txt", b"more");

    let output = fixture.sync(&user, &[]);

    // Lasting: exit 0, named, and nothing written -- never a silent fall back to direct.
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fixture.count("SYNO.FileStation.Upload.upload"), 0);
    assert_eq!(fixture.count("SYNO.FileStation.CreateFolder.create"), 0);
    let document = result(&output);
    let reasons = document["result"]["deferred"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["reason"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(!reasons.is_empty(), "{document}");
    assert!(
        reasons
            .iter()
            .all(|reason| reason == "deposit-not-atomic-unavailable"),
        "{reasons:?}"
    );
    assert!(issue_codes(&document).contains(&"deposit-not-atomic-unavailable".to_owned()));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("deposit-not-atomic-unavailable"),
        "{}",
        describe(&output)
    );
}

#[test]
fn a_torn_temporary_copy_never_reaches_the_final_name_and_the_next_run_converges() {
    let fixture = Fixture::new();
    let user = User::new();
    let source = fixture.dir.write("source/a.txt", b"0123456789abcdef");
    fixture.server.add_file(
        &format!("{ROOT}/a.txt"),
        b"the old content!",
        mtime(&source) - 100,
    );
    fixture.server.watch_file(&format!("{ROOT}/a.txt"));
    // The upload of the temporary copy is torn by a dropped link: the attempt, and the one more
    // attempt a torn upload gets.
    fixture.server.tear_next_upload_after(4);
    fixture.server.tear_next_upload_after(4);

    let output = fixture.sync(&user, &["--retries", "0"]);

    assert_ne!(output.status.code(), Some(0), "{}", describe(&output));
    let history = fixture.server.file_history(&format!("{ROOT}/a.txt"));
    assert_eq!(
        history,
        vec![("start".to_owned(), Some(b"the old content!".to_vec()))],
        "the final name never held a torn file"
    );
    assert!(
        fixture.reserved_on_nas().is_empty(),
        "{:?}",
        fixture.server.file_paths()
    );

    let again = fixture.sync(&user, &["--retries", "0"]);
    assert_eq!(again.status.code(), Some(0), "{}", describe(&again));
    assert_eq!(
        fixture.server.file_contents(&format!("{ROOT}/a.txt")),
        Some(b"0123456789abcdef".to_vec())
    );
    assert!(fixture.reserved_on_nas().is_empty());
}

#[test]
fn a_local_file_with_a_reserved_name_is_named_and_never_uploaded() {
    let fixture = Fixture::new();
    let user = User::new();
    fixture
        .dir
        .write("source/~sdsync-part-0123456789ab-run-1", b"mine");
    fixture.dir.write("source/a.txt", b"content");

    let output = fixture.sync(&user, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fixture.count("SYNO.FileStation.Upload.upload"), 1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("reserved for synology-drive-sync"),
        "{stdout}"
    );
}

#[test]
fn reserved_names_on_the_nas_are_never_compared_uploaded_over_or_deleted() {
    let fixture = Fixture::new();
    let user = User::new();
    fixture.dir.write("source/a.txt", b"content");
    let foreign = format!("{ROOT}/~sdsync-prev-0123456789ab-aaaaaaaaaaaa-1");
    fixture.server.add_file(&foreign, b"someone's backup", 1);

    let output = fixture.sync(&user, &["--delete", "--max-delete", "10"]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        fixture.server.file_contents(&foreign),
        Some(b"someone's backup".to_vec()),
        "a mirror run never deletes a reserved name"
    );
    let document = result(&output);
    assert!(
        issue_codes(&document).contains(&"deposit-orphan-unjournaled".to_owned()),
        "{document}"
    );
}

#[test]
fn a_file_under_a_comma_ambiguous_folder_is_deposited_directly_and_named() {
    let fixture = Fixture::new();
    let user = User::new();
    // `/team/atomic/a` exists, so a path list naming `/team/atomic/a,b/...` could address it
    // (NAS question N1): no Rename or Delete of a temporary copy may be sent there.
    fixture.server.add_file(&format!("{ROOT}/a"), b"a", 1);
    fixture.dir.write("source/a", b"a");
    fixture
        .dir
        .write("source/a,b/x.txt", b"inside the comma folder");
    fixture.dir.write("source/plain.txt", b"elsewhere");

    let output = fixture.sync(&user, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        fixture.server.file_contents(&format!("{ROOT}/a,b/x.txt")),
        Some(b"inside the comma folder".to_vec())
    );
    let uploads = fixture
        .server
        .requests()
        .into_iter()
        .filter(|request| request.operation() == "SYNO.FileStation.Upload.upload")
        .filter_map(|request| request.upload_filename)
        .collect::<Vec<_>>();
    assert!(uploads.contains(&"x.txt".to_owned()), "{uploads:?}");
    assert!(
        uploads.iter().any(|name| name.starts_with("~sdsync-part-")),
        "the other file is still deposited atomically: {uploads:?}"
    );
    let document = result(&output);
    let issues = deposit(&document)["issues"].as_array().unwrap().clone();
    assert!(
        issues
            .iter()
            .any(|issue| issue["code"] == "deposit-not-atomic"
                && issue["path"] == format!("{ROOT}/a,b/x.txt")),
        "{document}"
    );
    assert!(fixture.reserved_on_nas().is_empty());
}
