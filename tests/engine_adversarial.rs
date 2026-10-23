//! The sync engine against the adversarial audit's scenarios (ADV-E-1 .. ADV-E-11), end to end:
//! the real binary against the File Station mock.
//!
//! Each test states the behaviour the engine must have. They were written against the audit's
//! probes, which asserted the defects, and failed on the code those probes ran against.
#[allow(dead_code)]
mod support;

use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("engine-adv-env");
    for directory in ["home", "appdata", "local-appdata", "xdg"] {
        fs::create_dir_all(environment.child(directory)).unwrap();
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
    command.output().unwrap()
}

struct Fixture {
    _dir: TestDir,
    source: std::path::PathBuf,
    password: std::path::PathBuf,
    server: MockFileStation,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = TestDir::new(label);
        let source = dir.child("source");
        fs::create_dir_all(&source).unwrap();
        let password = dir.write("password", PASSWORD);
        Self {
            _dir: dir,
            source,
            password,
            server: MockFileStation::start(),
        }
    }

    fn write(&self, relative: &str, contents: &[u8]) {
        let path = self.source.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn sync(&self, destination: &str, extra: &[&str]) -> Output {
        let source = self.source.to_str().unwrap();
        let password = self.password.to_str().unwrap();
        let mut arguments = vec![
            "--quiet",
            "--output",
            "json",
            "sync",
            source,
            destination,
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            password,
            "--no-vault",
            "--allow-http",
            "--jobs",
            "1",
        ];
        arguments.extend_from_slice(extra);
        run(&arguments)
    }

    fn count(&self, operation: &str) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|request| request.operation() == operation)
            .count()
    }

    #[cfg(windows)]
    fn count_naming(&self, operation: &str, needle: &str) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|request| {
                request.operation() == operation
                    && request.fields.values().any(|value| value.contains(needle))
            })
            .count()
    }

    fn text(&self, path: &str) -> Option<String> {
        self.server
            .file_contents(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}); exit={:?}\nstdout={}\nstderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn describe(output: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `(path, reason)` of every deferral in the result.
fn deferred(document: &Value) -> Vec<(String, String)> {
    document["result"]["deferred"]
        .as_array()
        .map(|records| {
            records
                .iter()
                .map(|record| {
                    (
                        record["path"].as_str().unwrap_or_default().to_owned(),
                        record["reason"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `(path, reason)` of every plan skip.
fn skipped(document: &Value) -> Vec<(String, String)> {
    document["plan"]["skipped"]
        .as_array()
        .map(|records| {
            records
                .iter()
                .map(|record| {
                    (
                        record["path"].as_str().unwrap_or_default().to_owned(),
                        record["reason"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn has(records: &[(String, String)], path: &str, reason: &str) -> bool {
    records
        .iter()
        .any(|(candidate, why)| candidate == path && why == reason)
}

const DELETE: &str = "SYNO.FileStation.Delete.delete";
const RENAME: &str = "SYNO.FileStation.Rename.rename";
const UPLOAD: &str = "SYNO.FileStation.Upload.upload";
#[cfg(windows)]
const DOWNLOAD: &str = "SYNO.FileStation.Download.download";

// ---------------------------------------------------------------------------------------------
// ADV-E-1: case-only renames
// ---------------------------------------------------------------------------------------------

/// A case-only folder rename is carried by one File Station Rename: nothing is deleted, the folder
/// keeps its content, and later edits keep reaching the NAS under the new spelling.
#[test]
fn case_rename_of_a_folder_is_one_server_rename() {
    let fixture = Fixture::new("adv-e1-rename");
    fixture.write("photos/a.jpg", b"jpeg-a");
    let server = &fixture.server;
    server.add_directory("/team/e1");
    server.add_directory("/team/e1/Photos");
    server.add_file("/team/e1/Photos/a.jpg", b"jpeg-a", 1_700_000_000);

    let first = fixture.sync("/team/e1", &["--delete", "--compare", "content"]);
    assert_eq!(first.status.code(), Some(0), "{}", describe(&first));
    assert_eq!(fixture.count(DELETE), 0, "a case rename deletes nothing");
    assert_eq!(fixture.count(RENAME), 1, "{}", describe(&first));
    assert_eq!(json(&first)["result"]["renamed"], 1, "{}", describe(&first));
    assert!(
        !server
            .directories()
            .iter()
            .any(|dir| dir.starts_with("/team/e1/Photos"))
    );
    assert_eq!(
        fixture.text("/team/e1/photos/a.jpg").as_deref(),
        Some("jpeg-a")
    );

    fixture.write("photos/a.jpg", b"jpeg-a edited, longer");
    fixture.write("photos/b.jpg", b"jpeg-b");
    let second = fixture.sync("/team/e1", &["--delete", "--compare", "content"]);
    assert_eq!(second.status.code(), Some(0), "{}", describe(&second));
    assert_eq!(
        fixture.text("/team/e1/photos/b.jpg").as_deref(),
        Some("jpeg-b")
    );
    assert_eq!(
        fixture.text("/team/e1/photos/a.jpg").as_deref(),
        Some("jpeg-a edited, longer")
    );
    assert_eq!(fixture.count(DELETE), 0);
}

/// The `@eaDir` case: a NAS folder holding DSM's thumbnails is not renamed (the rename would move
/// DSM-managed content this run does not own) and nothing is deleted. The content keeps syncing
/// under the spelling the NAS has, and the rename is a named warning on every run until someone renames
/// the folder in File Station.
#[test]
fn case_rename_of_a_folder_holding_eadir_keeps_syncing_with_a_warning() {
    let fixture = Fixture::new("adv-e1-eadir");
    fixture.write("photos/a.jpg", b"jpeg-a");
    let server = &fixture.server;
    server.add_directory("/team/e1e");
    server.add_directory("/team/e1e/Photos");
    server.add_directory("/team/e1e/Photos/@eaDir");
    server.add_directory("/team/e1e/Photos/@eaDir/a.jpg");
    server.add_file(
        "/team/e1e/Photos/@eaDir/a.jpg/SYNOPHOTO_THUMB_M.jpg",
        b"thumb",
        1_700_000_000,
    );
    server.add_file("/team/e1e/Photos/a.jpg", b"jpeg-a", 1_700_000_000);

    for round in 0..2 {
        if round == 1 {
            fixture.write("photos/a.jpg", b"jpeg-a edited, longer");
            fixture.write("photos/b.jpg", b"jpeg-b");
        }
        let output = fixture.sync("/team/e1e", &["--delete"]);
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        assert!(
            has(&deferred(&json(&output)), "photos", "case-rename-pending"),
            "{}",
            describe(&output)
        );
    }
    assert_eq!(fixture.count(DELETE), 0);
    assert_eq!(fixture.count(RENAME), 0);
    assert_eq!(
        fixture.text("/team/e1e/Photos/a.jpg").as_deref(),
        Some("jpeg-a edited, longer")
    );
    assert_eq!(
        fixture.text("/team/e1e/Photos/b.jpg").as_deref(),
        Some("jpeg-b")
    );
    assert_eq!(
        fixture
            .text("/team/e1e/Photos/@eaDir/a.jpg/SYNOPHOTO_THUMB_M.jpg")
            .as_deref(),
        Some("thumb")
    );
    assert!(
        !server
            .directories()
            .contains(&"/team/e1e/photos".to_owned())
    );
}

/// Mandatory: when File Station refuses the rename, nothing at all is deleted, the content is
/// still backed up under the old spelling, the rename is a named warning, and the next run tries
/// it again.
#[test]
fn refused_case_rename_deletes_nothing_and_is_retried() {
    let fixture = Fixture::new("adv-e1-refused");
    fixture.write("photos/a.jpg", b"jpeg-a edited");
    fixture.write("photos/new.jpg", b"new");
    fixture.write("photos/notes/n.txt", b"note");
    let server = &fixture.server;
    server.add_directory("/team/e1r");
    server.add_directory("/team/e1r/Photos");
    server.add_directory("/team/e1r/Photos/notes");
    server.add_file("/team/e1r/Photos/notes/n.txt", b"note", 1_700_000_000);
    server.add_file("/team/e1r/Photos/a.jpg", b"jpeg-a", 1_700_000_000);
    server.fail_next_api_operation(RENAME, 900);

    let first = fixture.sync("/team/e1r", &["--delete"]);
    assert_eq!(first.status.code(), Some(0), "{}", describe(&first));
    let document = json(&first);
    assert_eq!(document["result"]["outcome"], "complete");
    assert!(
        has(&deferred(&document), "photos", "case-rename-pending"),
        "{}",
        describe(&first)
    );
    assert_eq!(
        fixture.count(DELETE),
        0,
        "nothing is deleted when the rename fails"
    );
    // Backed up under the spelling the NAS still has.
    assert_eq!(
        fixture.text("/team/e1r/Photos/a.jpg").as_deref(),
        Some("jpeg-a edited")
    );
    assert_eq!(
        fixture.text("/team/e1r/Photos/new.jpg").as_deref(),
        Some("new")
    );
    assert_eq!(
        fixture.text("/team/e1r/Photos/notes/n.txt").as_deref(),
        Some("note")
    );

    let second = fixture.sync("/team/e1r", &["--delete"]);
    assert_eq!(second.status.code(), Some(0), "{}", describe(&second));
    assert_eq!(fixture.count(DELETE), 0);
    assert_eq!(
        fixture.text("/team/e1r/photos/a.jpg").as_deref(),
        Some("jpeg-a edited")
    );
    assert_eq!(
        fixture.text("/team/e1r/photos/notes/n.txt").as_deref(),
        Some("note")
    );
    assert!(fixture.text("/team/e1r/Photos/a.jpg").is_none());
}

/// A case-only file rename whose Rename is refused once: the edit still reaches the NAS (under the
/// old spelling), nothing is deleted, and the next run finishes the rename instead of freezing.
#[test]
fn refused_file_case_rename_keeps_edits_flowing() {
    let fixture = Fixture::new("adv-e1-file");
    fixture.write("report.pdf", b"v2 - the edit that must be backed up");
    let server = &fixture.server;
    server.add_directory("/team/e1f");
    server.add_file("/team/e1f/Report.pdf", b"v1", 1_700_000_000);
    server.fail_next_api_operation(RENAME, 900);

    let first = fixture.sync("/team/e1f", &["--delete"]);
    assert_eq!(first.status.code(), Some(0), "{}", describe(&first));
    assert_eq!(fixture.count(DELETE), 0);
    assert_eq!(
        fixture.text("/team/e1f/Report.pdf").as_deref(),
        Some("v2 - the edit that must be backed up")
    );

    let second = fixture.sync("/team/e1f", &["--delete"]);
    assert_eq!(second.status.code(), Some(0), "{}", describe(&second));
    assert_eq!(
        server.file_paths(),
        vec!["/team/e1f/report.pdf".to_owned()],
        "{}",
        describe(&second)
    );
    assert_eq!(fixture.count(DELETE), 0);
}

/// A NAS that already holds both spellings (left by an earlier version, or a third party): the
/// spelling the source holds keeps syncing, and the old one is never touched -- not deleted, its
/// `@eaDir` included -- and is named as a warning on every run until someone removes it.
#[test]
fn leftover_second_spelling_is_a_named_lasting_warning() {
    let fixture = Fixture::new("adv-e1-leftover");
    fixture.write("photos/a.jpg", b"jpeg-a edited");
    fixture.write("photos/b.jpg", b"jpeg-b");
    let server = &fixture.server;
    server.add_directory("/team/e1l");
    server.add_directory("/team/e1l/Photos");
    server.add_directory("/team/e1l/Photos/@eaDir");
    server.add_file("/team/e1l/Photos/@eaDir/thumb.jpg", b"thumb", 1_700_000_000);
    server.add_file("/team/e1l/Photos/a.jpg", b"jpeg-a", 1_700_000_000);
    server.add_directory("/team/e1l/photos");
    server.add_file("/team/e1l/photos/a.jpg", b"jpeg-a", 1_700_000_000);

    for _ in 0..2 {
        let output = fixture.sync("/team/e1l", &["--delete"]);
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        let document = json(&output);
        assert!(
            has(&deferred(&document), "Photos", "case-rename-pending"),
            "{}",
            describe(&output)
        );
        assert!(
            !skipped(&document)
                .iter()
                .any(|(_, reason)| reason == "remote-case-collision"),
            "{}",
            describe(&output)
        );
    }
    assert_eq!(
        fixture.text("/team/e1l/photos/a.jpg").as_deref(),
        Some("jpeg-a edited")
    );
    assert_eq!(
        fixture.text("/team/e1l/photos/b.jpg").as_deref(),
        Some("jpeg-b")
    );
    assert_eq!(
        fixture.count(DELETE),
        0,
        "the old spelling is never touched"
    );
    assert_eq!(
        fixture.text("/team/e1l/Photos/a.jpg").as_deref(),
        Some("jpeg-a")
    );
    assert_eq!(
        fixture.text("/team/e1l/Photos/@eaDir/thumb.jpg").as_deref(),
        Some("thumb")
    );
}

/// Without `--delete` the leftover cannot be removed, but the spelling the source holds still
/// syncs and the run says, by name, that it is not finished.
#[test]
fn leftover_second_spelling_without_delete_still_syncs_the_kept_spelling() {
    let fixture = Fixture::new("adv-e1-leftover-nodelete");
    fixture.write("photos/a.jpg", b"jpeg-a edited");
    fixture.write("photos/b.jpg", b"jpeg-b");
    let server = &fixture.server;
    server.add_directory("/team/e1n");
    server.add_directory("/team/e1n/Photos");
    server.add_file("/team/e1n/Photos/a.jpg", b"jpeg-a", 1_700_000_000);
    server.add_directory("/team/e1n/photos");
    server.add_file("/team/e1n/photos/a.jpg", b"jpeg-a", 1_700_000_000);

    let output = fixture.sync("/team/e1n", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(has(
        &deferred(&json(&output)),
        "Photos",
        "case-rename-pending"
    ));
    assert_eq!(
        fixture.text("/team/e1n/photos/b.jpg").as_deref(),
        Some("jpeg-b")
    );
    assert_eq!(
        fixture.text("/team/e1n/photos/a.jpg").as_deref(),
        Some("jpeg-a edited")
    );
    assert_eq!(
        fixture.text("/team/e1n/Photos/a.jpg").as_deref(),
        Some("jpeg-a")
    );
    assert_eq!(fixture.count(DELETE), 0);
}

/// A scoped run naming the new spelling must not create it beside the old one.
#[test]
fn scoped_run_does_not_create_a_second_spelling() {
    let fixture = Fixture::new("adv-e1-scoped");
    fixture.write("photos/a.jpg", b"jpeg-a");
    let server = &fixture.server;
    server.add_directory("/team/e1s");
    server.add_directory("/team/e1s/Photos");
    server.add_file("/team/e1s/Photos/a.jpg", b"jpeg-a", 1_700_000_000);

    let scoped = fixture.sync("/team/e1s", &["--scope", "photos"]);
    assert_eq!(scoped.status.code(), Some(0), "{}", describe(&scoped));
    assert!(
        has(&skipped(&json(&scoped)), "photos", "remote-case-collision"),
        "{}",
        describe(&scoped)
    );
    assert!(
        !server
            .directories()
            .contains(&"/team/e1s/photos".to_owned()),
        "{:?}",
        server.directories()
    );
    assert_eq!(fixture.count(UPLOAD), 0);

    // The unscoped mirror run then carries the rename.
    let mirror = fixture.sync("/team/e1s", &["--delete"]);
    assert_eq!(mirror.status.code(), Some(0), "{}", describe(&mirror));
    assert!(
        server
            .directories()
            .contains(&"/team/e1s/photos".to_owned())
    );
    assert!(
        !server
            .directories()
            .contains(&"/team/e1s/Photos".to_owned())
    );
}

// ---------------------------------------------------------------------------------------------
// ADV-E-2: a shielded remote file is never downloaded for a deletion guard
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
#[test]
fn shielded_remote_file_is_not_downloaded_for_a_deletion_guard() {
    let fixture = Fixture::new("adv-e2");
    let skipped_file = fixture.source.join("vault.bin");
    fs::write(&skipped_file, vec![7_u8; 256 * 1024]).unwrap();
    let marked = Command::new("attrib")
        .arg("+S")
        .arg(&skipped_file)
        .status()
        .is_ok_and(|status| status.success());
    assert!(marked, "attrib +S");
    let server = &fixture.server;
    server.add_directory("/team/e2");
    server.add_file("/team/e2/vault.bin", &vec![7_u8; 256 * 1024], 1_700_000_000);

    for round in 0..2 {
        fixture.write(&format!("new{round}.txt"), b"payload");
        let output = fixture.sync("/team/e2", &["--delete"]);
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    }
    let downloads = fixture.count_naming(DOWNLOAD, "vault.bin");
    let deletes = fixture.count_naming(DELETE, "vault.bin");
    let _ = Command::new("attrib").arg("-S").arg(&skipped_file).status();
    assert_eq!(deletes, 0, "the shield must hold");
    assert_eq!(
        downloads, 0,
        "a file that can never be deleted is never downloaded"
    );
    assert!(server.file_contents("/team/e2/vault.bin").is_some());
}

// ---------------------------------------------------------------------------------------------
// ADV-E-3 / ADV-R-1: one refused name is that path's problem, not the run's
// ---------------------------------------------------------------------------------------------

/// One refusal policy for every path the tool or DSM will not take: a refused local name is that
/// path's lasting `unsupported-entry` deferral, a named warning (never a plain green run) until someone
/// renames or excludes it. Its NAS copy is never deleted. It used to fail the whole run.
#[test]
fn a_refused_local_name_is_skipped_and_the_rest_uploads() {
    let fixture = Fixture::new("adv-e3-tilde");
    for index in 0..5 {
        fixture.write(&format!("f{index:02}.txt"), b"payload");
    }
    fixture.write("~WRL0001.tmp", b"word autosave");
    fixture.server.add_directory("/team/e3");
    // An earlier version uploaded it; the skip must shield it from mirror deletion.
    fixture
        .server
        .add_file("/team/e3/~WRL0001.tmp", b"old autosave", 1_700_000_000);

    let output = fixture.sync("/team/e3", &["--delete"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let document = json(&output);
    assert_eq!(document["result"]["outcome"], "complete");
    assert!(has(
        &deferred(&document),
        "~WRL0001.tmp",
        "unsupported-entry"
    ));
    assert_eq!(fixture.count(UPLOAD), 5);
    assert_eq!(fixture.count(DELETE), 0, "a skipped path is never deleted");
    assert!(fixture.text("/team/e3/~WRL0001.tmp").is_some());
}

#[test]
fn a_path_too_long_with_the_destination_prefix_is_skipped_not_fatal() {
    let fixture = Fixture::new("adv-e3-deep");
    for index in 0..3 {
        fixture.write(&format!("f{index}.txt"), b"payload");
    }
    let deep = format!("{}/{}/{}", "a".repeat(70), "b".repeat(70), "c".repeat(70));
    let relative = format!("{deep}/report-2026-final.pdf");
    if fs::create_dir_all(fixture.source.join(&deep)).is_err()
        || fs::write(fixture.source.join(&relative), b"deep").is_err()
    {
        eprintln!("skipping: this host cannot create the deep fixture");
        return;
    }
    fixture.server.add_directory("/team/Backups");
    fixture.server.add_directory("/team/Backups/Laptop-2026");

    let output = fixture.sync("/team/Backups/Laptop-2026", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        has(&deferred(&json(&output)), &relative, "unsupported-entry"),
        "{}",
        describe(&output)
    );
    assert_eq!(fixture.count(UPLOAD), 3);
}

/// 90 CJK characters are 270 UTF-8 bytes: within 255 characters, over ext4's 255-byte NAME_MAX.
#[test]
fn a_name_over_255_bytes_is_skipped_by_name() {
    let fixture = Fixture::new("adv-e3-bytes");
    let name = format!("m-{}.txt", "\u{65e5}".repeat(90));
    fixture.write("a.txt", b"a");
    // ext4 and btrfs refuse the name outright (that is the limit being enforced); NTFS and APFS,
    // which count characters, hold it.
    if fs::write(fixture.source.join(&name), b"m").is_err() {
        eprintln!("skipping: this filesystem cannot hold a 270-byte name");
        return;
    }
    fixture.write("z.txt", b"z");
    fixture.server.add_directory("/team/e3b");
    let output = fixture.sync("/team/e3b", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(has(&deferred(&json(&output)), &name, "unsupported-entry"));
    assert!(fixture.text("/team/e3b/z.txt").is_some());
}

/// DSM refuses one upload by name (418): that file is a `remote-refused` deferral naming the path
/// and the cause, its peers still upload, and the run completes with that warning.
#[test]
fn a_name_dsm_refuses_is_a_deferral_and_peers_upload() {
    let fixture = Fixture::new("adv-e3-dsm-refuses");
    let name = format!("m-{}.txt", "\u{65e5}".repeat(60));
    fixture.write("a.txt", b"a");
    fixture.write(&name, b"m");
    fixture.write("z.txt", b"z");
    fixture.server.add_directory("/team/e3r");
    fixture.server.refuse_names_over_bytes(100, 418);

    let output = fixture.sync("/team/e3r", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let document = json(&output);
    assert_eq!(document["result"]["outcome"], "complete");
    assert!(
        has(&deferred(&document), &name, "remote-refused"),
        "{}",
        describe(&output)
    );
    let detail = document["result"]["deferred"][0]["detail"]
        .as_str()
        .unwrap_or_default();
    assert!(detail.contains("418"), "{detail}");
    assert!(detail.contains(&name), "the error names the path: {detail}");
    assert!(fixture.text("/team/e3r/a.txt").is_some());
    assert!(fixture.text("/team/e3r/z.txt").is_some());
}

/// A refusal that is really about the share (every name refused for one cause) fails the run,
/// once, naming the cause and the paths, instead of reporting a run of deferrals.
#[test]
fn a_share_wide_refusal_fails_the_run() {
    let fixture = Fixture::new("adv-e3-breaker");
    for index in 0..4 {
        fixture.write(&format!("m-{index}-{}.txt", "\u{65e5}".repeat(40)), b"m");
    }
    fixture.server.add_directory("/team/e3x");
    fixture.server.refuse_names_over_bytes(10, 418);

    let output = fixture.sync("/team/e3x", &[]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("for one cause"), "{stderr}");
    assert!(stderr.contains(":418"), "{stderr}");
}

/// DSM refuses one folder by name: its subtree is withheld, everything else uploads.
#[test]
fn a_folder_dsm_refuses_withholds_its_subtree_only() {
    let fixture = Fixture::new("adv-e3-dsm-refuses-folder");
    let folder = format!("d-{}", "\u{65e5}".repeat(60));
    fixture.write("a.txt", b"a");
    fixture.write(&format!("{folder}/inner.txt"), b"inner");
    fixture.write("z.txt", b"z");
    fixture.server.add_directory("/team/e3f");
    fixture.server.refuse_names_over_bytes(100, 418);

    let output = fixture.sync("/team/e3f", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        has(&deferred(&json(&output)), &folder, "remote-refused"),
        "{}",
        describe(&output)
    );
    assert!(fixture.text("/team/e3f/a.txt").is_some());
    assert!(fixture.text("/team/e3f/z.txt").is_some());
}

// ---------------------------------------------------------------------------------------------
// ADV-E-4 / ADV-E-5: protected entries no longer fail runs or pin folders silently
// ---------------------------------------------------------------------------------------------

#[test]
fn a_type_conflict_over_a_protected_child_is_a_skip_and_the_run_proceeds() {
    let fixture = Fixture::new("adv-e4");
    fixture.write("photos", b"now a file");
    fixture.write("other.txt", b"payload");
    let server = &fixture.server;
    server.add_directory("/team/e4");
    server.add_directory("/team/e4/photos");
    server.add_file("/team/e4/photos/a.jpg", b"jpeg", 1_700_000_000);
    server.add_file("/team/e4/photos/.DS_Store", b"finder", 1_700_000_000);

    let output = fixture.sync("/team/e4", &["--delete"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(has(&skipped(&json(&output)), "photos", "type-conflict"));
    assert!(fixture.text("/team/e4/other.txt").is_some());
    assert_eq!(
        fixture.count(DELETE),
        0,
        "nothing in the conflicting folder is deleted"
    );
    assert!(fixture.text("/team/e4/photos/a.jpg").is_some());
}

#[test]
fn a_type_conflict_without_delete_is_a_skip_and_the_run_proceeds() {
    let fixture = Fixture::new("adv-e4-nodelete");
    fixture.write("photos", b"now a file");
    fixture.write("other.txt", b"payload");
    let server = &fixture.server;
    server.add_directory("/team/e4n");
    server.add_directory("/team/e4n/photos");
    server.add_file("/team/e4n/photos/a.jpg", b"jpeg", 1_700_000_000);

    let output = fixture.sync("/team/e4n", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(has(&skipped(&json(&output)), "photos", "type-conflict"));
    assert!(fixture.text("/team/e4n/other.txt").is_some());
    assert!(fixture.text("/team/e4n/photos/a.jpg").is_some());
}

#[test]
fn a_folder_kept_for_a_protected_entry_is_named() {
    let fixture = Fixture::new("adv-e5");
    fixture.write("keep.txt", b"payload");
    let server = &fixture.server;
    server.add_directory("/team/e5");
    server.add_directory("/team/e5/Old");
    server.add_file("/team/e5/keep.txt", b"payload", 1_600_000_000);
    server.add_file("/team/e5/Old/a.jpg", b"jpeg", 1_700_000_000);
    server.add_file("/team/e5/Old/.DS_Store", b"finder", 1_700_000_000);

    let output = fixture.sync("/team/e5", &["--delete"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let document = json(&output);
    assert!(
        has(&skipped(&document), "Old", "protected-descendant"),
        "{}",
        describe(&output)
    );
    assert_eq!(document["result"]["skipped"], 1);
    assert!(fixture.text("/team/e5/Old/a.jpg").is_none());
    assert!(fixture.text("/team/e5/Old/.DS_Store").is_some());
}

// ---------------------------------------------------------------------------------------------
// ADV-E-6: lasting conditions are lasting
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
#[test]
fn a_file_this_account_may_not_read_is_lasting_not_partial() {
    let fixture = Fixture::new("adv-e6");
    fixture.write("ok.txt", b"payload");
    let locked = fixture.source.join("secret.txt");
    fs::write(&locked, b"secret").unwrap();
    let user = std::env::var("USERNAME").unwrap_or_default();
    let denied = Command::new("icacls")
        .arg(&locked)
        .args(["/deny", &format!("{user}:(R)")])
        .output()
        .is_ok_and(|output| output.status.success());
    if !denied {
        eprintln!("skipping: icacls deny failed");
        return;
    }
    fixture.server.add_directory("/team/e6");
    let mut results = Vec::new();
    for _ in 0..2 {
        results.push(fixture.sync("/team/e6", &[]));
    }
    let _ = Command::new("icacls")
        .arg(&locked)
        .args(["/remove:d", &user])
        .output();
    for output in &results {
        assert_eq!(output.status.code(), Some(0), "{}", describe(output));
        assert!(
            has(
                &deferred(&json(output)),
                "secret.txt",
                "local-permission-denied"
            ),
            "{}",
            describe(output)
        );
    }
    assert!(fixture.text("/team/e6/ok.txt").is_some());
}

/// A delete DSM refuses for permission (407) recurs every run: lasting, not partial forever.
#[test]
fn a_delete_dsm_refuses_for_permission_is_lasting() {
    let fixture = Fixture::new("adv-e6-delete");
    fixture.write("keep.txt", b"payload");
    let server = &fixture.server;
    server.add_directory("/team/e6d");
    server.add_file("/team/e6d/keep.txt", b"payload", 1_600_000_000);
    server.add_file("/team/e6d/gone.txt", b"old", 1_600_000_000);
    server.fail_next_api_operation(DELETE, 407);

    let output = fixture.sync("/team/e6d", &["--delete"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        has(
            &deferred(&json(&output)),
            "gone.txt",
            "delete-not-permitted"
        ),
        "{}",
        describe(&output)
    );
    assert!(fixture.text("/team/e6d/gone.txt").is_some());
}

// ---------------------------------------------------------------------------------------------
// ADV-E-8: content mode does not re-upload identical bytes because the time moved
// ---------------------------------------------------------------------------------------------

#[test]
fn content_mode_does_not_reupload_identical_bytes_whose_time_moved() {
    let fixture = Fixture::new("adv-e8");
    let body = vec![3_u8; 512 * 1024];
    fixture.write("big.bin", &body);
    let local_seconds = fs::metadata(fixture.source.join("big.bin"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    fixture.server.add_directory("/team/e8");
    fixture
        .server
        .add_file("/team/e8/big.bin", &body, local_seconds - 3600);

    let output = fixture.sync("/team/e8", &["--compare", "content"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fixture.count(UPLOAD), 0, "{}", describe(&output));

    // A real difference of the same size is still uploaded.
    fixture.write("big.bin", &vec![4_u8; 512 * 1024]);
    let changed = fixture.sync("/team/e8", &["--compare", "content"]);
    assert_eq!(changed.status.code(), Some(0), "{}", describe(&changed));
    assert_eq!(fixture.count(UPLOAD), 1);
}

// ---------------------------------------------------------------------------------------------
// ADV-E-9: a dead session in the delete phase fails the run once
// ---------------------------------------------------------------------------------------------

/// A regression guard rather than a reproduction: end to end, the `getinfo` that re-checks each
/// deletion's snapshot already meets the dead session and stops the run, on 4205381 as now. The
/// delete loop's own handling is pinned by
/// `sync::tests::a_dead_session_in_the_delete_phase_stops_at_the_first_deletion`.
#[test]
fn a_session_lost_during_deletes_fails_the_run_once() {
    let fixture = Fixture::new("adv-e9");
    fixture.write("keep.txt", b"payload");
    let server = &fixture.server;
    server.add_directory("/team/e9");
    server.add_file("/team/e9/keep.txt", b"payload", 1_600_000_000);
    for index in 0..4 {
        server.add_file(&format!("/team/e9/gone{index}.txt"), b"old", 1_600_000_000);
    }
    // The session dies on the first delete and every later login is refused.
    let password = fixture.password.clone();
    server.on_next_request(DELETE, move || {
        fs::write(&password, b"not the password\n").unwrap();
    });
    server.expire_session_on(DELETE);

    let output = fixture.sync("/team/e9", &["--delete"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let attempts = fixture.count(DELETE);
    assert!(
        attempts <= 2,
        "attempted {attempts} deletes on a dead session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("delete-failed"), "{stderr}");
}

// ---------------------------------------------------------------------------------------------
// ADV-E-10: an excluded local spelling protects its NAS case variant
// ---------------------------------------------------------------------------------------------

/// ADV-E-10 (verify-core's probe): on a source that resolves names regardless of case, an exclude
/// covers every spelling. `--exclude report.pdf --delete` never deletes the NAS-only `Report.pdf`.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn an_exclude_protects_every_nas_spelling_from_deletion() {
    let fixture = Fixture::new("adv-e10");
    fixture.write("keep.txt", b"payload");
    let server = &fixture.server;
    server.add_directory("/team/e10");
    server.add_file("/team/e10/Report.pdf", b"the only backup", 1_600_000_000);
    server.add_file("/team/e10/keep.txt", b"payload", 1_600_000_000);

    let output = fixture.sync("/team/e10", &["--delete", "--exclude", "report.pdf"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fixture.count(DELETE), 0, "{}", describe(&output));
    assert_eq!(
        fixture.text("/team/e10/Report.pdf").as_deref(),
        Some("the only backup")
    );
}

/// ADV-E-10, the other direction: a local `Report.pdf` is never uploaded under
/// `--exclude report.pdf` on a source that resolves names regardless of case.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn an_exclude_keeps_every_local_spelling_from_upload() {
    let fixture = Fixture::new("adv-e10-upload");
    fixture.write("Report.pdf", b"excluded");
    fixture.write("keep.txt", b"payload");
    fixture.server.add_directory("/team/e10u");

    let output = fixture.sync("/team/e10u", &["--exclude", "report.pdf"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(fixture.text("/team/e10u/Report.pdf").is_none());
    assert!(fixture.text("/team/e10u/keep.txt").is_some());
}

/// A local folder where the NAS holds a symbolic link to another folder: nothing is written
/// through the link, its target is untouched, the path is named, and the rest of the run proceeds.
/// It used to fail every run.
#[test]
fn a_local_folder_at_a_nas_link_is_skipped_by_name() {
    let fixture = Fixture::new("adv-link");
    fixture.write("linked/x.txt", b"local");
    fixture.write("other.txt", b"payload");
    let server = &fixture.server;
    server.add_directory("/team/elsewhere");
    server.add_file("/team/elsewhere/target.txt", b"target", 1_600_000_000);
    server.add_directory("/team/el");
    server.add_directory("/team/el/linked");
    server.add_directory_symlink("/team/el/linked", "/team/elsewhere");

    let output = fixture.sync("/team/el", &["--delete"]);
    let document = json(&output);
    assert!(
        has(&skipped(&document), "linked", "remote-link"),
        "{}",
        describe(&output)
    );
    assert_ne!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(fixture.text("/team/el/other.txt").is_some());
    assert_eq!(
        fixture.text("/team/elsewhere/target.txt").as_deref(),
        Some("target")
    );
    assert!(fixture.text("/team/elsewhere/x.txt").is_none());
    assert_eq!(fixture.count(DELETE), 0);
}

/// A NAS whose real paths do not have the shape the walk expects makes almost every folder look
/// like a link. That is an API-shape mismatch, not a tree of links: the run fails, once, with the
/// named cause, instead of skipping the whole tree folder by folder.
#[test]
fn a_tree_of_link_boundaries_fails_the_run_with_its_cause() {
    let fixture = Fixture::new("adv-link-breaker");
    let server = &fixture.server;
    server.add_directory("/team/elsewhere");
    server.add_directory("/team/lb");
    for name in ["a", "b", "c", "d"] {
        fixture.write(&format!("{name}/x.txt"), b"local");
        server.add_directory(&format!("/team/lb/{name}"));
        server.add_directory_symlink(&format!("/team/lb/{name}"), "/team/elsewhere");
    }

    let output = fixture.sync("/team/lb", &["--delete"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("remote-real-path-unusable"), "{stderr}");
    assert_eq!(fixture.count(UPLOAD), 0);
    assert_eq!(fixture.count(DELETE), 0);
}

/// The mock answers real paths the way DSM does, under the volume (`/volume1/team/...`). An
/// ordinary tree read that way is folders, not links: everything syncs and nothing is skipped.
#[test]
fn a_volume_prefixed_real_path_is_not_a_link() {
    let fixture = Fixture::new("adv-link-normal");
    for name in ["a", "b", "c", "d"] {
        fixture.write(&format!("{name}/x.txt"), b"local");
        fixture.write(&format!("{name}/deeper/y.txt"), b"local");
    }
    let server = &fixture.server;
    server.add_directory("/team/ln");
    for name in ["a", "b", "c", "d"] {
        server.add_directory(&format!("/team/ln/{name}"));
        server.add_file(&format!("/team/ln/{name}/old.txt"), b"old", 1_600_000_000);
    }

    let output = fixture.sync("/team/ln", &["--delete"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let document = json(&output);
    assert!(skipped(&document).is_empty(), "{}", describe(&output));
    assert!(deferred(&document).is_empty(), "{}", describe(&output));
    assert_eq!(fixture.count(UPLOAD), 8);
    assert!(
        fixture.text("/team/ln/a/old.txt").is_none(),
        "mirrored as usual"
    );
    // The listings carried volume-prefixed real paths, which is what made the guard run at all.
    assert!(
        server.requests().iter().any(|request| request
            .fields
            .values()
            .any(|value| value.contains("real_path"))),
        "the walk asked for real paths"
    );
}

/// The breaker on the shape main asked for: a DSM whose `real_path` answers do not match the
/// listing for any folder. The run fails with the named cause, uploads nothing and deletes nothing.
#[test]
fn misreported_real_paths_fail_the_run_with_their_cause() {
    let fixture = Fixture::new("adv-real-path-misreported");
    let server = &fixture.server;
    server.add_directory("/team/mr");
    for name in ["a", "b", "c", "d"] {
        fixture.write(&format!("{name}/x.txt"), b"local");
        server.add_directory(&format!("/team/mr/{name}"));
        server.add_file(&format!("/team/mr/{name}/old.txt"), b"old", 1_600_000_000);
    }
    server.misreport_real_paths();

    let output = fixture.sync("/team/mr", &["--delete"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("remote-real-path-unusable"), "{stderr}");
    assert_eq!(fixture.count(UPLOAD), 0);
    assert_eq!(fixture.count(DELETE), 0);
    assert!(fixture.text("/team/mr/a/old.txt").is_some());
}
