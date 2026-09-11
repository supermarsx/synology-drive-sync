//! The published progress record, end to end through a real command.
//!
//! This is the negative control for the writer. The unit tests in `progress_record` prove that the
//! recorder writes what it is told to write; nothing there notices if the phase calls are removed
//! from `run_status`, or if the flag stops reaching the recorder, or if the phase ids drift out of
//! the catalogue. All three of those are silent failures -- no test goes red, no error is logged,
//! and the operator simply watches a bare `pending` forever, which is exactly the state this whole
//! change set exists to end.
//!
//! So these run the shipped binary against the mock File Station and assert on the file it leaves
//! behind.

/// Only the listing, MD5, and login surface is exercised here.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";

/// A request id and a job id of the shapes the bridge issues.
///
/// The record's file name carries both, because that pair is the binding the reader checks and the
/// writer is handed exactly one value to work from.
const REQUEST_ID: &str = "0123456789abcdef0123456789abcdef";
const JOB_ID: &str = "00060f5e12345678fedcba98765432100123456789abcdef";

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("progress-environment");
    for directory in ["home", "appdata", "local-appdata", "xdg"] {
        fs::create_dir_all(environment.child(directory))
            .expect("create isolated subprocess environment directory");
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
    command.output().expect("run synology-drive-sync")
}

/// One status walk, with whatever extra arguments the case needs.
fn status(server: &MockFileStation, source: &Path, password: &Path, extra: &[&str]) -> Output {
    let mut arguments = vec![
        "--quiet",
        "--output",
        "json",
        "status",
        source.to_str().expect("UTF-8 source path"),
        "/team/export",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--no-vault",
        "--allow-http",
        "--all",
        // The DSM manager forces the terminal renderer off, because a queued request has no
        // terminal. Every case here runs that way on purpose: a record writer wired through
        // `ProgressRenderer` would be suppressed by exactly this flag, and would then pass a suite
        // that did not set it while being dead on the only path it exists for.
        "--progress",
        "never",
        // Content mode is what a dashboard status walk runs, and it is the only mode that reaches
        // the comparison and cache-store phases at all.
        "--compare",
        "content",
    ];
    arguments.extend_from_slice(extra);
    run(&arguments)
}

fn record_path(directory: &Path) -> PathBuf {
    directory.join(format!("{REQUEST_ID}.{JOB_ID}.json"))
}

fn published(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|error| {
        panic!(
            "no progress record was published at {}: {error}",
            path.display()
        )
    });
    serde_json::from_slice(&bytes).expect("the published record is a JSON object")
}

/// Fill a fixture with a source tree, a password, and a destination the mock will serve.
fn fixture(label: &str) -> (TestDir, MockFileStation, PathBuf, PathBuf) {
    let directory = TestDir::new(label);
    let source = directory.child("source");
    fs::create_dir_all(source.join("nested")).expect("create the source tree");
    fs::write(source.join("first.txt"), b"first").expect("write a payload");
    fs::write(source.join("nested").join("second.txt"), b"second").expect("write a payload");
    let password = directory.write("password", PASSWORD);

    let server = MockFileStation::start();
    server.add_directory("/team/export");
    server.add_file("/team/export/first.txt", b"first", 1_700_000_000);
    (directory, server, source, password)
}

/// A real status walk publishes a record, and it names the last phase it actually reached.
///
/// Every link in the chain is load-bearing here: the flag has to parse, the recorder has to be
/// attached to the token, `run_status` has to announce its phases, and the ids it announces have
/// to be the ones in the shared catalogue. Break any of them and this test is the only thing that
/// notices, because a progress record that is never written produces no error anywhere.
#[test]
fn a_status_walk_publishes_the_phase_it_reached() {
    let (workspace, server, source, password) = fixture("progress-status");
    let records = workspace.child("progress");
    fs::create_dir_all(&records).expect("create the progress directory");
    let cache = workspace.child("cache");
    fs::create_dir(&cache).expect("create the cache directory");
    let path = record_path(&records);

    let output = status(
        &server,
        &source,
        &password,
        &[
            "--status-cache",
            cache.to_str().expect("UTF-8 cache path"),
            "--progress-record",
            path.to_str().expect("UTF-8 record path"),
        ],
    );
    assert!(
        output.status.success(),
        "the walk itself must succeed; stderr was {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let document = published(&path);
    assert_eq!(
        document["schema"], "sdsync.dsm-request-progress.v1",
        "the record must carry the schema the reader checks"
    );
    assert_eq!(document["request_id"], REQUEST_ID);
    assert_eq!(document["job_id"], JOB_ID);
    assert!(
        document["updated_at"]
            .as_u64()
            .is_some_and(|at| at > 1_700_000_000),
        "the record must carry a real wall-clock second"
    );
    assert!(
        document["count"].as_u64().is_some(),
        "the record must carry an integer count"
    );

    // The last phase a cached content walk reaches. Asserting the *final* phase rather than merely
    // "some phase" is what makes this a control on the whole sequence: an emission dropped
    // anywhere earlier leaves the record naming a different id.
    assert_eq!(
        document["section"], "store_results",
        "the walk must publish every phase through to the last one it performs"
    );

    // Exactly the keys the reader expects, and no others. A seventh key does not degrade the
    // render; the dashboard validates the object by exact key count and drops the whole record.
    let mut keys: Vec<&str> = document
        .as_object()
        .expect("the record is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "count",
            "job_id",
            "request_id",
            "schema",
            "section",
            "updated_at"
        ]
    );
}

/// Without the flag, nothing is published and nothing is created.
///
/// The other half of the contract: every invocation that is not a queued dashboard request -- a
/// cron run, a Docker run, a person at a terminal -- must pay nothing at all for this feature.
#[test]
fn a_run_without_the_flag_publishes_nothing() {
    let (workspace, server, source, password) = fixture("progress-absent");
    let records = workspace.child("progress");
    fs::create_dir_all(&records).expect("create the progress directory");

    let output = status(&server, &source, &password, &[]);
    assert!(
        output.status.success(),
        "the walk itself must succeed; stderr was {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_dir(&records)
            .expect("the progress directory is readable")
            .count(),
        0,
        "a run that was not asked to publish progress must write nothing"
    );
}

/// A record path the writer cannot use costs the run nothing.
///
/// Progress is advisory, and this is where that has to hold in practice rather than in a doc
/// comment: a directory that does not exist, or a file name the writer cannot read an identity out
/// of, is the ordinary consequence of a package upgraded one half at a time. The walk must still
/// answer.
#[test]
fn an_unusable_record_path_does_not_fail_the_run() {
    let (workspace, server, source, password) = fixture("progress-unusable");
    let malformed = workspace.child("progress-malformed");
    fs::create_dir_all(&malformed).expect("create the progress directory");

    for record in [
        workspace
            .child("absent-directory")
            .join(format!("{REQUEST_ID}.{JOB_ID}.json")),
        malformed.join("not-a-job.json"),
    ] {
        let output = status(
            &server,
            &source,
            &password,
            &[
                "--progress-record",
                record.to_str().expect("UTF-8 record path"),
            ],
        );
        assert!(
            output.status.success(),
            "an unusable record path must not fail the walk; stderr was {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !record.exists(),
            "nothing must be created beside an unusable path"
        );
    }
}
