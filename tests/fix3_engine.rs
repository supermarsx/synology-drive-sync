//! Fix round 3, engine: the auditors' data-loss probes, inverted. Each test pins the safe outcome
//! of one finding (D3-2..D3-7, V3-1, V3-2, D3-3) end to end, through the real binary and the File
//! Station mock.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";

fn run_in(environment: &TestDir, arguments: &[&str]) -> Output {
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

fn describe(output: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not JSON ({error}); {}", describe(output)))
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

fn mtime_seconds(path: &Path) -> i64 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A result counter; a run with nothing to do reports only `changed: false`.
fn count(document: &Value, field: &str) -> u64 {
    let result = &document["result"];
    match result[field].as_u64() {
        Some(value) => value,
        None if result["changed"] == false => 0,
        None => panic!("no result.{field}: {document}"),
    }
}

/// The run's outcome; a run with nothing to do is complete.
#[cfg_attr(not(unix), allow(dead_code))]
fn outcome(document: &Value) -> String {
    let result = &document["result"];
    match result["outcome"].as_str() {
        Some(value) => value.to_owned(),
        None if result["changed"] == false => "complete".to_owned(),
        None => panic!("no result.outcome: {document}"),
    }
}

/// `(reason, path)` of every deferral the run reported.
fn deferred(document: &Value) -> Vec<(String, String)> {
    if document["result"]["changed"] == false && document["result"]["deferred"].is_null() {
        return Vec::new();
    }
    document["result"]["deferred"]
        .as_array()
        .unwrap_or_else(|| panic!("no deferred array: {document}"))
        .iter()
        .map(|record| {
            (
                record["reason"].as_str().unwrap_or_default().to_owned(),
                record["path"]
                    .as_str()
                    .or_else(|| record["relative"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

/// `(reason, path)` of every skip the plan reported.
#[cfg_attr(not(unix), allow(dead_code))]
fn plan_skips(document: &Value) -> Vec<(String, String)> {
    document["plan"]["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("no plan.skipped array: {document}"))
        .iter()
        .map(|record| {
            (
                record["reason"].as_str().unwrap_or_default().to_owned(),
                record["path"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

struct Fixture {
    dir: TestDir,
    source: PathBuf,
    password: PathBuf,
    server: MockFileStation,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = TestDir::new(label);
        let source = dir.child("source");
        fs::create_dir_all(&source).unwrap();
        let password = dir.write("password", PASSWORD);
        Self {
            dir,
            source,
            password,
            server: MockFileStation::start(),
        }
    }

    fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.source.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn sync(&self, destination: &str, extra: &[&str]) -> Output {
        let mut arguments = vec![
            "--quiet",
            "--output",
            "json",
            "sync",
            text(&self.source),
            destination,
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            text(&self.password),
            "--no-vault",
            "--allow-http",
            "--jobs",
            "1",
        ];
        arguments.extend_from_slice(extra);
        let environment = TestDir::new("fix3-env");
        run_in(&environment, &arguments)
    }

    fn text(&self, path: &str) -> Option<String> {
        self.server
            .file_contents(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// D3-2 (S1): an exclude spelled the way the source now spells a case-renamed folder protects the
/// NAS-only content under the folder's old NAS spelling. Nothing is deleted near it, the rename
/// waits as the lasting `case-rename-pending`, and the run is exit 0.
#[cfg(unix)]
#[test]
fn an_exclude_on_the_new_spelling_keeps_the_nas_only_content_under_the_old_one() {
    let fixture = Fixture::new("fix3-excl");
    let keep = fixture.write("photos/keep.jpg", b"keep");
    fixture.server.add_directory("/team/excl");
    fixture.server.add_directory("/team/excl/Photos");
    fixture.server.add_directory("/team/excl/Photos/raw");
    fixture
        .server
        .add_file("/team/excl/Photos/keep.jpg", b"keep", mtime_seconds(&keep));
    fixture.server.add_file(
        "/team/excl/Photos/raw/IMG_0001.CR2",
        b"the only copy of the raw",
        1_700_000_000,
    );
    fixture.server.add_file(
        "/team/excl/Photos/raw/IMG_0002.CR2",
        b"another raw",
        1_700_000_000,
    );
    let output = fixture.sync(
        "/team/excl",
        &[
            "--delete",
            "--compare",
            "metadata",
            "--exclude",
            "photos/raw/",
        ],
    );
    let document = json(&output);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        fixture
            .text("/team/excl/Photos/raw/IMG_0001.CR2")
            .as_deref(),
        Some("the only copy of the raw")
    );
    assert!(fixture.text("/team/excl/Photos/raw/IMG_0002.CR2").is_some());
    assert!(
        fixture
            .text("/team/excl/Photos/keep.jpg")
            .or_else(|| fixture.text("/team/excl/photos/keep.jpg"))
            .is_some()
    );
    assert_eq!(count(&document, "deleted"), 0, "{}", describe(&output));
    assert_eq!(count(&document, "renamed"), 0, "{}", describe(&output));
    assert!(
        deferred(&document)
            .iter()
            .any(|(reason, path)| reason == "case-rename-pending" && path == "photos"),
        "{}",
        describe(&output)
    );
}

/// V3-1: the inverse of D3-2. The exclude names the NAS (old) spelling; the local content under the
/// new spelling is excluded with it, so nothing is uploaded into the NAS folder the remote walk
/// excluded. Two runs, both exit 0 with `case-rename-pending`, and the NAS raw is untouched.
#[test]
fn an_exclude_on_the_old_spelling_excludes_the_local_content_under_the_new_one() {
    let fixture = Fixture::new("fix3-inverse");
    let keep = fixture.write("photos/keep.jpg", b"keep");
    fixture.write("photos/raw/IMG_0001.CR2", b"local raw, different");
    fixture.server.add_directory("/team/inv");
    fixture.server.add_directory("/team/inv/Photos");
    fixture.server.add_directory("/team/inv/Photos/raw");
    fixture
        .server
        .add_file("/team/inv/Photos/keep.jpg", b"keep", mtime_seconds(&keep));
    fixture.server.add_file(
        "/team/inv/Photos/raw/IMG_0001.CR2",
        b"the NAS raw",
        1_700_000_000,
    );
    for run in 0..2 {
        let output = fixture.sync(
            "/team/inv",
            &[
                "--delete",
                "--compare",
                "metadata",
                "--exclude",
                "Photos/raw/",
            ],
        );
        let document = json(&output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "run {run}: {}",
            describe(&output)
        );
        assert_eq!(
            count(&document, "uploaded"),
            0,
            "run {run}: {}",
            describe(&output)
        );
        assert_eq!(count(&document, "deleted"), 0, "run {run}");
        if cfg!(unix) {
            // On a case-sensitive source the two spellings are a rename the excluded folder
            // blocks; a folding source reads them as one name and has nothing to rename.
            assert!(
                deferred(&document)
                    .iter()
                    .any(|(reason, path)| reason == "case-rename-pending" && path == "photos"),
                "run {run}: {}",
                describe(&output)
            );
        }
        assert_eq!(
            fixture.text("/team/inv/Photos/raw/IMG_0001.CR2").as_deref(),
            Some("the NAS raw"),
            "run {run}"
        );
    }
}

/// V3-2 (S1): an exclude typed decomposed (pasted from a macOS path) protects the NAS's composed
/// folder on a case-sensitive source: its NAS-only archive survives a mirror run, counted as
/// protected.
#[cfg(unix)]
#[test]
fn a_decomposed_exclude_protects_the_composed_nas_folder() {
    let fixture = Fixture::new("fix3-nfdx");
    let keep = fixture.write("cafe\u{301}/keep.txt", b"keep");
    fixture.server.add_directory("/team/nfdx");
    fixture.server.add_directory("/team/nfdx/caf\u{e9}");
    fixture.server.add_directory("/team/nfdx/caf\u{e9}/archive");
    fixture.server.add_file(
        "/team/nfdx/caf\u{e9}/keep.txt",
        b"keep",
        mtime_seconds(&keep),
    );
    fixture.server.add_file(
        "/team/nfdx/caf\u{e9}/archive/only.txt",
        b"only copy",
        1_700_000_000,
    );
    let output = fixture.sync(
        "/team/nfdx",
        &[
            "--delete",
            "--compare",
            "metadata",
            "--exclude",
            "cafe\u{301}/archive/",
        ],
    );
    let document = json(&output);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        fixture
            .text("/team/nfdx/caf\u{e9}/archive/only.txt")
            .as_deref(),
        Some("only copy")
    );
    assert_eq!(count(&document, "deleted"), 0, "{}", describe(&output));
    assert!(
        document["plan"]["summary"]["protected_entries"]
            .as_u64()
            .is_some_and(|count| count >= 1),
        "{}",
        describe(&output)
    );
}

/// D3-4: an exclude typed composed excludes the decomposed local folder on a case-sensitive
/// source. Its content is never uploaded, and three runs in a row are exit 0, not partial forever.
#[cfg(unix)]
#[test]
fn a_composed_exclude_excludes_the_decomposed_local_folder() {
    let fixture = Fixture::new("fix3-nfcx");
    fixture.write("cafe\u{301}/keep.txt", b"keep");
    fixture.write("cafe\u{301}/secret/passwords.txt", b"hunter2");
    fixture.server.add_directory("/team/nfcx");
    fixture.server.add_directory("/team/nfcx/caf\u{e9}");
    for run in 0..3 {
        let output = fixture.sync(
            "/team/nfcx",
            &[
                "--delete",
                "--compare",
                "metadata",
                "--exclude",
                "caf\u{e9}/secret/",
            ],
        );
        let document = json(&output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "run {run}: {}",
            describe(&output)
        );
        assert_eq!(outcome(&document), "complete", "run {run}");
    }
    let files = fixture.server.file_paths();
    assert!(
        !files.iter().any(|path| path.contains("passwords")),
        "{files:?}"
    );
    assert!(
        files.iter().any(|path| path.ends_with("/keep.txt")),
        "{files:?}"
    );
}

/// D3-5: on a case-sensitive source a skipped spelling no longer silently shields its case-variant
/// sibling. `Notes.txt` is named as a `local-case-collision` with the skipped `notes.txt`, and is
/// neither uploaded nor deleted.
#[cfg(unix)]
#[test]
fn a_skipped_spelling_names_its_case_variant_sibling() {
    let fixture = Fixture::new("fix3-shadow");
    if source_folds_names(&fixture) {
        eprintln!(
            "skipped: {} resolves names regardless of case (macOS APFS, for one), so it cannot \
             hold notes.txt beside Notes.txt; the rule itself is covered by \
             local::tests::a_skipped_spelling_makes_its_case_variant_sibling_a_named_collision",
            fixture.source.display()
        );
        return;
    }
    fixture.write("Notes.txt", b"the real notes");
    fixture.write("target.txt", b"link target");
    std::os::unix::fs::symlink("target.txt", fixture.source.join("notes.txt")).unwrap();
    fixture.server.add_directory("/team/shadow");
    fixture
        .server
        .add_file("/team/shadow/Notes.txt", b"older notes", 1_700_000_000);
    let output = fixture.sync("/team/shadow", &["--delete"]);
    let document = json(&output);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let skips = plan_skips(&document);
    assert!(
        skips
            .iter()
            .any(|(reason, path)| reason == "local-case-collision" && path == "Notes.txt"),
        "{skips:?}"
    );
    assert_eq!(
        fixture.text("/team/shadow/Notes.txt").as_deref(),
        Some("older notes"),
        "the NAS copy is neither replaced nor deleted"
    );
}

/// Whether the fixture's source resolves names regardless of case, asked the way the sync asks
/// it: the ignore rules' own probe of the source.
#[cfg(unix)]
fn source_folds_names(fixture: &Fixture) -> bool {
    synology_drive_sync::local::IgnoreRules::build(&fixture.source, &[])
        .expect("probe the fixture source")
        .folds_names()
}

/// D3-5 on a source that folds case but keeps Unicode normalisations apart (NTFS): a skipped
/// decomposed `café.txt` (a SYSTEM file) names its composed sibling as a `local-case-collision`
/// rather than silently shielding it, and the NAS copy is left alone.
#[cfg(windows)]
#[test]
fn a_skipped_spelling_names_its_normalisation_variant_sibling() {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    let fixture = Fixture::new("fix3-shadow-nfd");
    fixture.write("caf\u{e9}.txt", b"the real notes");
    let mut system = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .attributes(FILE_ATTRIBUTE_SYSTEM)
        .open(fixture.source.join("cafe\u{301}.txt"))
        .expect("NTFS keeps a decomposed name beside its composed twin");
    std::io::Write::write_all(&mut system, b"system file").unwrap();
    drop(system);
    fixture.server.add_directory("/team/shadow-nfd");
    fixture.server.add_file(
        "/team/shadow-nfd/caf\u{e9}.txt",
        b"older notes",
        1_700_000_000,
    );
    let output = fixture.sync("/team/shadow-nfd", &["--delete"]);
    let document = json(&output);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let skips = plan_skips(&document);
    assert!(
        skips
            .iter()
            .any(|(reason, path)| reason == "local-case-collision" && path == "caf\u{e9}.txt"),
        "{skips:?}"
    );
    assert_eq!(
        fixture.text("/team/shadow-nfd/caf\u{e9}.txt").as_deref(),
        Some("older notes"),
        "the NAS copy is neither replaced nor deleted"
    );
}

/// D3-6: the reappeared-file guard holds under a folder the plan pairs by Unicode normalisation.
/// A file recreated during the run keeps its NAS copy, which is then refreshed, exactly as the
/// ASCII control does.
#[cfg(unix)]
#[test]
fn a_file_recreated_under_a_decomposed_paired_folder_keeps_its_nas_copy() {
    for (local_dir, remote_dir, label) in [
        ("cafe\u{301}", "caf\u{e9}", "nfd"),
        ("plain", "plain", "ascii"),
    ] {
        let fixture = Fixture::new(&format!("fix3-nfd-{label}"));
        let keep = fixture.write(&format!("{local_dir}/keep.txt"), b"keep");
        fixture.write("new.txt", b"new");
        fixture.server.add_directory("/team/nfd");
        fixture
            .server
            .add_directory(&format!("/team/nfd/{remote_dir}"));
        fixture.server.add_file(
            &format!("/team/nfd/{remote_dir}/keep.txt"),
            b"keep",
            mtime_seconds(&keep),
        );
        fixture.server.add_file(
            &format!("/team/nfd/{remote_dir}/gone.txt"),
            b"v1",
            1_700_000_000,
        );
        let recreated = fixture.source.join(local_dir).join("gone.txt");
        fixture
            .server
            .on_next_request("SYNO.FileStation.Upload.upload", move || {
                fs::write(&recreated, b"v2 written during the run").unwrap();
            });
        let output = fixture.sync("/team/nfd", &["--delete", "--compare", "metadata"]);
        let document = json(&output);
        assert_eq!(
            fixture
                .text(&format!("/team/nfd/{remote_dir}/gone.txt"))
                .as_deref(),
            Some("v2 written during the run"),
            "[{label}] {}",
            describe(&output)
        );
        assert_eq!(count(&document, "deleted"), 0, "[{label}]");
        assert_eq!(count(&document, "kept_deletes_uploaded"), 1, "[{label}]");
        assert_eq!(
            output.status.code(),
            Some(0),
            "[{label}] {}",
            describe(&output)
        );
    }
}

/// F-5: a remote-only leaf recreated locally under another Unicode normalisation of its own name,
/// beneath a normalisation-paired folder, is present: the deletion is kept.
#[cfg(unix)]
#[test]
fn a_leaf_recreated_decomposed_under_a_paired_folder_keeps_its_deletion() {
    let fixture = Fixture::new("fix3-nfd-leaf");
    let keep = fixture.write("cafe\u{301}/keep.txt", b"keep");
    fixture.write("new.txt", b"new");
    fixture.server.add_directory("/team/leaf");
    fixture.server.add_directory("/team/leaf/caf\u{e9}");
    fixture.server.add_file(
        "/team/leaf/caf\u{e9}/keep.txt",
        b"keep",
        mtime_seconds(&keep),
    );
    fixture
        .server
        .add_file("/team/leaf/caf\u{e9}/na\u{ef}ve.txt", b"v1", 1_700_000_000);
    let recreated = fixture.source.join("cafe\u{301}").join("nai\u{308}ve.txt");
    fixture
        .server
        .on_next_request("SYNO.FileStation.Upload.upload", move || {
            fs::write(&recreated, b"v2").unwrap();
        });
    let output = fixture.sync("/team/leaf", &["--delete", "--compare", "metadata"]);
    let document = json(&output);
    assert_eq!(count(&document, "deleted"), 0, "{}", describe(&output));
    assert!(
        fixture.server.file_paths().iter().any(|path| path
            .strip_prefix("/team/leaf/")
            .is_some_and(|rest| rest.ends_with(".txt") && rest.contains("ve.txt"))),
        "{:?}",
        fixture.server.file_paths()
    );
}

/// D3-7: a kept deletion under a case-renamed folder is uploaded from the local spelling, so the
/// NAS holds the new content, as the control does.
#[cfg(unix)]
#[test]
fn a_kept_deletion_under_a_case_renamed_folder_is_uploaded() {
    for (local_dir, remote_dir, label) in
        [("photos", "Photos", "renamed"), ("same", "same", "control")]
    {
        let fixture = Fixture::new(&format!("fix3-kept-{label}"));
        let a = fixture.write(&format!("{local_dir}/a.jpg"), b"jpeg-a");
        fixture.write("new.txt", b"new");
        fixture.server.add_directory("/team/kept");
        fixture
            .server
            .add_directory(&format!("/team/kept/{remote_dir}"));
        fixture.server.add_file(
            &format!("/team/kept/{remote_dir}/a.jpg"),
            b"jpeg-a",
            mtime_seconds(&a),
        );
        fixture.server.add_file(
            &format!("/team/kept/{remote_dir}/gone.jpg"),
            b"old",
            1_700_000_000,
        );
        let recreated = fixture.source.join(local_dir).join("gone.jpg");
        fixture
            .server
            .on_next_request("SYNO.FileStation.Upload.upload", move || {
                fs::write(&recreated, b"new content written during the run").unwrap();
            });
        let output = fixture.sync("/team/kept", &["--delete", "--compare", "metadata"]);
        let document = json(&output);
        assert_eq!(
            count(&document, "kept_deletes_uploaded"),
            1,
            "[{label}] {}",
            describe(&output)
        );
        assert_eq!(
            fixture
                .text(&format!("/team/kept/{local_dir}/gone.jpg"))
                .as_deref(),
            Some("new content written during the run"),
            "[{label}] {:?}",
            fixture.server.file_paths()
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "[{label}] {}",
            describe(&output)
        );
    }
}

/// I-1, monotone, on every source: a negation the walks honour regardless of normalisation (or,
/// on a folding source, of case) must not expose a NAS-only name the rules as typed exclude.
/// `*.txt` + `!café.txt` (typed composed) with a NAS-only decomposed `café.txt`, and `*.txt` +
/// `!Report.txt` with a NAS-only `report.txt`, under `--delete`: both survive. On macOS (a folding
/// source) the decomposed file used to be deleted, as on Windows the lower-case one.
#[test]
fn a_composed_negation_does_not_expose_a_decomposed_nas_only_name() {
    for (label, negation, remote_only) in [
        ("nfd", "!caf\u{e9}.txt", "cafe\u{301}.txt"),
        ("case", "!Report.txt", "report.txt"),
    ] {
        let fixture = Fixture::new(&format!("fix3-mono-{label}"));
        fixture.write("keep.bin", b"keep");
        fixture.server.add_directory("/team/mono");
        fixture.server.add_file(
            &format!("/team/mono/{remote_only}"),
            b"only copy",
            1_700_000_000,
        );
        fixture
            .server
            .add_file("/team/mono/stale.bin", b"stale", 1_700_000_000);
        let output = fixture.sync(
            "/team/mono",
            &[
                "--delete",
                "--compare",
                "metadata",
                "--exclude",
                "*.txt",
                "--exclude",
                negation,
            ],
        );
        let document = json(&output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "[{label}] {}",
            describe(&output)
        );
        assert_eq!(
            fixture
                .text(&format!("/team/mono/{remote_only}"))
                .as_deref(),
            Some("only copy"),
            "[{label}] {}",
            describe(&output)
        );
        // The mirror itself still runs: only the protected name is kept.
        assert!(fixture.text("/team/mono/stale.bin").is_none(), "[{label}]");
        assert_eq!(count(&document, "deleted"), 1, "[{label}]");
    }
}

fn overlap_config(
    fixture: &Fixture,
    outer_delete: bool,
    inner_delete: bool,
    inner_url: &str,
) -> PathBuf {
    let outer = fixture.dir.child("outer");
    let inner = fixture.dir.child("inner");
    fs::create_dir_all(&outer).unwrap();
    fs::create_dir_all(&inner).unwrap();
    fs::write(outer.join("shared.txt"), b"outer").unwrap();
    fs::write(inner.join("thesis.docx"), b"the only NAS copy").unwrap();
    let config = format!(
        r#"default-profile = "outer"

[profiles.outer]
source = "{}"
remote = "/team/backup"
url = "{url}"
username = "e2e-user"
password-file = "{pw}"
no-vault = true
allow-http = true
compare = "metadata"
jobs = 1
delete = {outer_delete}
max-delete = 20
progress = "never"

[profiles.inner]
source = "{}"
remote = "/team/backup/laptop"
url = "{inner_url}"
username = "e2e-user"
password-file = "{pw}"
no-vault = true
allow-http = true
compare = "metadata"
jobs = 1
delete = {inner_delete}
max-delete = 20
progress = "never"
"#,
        toml_path(&outer),
        toml_path(&inner),
        url = fixture.server.base_url(),
        pw = toml_path(&fixture.password),
    );
    fixture.dir.write("config.toml", config.as_bytes())
}

/// D3-3: a single-profile run refuses a profile whose remote nests with another profile's on the
/// same endpoint when either has `delete = true`, with the configuration exit the batch uses, and
/// names both. `config validate` reports the same. The inner backup survives.
#[test]
fn a_single_profile_run_refuses_an_overlapping_profile_with_delete() {
    let fixture = Fixture::new("fix3-overlap");
    fixture.server.add_directory("/team/backup");
    let config = overlap_config(&fixture, true, false, fixture.server.base_url());
    let env = TestDir::new("fix3-env");
    let validate = run_in(&env, &["--config", text(&config), "config", "validate"]);
    assert_eq!(validate.status.code(), Some(2), "{}", describe(&validate));
    let validate_text = String::from_utf8_lossy(&validate.stderr).into_owned()
        + &String::from_utf8_lossy(&validate.stdout);
    assert!(
        validate_text.contains("\"inner\"") && validate_text.contains("\"outer\""),
        "{}",
        describe(&validate)
    );
    for profile in ["inner", "outer"] {
        let run = run_in(
            &env,
            &[
                "--config",
                text(&config),
                "--output",
                "json",
                "sync",
                "--profile",
                profile,
            ],
        );
        assert_eq!(run.status.code(), Some(2), "[{profile}] {}", describe(&run));
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("\"inner\"") && stderr.contains("\"outer\""),
            "[{profile}] {}",
            describe(&run)
        );
        let plan = run_in(
            &env,
            &[
                "--config",
                text(&config),
                "--output",
                "json",
                "plan",
                "--profile",
                profile,
            ],
        );
        assert_eq!(
            plan.status.code(),
            Some(2),
            "[{profile}] plan {}",
            describe(&plan)
        );
    }
    // Refused before anything ran: the NAS holds nothing new, and nothing was deleted.
    assert!(
        fixture.server.file_paths().is_empty(),
        "{:?}",
        fixture.server.file_paths()
    );
}

/// D3-3: the same nesting with `delete = false` on both profiles cannot lose data, so it still
/// runs, with a warning naming both profiles.
#[test]
fn an_overlapping_profile_without_delete_runs_with_a_warning() {
    let fixture = Fixture::new("fix3-overlap-nodelete");
    fixture.server.add_directory("/team/backup");
    let config = overlap_config(&fixture, false, false, fixture.server.base_url());
    let env = TestDir::new("fix3-env");
    let validate = run_in(&env, &["--config", text(&config), "config", "validate"]);
    assert_eq!(validate.status.code(), Some(0), "{}", describe(&validate));
    let run = run_in(
        &env,
        &[
            "--config",
            text(&config),
            "--output",
            "json",
            "sync",
            "--profile",
            "inner",
        ],
    );
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("\"inner\"") && stderr.contains("\"outer\""),
        "{}",
        describe(&run)
    );
    assert_eq!(
        fixture.text("/team/backup/laptop/thesis.docx").as_deref(),
        Some("the only NAS copy")
    );
}

/// D3-3: the same remote path on two different endpoints is two different folders.
#[test]
fn the_same_remote_on_another_endpoint_does_not_overlap() {
    let fixture = Fixture::new("fix3-overlap-endpoints");
    fixture.server.add_directory("/team/backup");
    let config = overlap_config(&fixture, true, true, "https://other-nas.example:5001");
    let env = TestDir::new("fix3-env");
    let validate = run_in(&env, &["--config", text(&config), "config", "validate"]);
    assert_eq!(validate.status.code(), Some(0), "{}", describe(&validate));
    let run = run_in(
        &env,
        &[
            "--config",
            text(&config),
            "--output",
            "json",
            "sync",
            "--profile",
            "outer",
        ],
    );
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
}

/// D3-3, scoped: `config validate --profile NAME` judges only the overlapping pairs NAME belongs
/// to. The package validates before every run with the run's profile, so one overlapping deleting
/// pair stops only that pair's runs; unscoped, validation still refuses the whole configuration.
#[test]
fn a_scoped_validation_refuses_only_the_overlapping_pair() {
    let fixture = Fixture::new("fix3-overlap-scoped");
    let config = overlap_config(&fixture, true, false, fixture.server.base_url());
    let unrelated = fixture.dir.child("unrelated");
    fs::create_dir_all(&unrelated).unwrap();
    let mut body = fs::read_to_string(&config).unwrap();
    body.push_str(&format!(
        r#"
[profiles.unrelated]
source = "{}"
remote = "/team/elsewhere"
url = "{}"
username = "e2e-user"
password-file = "{}"
no-vault = true
allow-http = true
delete = true
max-delete = 20
progress = "never"
"#,
        toml_path(&unrelated),
        fixture.server.base_url(),
        toml_path(&fixture.password),
    ));
    fs::write(&config, body).unwrap();
    let env = TestDir::new("fix3-env");
    let validate = |profile: Option<&str>| {
        let mut arguments = vec!["--config", text(&config)];
        if let Some(profile) = profile {
            arguments.extend_from_slice(&["--profile", profile]);
        }
        arguments.extend_from_slice(&["config", "validate"]);
        run_in(&env, &arguments)
    };
    let unrelated_run = validate(Some("unrelated"));
    assert_eq!(
        unrelated_run.status.code(),
        Some(0),
        "{}",
        describe(&unrelated_run)
    );
    for profile in ["inner", "outer"] {
        let refused = validate(Some(profile));
        assert_eq!(
            refused.status.code(),
            Some(2),
            "[{profile}] {}",
            describe(&refused)
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("profiles \"inner\" and \"outer\" have overlapping NAS destinations"),
            "[{profile}] {}",
            describe(&refused)
        );
    }
    let whole = validate(None);
    assert_eq!(whole.status.code(), Some(2), "{}", describe(&whole));
}
