//! Fix round 3, engine: the share-wide refusal breaker (G-2, G-5, BK-1) and the server-copy
//! fallback (G-4), end to end through the real binary and the File Station mock. The verifier's
//! probes (re1, re2, re4, bk1, bk3, bk4), inverted.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use support::file_station_mock::MockFileStation;
use support::{TestDir, copy_payload};

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const CREATE: &str = "SYNO.FileStation.CreateFolder.create";
const UPLOAD: &str = "SYNO.FileStation.Upload.upload";
const DELETE: &str = "SYNO.FileStation.Delete.delete";
const COPY_START: &str = "SYNO.FileStation.CopyMove.start";
const COPY_STATUS: &str = "SYNO.FileStation.CopyMove.status";
const COPY_STOP: &str = "SYNO.FileStation.CopyMove.stop";

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("fix3-breaker-env");
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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

fn mtime_seconds(path: &std::path::Path) -> i64 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct Fixture {
    _dir: TestDir,
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
            _dir: dir,
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

    fn command(&self, command: &str, destination: &str, extra: &[&str]) -> Output {
        let source = self.source.to_str().unwrap();
        let password = self.password.to_str().unwrap();
        let mut arguments = vec![
            "--quiet",
            "--output",
            "json",
            command,
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
        ];
        if !extra.contains(&"--jobs") {
            arguments.extend_from_slice(&["--jobs", "1"]);
        }
        arguments.extend_from_slice(extra);
        run(&arguments)
    }

    fn sync(&self, destination: &str, extra: &[&str]) -> Output {
        self.command("sync", destination, extra)
    }

    fn count(&self, operation: &str) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|request| request.operation() == operation)
            .count()
    }

    fn text(&self, path: &str) -> Option<String> {
        self.server
            .file_contents(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn assert_share_wide(output: &Output, class: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", describe(output));
    let text = stderr(output);
    assert!(
        text.contains(&format!("remote-refused-share-wide: {class}")),
        "{}",
        describe(output)
    );
}

/// G-2 (re2 inverted): three refused new folders among 34 planned writes are three folders'
/// refusals, not the share's. The 31 other files upload, and the run is amber, exit 0.
#[test]
fn three_refused_folders_among_many_writes_do_not_fail_the_run() {
    let fixture = Fixture::new("fix3-re2");
    fixture.server.add_directory("/team/re2");
    fixture.server.add_directory("/team/re2/keep");
    for index in 0..30 {
        fixture.write(&format!("keep/f{index}.txt"), b"payload");
    }
    for index in 0..3 {
        fixture.write(&format!("longfoldername{index}/a.txt"), b"x");
    }
    fixture.write("ok/a.txt", b"x");
    fixture.server.refuse_names_over_bytes(12, 418);
    let output = fixture.sync("/team/re2", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fixture.count(UPLOAD), 31);
    let deferred = deferred(&json(&output));
    for index in 0..3 {
        assert!(
            deferred.contains(&(
                format!("longfoldername{index}"),
                "remote-refused".to_owned()
            )),
            "{deferred:?}"
        );
    }
}

/// G-2: DSM refusing every new folder fails the run once the folders are settled, before a single
/// upload is sent beneath them, naming the class.
#[test]
fn a_share_wide_folder_refusal_trips_before_any_upload() {
    let fixture = Fixture::new("fix3-folders");
    fixture.server.add_directory("/team/folders");
    for folder in 0..4 {
        for index in 0..25 {
            fixture.write(&format!("new{folder}/f{index}.txt"), b"payload");
        }
    }
    fixture.server.fail_api_operation_times(CREATE, 407, 1_000);
    let output = fixture.sync("/team/folders", &[]);
    assert_share_wide(&output, "permission");
    assert!(
        stderr(&output).contains("4 of the 4"),
        "{}",
        describe(&output)
    );
    assert_eq!(fixture.count(UPLOAD), 0);
}

/// G-2: the contents of a refused folder are neither attempts nor refusals. Three refused folders
/// of 100 files each beside 30 good files are 3 refusals of 33.
#[test]
fn the_contents_of_refused_folders_do_not_dilute_or_convict() {
    let fixture = Fixture::new("fix3-dilute");
    fixture.server.add_directory("/team/dilute");
    for folder in 0..3 {
        for index in 0..100 {
            fixture.write(&format!("longfoldername{folder}/f{index}.txt"), b"payload");
        }
    }
    for index in 0..30 {
        fixture.write(&format!("r{index}.txt"), b"payload");
    }
    fixture.server.refuse_names_over_bytes(12, 418);
    let output = fixture.sync("/team/dilute", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fixture.count(UPLOAD), 30);
}

/// G-2 (bk4 inverted): a share-wide upload refusal stops once it is certain. With one worker that
/// is the 21st refusal of 40, and the 19 left are named as not attempted.
#[test]
fn a_share_wide_upload_refusal_stops_sending_once_it_is_certain() {
    let fixture = Fixture::new("fix3-bk4");
    fixture.server.add_directory("/team/bk4");
    for index in 0..40 {
        fixture.write(&format!("f{index:02}.txt"), b"payload");
    }
    fixture.server.fail_api_operation_times(UPLOAD, 407, 1_000);
    let output = fixture.sync("/team/bk4", &[]);
    assert_share_wide(&output, "permission");
    assert_eq!(fixture.count(UPLOAD), 21);
    assert!(
        stderr(&output).contains("19 ") && stderr(&output).contains("not attempted"),
        "{}",
        describe(&output)
    );
    // Every refused path is still named.
    assert_eq!(deferred(&json(&output)).len(), 21);
}

/// G-2: with parallel workers the in-flight peers stop too, and the run's error is the breaker's,
/// never the `Cancelled` its peers answer with.
#[test]
fn a_share_wide_upload_refusal_with_parallel_workers_reports_the_breaker() {
    let fixture = Fixture::new("fix3-bk4-jobs");
    fixture.server.add_directory("/team/bk4j");
    for index in 0..40 {
        fixture.write(&format!("f{index:02}.txt"), b"payload");
    }
    fixture.server.fail_api_operation_times(UPLOAD, 407, 1_000);
    let output = fixture.sync("/team/bk4j", &["--jobs", "4"]);
    assert_share_wide(&output, "permission");
    // Exact, not a margin: the 21st refusal trips the breaker, and the worker that sees it sets the
    // stop before any peer starts another upload, so only the other three workers' requests, each
    // issued before the trip, can follow it. The verdict is taken by the workers, not by the thread
    // that reports progress, so how slow that thread is cannot move this bound (see
    // `tests/sdk_e2e.rs`, which makes it slow on purpose).
    let uploads = fixture.count(UPLOAD);
    assert!((21..=24).contains(&uploads), "{uploads}");
    assert!(
        !stderr(&output).to_lowercase().contains("cancelled"),
        "{}",
        describe(&output)
    );
}

/// G-2: a tripped breaker never reaches the mirror deletions.
#[test]
fn a_share_wide_refusal_deletes_nothing() {
    let fixture = Fixture::new("fix3-nodelete");
    fixture.server.add_directory("/team/nodelete");
    fixture
        .server
        .add_file("/team/nodelete/stale.txt", b"remote only", 1_700_000_000);
    for index in 0..4 {
        fixture.write(&format!("f{index}.txt"), b"payload");
    }
    fixture.server.fail_api_operation_times(UPLOAD, 407, 1_000);
    let output = fixture.sync("/team/nodelete", &["--delete"]);
    assert_share_wide(&output, "permission");
    assert_eq!(fixture.count(DELETE), 0);
    assert!(fixture.text("/team/nodelete/stale.txt").is_some());
}

/// G-5 (bk3 inverted): a moved folder is all server copies. Every copy refused to start and every
/// fallback upload refused is one permission refusal of the share: the run fails, and stops after
/// the third.
#[test]
fn refused_copy_fallbacks_count_towards_the_breaker() {
    let fixture = Fixture::new("fix3-bk3");
    let server = &fixture.server;
    server.add_directory("/team/bk3");
    server.add_directory("/team/bk3/old");
    for name in ["a", "b", "c", "d"] {
        let body = copy_payload(format!("content of {name} moved on the NAS").as_bytes());
        let local = fixture.write(&format!("new/{name}.bin"), &body);
        server.add_file(
            &format!("/team/bk3/old/{name}.bin"),
            &body,
            mtime_seconds(&local),
        );
    }
    server.fail_api_operation_times(COPY_START, 407, 100);
    server.fail_api_operation_times(UPLOAD, 407, 100);
    let output = fixture.sync("/team/bk3", &[]);
    assert_share_wide(&output, "permission");
    assert_eq!(fixture.count(COPY_START), 3);
}

/// G-5 (re1 inverted): one permission refusal across folder creates and uploads is one cause.
#[test]
fn a_permission_refusal_across_operations_is_one_cause() {
    let fixture = Fixture::new("fix3-re1");
    fixture.write("Work/a.txt", b"a");
    fixture.write("Personal/b.txt", b"b");
    fixture.write("r1.txt", b"1");
    fixture.write("r2.txt", b"2");
    fixture.server.add_directory("/team/re1");
    fixture.server.fail_api_operation_times(CREATE, 407, 100);
    fixture.server.fail_api_operation_times(UPLOAD, 407, 100);
    let output = fixture.sync("/team/re1", &[]);
    assert_share_wide(&output, "permission");
    let text = stderr(&output);
    assert!(text.contains(&format!("{CREATE}:407")), "{text}");
    assert!(text.contains(&format!("{UPLOAD}:407")), "{text}");
}

/// G-2: `plan` never trips the breaker, because it writes nothing.
#[test]
fn a_plan_never_trips_the_breaker() {
    let fixture = Fixture::new("fix3-plan");
    fixture.server.add_directory("/team/plan");
    for index in 0..10 {
        fixture.write(&format!("f{index}.txt"), b"payload");
    }
    fixture.server.fail_api_operation_times(UPLOAD, 407, 1_000);
    let output = fixture.command("plan", "/team/plan", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(fixture.count(UPLOAD), 0);
}

/// BK-1 (bk1 inverted): three names the volume rejects are three lasting refusals among files
/// already in sync. Every later quiet run stays amber, exit 0, naming them; it used to fail red
/// from the second run on, blaming the share and skipping every mirror deletion.
#[test]
fn lasting_refusals_on_a_synced_share_stay_amber_on_every_run() {
    let fixture = Fixture::new("fix3-bk1");
    fixture.server.add_directory("/team/bk1");
    for index in 0..30 {
        fixture.write(&format!("f{index}.txt"), b"payload");
    }
    for index in 0..3 {
        fixture.write(&format!("averylongname{index}.txt"), b"x");
    }
    fixture.server.refuse_names_over_bytes(12, 418);
    for round in 1..=3 {
        let output = fixture.sync("/team/bk1", &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "round {round}: {}",
            describe(&output)
        );
        let deferred = deferred(&json(&output));
        assert_eq!(
            deferred
                .iter()
                .filter(|(_, reason)| reason == "remote-refused")
                .count(),
            3,
            "round {round}: {deferred:?}"
        );
    }
}

fn moved_file_fixture(label: &str, destination: &str) -> Fixture {
    let fixture = Fixture::new(label);
    let local = fixture.write("new/item.bin", &copy_payload(b"copy-me-on-nas"));
    let server = &fixture.server;
    server.add_directory(destination);
    server.add_directory(&format!("{destination}/old"));
    server.add_file(
        &format!("{destination}/old/item.bin"),
        &copy_payload(b"copy-me-on-nas"),
        mtime_seconds(&local),
    );
    fixture
}

/// G-4 (re4 inverted): a copy that fails the same way every time (FAT32's 4 GiB limit, 1006) is
/// uploaded in the same run, and the next run has nothing to do. It used to be partial forever.
#[test]
fn a_deterministically_failing_server_copy_is_uploaded_in_the_same_run() {
    let fixture = moved_file_fixture("fix3-re4", "/team/re4");
    fixture.server.fail_next_copy_status_with_envelope(1006);
    let first = fixture.sync("/team/re4", &[]);
    assert_eq!(first.status.code(), Some(0), "{}", describe(&first));
    assert_eq!(
        fixture.text("/team/re4/new/item.bin").as_deref(),
        String::from_utf8(copy_payload(b"copy-me-on-nas"))
            .ok()
            .as_deref()
    );
    assert_eq!(
        json(&first)["result"]["recovered"]["server_copy_fallbacks"],
        1,
        "{}",
        describe(&first)
    );
    assert_eq!(
        fixture.count(COPY_STOP),
        0,
        "an ended task is never stopped"
    );
    assert!(deferred(&json(&first)).is_empty());

    fixture.server.fail_next_copy_status_with_envelope(1006);
    let second = fixture.sync("/team/re4", &[]);
    assert_eq!(second.status.code(), Some(0), "{}", describe(&second));
    assert_eq!(fixture.count(UPLOAD), 1, "nothing more to upload");
}

/// G-4: a failed copy that left a partial file at the destination is not uploaded over in that
/// run. It is the transient `server-copy-failed` once, and the next run replaces it.
#[test]
fn a_failed_copy_that_left_a_partial_file_converges_in_one_more_run() {
    let fixture = moved_file_fixture("fix3-partial", "/team/partial");
    fixture
        .server
        .fail_next_copy_status_with_envelope_leaving_partial(1006, 4);
    let first = fixture.sync("/team/partial", &[]);
    assert_eq!(first.status.code(), Some(3), "{}", describe(&first));
    let deferred = json(&first)["result"]["deferred"].clone();
    assert_eq!(deferred[0]["reason"], "server-copy-failed", "{deferred}");
    assert!(
        deferred[0]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("the destination now holds an entry"),
        "{deferred}"
    );
    let second = fixture.sync("/team/partial", &[]);
    assert_eq!(second.status.code(), Some(0), "{}", describe(&second));
    assert_eq!(
        fixture.text("/team/partial/new/item.bin").as_deref(),
        String::from_utf8(copy_payload(b"copy-me-on-nas"))
            .ok()
            .as_deref()
    );
}

/// G-4: a full volume (416) on the copy and on its fallback upload fails the run, naming it. It
/// used to be partial forever.
#[test]
fn a_full_volume_on_the_copy_and_its_fallback_fails_the_run() {
    let fixture = moved_file_fixture("fix3-416", "/team/full");
    fixture.server.fail_next_copy_status_with_envelope(416);
    fixture.server.fail_api_operation_times(UPLOAD, 416, 10);
    let output = fixture.sync("/team/full", &[]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(stderr(&output).contains("416"), "{}", describe(&output));
    assert!(
        !deferred(&json(&output))
            .iter()
            .any(|(_, reason)| reason == "server-copy-failed")
    );
}

/// G-4 guard: a copy whose status poll fails is still running as far as anyone knows. It is
/// stopped and the run fails; it is never answered with an upload.
#[test]
fn a_copy_whose_status_poll_fails_is_stopped_and_never_uploaded() {
    let fixture = moved_file_fixture("fix3-poll", "/team/poll");
    fixture
        .server
        .fail_api_operation_times(COPY_STATUS, 599, 10);
    let output = fixture.sync("/team/poll", &[]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert_eq!(fixture.count(UPLOAD), 0);
    assert_eq!(fixture.count(COPY_STOP), 1);
}

// ---------------------------------------------------------------------------------------------
// G-3, G-11, RR-5 and L-1: the link guard's breaker and its unchecked count.
// ---------------------------------------------------------------------------------------------

const REAL_PATH_UNUSABLE: &str = "remote-real-path-unusable:";

/// G-3 (re3b inverted): DSM omitting the real path of every file reads as every file being a link.
/// Counted per kind, that fails the run by name instead of withholding every edit with exit 0.
#[test]
fn files_whose_real_paths_all_look_wrong_fail_the_run_by_name() {
    let fixture = Fixture::new("fix3-re3b");
    let server = &fixture.server;
    server.add_directory("/team/re3b");
    for index in 0..10 {
        let path = format!("/team/re3b/r{index}.txt");
        server.add_file(&path, b"old", 1_600_000_000);
        server.withhold_real_path(&path);
        fixture.write(&format!("r{index}.txt"), b"new local content");
    }
    let output = fixture.sync("/team/re3b", &[]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stderr(&output).contains(REAL_PATH_UNUSABLE),
        "{}",
        describe(&output)
    );
    assert_eq!(fixture.count(UPLOAD), 0);
}

/// G-3 (re3 inverted): misreported real paths on a destination with two folders and five files
/// fail the run, though the folders alone are too few to say so.
#[test]
fn misreported_real_paths_on_a_small_destination_fail_the_run() {
    let fixture = Fixture::new("fix3-re3");
    let server = &fixture.server;
    server.add_directory("/team/re3");
    for name in ["a", "b"] {
        server.add_directory(&format!("/team/re3/{name}"));
        server.add_file(&format!("/team/re3/{name}/x.txt"), b"old", 1_600_000_000);
        fixture.write(&format!("{name}/x.txt"), b"new local content");
    }
    for index in 0..5 {
        server.add_file(&format!("/team/re3/r{index}.txt"), b"old", 1_600_000_000);
        fixture.write(&format!("r{index}.txt"), b"new local content");
    }
    server.misreport_real_paths();
    let output = fixture.sync("/team/re3", &["--delete"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        stderr(&output).contains(REAL_PATH_UNUSABLE),
        "{}",
        describe(&output)
    );
    assert_eq!(fixture.count(UPLOAD), 0);
    assert_eq!(fixture.count(DELETE), 0);
}

/// G-11: folders listed without a real path are counted in the plan and the result, whatever
/// `--quiet` says; `--quiet` hides only the stderr line.
#[test]
fn folders_the_link_guard_could_not_check_are_counted_even_when_quiet() {
    let fixture = Fixture::new("fix3-g11");
    fixture.server.add_directory("/team/g11");
    fixture.server.add_directory("/team/g11/sub");
    fixture.server.withhold_real_path("/team/g11");
    fixture.write("sub/a.txt", b"a");
    let output = fixture.sync("/team/g11", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let document = json(&output);
    assert!(
        document["result"]["link_guard_unchecked"]
            .as_u64()
            .is_some_and(|count| count >= 1),
        "{document}"
    );
    assert!(
        document["plan"]["link_guard_unchecked"]
            .as_u64()
            .is_some_and(|count| count >= 1),
        "{document}"
    );
    assert!(
        !stderr(&output).contains("could not check for symbolic links"),
        "{}",
        describe(&output)
    );
}

/// RR-5: a scope naming one file lists no folder, so there is nothing the guard failed to check:
/// no warning and a count of 0. It used to warn on every file-scoped re-upload.
#[test]
fn a_file_scope_reports_no_unchecked_folders() {
    let fixture = Fixture::new("fix3-rr5");
    fixture.server.add_directory("/team/rr5");
    fixture.server.add_directory("/team/rr5/docs");
    fixture.write("docs/a.txt", b"a");
    let source = fixture.source.to_str().unwrap().to_owned();
    let password = fixture.password.to_str().unwrap().to_owned();
    let output = run(&[
        "--output",
        "json",
        "sync",
        &source,
        "/team/rr5",
        "--url",
        fixture.server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        &password,
        "--no-vault",
        "--allow-http",
        "--scope",
        "docs/a.txt",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(json(&output)["result"]["link_guard_unchecked"], 0);
    assert!(
        !stderr(&output).contains("could not check for symbolic links"),
        "{}",
        describe(&output)
    );
}

/// L-1: status names a local file beneath a NAS folder link as excluded (`mount-boundary`), not
/// `missing-remote`, which offered a Re-upload through the link.
#[test]
fn status_names_a_file_beneath_a_nas_link_as_excluded() {
    let fixture = Fixture::new("fix3-l1");
    let server = &fixture.server;
    server.add_directory("/team/l1");
    server.add_directory("/team/elsewhere");
    server.add_file("/team/elsewhere/precious.txt", b"not ours", 1_600_000_000);
    server.add_directory("/team/l1/backup-link");
    server.add_directory_symlink("/team/l1/backup-link", "/team/elsewhere");
    fixture.write("backup-link/file.txt", b"local");
    fixture.write("plain.txt", b"plain");
    let source = fixture.source.to_str().unwrap().to_owned();
    let password = fixture.password.to_str().unwrap().to_owned();
    let output = run(&[
        "--output",
        "json",
        "status",
        &source,
        "/team/l1",
        "--url",
        fixture.server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        &password,
        "--no-vault",
        "--allow-http",
        "--all",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let document = json(&output);
    let state_of = |relative: &str| {
        document["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["relative"] == relative)
            .map(|entry| entry["state"].as_str().unwrap_or_default().to_owned())
    };
    assert_eq!(
        state_of("backup-link/file.txt").as_deref(),
        Some("excluded"),
        "{document}"
    );
    assert_eq!(state_of("plain.txt").as_deref(), Some("missing-remote"));
}
