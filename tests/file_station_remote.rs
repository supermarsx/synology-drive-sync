//! The File Station layer against DSM behaviour the plain mock leaves out (ADV-R-*).
//!
//! Each test names the finding it pins. The mock's opt-in modes model what a real NAS does and the
//! default mock does not: names it refuses, failures reported as envelope codes, links inside a
//! share, an environment forward proxy.
#![allow(dead_code)]

mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;
use synology_drive_sync::Error;
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

fn modified_ms(path: &Path) -> i64 {
    i64::try_from(
        fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A local file entry built directly, so a name the local scan would refuse can still be offered
/// to the upload path.
fn local_file(directory: &TestDir, name: &str, contents: &[u8]) -> LocalEntry {
    let full_path = directory.write(name, contents);
    LocalEntry {
        relative: name.to_owned(),
        mtime_ms: modified_ms(&full_path),
        full_path,
        kind: EntryKind::File,
        size: contents.len() as u64,
        identity: FileIdentity::default(),
        content_md5: None,
    }
}

fn run(arguments: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let environment = TestDir::new("remote-env");
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
    for (name, value) in extra_env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn sync_arguments<'a>(
    source: &'a str,
    destination: &'a str,
    url: &'a str,
    password: &'a str,
    extra: &[&'a str],
) -> Vec<&'a str> {
    let mut arguments = vec![
        "--output",
        "json",
        "sync",
        source,
        destination,
        "--url",
        url,
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
    arguments
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

fn count_requests(server: &MockFileStation, operation: &str) -> usize {
    server
        .requests()
        .iter()
        .filter(|request| request.operation() == operation)
        .count()
}

// ---------------------------------------------------------------------------------------------
// ADV-R-1: a name DSM refuses is a refusal of that one path, and it names the path.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_upload_dsm_refuses_by_name_is_a_per_path_refusal_naming_the_path() {
    let server = MockFileStation::start();
    server.add_directory("/team/r1");
    // eCryptfs-encrypted shares hold names up to about 143 bytes: 50 CJK characters are 150.
    server.refuse_names_over_bytes(143, 418);
    let source = TestDir::new("r1-upload");
    let name = format!("m-{}.txt", "\u{65e5}".repeat(50));
    let local = local_file(&source, &name, b"m");
    let client = connect(&server, 0);
    let remote = format!("/team/r1/{name}");

    let error = client
        .upload(&local, &remote)
        .expect_err("DSM refuses the name");

    assert_eq!(
        error.refused_remote_path(),
        Some(remote.as_str()),
        "{error}"
    );
    assert_eq!(error.api_code(), Some(418));
    let rendered = error.to_string();
    assert!(
        rendered.contains(&name),
        "the error names the file: {rendered}"
    );
    assert!(
        rendered.contains("illegal remote name or path"),
        "{rendered}"
    );
}

#[test]
fn a_folder_dsm_refuses_by_name_is_a_per_path_refusal_naming_the_path() {
    let server = MockFileStation::start();
    server.add_directory("/team/r1");
    server.refuse_names_over_bytes(143, 418);
    let client = connect(&server, 0);
    let remote = format!("/team/r1/d-{}", "\u{65e5}".repeat(50));

    let error = client
        .create_folder(&remote)
        .expect_err("DSM refuses the name");

    assert_eq!(
        error.refused_remote_path(),
        Some(remote.as_str()),
        "{error}"
    );
    assert!(error.to_string().contains(&remote[9..]), "{error}");
}

#[test]
fn a_quota_refusal_stays_a_run_failure_but_names_the_path() {
    let server = MockFileStation::start();
    server.add_directory("/team/r1");
    server.fail_next_api_operation("SYNO.FileStation.Upload.upload", 415);
    let source = TestDir::new("r1-quota");
    let local = local_file(&source, "a.txt", b"a");
    let client = connect(&server, 0);

    let error = client
        .upload(&local, "/team/r1/a.txt")
        .expect_err("quota exceeded");

    assert!(
        matches!(error, Error::Api { code: 415, .. }),
        "a full quota concerns every file, not this one: {error:?}"
    );
    assert_eq!(error.refused_remote_path(), None);
    assert!(error.to_string().contains("/team/r1/a.txt"), "{error}");
}

#[test]
fn creating_a_folder_that_already_exists_as_a_folder_is_not_a_refusal() {
    let server = MockFileStation::start();
    server.add_directory("/team/r1");
    server.add_directory("/team/r1/there");
    let client = connect(&server, 0);

    client
        .create_folder("/team/r1/there")
        .expect("a folder already in place is what the run wanted");

    server.add_file("/team/r1/file", b"x", 1_700_000_000);
    let error = client
        .create_folder("/team/r1/file")
        .expect_err("a file in the way is a refusal of that path");
    assert_eq!(error.refused_remote_path(), Some("/team/r1/file"));
    assert_eq!(error.api_code(), Some(414));
}

#[test]
fn a_refused_name_no_longer_stops_the_rest_of_the_tree() {
    let fixture = TestDir::new("r1-cli");
    let source = fixture.child("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), b"a").unwrap();
    let long = format!("m-{}.txt", "\u{65e5}".repeat(50));
    fs::write(source.join(&long), b"m").unwrap();
    fs::write(source.join("z.txt"), b"z").unwrap();
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/r1");
    server.refuse_names_over_bytes(143, 418);
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/r1",
        server.base_url(),
        password.to_str().unwrap(),
        &[],
    );

    let output = run(&arguments, &[]);

    assert!(
        server.file_contents("/team/r1/z.txt").is_some(),
        "the file queued after the refused one is uploaded: {}",
        describe(&output)
    );
    let document = result_document(&output);
    let deferred = document["result"]["deferred"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        deferred
            .iter()
            .any(|record| record["path"] == long.as_str() && record["reason"] == "remote-refused"),
        "the refused file is named with its reason: {}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------------------------
// ADV-R-2: a remote name this tool cannot address is skipped by name, never a run failure.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_remote_icon_cr_or_backslash_name_is_skipped_by_name_not_a_run_failure() {
    for name in ["Icon\r", "back\\slash.txt", "new\nline"] {
        let server = MockFileStation::start();
        server.add_directory("/team/r2");
        server.add_directory("/team/r2/photos");
        server.add_file(&format!("/team/r2/photos/{name}"), b"", 1_700_000_000);
        server.add_file("/team/r2/photos/kept.jpg", b"jpg", 1_700_000_000);
        let client = connect(&server, 0);
        let root = RemoteRoot::parse("/team/r2").unwrap();

        let scanned = client
            .remote_inventory_scoped(
                &root,
                &Scope::root(),
                &IgnoreRules::none(),
                usize::MAX,
                &CancellationToken::default(),
            )
            .unwrap_or_else(|error| panic!("{name:?} must not fail the walk: {error}"));

        assert!(scanned.inventory.entries.contains_key("photos/kept.jpg"));
        assert_eq!(scanned.skipped.len(), 1, "{name:?}");
        let skipped = &scanned.skipped[0];
        assert_eq!(skipped.reason, RemoteSkipReason::UnrepresentableName);
        assert_eq!(skipped.parent, "photos");
        assert_eq!(skipped.name, name);
        assert!(!skipped.excluded);
        assert_eq!(skipped.display(), format!("photos/{}", name.escape_debug()));
        assert!(
            scanned.incomplete_directories.is_empty(),
            "the listing itself was whole; the entry is reported as skipped, not as a short listing"
        );
    }
}

#[test]
fn an_excluded_unrepresentable_remote_name_is_marked_excluded() {
    let server = MockFileStation::start();
    server.add_directory("/team/r2");
    server.add_file("/team/r2/Icon\r", b"", 1_700_000_000);
    let source = TestDir::new("r2-rules");
    let rules = IgnoreRules::build(source.path(), &["Icon*".to_owned()]).unwrap();
    let client = connect(&server, 0);

    let scanned = client
        .remote_inventory_scoped(
            &RemoteRoot::parse("/team/r2").unwrap(),
            &Scope::root(),
            &rules,
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();

    assert_eq!(scanned.skipped.len(), 1);
    assert!(
        scanned.skipped[0].excluded,
        "the exclude applies to the raw name"
    );
}

#[test]
fn a_remote_icon_cr_file_neither_fails_the_sync_nor_lets_its_folder_be_deleted() {
    let fixture = TestDir::new("r2-cli");
    let source = fixture.child("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), b"a").unwrap();
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/r2");
    // `photos` is absent locally, so a mirror would delete it and everything in it.
    server.add_directory("/team/r2/photos");
    server.add_file("/team/r2/photos/Icon\r", b"", 1_700_000_000);
    server.add_file("/team/r2/photos/stale.jpg", b"jpg", 1_700_000_000);
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/r2",
        server.base_url(),
        password.to_str().unwrap(),
        &["--compare", "metadata", "--delete"],
    );

    let output = run(&arguments, &[]);

    assert_ne!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        server.file_contents("/team/r2/a.txt").is_some(),
        "{}",
        describe(&output)
    );
    assert!(
        server.file_contents("/team/r2/photos/Icon\r").is_some(),
        "never deleted: {}",
        describe(&output)
    );
    // Its sibling is an ordinary remote-only file and is mirrored away; its folder, which still
    // holds the entry, is kept and named.
    assert!(
        server.file_contents("/team/r2/photos/stale.jpg").is_none(),
        "{}",
        describe(&output)
    );
    assert!(
        server.directories().contains(&"/team/r2/photos".to_owned()),
        "a folder holding an entry the tool cannot address is never deleted: {}",
        describe(&output)
    );
    let document = result_document(&output);
    let deferred = document["result"]["deferred"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        deferred
            .iter()
            .any(|record| record["reason"] == "remote-skipped"
                && (record["path"] == "photos/Icon\\r"
                    || record["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("photos/Icon\\\\r")))),
        "the entry is named: {}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------------------------
// ADV-R-5: a link inside the destination is never descended into, and nothing is deleted
// through it.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_folder_link_out_of_the_destination_is_recorded_but_never_descended() {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/elsewhere");
    server.add_file("/team/elsewhere/precious.txt", b"precious", 1_700_000_000);
    server.add_directory_symlink("/team/r5/link", "/team/elsewhere");
    let client = connect(&server, 0);

    let scanned = client
        .remote_inventory_scoped(
            &RemoteRoot::parse("/team/r5").unwrap(),
            &Scope::root(),
            &IgnoreRules::none(),
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();

    let link = &scanned.inventory.entries["link"];
    assert_eq!(
        link.mount_point_type.as_deref(),
        Some(LINKED_ENTRY_MOUNT_TYPE)
    );
    assert!(
        !scanned.inventory.entries.contains_key("link/precious.txt"),
        "never descended: {:?}",
        scanned.inventory.entries.keys().collect::<Vec<_>>()
    );
    assert_eq!(scanned.skipped.len(), 1);
    assert_eq!(
        scanned.skipped[0].reason,
        RemoteSkipReason::LinksOutsideDestination
    );
    assert!(
        scanned.skipped[0]
            .detail()
            .contains("/volume1/team/elsewhere"),
        "{}",
        scanned.skipped[0].detail()
    );
}

/// ADV-R-5, fail closed: once DSM reports real paths, an entry it gives none for is treated as a
/// link rather than walked on trust.
#[test]
fn a_folder_without_a_real_path_is_never_descended_once_real_paths_are_reported() {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/r5/opaque");
    server.add_file("/team/r5/opaque/inside.txt", b"i", 1_700_000_000);
    server.add_directory("/team/r5/plain");
    server.withhold_real_path("/team/r5/opaque");
    let client = connect(&server, 0);

    let scanned = client
        .remote_inventory_scoped(
            &RemoteRoot::parse("/team/r5").unwrap(),
            &Scope::root(),
            &IgnoreRules::none(),
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();

    assert!(scanned.real_paths_reported);
    assert_eq!(
        scanned.inventory.entries["opaque"]
            .mount_point_type
            .as_deref(),
        Some(LINKED_ENTRY_MOUNT_TYPE)
    );
    assert!(!scanned.inventory.entries.contains_key("opaque/inside.txt"));
    assert_eq!(scanned.inventory.entries["plain"].mount_point_type, None);
    assert_eq!(scanned.skipped.len(), 1);
    assert!(
        scanned.skipped[0].detail().contains("no real path"),
        "{}",
        scanned.skipped[0].detail()
    );
}

#[test]
fn a_destination_without_real_paths_reports_that_the_link_guard_could_not_run() {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/r5/sub");
    server.withhold_real_path("/team/r5");
    let client = connect(&server, 0);

    let scanned = client
        .remote_inventory_scoped(
            &RemoteRoot::parse("/team/r5").unwrap(),
            &Scope::root(),
            &IgnoreRules::none(),
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();

    assert!(!scanned.real_paths_reported);
}

/// DSM volumes are case-sensitive: `Photos` and `photos` side by side are two ordinary folders,
/// and both are walked. Neither is a link back to the other.
#[test]
fn sibling_folders_differing_only_in_case_are_both_walked() {
    let server = MockFileStation::start();
    server.add_directory("/team/case");
    server.add_directory("/team/case/Photos");
    server.add_directory("/team/case/Photos/@eaDir");
    server.add_file("/team/case/Photos/a.jpg", b"A", 1_700_000_000);
    server.add_directory("/team/case/photos");
    server.add_file("/team/case/photos/a.jpg", b"a", 1_700_000_000);
    let client = connect(&server, 0);

    let scanned = client
        .remote_inventory_scoped(
            &RemoteRoot::parse("/team/case").unwrap(),
            &Scope::root(),
            &IgnoreRules::none(),
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();

    assert!(scanned.real_paths_reported);
    assert!(scanned.skipped.is_empty(), "{:?}", scanned.skipped);
    for folder in ["Photos", "photos"] {
        assert_eq!(
            scanned.inventory.entries[folder].mount_point_type, None,
            "{folder}"
        );
        assert!(
            scanned
                .inventory
                .entries
                .contains_key(&format!("{folder}/a.jpg")),
            "{folder} is walked"
        );
    }
}

/// A DSM whose real paths do not mean what the guard assumes would mark every folder a link; the
/// SDK fails such a run by name instead of skipping the tree folder by folder.
#[test]
fn misreported_real_paths_mark_folders_as_links_for_the_breaker_to_catch() {
    let server = MockFileStation::start();
    server.add_directory("/team/mis");
    for folder in ["a", "b", "c", "d"] {
        server.add_directory(&format!("/team/mis/{folder}"));
        server.add_file(&format!("/team/mis/{folder}/x.txt"), b"x", 1_700_000_000);
    }
    server.misreport_real_paths();
    let client = connect(&server, 0);

    let scanned = client
        .remote_inventory_scoped(
            &RemoteRoot::parse("/team/mis").unwrap(),
            &Scope::root(),
            &IgnoreRules::none(),
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();

    assert_eq!(scanned.skipped.len(), 4, "{:?}", scanned.skipped);
    assert!(synology_drive_sync::sync::check_link_boundaries(&scanned.inventory).is_err());
}

#[test]
fn a_link_cycle_ends_the_walk() {
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_file("/team/r5/a.txt", b"a", 1_700_000_000);
    server.add_directory_symlink("/team/r5/loop", "/team/r5");
    let client = connect(&server, 0);

    let scanned = client
        .remote_inventory_scoped(
            &RemoteRoot::parse("/team/r5").unwrap(),
            &Scope::root(),
            &IgnoreRules::none(),
            usize::MAX,
            &CancellationToken::default(),
        )
        .unwrap();

    assert_eq!(
        scanned.inventory.entries.keys().collect::<Vec<_>>(),
        ["a.txt", "loop"]
    );
    assert_eq!(scanned.skipped.len(), 1);
    assert!(count_requests(&server, "SYNO.FileStation.List.list") <= 2);
}

#[test]
fn a_mirror_never_deletes_through_a_folder_link() {
    let fixture = TestDir::new("r5-cli");
    let source = fixture.child("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), b"a").unwrap();
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/r5");
    server.add_directory("/team/elsewhere");
    server.add_file("/team/elsewhere/precious.txt", b"precious", 1_700_000_000);
    server.add_directory_symlink("/team/r5/backup-link", "/team/elsewhere");
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/r5",
        server.base_url(),
        password.to_str().unwrap(),
        &["--compare", "metadata", "--delete"],
    );

    let output = run(&arguments, &[]);

    assert!(
        server
            .file_contents("/team/elsewhere/precious.txt")
            .is_some(),
        "deleted through the link: {}",
        describe(&output)
    );
    assert!(server.directories().contains(&"/team/elsewhere".to_owned()));
    assert_eq!(count_requests(&server, "SYNO.FileStation.Delete.delete"), 0);
    assert!(server.file_contents("/team/r5/a.txt").is_some());
}

// ---------------------------------------------------------------------------------------------
// ADV-R-3: a CopyMove failure DSM reports as an envelope code defers the copy.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_copy_dsm_fails_with_an_envelope_code_is_deferred_and_not_stopped() {
    let fixture = TestDir::new("r3-copy");
    let source = fixture.child("source");
    fs::create_dir_all(source.join("new")).unwrap();
    let local_file = source.join("new/item.bin");
    fs::write(&local_file, b"copy-me-on-nas").unwrap();
    fs::write(source.join("other.txt"), b"other").unwrap();
    let mtime = modified_ms(&local_file).div_euclid(1000);
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/copy");
    server.add_directory("/team/copy/old");
    server.add_file("/team/copy/old/item.bin", b"copy-me-on-nas", mtime);
    server.fail_next_copy_status_with_envelope(1002);
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/copy",
        server.base_url(),
        password.to_str().unwrap(),
        &[],
    );

    let output = run(&arguments, &[]);

    assert_ne!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        server.file_contents("/team/copy/other.txt").is_some(),
        "an unrelated upload still happens: {}",
        describe(&output)
    );
    assert_eq!(
        count_requests(&server, "SYNO.FileStation.CopyMove.stop"),
        0,
        "a task DSM reported ended is not stopped"
    );
    let document = result_document(&output);
    assert!(
        document["result"]["deferred"]
            .as_array()
            .is_some_and(|records| records
                .iter()
                .any(|record| record["reason"] == "server-copy-failed")),
        "{}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------------------------
// ADV-R-7: CopyMove.start re-authenticates like every other mutation.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_session_expiry_at_copy_start_is_recovered() {
    let fixture = TestDir::new("r7-copy");
    let source = fixture.child("source");
    fs::create_dir_all(source.join("new")).unwrap();
    let local_file = source.join("new/item.bin");
    fs::write(&local_file, b"copy-me-on-nas").unwrap();
    let mtime = modified_ms(&local_file).div_euclid(1000);
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/copy");
    server.add_directory("/team/copy/old");
    server.add_file("/team/copy/old/item.bin", b"copy-me-on-nas", mtime);
    server.expire_session_on("SYNO.FileStation.CopyMove.start");
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/copy",
        server.base_url(),
        password.to_str().unwrap(),
        &[],
    );

    let output = run(&arguments, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(server.logins_seen(), 2, "re-authenticated and resent");
    assert_eq!(
        count_requests(&server, "SYNO.FileStation.CopyMove.start"),
        2
    );
    assert!(server.file_contents("/team/copy/new/item.bin").is_some());
}

// ---------------------------------------------------------------------------------------------
// ADV-R-9: an environment forward proxy is named, not blamed on a reverse proxy.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_environment_forward_proxy_is_named_when_discovery_fails_through_it() {
    let fixture = TestDir::new("r9-proxy");
    let source = fixture.child("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), b"a").unwrap();
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/p");
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/p",
        server.base_url(),
        password.to_str().unwrap(),
        &["--connect-timeout", "2", "--retries", "0"],
    );

    let output = run(
        &arguments,
        &[
            ("HTTP_PROXY", "http://user:secret@127.0.0.1:9"),
            ("http_proxy", "http://user:secret@127.0.0.1:9"),
        ],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let all = format!("{stdout}{stderr}");
    assert!(
        all.contains("forward proxy http://127.0.0.1:9/ (from HTTP_PROXY)"),
        "{}",
        describe(&output)
    );
    assert!(!all.contains("reverse proxy;"), "{}", describe(&output));
    assert!(
        !all.contains("secret"),
        "proxy credentials are never printed"
    );
    assert_eq!(server.requests().len(), 0);
}

// ---------------------------------------------------------------------------------------------
// ADV-R-10: an upload the rate cap cannot finish in time is refused before it is sent.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_upload_that_cannot_finish_within_the_timeout_is_refused_before_sending() {
    let fixture = TestDir::new("r10-rate");
    let source = fixture.child("source");
    fs::create_dir_all(&source).unwrap();
    // 64 KiB at 1 KiB/s is over a minute; the deadline is 5 s.
    fs::write(source.join("big.bin"), vec![7_u8; 64 * 1024]).unwrap();
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/r10");
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/r10",
        server.base_url(),
        password.to_str().unwrap(),
        &["--max-rate", "1024", "--timeout", "5", "--retries", "2"],
    );

    let output = run(&arguments, &[]);

    assert_ne!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        count_requests(&server, "SYNO.FileStation.Upload.upload"),
        0,
        "nothing is sent: {}",
        describe(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("big.bin") && stderr.contains("--timeout"),
        "{}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------------------------
// ADV-R-11: a retry's "did it land?" probe also requires the modification time it sent.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_retried_touch_only_upload_leaves_the_new_modification_time() {
    let fixture = TestDir::new("r11-touch");
    let source = fixture.child("source");
    fs::create_dir_all(&source).unwrap();
    let local = source.join("same.txt");
    fs::write(&local, b"same bytes").unwrap();
    let local_seconds = modified_ms(&local).div_euclid(1000);
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/r11");
    // Identical content, older time: the plan re-uploads to carry the new time.
    server.add_file("/team/r11/same.txt", b"same bytes", local_seconds - 3600);
    // The first attempt fails in transport before DSM writes anything.
    server.fail_next_http_operation("SYNO.FileStation.Upload.upload", 502);
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/r11",
        server.base_url(),
        password.to_str().unwrap(),
        &["--compare", "metadata", "--retries", "2"],
    );

    let output = run(&arguments, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        server.file_mtime("/team/r11/same.txt"),
        Some(local_seconds),
        "the old copy matched by content only; it was resent with the new time: {}",
        describe(&output)
    );
    assert_eq!(count_requests(&server, "SYNO.FileStation.Upload.upload"), 2);
}

// ---------------------------------------------------------------------------------------------
// ADV-R-6: an upload whose last attempt failed but whose bytes landed is not a failure.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_upload_whose_last_attempt_landed_despite_a_transport_error_is_proved_and_kept() {
    let fixture = TestDir::new("r6-landed");
    let source = fixture.child("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), b"landed").unwrap();
    let password = fixture.write("password", format!("{PASSWORD}\n").as_bytes());
    let server = MockFileStation::start();
    server.add_directory("/team/r6");
    // DSM commits the file, then the proxy loses the response.
    server.fail_next_upload_response_after_commit(502);
    let arguments = sync_arguments(
        source.to_str().unwrap(),
        "/team/r6",
        server.base_url(),
        password.to_str().unwrap(),
        &["--retries", "0"],
    );

    let output = run(&arguments, &[]);

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(count_requests(&server, "SYNO.FileStation.Upload.upload"), 1);
}

// ---------------------------------------------------------------------------------------------
// C-R2 / ADV-E-1: Rename, and the sibling listing a scoped run checks.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_case_only_rename_moves_the_entry_and_is_verified() {
    let server = MockFileStation::start();
    server.add_directory("/team/rn");
    server.add_directory("/team/rn/Photos");
    server.add_file("/team/rn/Photos/a.jpg", b"a", 1_700_000_000);
    let client = connect(&server, 0);
    let root = RemoteRoot::parse("/team/rn").unwrap();
    assert!(client.supports_rename());

    let renamed = client
        .rename(&root, "/team/rn/Photos", "photos")
        .expect("a case-only rename");

    assert_eq!(renamed, "/team/rn/photos");
    assert!(server.file_contents("/team/rn/photos/a.jpg").is_some());
    assert!(!server.directories().contains(&"/team/rn/Photos".to_owned()));
}

#[test]
fn a_rename_onto_a_taken_name_or_a_failing_rename_names_the_path_and_moves_nothing() {
    let server = MockFileStation::start();
    server.add_directory("/team/rn");
    server.add_file("/team/rn/a.txt", b"a", 1_700_000_000);
    server.add_file("/team/rn/A.txt", b"A", 1_700_000_000);
    let client = connect(&server, 0);
    let root = RemoteRoot::parse("/team/rn").unwrap();

    let taken = client
        .rename(&root, "/team/rn/a.txt", "A.txt")
        .expect_err("the new name is taken");
    assert_eq!(taken.refused_remote_path(), Some("/team/rn/a.txt"));
    assert_eq!(taken.api_code(), Some(414));

    server.fail_api_operation_times("SYNO.FileStation.Rename.rename", 900, 2);
    for _ in 0..2 {
        let failed = client
            .rename(&root, "/team/rn/a.txt", "b.txt")
            .expect_err("injected failure");
        assert!(failed.to_string().contains("/team/rn/a.txt"), "{failed}");
    }
    assert_eq!(server.file_contents("/team/rn/a.txt"), Some(b"a".to_vec()));
    assert_eq!(server.file_contents("/team/rn/A.txt"), Some(b"A".to_vec()));
    assert!(server.file_contents("/team/rn/b.txt").is_none());

    let escaped = client.rename(&root, "/team/rn/a.txt", "../x.txt");
    assert!(
        escaped.is_err(),
        "a new name is one component inside the root"
    );
}

#[test]
fn child_names_are_listed_exactly_as_dsm_lists_them() {
    let server = MockFileStation::start();
    server.add_directory("/team/ls");
    server.add_directory("/team/ls/@eaDir");
    server.add_file("/team/ls/Readme.TXT", b"r", 1_700_000_000);
    server.add_file("/team/ls/readme.txt", b"r", 1_700_000_000);
    let client = connect(&server, 0);
    let cancellation = CancellationToken::default();

    let listed = client.list_child_names("/team/ls", &cancellation).unwrap();
    assert!(listed.complete);
    assert_eq!(listed.names, ["@eaDir", "Readme.TXT", "readme.txt"]);

    let absent = client
        .list_child_names("/team/ls/missing", &cancellation)
        .unwrap();
    assert!(absent.names.is_empty() && absent.complete);

    server.hide_list_entries("/team/ls", 1);
    let short = client.list_child_names("/team/ls", &cancellation).unwrap();
    assert!(
        !short.complete,
        "a short listing cannot prove a name absent"
    );
}
