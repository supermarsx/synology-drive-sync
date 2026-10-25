//! The File Station client against DSM protocol behaviour found by the third adversarial round
//! (ADV3-P-*, G-1, N1, R-6).
//!
//! Each test names the finding it pins, and each was written to fail on the code it fixes. The
//! mock modes it uses model what a real NAS (or the proxy in front of it) does and the default mock
//! does not: a sub-folder the account may not list, a folder deleted during the walk, a session
//! that dies at an upload, a proxy failing the read after a mutation, a connection closed after the
//! request was sent.
#![allow(dead_code)]

mod support;

use std::fs;
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;
use synology_drive_sync::api::{
    ApiClient, ClientOptions, LINKED_ENTRY_MOUNT_TYPE, RemoteSkipReason,
};
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::local::{EntryKind, FileIdentity, IgnoreRules, LocalEntry};
use synology_drive_sync::path::RemoteRoot;
use synology_drive_sync::plan::Scope;

const PASSWORD: &str = "correct horse battery staple";

fn connect(server: &MockFileStation, retries: u32) -> ApiClient {
    let mut client = ApiClient::connect(&ClientOptions {
        base_url: server.base_url().to_owned(),
        allow_http: true,
        accept_invalid_certs: false,
        ca_certificate: None,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        retries,
    })
    .expect("connect to the mock");
    client
        .login("e2e-user", PASSWORD, None)
        .expect("log in to the mock");
    client
}

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("protocol-env");
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
        let name_text = name.to_string_lossy();
        if name_text.starts_with("SDSYNC_") || name_text.to_ascii_lowercase().ends_with("_proxy") {
            command.env_remove(name);
        }
    }
    command.output().unwrap()
}

/// A source tree with each named file holding its own name, and a password file.
struct Fixture {
    _directory: TestDir,
    source: String,
    password: String,
}

fn fixture(label: &str, files: &[&str]) -> Fixture {
    let directory = TestDir::new(label);
    let source = directory.child("source");
    fs::create_dir_all(&source).unwrap();
    for name in files {
        let path = source.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, name.as_bytes()).unwrap();
    }
    let password = directory.write("password", format!("{PASSWORD}\n").as_bytes());
    Fixture {
        source: source.to_str().unwrap().to_owned(),
        password: password.to_str().unwrap().to_owned(),
        _directory: directory,
    }
}

fn sync(fixture: &Fixture, destination: &str, server: &MockFileStation, extra: &[&str]) -> Output {
    let mut arguments = vec![
        "--output",
        "json",
        "sync",
        fixture.source.as_str(),
        destination,
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        fixture.password.as_str(),
        "--no-vault",
        "--allow-http",
        "--compare",
        "metadata",
    ];
    if !extra.contains(&"--jobs") {
        arguments.extend_from_slice(&["--jobs", "1"]);
    }
    arguments.extend_from_slice(extra);
    run(&arguments)
}

fn describe(output: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout\n{}\n--- stderr\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn result_document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
}

fn count(server: &MockFileStation, operation: &str) -> usize {
    server
        .requests()
        .iter()
        .filter(|request| request.operation() == operation)
        .count()
}

fn text(server: &MockFileStation, path: &str) -> Option<String> {
    server
        .file_contents(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Every deferral in a result document, as `(reason, path)`.
fn deferrals(document: &Value) -> Vec<(String, String)> {
    document["result"]["deferred"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|record| {
            (
                record["reason"].as_str().unwrap_or_default().to_owned(),
                record["path"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// A NAS where `/team/r5/backup-link` is a folder link to `/team/elsewhere`, which holds the only
/// copies of `precious.txt` and `sub/deep.txt`.
fn linked_share() -> MockFileStation {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/elsewhere");
    server.add_directory("/team/elsewhere/sub");
    server.add_file("/team/elsewhere/precious.txt", b"precious", 1_700_000_000);
    server.add_file("/team/elsewhere/sub/deep.txt", b"deep", 1_700_000_000);
    server.add_directory_symlink("/team/r5/backup-link", "/team/elsewhere");
    server
}

/// Nothing was written or deleted anywhere, and the link target is as it was.
fn assert_nothing_went_through_the_link(server: &MockFileStation, output: &Output) {
    for operation in [
        "SYNO.FileStation.Delete.delete",
        "SYNO.FileStation.Upload.upload",
        "SYNO.FileStation.CreateFolder.create",
        "SYNO.FileStation.CopyMove.start",
        "SYNO.FileStation.Rename.rename",
    ] {
        assert_eq!(
            count(server, operation),
            0,
            "{operation}: {}",
            describe(output)
        );
    }
    assert_eq!(
        text(server, "/team/elsewhere/precious.txt").as_deref(),
        Some("precious")
    );
    assert_eq!(
        text(server, "/team/elsewhere/sub/deep.txt").as_deref(),
        Some("deep")
    );
}

/// The scoped request was refused by name, as a NAS link on the scope.
fn assert_refused_as_a_link_in_scope(output: &Output, listed: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", describe(output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("{listed:?}")) && stderr.contains("NAS link"),
        "{stderr}"
    );
    assert!(stderr.contains("run without --scope"), "{stderr}");
}

fn walk(
    server: &MockFileStation,
    root: &str,
    scope: &str,
) -> synology_drive_sync::Result<synology_drive_sync::api::ScopedRemoteInventory> {
    connect(server, 0).remote_inventory_scoped(
        &RemoteRoot::parse(root).unwrap(),
        &if scope.is_empty() {
            Scope::root()
        } else {
            Scope::parse(scope).unwrap()
        },
        &IgnoreRules::none(),
        usize::MAX,
        &CancellationToken::default(),
    )
}

// ---------------------------------------------------------------------------------------------
// G-1: a scope at or beneath a NAS link is refused by name; nothing goes through the link.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_scoped_walk_refuses_a_scope_that_is_a_folder_link() {
    let server = linked_share();

    let error = walk(&server, "/team/r5", "backup-link").unwrap_err();

    match &error {
        synology_drive_sync::Error::RemoteLinkInScope { path, cause } => {
            assert_eq!(path, "/team/r5/backup-link");
            assert!(cause.contains("/volume1/team/elsewhere"), "{cause}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&server, "SYNO.FileStation.List.list"), 0);
}

#[test]
fn scoped_mirror_on_a_folder_link_is_refused() {
    let fixture = fixture("g1-mirror", &["a.txt", "backup-link/keep.txt"]);
    let server = linked_share();

    let output = sync(
        &fixture,
        "/team/r5",
        &server,
        &["--delete", "--scope", "backup-link"],
    );

    assert_refused_as_a_link_in_scope(&output, "/team/r5/backup-link");
    assert_nothing_went_through_the_link(&server, &output);
}

#[test]
fn scoped_mirror_below_a_folder_link_is_refused() {
    let fixture = fixture("g1-below", &["backup-link/sub/other.txt"]);
    let server = linked_share();

    let output = sync(
        &fixture,
        "/team/r5",
        &server,
        &["--delete", "--scope", "backup-link/sub"],
    );

    assert_refused_as_a_link_in_scope(&output, "/team/r5/backup-link");
    assert_nothing_went_through_the_link(&server, &output);
}

#[test]
fn scope_naming_a_file_beneath_a_link_is_refused() {
    // The file missing locally under --delete.
    let missing = fixture("g1-file-delete", &["a.txt"]);
    let server = linked_share();
    let output = sync(
        &missing,
        "/team/r5",
        &server,
        &[
            "--delete",
            "--allow-empty-source",
            "--scope",
            "backup-link/precious.txt",
        ],
    );
    assert_refused_as_a_link_in_scope(&output, "/team/r5/backup-link");
    assert_nothing_went_through_the_link(&server, &output);

    // A local edit of it.
    let fixture = fixture("g1-file-edit", &["backup-link/precious.txt"]);
    fs::write(
        std::path::Path::new(&fixture.source).join("backup-link/precious.txt"),
        b"LOCAL-OVERWRITE",
    )
    .unwrap();
    let server = linked_share();
    let output = sync(
        &fixture,
        "/team/r5",
        &server,
        &["--scope", "backup-link/precious.txt"],
    );
    assert_refused_as_a_link_in_scope(&output, "/team/r5/backup-link");
    assert_nothing_went_through_the_link(&server, &output);
}

#[test]
fn scoped_new_file_beneath_a_link_is_never_uploaded() {
    let fixture = fixture("g1-new", &["backup-link/new.txt"]);
    let server = linked_share();

    let output = sync(&fixture, "/team/r5", &server, &["--scope", "backup-link"]);

    assert_refused_as_a_link_in_scope(&output, "/team/r5/backup-link");
    assert_nothing_went_through_the_link(&server, &output);
    assert!(server.file_contents("/team/elsewhere/new.txt").is_none());
}

#[test]
fn resync_on_a_link_scope_is_refused_in_both_phases() {
    for scope in ["backup-link", "backup-link/precious.txt"] {
        let fixture = fixture("g1-resync", &["backup-link/precious.txt"]);
        fs::write(
            std::path::Path::new(&fixture.source).join("backup-link/precious.txt"),
            b"LOCAL-RESYNC",
        )
        .unwrap();
        let server = linked_share();
        let resync = |extra: &[&str]| {
            let mut arguments = vec![
                "--output",
                "json",
                "resync",
                fixture.source.as_str(),
                "/team/r5",
                "--url",
                server.base_url(),
                "--username",
                "e2e-user",
                "--password-file",
                fixture.password.as_str(),
                "--no-vault",
                "--allow-http",
                "--scope",
                scope,
            ];
            arguments.extend_from_slice(extra);
            run(&arguments)
        };

        let planned = resync(&[]);
        assert_refused_as_a_link_in_scope(&planned, "/team/r5/backup-link");
        assert!(
            result_document(&planned)["ticket"].as_str().is_none(),
            "{scope}: no ticket is issued: {}",
            describe(&planned)
        );
        // A ticket from anywhere does not get past the refusal either.
        let confirmed = resync(&["--confirm", "0123456789abcdef"]);
        assert_refused_as_a_link_in_scope(&confirmed, "/team/r5/backup-link");
        assert_nothing_went_through_the_link(&server, &confirmed);
    }
}

#[test]
fn scoped_chain_element_without_real_path_is_refused_once_root_reported_one() {
    let fixture = fixture("g1-opaque", &["a/b/x.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/r5/a");
    server.add_directory("/team/r5/a/b");
    server.withhold_real_path("/team/r5/a");

    let output = sync(&fixture, "/team/r5", &server, &["--scope", "a/b"]);

    assert_refused_as_a_link_in_scope(&output, "/team/r5/a");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no real path"),
        "{}",
        describe(&output)
    );
    assert_eq!(count(&server, "SYNO.FileStation.Upload.upload"), 0);
}

/// RR-6, scoped: a link whose target differs from its listing only in letter case.
#[test]
fn scoped_case_variant_link_is_refused() {
    let fixture = fixture("g1-case", &["Photos/x.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/cs");
    server.add_directory("/team/CS");
    server.add_directory("/team/CS/Photos");
    server.add_file("/team/CS/Photos/precious.txt", b"precious", 1_700_000_000);
    server.add_directory_symlink("/team/cs/Photos", "/team/CS/Photos");

    let output = sync(
        &fixture,
        "/team/cs",
        &server,
        &["--delete", "--scope", "Photos"],
    );

    assert_refused_as_a_link_in_scope(&output, "/team/cs/Photos");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("only in letter case"),
        "{}",
        describe(&output)
    );
    assert!(
        server
            .file_contents("/team/CS/Photos/precious.txt")
            .is_some()
    );
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 0);
}

/// RR-6, unscoped (VL8 inverted): the case-variant link is a link, so nothing is deleted through
/// it; the run is green with the link named as a lasting `remote-skipped`.
#[test]
fn an_unscoped_mirror_never_deletes_through_a_case_variant_link() {
    let fixture = fixture("rr6", &["a.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/cs");
    server.add_directory("/team/CS");
    server.add_directory("/team/CS/Photos");
    server.add_file("/team/CS/Photos/precious.txt", b"precious", 1_700_000_000);
    server.add_directory_symlink("/team/cs/Photos", "/team/CS/Photos");

    let output = sync(&fixture, "/team/cs", &server, &["--delete"]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        server
            .file_contents("/team/CS/Photos/precious.txt")
            .is_some(),
        "deleted through the case-variant link: {}",
        describe(&output)
    );
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 0);
    let document = result_document(&output);
    assert!(
        deferrals(&document)
            .iter()
            .any(|(reason, path)| reason == "remote-skipped" && path == "Photos"),
        "the link is named: {}",
        describe(&output)
    );
    let scanned = walk(&server, "/team/cs", "").unwrap();
    assert_eq!(
        scanned.inventory.entries["Photos"]
            .mount_point_type
            .as_deref(),
        Some(LINKED_ENTRY_MOUNT_TYPE)
    );
}

#[test]
fn scoped_link_back_into_the_tree_is_refused() {
    let fixture = fixture("g1-loop", &["loop/x.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_file("/team/r5/a.txt", b"a", 1_700_000_000);
    server.add_directory_symlink("/team/r5/loop", "/team/r5");

    let output = sync(&fixture, "/team/r5", &server, &["--scope", "loop"]);

    assert_refused_as_a_link_in_scope(&output, "/team/r5/loop");
    assert_eq!(count(&server, "SYNO.FileStation.Upload.upload"), 0);
    assert!(matches!(
        walk(&server, "/team/r5", "loop/x"),
        Err(synology_drive_sync::Error::RemoteLinkInScope { .. })
    ));
}

/// G-11: a DSM that reports no real path anywhere leaves the guard unable to run. That is the
/// fixed decision's named warning, not a refusal, and it is counted.
#[test]
fn scoped_run_without_any_real_paths_proceeds_and_counts_unchecked() {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/r5/sub");
    server.add_file("/team/r5/sub/a.txt", b"a", 1_700_000_000);
    for path in ["/team/r5", "/team/r5/sub", "/team/r5/sub/a.txt"] {
        server.withhold_real_path(path);
    }

    let scanned = walk(&server, "/team/r5", "sub").unwrap();

    assert!(
        scanned.link_guard_unchecked >= 1,
        "{}",
        scanned.link_guard_unchecked
    );
    assert!(scanned.inventory.entries.contains_key("sub/a.txt"));
}

/// RR-5: the guard ran wherever it was needed, so nothing is counted: a scope naming a file lists
/// no folder, and a scope that does not exist lists nothing.
#[test]
fn a_file_scope_or_an_absent_scope_counts_nothing_unchecked() {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/r5/sub");
    server.add_file("/team/r5/sub/file.txt", b"f", 1_700_000_000);

    for scope in ["sub/file.txt", "sub/missing", "sub", ""] {
        let scanned = walk(&server, "/team/r5", scope).unwrap();
        assert_eq!(scanned.link_guard_unchecked, 0, "{scope:?}");
        assert!(
            scanned.skipped.is_empty(),
            "{scope:?}: {:?}",
            scanned.skipped
        );
    }

    let fixture = fixture("rr5", &["sub/file.txt"]);
    let output = sync(&fixture, "/team/r5", &server, &["--scope", "sub/file.txt"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("real path"),
        "no false guard warning: {}",
        describe(&output)
    );
}

/// An ordinary scope keeps its ancestors as plain folders and still walks.
#[test]
fn an_ordinary_scope_is_unaffected_by_the_ancestor_link_guard() {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/r5/docs");
    server.add_directory("/team/r5/docs/2026");
    server.add_file("/team/r5/docs/2026/a.txt", b"a", 1_700_000_000);

    let scanned = walk(&server, "/team/r5", "docs/2026").unwrap();

    assert!(scanned.skipped.is_empty(), "{:?}", scanned.skipped);
    assert_eq!(scanned.link_guard_unchecked, 0);
    for key in ["docs", "docs/2026", "docs/2026/a.txt"] {
        assert_eq!(
            scanned.inventory.entries[key].mount_point_type, None,
            "{key}"
        );
    }
}

/// The destination itself may live behind a link: that is where the operator pointed the tool, so
/// only what is found at or below the root is judged.
#[test]
fn a_destination_reached_through_a_link_is_still_walked() {
    let server = MockFileStation::start();
    server.add_directory("/team/real");
    server.add_file("/team/real/a.txt", b"a", 1_700_000_000);
    server.add_directory_symlink("/team/alias", "/team/real");

    let scanned = walk(&server, "/team/alias", "").unwrap();

    assert!(scanned.skipped.is_empty(), "{:?}", scanned.skipped);
    assert!(scanned.inventory.entries.contains_key("a.txt"));
}

/// G-3: what the guard evaluated is counted per kind, without excluded entries or revisits.
#[test]
fn the_walk_counts_what_the_link_guard_evaluated_per_kind() {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/r5/plain");
    server.add_file("/team/r5/a.txt", b"a", 1_700_000_000);
    server.add_file("/team/r5/plain/b.txt", b"b", 1_700_000_000);
    server.add_directory("/team/elsewhere");
    server.add_directory_symlink("/team/r5/out", "/team/elsewhere");
    server.add_directory_symlink("/team/r5/loop", "/team/r5");

    let scanned = walk(&server, "/team/r5", "").unwrap();

    let checks = scanned.link_checks;
    assert_eq!(checks.files_checked, 2, "{checks:?}");
    assert_eq!(checks.files_linked, 0, "{checks:?}");
    // `plain` and `out`; `loop` is a revisit, which a genuine link looks like, and is not counted.
    assert_eq!(checks.folders_checked, 2, "{checks:?}");
    assert_eq!(checks.folders_linked, 1, "{checks:?}");
}

// ---------------------------------------------------------------------------------------------
// Mock fidelity: the default mock keeps connections open and follows links for every call.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_mock_keeps_connections_open_between_requests() {
    let server = MockFileStation::start();
    server.add_directory("/team/ka");
    for index in 0..5 {
        server.add_file(&format!("/team/ka/{index}.txt"), b"x", 1_700_000_000);
    }
    let client = connect(&server, 0);
    for _ in 0..3 {
        client
            .remote_inventory(
                &RemoteRoot::parse("/team/ka").unwrap(),
                &IgnoreRules::none(),
                &CancellationToken::default(),
            )
            .unwrap();
    }
    let requests = server.connections().len();
    assert!(requests > 6, "{requests}");
    assert!(
        server.accepted_connections() < requests / 2,
        "{} connections for {requests} requests: none was reused",
        server.accepted_connections()
    );
}

#[test]
fn a_remote_checksum_through_a_folder_link_follows_the_link() {
    let server = linked_share();
    let client = connect(&server, 0);
    client
        .remote_content_md5(
            "/team/r5/backup-link/precious.txt",
            &CancellationToken::default(),
        )
        .expect("MD5.start follows links as every other call does");
}

// ---------------------------------------------------------------------------------------------
// P-1: a session expiry at an Upload is one more attempt, numbered as such.
// ---------------------------------------------------------------------------------------------

fn session_expiry_at_an_upload(jobs: &str) {
    let fixture = fixture(&format!("p1-jobs{jobs}"), &["a.txt", "b.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p1");
    server.expire_session_on("SYNO.FileStation.Upload.upload");

    let output = sync(&fixture, "/team/p1", &server, &["--jobs", jobs]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(text(&server, "/team/p1/a.txt").as_deref(), Some("a.txt"));
    assert_eq!(text(&server, "/team/p1/b.txt").as_deref(), Some("b.txt"));
    assert_eq!(server.logins_seen(), 2);
    // One accepted send for each file, after one refused send -- or two, when both workers were
    // mid-upload as the session died.
    let uploads = count(&server, "SYNO.FileStation.Upload.upload");
    assert!((3..=4).contains(&uploads), "{uploads}");
    let document = result_document(&output);
    assert_eq!(document["result"]["recovered"]["reauthentications"], 1);
    assert_eq!(document["result"]["uploaded"], 2);
}

#[test]
fn a_session_expiry_at_an_upload_is_recovered_and_the_run_completes() {
    session_expiry_at_an_upload("1");
}

#[test]
fn a_session_expiry_at_an_upload_is_recovered_with_parallel_jobs() {
    session_expiry_at_an_upload("4");
}

// ---------------------------------------------------------------------------------------------
// P-2: the reads that verify or guard a mutation are retried like every other read.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_transient_502_on_post_upload_verification_is_retried() {
    let fixture = fixture("p2-upload", &["a.txt", "b.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p2");
    server.fail_http_after_operation(
        "SYNO.FileStation.Upload.upload",
        "SYNO.FileStation.List.getinfo",
        502,
    );

    let output = sync(&fixture, "/team/p2", &server, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(text(&server, "/team/p2/a.txt").as_deref(), Some("a.txt"));
    assert_eq!(text(&server, "/team/p2/b.txt").as_deref(), Some("b.txt"));
    let document = result_document(&output);
    assert_eq!(document["result"]["uploaded"], 2);
    assert_eq!(document["result"]["recovered"]["retries"], 1);
    assert_eq!(server.pending_faults(), 0);
}

#[test]
fn a_transient_502_on_post_copy_verification_is_retried() {
    let directory = TestDir::new("p2-copy");
    let source = directory.child("source");
    fs::create_dir_all(source.join("new")).unwrap();
    let local = source.join("new/item.bin");
    fs::write(&local, b"copy-me-on-nas").unwrap();
    let mtime = fs::metadata(&local)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let password = directory.write("password", format!("{PASSWORD}\n").as_bytes());
    let fixture = Fixture {
        source: source.to_str().unwrap().to_owned(),
        password: password.to_str().unwrap().to_owned(),
        _directory: directory,
    };
    let server = MockFileStation::start();
    server.add_directory("/team/p2c");
    server.add_directory("/team/p2c/old");
    server.add_file("/team/p2c/old/item.bin", b"copy-me-on-nas", mtime);
    server.fail_http_after_operation(
        "SYNO.FileStation.CopyMove.status",
        "SYNO.FileStation.List.getinfo",
        502,
    );

    let output = run(&[
        "--output",
        "json",
        "sync",
        fixture.source.as_str(),
        "/team/p2c",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        fixture.password.as_str(),
        "--no-vault",
        "--allow-http",
        "--jobs",
        "1",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(count(&server, "SYNO.FileStation.CopyMove.start"), 1);
    assert_eq!(
        text(&server, "/team/p2c/new/item.bin").as_deref(),
        Some("copy-me-on-nas")
    );
    assert_eq!(server.pending_faults(), 0, "the fault was reached");
    let document = result_document(&output);
    assert!(
        document["result"]["recovered"]["retries"].as_u64() >= Some(1),
        "{}",
        document["result"]
    );
}

#[test]
fn a_transient_502_on_the_live_check_before_a_delete_is_retried() {
    let fixture = fixture("p2-delete", &[]);
    let server = MockFileStation::start();
    server.add_directory("/team/p2d");
    server.add_file("/team/p2d/extra.txt", b"extra", 1_700_000_000);
    server.fail_next_getinfo_of("/team/p2d/extra.txt", 502);

    let output = sync(
        &fixture,
        "/team/p2d",
        &server,
        &["--delete", "--allow-empty-source"],
    );

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(server.file_contents("/team/p2d/extra.txt").is_none());
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 1);
    let document = result_document(&output);
    assert_eq!(document["result"]["recovered"]["retries"], 1);
}

#[test]
fn a_502_then_a_changed_answer_before_a_delete_still_keeps_the_file() {
    let fixture = fixture("p2-changed", &[]);
    let server = MockFileStation::start();
    server.add_directory("/team/p2x");
    server.add_file("/team/p2x/extra.txt", b"extra", 1_700_000_000);
    server.fail_next_getinfo_of_then_change(
        "/team/p2x/extra.txt",
        502,
        b"rewritten on the NAS meanwhile",
        1_700_000_500,
    );

    let output = sync(
        &fixture,
        "/team/p2x",
        &server,
        &["--delete", "--allow-empty-source"],
    );

    assert_eq!(
        text(&server, "/team/p2x/extra.txt").as_deref(),
        Some("rewritten on the NAS meanwhile"),
        "{}",
        describe(&output)
    );
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 0);
    let document = result_document(&output);
    assert!(
        deferrals(&document)
            .iter()
            .any(|(reason, path)| reason == "remote-changed" && path == "extra.txt"),
        "{}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------------------------
// P-3: a sub-folder this account may not list is named and protected, not a run failure.
// ---------------------------------------------------------------------------------------------

fn unlistable_share(code: i64) -> MockFileStation {
    let server = MockFileStation::start();
    server.add_directory("/team/p3");
    server.add_directory("/team/p3/hr-private");
    server.add_file("/team/p3/hr-private/salaries.xlsx", b"x", 1_700_000_000);
    server.refuse_listing("/team/p3/hr-private", code);
    server
}

#[test]
fn an_unlistable_remote_subfolder_is_recorded_protected_and_not_descended() {
    for code in [105, 407] {
        let server = unlistable_share(code);
        let client = connect(&server, 0).with_reauthenticator(std::sync::Arc::new(
            |client: &mut ApiClient| client.login("e2e-user", PASSWORD, None),
        ));

        let scanned = client
            .remote_inventory_scoped(
                &RemoteRoot::parse("/team/p3").unwrap(),
                &Scope::root(),
                &IgnoreRules::none(),
                usize::MAX,
                &CancellationToken::default(),
            )
            .unwrap_or_else(|error| panic!("code {code}: {error}"));

        assert_eq!(
            scanned.inventory.entries["hr-private"]
                .mount_point_type
                .as_deref(),
            Some(synology_drive_sync::api::UNLISTABLE_ENTRY_MOUNT_TYPE),
            "code {code}"
        );
        assert_eq!(scanned.skipped.len(), 1, "code {code}");
        assert_eq!(
            scanned.skipped[0].reason,
            RemoteSkipReason::UnlistableFolder
        );
        assert!(
            scanned.skipped[0].detail().contains(&code.to_string()),
            "{}",
            scanned.skipped[0].detail()
        );
        assert!(
            !scanned.skipped[0].detail().contains("  "),
            "{}",
            scanned.skipped[0].detail()
        );
        assert!(
            scanned
                .incomplete_directories
                .contains(&"hr-private".to_owned()),
            "{:?}",
            scanned.incomplete_directories
        );
        // No re-login: the session was proven alive.
        assert_eq!(server.logins_seen(), 1, "code {code}");
    }
}

#[test]
fn an_unlistable_remote_subfolder_no_longer_fails_the_run() {
    for code in [105, 407] {
        let fixture = fixture(&format!("p3-{code}"), &["a.txt", "docs/b.txt"]);
        let server = unlistable_share(code);

        for round in 1..=2 {
            let output = sync(&fixture, "/team/p3", &server, &["--delete"]);
            assert_eq!(
                output.status.code(),
                Some(0),
                "code {code} round {round}: {}",
                describe(&output)
            );
            let document = result_document(&output);
            assert!(
                deferrals(&document)
                    .iter()
                    .any(|(_, path)| path == "hr-private"),
                "named on every run: {}",
                describe(&output)
            );
        }
        assert_eq!(text(&server, "/team/p3/a.txt").as_deref(), Some("a.txt"));
        assert_eq!(
            text(&server, "/team/p3/docs/b.txt").as_deref(),
            Some("docs/b.txt")
        );
        assert!(
            server
                .file_contents("/team/p3/hr-private/salaries.xlsx")
                .is_some(),
            "code {code}: the unlistable folder survives --delete"
        );
        assert!(
            server
                .directories()
                .contains(&"/team/p3/hr-private".to_owned())
        );
        assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 0);
    }
}

#[test]
fn local_content_beneath_an_unlistable_remote_folder_is_not_uploaded() {
    let fixture = fixture("p3-local", &["a.txt", "hr-private/mine.txt"]);
    let server = unlistable_share(407);

    let output = sync(&fixture, "/team/p3", &server, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        server
            .file_contents("/team/p3/hr-private/mine.txt")
            .is_none()
    );
    assert_eq!(count(&server, "SYNO.FileStation.Upload.upload"), 1);
    let document = result_document(&output);
    assert!(
        deferrals(&document)
            .iter()
            .any(|(reason, path)| reason == "remote-unreadable" && path.starts_with("hr-private")),
        "{}",
        describe(&output)
    );
}

#[test]
fn most_folders_unlistable_fails_the_run_by_name() {
    let fixture = fixture("p3-most", &["a.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p3m");
    for folder in ["one", "two", "three", "four"] {
        server.add_directory(&format!("/team/p3m/{folder}"));
    }
    for folder in ["one", "two", "three"] {
        server.refuse_listing(&format!("/team/p3m/{folder}"), 407);
    }

    let output = sync(&fixture, "/team/p3m", &server, &[]);

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("one") && stderr.contains("three"),
        "{stderr}"
    );
    assert_eq!(count(&server, "SYNO.FileStation.Upload.upload"), 0);
}

/// 105 with a session that also fails the liveness probe is a dead session: re-login, as before.
#[test]
fn a_105_listing_with_a_dead_session_still_logs_in_again() {
    let fixture = fixture("p3-dead", &["a.txt", "docs/b.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p3d");
    server.add_directory("/team/p3d/docs");
    // The session dies as the walk starts, and DSM says so with 105 rather than 119, to the
    // listing and to the liveness probe alike.
    server.answer_dead_sessions_with(105);
    server.expire_session_on("SYNO.FileStation.List.list");

    let output = sync(&fixture, "/team/p3d", &server, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(server.logins_seen(), 2);
    assert_eq!(
        text(&server, "/team/p3d/docs/b.txt").as_deref(),
        Some("docs/b.txt")
    );
}

// ---------------------------------------------------------------------------------------------
// P-5: a request the peer closed after receiving it is retried when retrying is safe.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_listing_whose_connection_is_closed_unanswered_is_retried() {
    let server = MockFileStation::start();
    server.add_directory("/team/p5");
    server.add_file("/team/p5/a.txt", b"a", 1_700_000_000);
    let client = connect(&server, 2);
    server.drop_next_connection_on("SYNO.FileStation.List.list");

    let inventory = client
        .remote_inventory(
            &RemoteRoot::parse("/team/p5").unwrap(),
            &IgnoreRules::none(),
            &CancellationToken::default(),
        )
        .expect("the closed request is sent again");

    assert!(inventory.entries.contains_key("a.txt"));
    assert_eq!(count(&server, "SYNO.FileStation.List.list"), 2);
    assert_eq!(client.transport_counters().retries, 1);
}

#[test]
fn a_delete_whose_connection_is_closed_unanswered_is_sent_once() {
    let server = MockFileStation::start();
    server.add_directory("/team/p5d");
    server.add_file("/team/p5d/a.txt", b"a", 1_700_000_000);
    let client = connect(&server, 2);
    server.drop_next_connection_on("SYNO.FileStation.Delete.delete");

    let result =
        client.delete_non_recursive(&RemoteRoot::parse("/team/p5d").unwrap(), "/team/p5d/a.txt");

    assert!(result.is_err(), "{result:?}");
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 1);
}

#[test]
fn an_upload_whose_connection_closes_after_the_body_landed_is_proved_not_resent() {
    let fixture = fixture("p5-upload", &["a.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p5u");
    server.drop_upload_connection_after_commit();

    let output = sync(&fixture, "/team/p5u", &server, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(text(&server, "/team/p5u/a.txt").as_deref(), Some("a.txt"));
    assert_eq!(count(&server, "SYNO.FileStation.Upload.upload"), 1);
    assert!(
        count(&server, "SYNO.FileStation.MD5.start") >= 1,
        "landed-probe ran"
    );
}

// ---------------------------------------------------------------------------------------------
// P-7: a folder deleted during the walk is transient; a folder that never holds still is named.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_remote_subfolder_that_vanishes_mid_walk_is_transient_and_withholds_deletes() {
    let fixture = fixture("p7-vanish", &["a.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p7");
    server.add_file("/team/p7/old.txt", b"old", 1_700_000_000);
    server.add_directory("/team/p7/tmp-123");
    server.add_file("/team/p7/tmp-123/scratch", b"x", 1_700_000_000);
    server.vanish_after_listing("/team/p7", "/team/p7/tmp-123");

    let output = sync(&fixture, "/team/p7", &server, &["--delete"]);

    assert_eq!(output.status.code(), Some(3), "{}", describe(&output));
    assert_eq!(text(&server, "/team/p7/a.txt").as_deref(), Some("a.txt"));
    assert!(
        server.file_contents("/team/p7/old.txt").is_some(),
        "the vanished folder's parent listing proves nothing"
    );
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 0);
    let document = result_document(&output);
    assert!(
        deferrals(&document)
            .iter()
            .any(|(reason, path)| reason == "remote-changed" && path == "tmp-123"),
        "{}",
        describe(&output)
    );
}

#[test]
fn a_vanished_subfolder_is_reported_by_the_walk() {
    let server = MockFileStation::start();
    server.add_directory("/team/p7w");
    server.add_directory("/team/p7w/tmp-123");
    server.add_file("/team/p7w/tmp-123/scratch", b"x", 1_700_000_000);
    server.vanish_after_listing("/team/p7w", "/team/p7w/tmp-123");
    let client = connect(&server, 0);

    let scanned = client
        .remote_inventory_scoped(
            &RemoteRoot::parse("/team/p7w").unwrap(),
            &Scope::root(),
            &IgnoreRules::none(),
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();

    assert_eq!(scanned.vanished_directories, ["tmp-123"]);
    assert!(!scanned.inventory.entries.contains_key("tmp-123"));
    assert_eq!(scanned.incomplete_directories, [String::new()]);
}

#[test]
fn a_remote_folder_that_changes_on_every_pass_is_named_and_nothing_is_deleted_there() {
    let fixture = fixture("p7-churn", &["a.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p7c");
    server.add_directory("/team/p7c/logs");
    // One page is enough: a total that moves between the two passes is the same disagreement.
    for index in 0..3 {
        server.add_file(
            &format!("/team/p7c/logs/{index:04}.log"),
            b"l",
            1_700_000_000,
        );
    }
    server.churn_listing("/team/p7c/logs");

    let output = sync(&fixture, "/team/p7c", &server, &["--delete"]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(text(&server, "/team/p7c/a.txt").as_deref(), Some("a.txt"));
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 0);
    assert!(server.file_contents("/team/p7c/logs/0000.log").is_some());
    let document = result_document(&output);
    assert!(
        document["result"]["incomplete_listing_folders"]
            .as_array()
            .is_some_and(|folders| folders.iter().any(|folder| folder == "logs")),
        "{}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------------------------
// P-8: the proxy's static hint survives on authenticated APIs; the body still does not.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_413_on_an_upload_names_the_body_size_limit_without_echoing_the_body() {
    let fixture = fixture("p8", &["a.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p8");
    server.fail_next_http_operation("SYNO.FileStation.Upload.upload", 413);

    let output = sync(&fixture, "/team/p8", &server, &[]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("body-size") || stderr.contains("body size"),
        "{}",
        describe(&output)
    );
    assert!(
        !stderr.contains("injected transient mock failure"),
        "{stderr}"
    );
}

// ---------------------------------------------------------------------------------------------
// P-9: a login a proxy answered 502 is tried again; a refused password is not.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_login_answered_502_by_the_proxy_is_tried_again() {
    let fixture = fixture("p9", &["a.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/p9");
    server.fail_next_http_operation("SYNO.API.Auth.login", 502);

    let output = sync(&fixture, "/team/p9", &server, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(count(&server, "SYNO.API.Auth.login"), 2);
    assert_eq!(server.logins_seen(), 1);
}

#[test]
fn a_refused_password_is_never_tried_again() {
    let fixture = fixture("p9-refused", &["a.txt"]);
    fs::write(&fixture.password, "wrong password\n").unwrap();
    let server = MockFileStation::start();
    server.add_directory("/team/p9r");

    let output = sync(&fixture, "/team/p9r", &server, &[]);

    assert_ne!(output.status.code(), Some(0));
    assert_eq!(count(&server, "SYNO.API.Auth.login"), 1);
}

// ---------------------------------------------------------------------------------------------
// R-6: a torn in-place overwrite found by the last attempt's probe gets one more full attempt.
// ---------------------------------------------------------------------------------------------

fn torn_overwrite(label: &str, tears: usize) -> (Fixture, MockFileStation, Output) {
    let fixture = fixture(label, &[]);
    let local = std::path::Path::new(&fixture.source).join("big.bin");
    let content: Vec<u8> = (0..64 * 1024).map(|index| (index % 251) as u8).collect();
    fs::write(&local, &content).unwrap();
    let server = MockFileStation::start();
    server.add_directory("/team/r6");
    server.add_file("/team/r6/big.bin", &vec![0xAA; 64 * 1024], 1_600_000_000);
    for _ in 0..tears {
        server.tear_next_upload_after(1024);
    }
    let output = sync(&fixture, "/team/r6", &server, &["--retries", "0"]);
    (fixture, server, output)
}

#[test]
fn a_torn_overwrite_is_repaired_by_one_more_attempt() {
    let (fixture, server, output) = torn_overwrite("r6-repair", 1);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let expected = fs::read(std::path::Path::new(&fixture.source).join("big.bin")).unwrap();
    assert_eq!(server.file_contents("/team/r6/big.bin"), Some(expected));
    assert_eq!(count(&server, "SYNO.FileStation.Upload.upload"), 2);
}

#[test]
fn a_torn_overwrite_whose_extra_attempt_also_fails_is_named_and_not_retried_again() {
    let (_fixture, server, output) = torn_overwrite("r6-twice", 2);

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("big.bin"), "{stderr}");
    assert!(stderr.contains("--compare content"), "{stderr}");
    // Exactly one extra attempt, never a loop, and never a placeholder write.
    assert_eq!(count(&server, "SYNO.FileStation.Upload.upload"), 2);
    assert!(
        server
            .requests()
            .iter()
            .filter(|request| request.operation() == "SYNO.FileStation.Upload.upload")
            .all(|request| request.upload_bytes == Some(64 * 1024)),
        "every attempt carried the whole file"
    );
}

// ---------------------------------------------------------------------------------------------
// N1: a mutation of a path whose part before the first comma names another entry is withheld.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_delete_of_a_comma_path_whose_prefix_exists_is_refused_by_the_client() {
    let server = MockFileStation::start();
    server.add_directory("/team/n1");
    server.add_file("/team/n1/a", b"a", 1_700_000_000);
    server.add_file("/team/n1/a,b.txt", b"ab", 1_700_000_000);
    let client = connect(&server, 0);

    let result =
        client.delete_non_recursive(&RemoteRoot::parse("/team/n1").unwrap(), "/team/n1/a,b.txt");

    assert!(
        matches!(
            result,
            Err(synology_drive_sync::Error::UnsafeRemotePath { .. })
        ),
        "{result:?}"
    );
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 0);
    assert!(server.file_contents("/team/n1/a").is_some());
}

#[test]
fn a_delete_of_a_comma_path_whose_prefix_is_absent_proceeds() {
    let server = MockFileStation::start();
    server.add_directory("/team/n1b");
    server.add_file("/team/n1b/a,b.txt", b"ab", 1_700_000_000);
    let client = connect(&server, 0);

    client
        .delete_non_recursive(
            &RemoteRoot::parse("/team/n1b").unwrap(),
            "/team/n1b/a,b.txt",
        )
        .unwrap();

    assert!(server.file_contents("/team/n1b/a,b.txt").is_none());
}

#[test]
fn a_mirror_withholds_deleting_a_comma_path_whose_prefix_is_an_entry() {
    let fixture = fixture("n1-mirror", &["a"]);
    let server = MockFileStation::start();
    server.add_directory("/team/n1m");
    server.add_file("/team/n1m/a", b"a", 1_600_000_000);
    server.add_file("/team/n1m/a,b.txt", b"ab", 1_700_000_000);

    let output = sync(&fixture, "/team/n1m", &server, &["--delete"]);

    assert!(
        server.file_contents("/team/n1m/a").is_some(),
        "{}",
        describe(&output)
    );
    assert!(server.file_contents("/team/n1m/a,b.txt").is_some());
    assert_eq!(count(&server, "SYNO.FileStation.Delete.delete"), 0);
    let document = result_document(&output);
    assert!(
        deferrals(&document)
            .iter()
            .any(|(reason, path)| reason == "remote-skipped" && path == "a,b.txt"),
        "{}",
        describe(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
}

#[test]
fn a_mirror_deletes_a_comma_path_whose_prefix_is_nothing() {
    let fixture = fixture("n1-plain", &["c.txt"]);
    let server = MockFileStation::start();
    server.add_directory("/team/n1p");
    server.add_file("/team/n1p/a,b.txt", b"ab", 1_700_000_000);

    let output = sync(&fixture, "/team/n1p", &server, &["--delete"]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(server.file_contents("/team/n1p/a,b.txt").is_none());
}

// ---------------------------------------------------------------------------------------------
// P-6: a DSM that ends every session is given a bounded number of re-logins, and says so.
// ---------------------------------------------------------------------------------------------

#[test]
fn sessions_killed_after_every_request_fail_the_run_naming_the_cap() {
    let names: Vec<String> = (0..12).map(|index| format!("f{index:02}.txt")).collect();
    let references: Vec<&str> = names.iter().map(String::as_str).collect();
    let fixture = fixture("p6", &references);
    let server = MockFileStation::start();
    server.add_directory("/team/p6");
    server.expire_session_every(1);

    let output = sync(&fixture, "/team/p6", &server, &[]);

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("re-authentication stopped") && stderr.contains("DSM ended"),
        "{stderr}"
    );
    // One login, then at most the four the rolling window admits.
    assert!(server.logins_seen() <= 5, "{}", server.logins_seen());
}

// ---------------------------------------------------------------------------------------------
// G-5 / RR-7: a 105 is a permission refusal of the path only once the session is proven alive.
// ---------------------------------------------------------------------------------------------

fn upload_source(directory: &TestDir, name: &str, contents: &[u8]) -> LocalEntry {
    let full_path = directory.write(name, contents);
    let modified = fs::metadata(&full_path).unwrap().modified().unwrap();
    LocalEntry {
        relative: name.to_owned(),
        mtime_ms: i64::try_from(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap(),
        full_path,
        kind: EntryKind::File,
        size: contents.len() as u64,
        identity: FileIdentity::default(),
        content_md5: None,
    }
}

#[test]
fn a_105_on_a_live_session_is_a_permission_refusal_of_the_path() {
    let directory = TestDir::new("rr7-live");
    let local = upload_source(&directory, "a.txt", b"a");
    let server = MockFileStation::start();
    server.add_directory("/team/rr7");
    let client = connect(&server, 0);
    server.fail_next_api_operation("SYNO.FileStation.Upload.upload", 105);

    let error = client.upload(&local, "/team/rr7/a.txt").unwrap_err();

    assert_eq!(
        error.refusal_class(),
        Some(synology_drive_sync::error::RefusalClass::Permission),
        "{error:?}"
    );
    assert_eq!(error.refused_remote_path(), Some("/team/rr7/a.txt"));
}

#[test]
fn a_105_on_a_dead_session_without_a_re_login_stays_a_session_failure() {
    let directory = TestDir::new("rr7-dead");
    let local = upload_source(&directory, "a.txt", b"a");
    let server = MockFileStation::start();
    server.add_directory("/team/rr7d");
    // The SDK's shape: no re-authenticator. The session dies at the upload, and DSM says so with
    // 105, to the liveness probe as well.
    let client = connect(&server, 0);
    server.answer_dead_sessions_with(105);
    server.expire_session_on("SYNO.FileStation.Upload.upload");

    let error = client.upload(&local, "/team/rr7d/a.txt").unwrap_err();

    assert!(
        matches!(error, synology_drive_sync::Error::Api { code: 105, .. }),
        "{error:?}"
    );
    assert_eq!(error.refusal_class(), None);
}

/// An upload the client gives up part-way through its body (cancellation, a breaker stopping
/// peers) is legitimate: the mock counts it, commits nothing, and does not fail the test.
#[test]
fn the_mock_counts_an_upload_body_the_client_abandons() {
    use std::io::Write as _;
    let server = MockFileStation::start();
    let address = server
        .base_url()
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .write_all(
            b"POST /prefix/webapi/entry.cgi HTTP/1.1\r\nHost: mock\r\n\
              Content-Type: multipart/form-data; boundary=x\r\nContent-Length: 100000\r\n\r\npartial",
        )
        .unwrap();
    drop(stream);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.aborted_requests() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.aborted_requests(), 1);
    assert!(server.requests().is_empty());
}
