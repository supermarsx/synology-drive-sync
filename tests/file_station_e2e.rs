mod support;

use std::fs;
use std::process::{Command, Output};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("process-environment");
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

fn modified_seconds(path: &std::path::Path) -> i64 {
    i64::try_from(
        fs::metadata(path)
            .expect("fixture metadata")
            .modified()
            .expect("fixture modification time")
            .duration_since(UNIX_EPOCH)
            .expect("fixture predates Unix epoch")
            .as_secs(),
    )
    .expect("fixture timestamp fits i64")
}

fn stdout_json(output: &Output) -> Value {
    assert!(
        output.stdout.ends_with(b"\n"),
        "machine output must end in exactly one record delimiter: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).expect("stdout is one JSON document")
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(output.stderr, b"", "quiet success must keep stderr empty");
}

fn stdout_ndjson(output: &Output) -> Vec<Value> {
    assert!(
        output.stdout.ends_with(b"\n"),
        "NDJSON output must end in a record delimiter"
    );
    String::from_utf8(output.stdout.clone())
        .expect("stdout is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid NDJSON record"))
        .collect()
}

fn toml_path(path: &std::path::Path) -> String {
    path.to_str()
        .expect("fixture path is UTF-8")
        .replace('\\', "/")
}

fn is_mutation(operation: &str) -> bool {
    matches!(
        operation,
        "SYNO.FileStation.CreateFolder.create"
            | "SYNO.FileStation.Upload.upload"
            | "SYNO.FileStation.CopyMove.start"
            | "SYNO.FileStation.Delete.delete"
    )
}

#[allow(clippy::too_many_arguments)]
fn write_two_profile_config(
    fixture: &TestDir,
    password: &std::path::Path,
    alpha_source: &std::path::Path,
    alpha_url: &str,
    alpha_remote: &str,
    beta_source: &std::path::Path,
    beta_url: &str,
    beta_remote: &str,
    compare: &str,
    delete: bool,
    retries: u8,
) -> std::path::PathBuf {
    let config = format!(
        r#"default-profile = "alpha"

[profiles.alpha]
source = "{}"
remote = "{alpha_remote}"
url = "{alpha_url}"
username = "e2e-user"
password-file = "{}"
no-vault = true
allow-http = true
compare = "{compare}"
jobs = 1
delete = {delete}
max-delete = 20
retries = {retries}
progress = "never"

[profiles.beta]
source = "{}"
remote = "{beta_remote}"
url = "{beta_url}"
username = "e2e-user"
password-file = "{}"
no-vault = true
allow-http = true
compare = "{compare}"
jobs = 1
delete = {delete}
max-delete = 20
retries = {retries}
progress = "never"
"#,
        toml_path(alpha_source),
        toml_path(password),
        toml_path(beta_source),
        toml_path(password),
    );
    fixture.write("config.toml", config.as_bytes())
}

#[test]
fn source_doctor_hashes_the_real_local_tree_without_network_output() {
    let fixture = TestDir::new("source-doctor");
    fixture.write("alpha.txt", b"alpha");
    fixture.write("nested/beta.bin", b"beta-data");
    fs::create_dir(fixture.child("empty")).expect("create empty fixture directory");

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "source",
        fixture.path().to_str().expect("UTF-8 fixture path"),
        "--hash",
    ]);
    assert_success(&output);
    let mut actual = stdout_json(&output);
    actual["source"]["elapsed_ms"] = Value::Null;
    assert_eq!(
        actual,
        json!({
            "schema": "sdsync.source-doctor.v1",
            "source": {
                "canonical_source": fs::canonicalize(fixture.path()).expect("canonical source"),
                "entries": 4,
                "files": 2,
                "directories": 2,
                "bytes": 14,
                "content_hashed": true,
                "hashed_files": 2,
                "elapsed_ms": null
            }
        })
    );
}

#[test]
fn routing_only_doctor_stops_after_reverse_proxy_discovery() {
    let server = MockFileStation::start();
    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--allow-http",
        "--routing-only",
    ]);
    assert_success(&output);
    let actual = stdout_json(&output);
    assert_eq!(actual["schema"], "sdsync.doctor.v1");
    assert_eq!(actual["level"], "quick");
    assert_eq!(actual["status"], "warn");
    assert_eq!(actual["routing"], true);
    assert_eq!(actual["api_discovery"], true);
    assert_eq!(actual["authenticated"], false);
    assert_eq!(actual["remote_checked"], false);
    assert_eq!(actual["remote_inventory"], Value::Null);
    assert_eq!(
        actual["summary"],
        json!({"pass":2,"warn":1,"fail":0,"skip":5})
    );
    let sections = actual["sections"].as_array().expect("section array");
    assert_eq!(sections.len(), 8);
    assert_eq!(sections[0]["id"], "routing_tls");
    assert_eq!(sections[0]["status"], "warn");
    assert_eq!(sections[1]["id"], "dsm_api_discovery");
    assert_eq!(sections[1]["status"], "pass");
    assert_eq!(sections[2]["id"], "dsm_session_auth");
    assert_eq!(sections[2]["status"], "skip");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].request_path, "/prefix/webapi/entry.cgi");
    assert_eq!(requests[0].operation(), "SYNO.API.Info.query");
}

#[test]
fn explicit_quick_target_level_is_unauthenticated_and_does_not_touch_the_destination() {
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--allow-http",
        "--level",
        "quick",
        "target",
        "/team/target",
    ]);
    assert_success(&output);
    let result = stdout_json(&output);
    assert_eq!(result["level"], "quick");
    assert_eq!(result["authenticated"], false);
    assert_eq!(result["remote_checked"], false);
    assert_eq!(result["sections"][2]["id"], "dsm_session_auth");
    assert_eq!(result["sections"][2]["status"], "skip");
    assert_eq!(
        server
            .requests()
            .iter()
            .map(|request| request.operation())
            .collect::<Vec<_>>(),
        ["SYNO.API.Info.query"]
    );
}

#[test]
fn discovery_http_failure_keeps_routing_evidence_and_returns_nonzero() {
    let server = MockFileStation::start();
    server.fail_next_http_operation("SYNO.API.Info.query", 503);
    server.fail_next_http_operation("SYNO.API.Info.query", 503);
    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--allow-http",
        "--retries",
        "0",
        "--routing-only",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let result = stdout_json(&output);
    assert_eq!(result["status"], "fail");
    assert_eq!(result["level"], "quick");
    assert_eq!(result["routing"], true);
    assert_eq!(result["api_discovery"], false);
    assert_eq!(result["sections"][0]["id"], "routing_tls");
    assert_eq!(result["sections"][0]["status"], "warn");
    assert_eq!(result["sections"][1]["id"], "dsm_api_discovery");
    assert_eq!(result["sections"][1]["status"], "fail");
    assert!(
        result["sections"][3]["detail"]
            .as_str()
            .expect("dependent skip detail")
            .contains("failed")
    );
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn authenticated_target_doctor_checks_exact_destination_and_logs_out() {
    let fixture = TestDir::new("target-doctor");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    server.add_file(
        "/team/target/existing.txt",
        b"already remote",
        1_700_000_000,
    );

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--no-vault",
        "--allow-http",
        "target",
        "/team/target",
    ]);
    assert_success(&output);
    let actual = stdout_json(&output);
    assert_eq!(actual["schema"], "sdsync.doctor.v1");
    assert_eq!(actual["level"], "standard");
    assert_eq!(actual["status"], "warn");
    assert_eq!(actual["routing"], true);
    assert_eq!(actual["api_discovery"], true);
    assert_eq!(actual["authenticated"], true);
    assert_eq!(actual["remote_checked"], true);
    assert_eq!(actual["remote_exists"], true);
    assert_eq!(actual["remote_entries"], 1);
    assert_eq!(actual["write_permission_scope"], "exact_destination");
    assert_eq!(actual["write_permission_path"], "/team/target");
    assert_eq!(actual["remote_inventory"]["scope"], "direct_children");
    assert_eq!(actual["remote_inventory"]["total_entries"], 1);
    assert_eq!(actual["remote_inventory"]["sample_count"], 1);
    assert_eq!(actual["remote_inventory"]["truncated"], false);
    assert_eq!(
        actual["remote_inventory"]["sample"][0]["name"],
        "existing.txt"
    );
    assert_eq!(actual["remote_inventory"]["sample"][0]["kind"], "file");
    assert_eq!(actual["write_test"]["status"], "not-requested");

    let requests = server.requests();
    let operations = requests
        .iter()
        .map(|request| request.operation())
        .collect::<Vec<_>>();
    assert_eq!(
        operations,
        [
            "SYNO.API.Info.query",
            "SYNO.API.Auth.login",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.CheckPermission.write",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.list",
            "SYNO.API.Auth.logout",
        ]
    );
    assert_eq!(
        requests[1].fields.get("account").map(String::as_str),
        Some("e2e-user")
    );
    assert_eq!(
        requests
            .last()
            .and_then(|request| request.fields.get("_sid")),
        Some(&"e2e-session-secret".to_owned())
    );
}

#[test]
fn additive_plan_then_sync_preserves_folder_parity_and_verifies_every_upload() {
    let fixture = TestDir::new("plan-sync");
    let source = fixture.child("source");
    fs::create_dir_all(source.join("nested")).expect("create nested source directory");
    fs::create_dir(source.join("empty")).expect("create empty source directory");
    fs::write(source.join("alpha.txt"), b"alpha").expect("write alpha source file");
    fs::write(source.join("nested/beta.bin"), b"beta-data").expect("write beta source file");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/sync");

    let source_text = source.to_str().expect("UTF-8 source path");
    let password_text = password.to_str().expect("UTF-8 password path");
    let plan = run(&[
        "--quiet",
        "--output",
        "json",
        "plan",
        source_text,
        "/team/sync",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password_text,
        "--no-vault",
        "--allow-http",
        "--jobs",
        "1",
    ]);
    assert_success(&plan);
    let planned = stdout_json(&plan);
    assert_eq!(planned["schema"], "sdsync.plan.v1");
    assert_eq!(
        planned["plan"]["summary"],
        json!({
            "uploads": 2,
            "upload_bytes": 14,
            "server_copy_fallback_bytes": 0,
            "server_copies": 0,
            "directories": 2,
            "deletions": 0,
            "unchanged_files": 0,
            "protected_entries": 0,
            "changes": true
        })
    );
    assert_eq!(
        planned["plan"]["actions"]["creates"]
            .as_array()
            .expect("create actions")
            .iter()
            .map(|action| action["relative"].as_str().expect("relative path"))
            .collect::<Vec<_>>(),
        ["empty", "nested"]
    );
    assert_eq!(
        planned["plan"]["actions"]["uploads"]
            .as_array()
            .expect("upload actions")
            .iter()
            .map(|action| {
                (
                    action["relative"].as_str().expect("relative path"),
                    action["reason"].as_str().expect("change reason"),
                )
            })
            .collect::<Vec<_>>(),
        [
            ("alpha.txt", "missing-remote"),
            ("nested/beta.bin", "missing-remote"),
        ]
    );
    assert!(planned.get("result").is_none());
    let plan_requests = server.requests();
    assert_eq!(
        plan_requests
            .iter()
            .map(|request| request.operation())
            .collect::<Vec<_>>(),
        [
            "SYNO.API.Info.query",
            "SYNO.API.Auth.login",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.CheckPermission.write",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.list",
            "SYNO.API.Auth.logout",
        ]
    );

    let sync = run(&[
        "--quiet",
        "--output",
        "json",
        "sync",
        source_text,
        "/team/sync",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password_text,
        "--no-vault",
        "--allow-http",
        "--jobs",
        "1",
    ]);
    assert_success(&sync);
    let synced = stdout_json(&sync);
    assert_eq!(synced["schema"], "sdsync.sync.v1");
    assert_eq!(synced["plan"]["summary"], planned["plan"]["summary"]);
    assert_eq!(synced["result"]["changed"], true);
    assert_eq!(synced["result"]["uploaded"], 2);
    assert_eq!(synced["result"]["server_copied"], 0);
    assert_eq!(synced["result"]["upload_bytes"], 14);
    assert_eq!(synced["result"]["directories_created"], 2);
    assert_eq!(synced["result"]["deleted"], 0);
    assert!(synced["result"]["elapsed_ms"].as_u64().is_some());

    assert!(
        server
            .directories()
            .contains(&"/team/sync/empty".to_owned())
    );
    assert!(
        server
            .directories()
            .contains(&"/team/sync/nested".to_owned())
    );
    assert_eq!(
        server.file_contents("/team/sync/alpha.txt"),
        Some(b"alpha".to_vec())
    );
    assert_eq!(
        server.file_contents("/team/sync/nested/beta.bin"),
        Some(b"beta-data".to_vec())
    );

    let sync_requests = &server.requests()[plan_requests.len()..];
    let uploads = sync_requests
        .iter()
        .filter(|request| request.operation() == "SYNO.FileStation.Upload.upload")
        .collect::<Vec<_>>();
    assert_eq!(uploads.len(), 2);
    assert_eq!(uploads[0].upload_filename.as_deref(), Some("alpha.txt"));
    assert_eq!(uploads[0].upload_bytes, Some(5));
    assert_eq!(uploads[1].upload_filename.as_deref(), Some("beta.bin"));
    assert_eq!(uploads[1].upload_bytes, Some(9));
    assert_eq!(
        sync_requests
            .iter()
            .filter(|request| request.operation() == "SYNO.FileStation.CreateFolder.create")
            .count(),
        2
    );
    assert_eq!(
        sync_requests
            .iter()
            .filter(|request| request.operation() == "SYNO.FileStation.Download.download")
            .count(),
        4,
        "each upload and each final reconciliation path must be content verified"
    );
    assert_eq!(
        sync_requests.last().map(|request| request.operation()),
        Some("SYNO.API.Auth.logout".to_owned())
    );
    assert_eq!(
        sync_requests
            .iter()
            .map(|request| request.operation())
            .collect::<Vec<_>>(),
        [
            "SYNO.API.Info.query",
            "SYNO.API.Auth.login",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.CheckPermission.write",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.list",
            "SYNO.FileStation.CreateFolder.create",
            "SYNO.FileStation.CreateFolder.create",
            "SYNO.FileStation.Upload.upload",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.Download.download",
            "SYNO.FileStation.Upload.upload",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.Download.download",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.list",
            "SYNO.FileStation.List.list",
            "SYNO.FileStation.List.list",
            "SYNO.FileStation.Download.download",
            "SYNO.FileStation.Download.download",
            "SYNO.API.Auth.logout",
        ]
    );
}

#[test]
fn missing_home_destination_is_provisioned_below_the_existing_home_root() {
    let fixture = TestDir::new("missing-home-destination");
    let source = fixture.child("source");
    fs::create_dir_all(source.join("empty")).expect("create empty source directory");
    fs::write(source.join("payload.txt"), b"home-backup").expect("write source payload");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/home");

    let target = "/home/Drive/Chosen Folder";
    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "sync",
        source.to_str().expect("UTF-8 source path"),
        target,
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--no-vault",
        "--allow-http",
        "--jobs",
        "1",
    ]);
    assert_success(&output);

    let synced = stdout_json(&output);
    assert_eq!(synced["plan"]["summary"]["directories"], 2);
    assert_eq!(synced["plan"]["summary"]["uploads"], 1);
    assert_eq!(synced["result"]["directories_created"], 2);
    assert_eq!(synced["result"]["uploaded"], 1);
    assert_eq!(synced["result"]["deleted"], 0);

    let directories = server.directories();
    for expected in [
        "/home",
        "/home/Drive",
        "/home/Drive/Chosen Folder",
        "/home/Drive/Chosen Folder/empty",
    ] {
        assert!(
            directories.contains(&expected.to_owned()),
            "missing {expected}"
        );
    }
    assert_eq!(
        server.file_contents("/home/Drive/Chosen Folder/payload.txt"),
        Some(b"home-backup".to_vec())
    );

    let requests = server.requests();
    let created_paths = requests
        .iter()
        .filter(|request| request.operation() == "SYNO.FileStation.CreateFolder.create")
        .map(|request| {
            let parent = serde_json::from_str::<Vec<String>>(
                request
                    .fields
                    .get("folder_path")
                    .expect("create folder parent"),
            )
            .expect("create folder parent JSON")
            .into_iter()
            .next()
            .expect("one create folder parent");
            let name = serde_json::from_str::<Vec<String>>(
                request.fields.get("name").expect("create folder name"),
            )
            .expect("create folder name JSON")
            .into_iter()
            .next()
            .expect("one create folder name");
            format!("{}/{}", parent.trim_end_matches('/'), name)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        created_paths,
        [
            "/home/Drive/Chosen Folder",
            "/home/Drive/Chosen Folder/empty"
        ]
    );
    assert!(
        !created_paths.iter().any(|path| path == "/home"),
        "the provisioned DSM home root must never be synthesized by sync"
    );

    let upload_index = requests
        .iter()
        .position(|request| request.operation() == "SYNO.FileStation.Upload.upload")
        .expect("payload upload request");
    let encoded_target = serde_json::to_string(target).expect("encode target path");
    let reconciliation_list_index = requests
        .iter()
        .enumerate()
        .skip(upload_index + 1)
        .find_map(|(index, request)| {
            (request.operation() == "SYNO.FileStation.List.list"
                && request.fields.get("folder_path") == Some(&encoded_target))
            .then_some(index)
        })
        .expect("post-upload reconciliation inventory of the chosen target");
    assert!(
        requests[reconciliation_list_index + 1..]
            .iter()
            .any(|request| request.operation() == "SYNO.FileStation.Download.download"),
        "final reconciliation must content-verify the uploaded payload"
    );
    assert_eq!(
        requests.last().map(|request| request.operation()),
        Some("SYNO.API.Auth.logout".to_owned())
    );
}

#[test]
fn reflected_authentication_failure_is_redacted_and_never_logs_out() {
    let fixture = TestDir::new("auth-redaction");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.reflect_login_failure("correct horse battery staple");

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--no-vault",
        "--allow-http",
        "target",
        "/team/target",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let result = stdout_json(&output);
    assert_eq!(result["status"], "fail");
    assert_eq!(result["level"], "standard");
    assert_eq!(result["authenticated"], false);
    assert_eq!(result["sections"][2]["id"], "dsm_session_auth");
    assert_eq!(result["sections"][2]["status"], "fail");
    assert_eq!(result["sections"][4]["status"], "skip");
    assert!(
        result["sections"][4]["detail"]
            .as_str()
            .expect("skip detail")
            .contains("failed")
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.starts_with("error: target diagnostic failed; inspect the section breakdown:"),
        "unexpected stderr: {stderr:?}"
    );
    let combined = format!("{}{}", String::from_utf8_lossy(&output.stdout), stderr);
    assert!(!combined.contains("correct horse battery staple"));
    assert!(!combined.contains("654321"));
    assert_eq!(stderr.lines().count(), 1);
    assert_eq!(
        server
            .requests()
            .iter()
            .map(|request| request.operation())
            .collect::<Vec<_>>(),
        ["SYNO.API.Info.query", "SYNO.API.Auth.login"]
    );
}

#[test]
fn failed_write_test_authentication_is_not_reported_as_preflighted() {
    let fixture = TestDir::new("write-test-auth-failure");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.reflect_login_failure("write probe must not run");

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--no-vault",
        "--allow-http",
        "--level",
        "extensive",
        "target",
        "/team/target",
        "--write-test",
    ]);

    assert_eq!(output.status.code(), Some(1));
    let result = stdout_json(&output);
    assert_eq!(result["status"], "fail");
    assert_eq!(result["sections"][2]["id"], "dsm_session_auth");
    assert_eq!(result["sections"][2]["status"], "fail");
    assert_eq!(
        result["sections"][6]["id"],
        "disposable_write_verify_cleanup"
    );
    assert_eq!(result["sections"][6]["status"], "skip");
    assert_eq!(result["write_test"]["requested"], true);
    assert_eq!(result["write_test"]["status"], "failed");
    assert!(result["write_test"]["report"].is_null());
    assert!(
        server
            .requests()
            .iter()
            .all(|request| !is_mutation(&request.operation())),
        "failed authentication must not reach a disposable write probe"
    );
}

#[test]
fn totp_is_generated_only_after_challenge_and_never_reaches_process_output() {
    let fixture = TestDir::new("totp-challenge");
    let password = fixture.write("password", PASSWORD);
    let totp = fixture.write("totp", b"JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP\n");
    let server = MockFileStation::start();
    server.add_directory("/team/totp");
    server.require_totp();

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--totp-secret-file",
        totp.to_str().expect("UTF-8 TOTP path"),
        "--no-vault",
        "--allow-http",
        "target",
        "/team/totp",
    ]);
    assert_success(&output);
    let result = stdout_json(&output);
    assert_eq!(result["authenticated"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("challenge-token-must-not-leak"));

    let requests = server.requests();
    assert_eq!(requests[1].operation(), "SYNO.API.Auth.login");
    assert!(!requests[1].fields.contains_key("otp_code"));
    assert_eq!(requests[2].operation(), "SYNO.API.Auth.login");
    let generated = requests[2]
        .fields
        .get("otp_code")
        .expect("second login carries generated TOTP");
    assert_eq!(generated.len(), 6);
    assert!(generated.bytes().all(|byte| byte.is_ascii_digit()));
    assert_eq!(
        requests.last().map(|request| request.operation()),
        Some("SYNO.API.Auth.logout".to_owned())
    );
}

#[test]
fn target_write_test_exercises_copy_verification_and_removes_every_probe_path() {
    let fixture = TestDir::new("write-probe");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/probe");

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--no-vault",
        "--allow-http",
        "target",
        "/team/probe",
        "--write-test",
    ]);
    assert_success(&output);
    let result = stdout_json(&output);
    assert_eq!(result["write_test"]["requested"], true);
    assert_eq!(result["level"], "extensive");
    assert_eq!(result["write_test"]["status"], "success");
    assert!(result["write_test"]["error"].is_null());
    let report = &result["write_test"]["report"];
    assert_eq!(report["target_verified"], true);
    assert_eq!(report["directory_created"], true);
    assert_eq!(report["upload_attempted"], true);
    assert_eq!(report["upload_verified"], true);
    assert_eq!(report["fingerprint_complete"], true);
    assert_eq!(report["uploaded_crc32"].as_str().expect("CRC32").len(), 8);
    assert_eq!(
        report["uploaded_sha256"].as_str().expect("SHA-256").len(),
        64
    );
    assert_eq!(report["server_copy_supported"], true);
    assert_eq!(report["server_copy_attempted"], true);
    assert_eq!(report["server_copy_verified"], true);
    assert_eq!(report["cleanup_completed"], true);
    assert!(report["leftover_remote_probe_path"].is_null());
    assert_eq!(
        result["sections"][6]["id"],
        "disposable_write_verify_cleanup"
    );
    assert_eq!(result["sections"][6]["status"], "pass");
    let probe_path = report["probe_path"].as_str().expect("probe path");
    assert!(probe_path.starts_with("/team/probe/.synology-drive-sync-probe-"));
    assert!(
        server
            .directories()
            .iter()
            .all(|path| !path.starts_with(probe_path))
    );

    let requests = server.requests();
    let operations = requests
        .iter()
        .map(|request| request.operation())
        .collect::<Vec<_>>();
    assert_eq!(
        operations
            .iter()
            .filter(|operation| operation.as_str() == "SYNO.FileStation.Upload.upload")
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| operation.as_str() == "SYNO.FileStation.CopyMove.start")
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| operation.as_str() == "SYNO.FileStation.Delete.delete")
            .count(),
        4
    );
    assert_eq!(
        operations.last().map(String::as_str),
        Some("SYNO.API.Auth.logout")
    );
    let deleted_paths = requests
        .iter()
        .filter(|request| request.operation() == "SYNO.FileStation.Delete.delete")
        .map(|request| {
            serde_json::from_str::<Vec<String>>(
                request.fields.get("path").expect("delete path field"),
            )
            .expect("delete path JSON")[0]
                .clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        deleted_paths,
        [
            format!("{probe_path}/copy/probe.bin"),
            format!("{probe_path}/copy"),
            format!("{probe_path}/probe.bin"),
            probe_path.to_owned(),
        ]
    );
}

#[test]
fn extensive_level_without_write_test_remains_non_mutating() {
    let fixture = TestDir::new("extensive-no-write");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/extensive");
    server.add_file("/team/extensive/example.bin", b"example", 1_700_000_000);

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--no-vault",
        "--allow-http",
        "--level",
        "extensive",
        "target",
        "/team/extensive",
    ]);
    assert_success(&output);
    let result = stdout_json(&output);
    assert_eq!(result["level"], "extensive");
    assert_eq!(result["write_test"]["requested"], false);
    assert_eq!(result["sections"][6]["status"], "skip");
    assert!(
        result["sections"][6]["detail"]
            .as_str()
            .expect("write-test skip detail")
            .contains("separate --write-test opt-in")
    );
    let operations = server
        .requests()
        .iter()
        .map(|request| request.operation())
        .collect::<Vec<_>>();
    assert!(!operations.iter().any(|operation| matches!(
        operation.as_str(),
        "SYNO.FileStation.CreateFolder.create"
            | "SYNO.FileStation.Upload.upload"
            | "SYNO.FileStation.Delete.delete"
    )));
}

#[test]
fn unique_cross_parent_rename_uses_verified_server_copy_without_uploading_bytes() {
    let fixture = TestDir::new("server-copy");
    let source = fixture.child("source");
    fs::create_dir_all(source.join("new")).expect("create renamed source directory");
    let local_file = source.join("new/item.bin");
    fs::write(&local_file, b"copy-me-on-nas").expect("write server-copy source file");
    let mtime = modified_seconds(&local_file);
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/copy");
    server.add_directory("/team/copy/old");
    server.add_file("/team/copy/old/item.bin", b"copy-me-on-nas", mtime);

    let source_text = source.to_str().expect("UTF-8 source path");
    let password_text = password.to_str().expect("UTF-8 password path");
    let plan = run(&[
        "--quiet",
        "--output",
        "json",
        "plan",
        source_text,
        "/team/copy",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password_text,
        "--no-vault",
        "--allow-http",
        "--jobs",
        "1",
    ]);
    assert_success(&plan);
    let planned = stdout_json(&plan);
    assert_eq!(planned["plan"]["summary"]["uploads"], 0);
    assert_eq!(planned["plan"]["summary"]["upload_bytes"], 0);
    assert_eq!(planned["plan"]["summary"]["server_copies"], 1);
    assert_eq!(planned["plan"]["summary"]["server_copy_fallback_bytes"], 14);
    assert_eq!(planned["plan"]["summary"]["directories"], 1);
    let copy = &planned["plan"]["actions"]["copies"][0];
    assert_eq!(copy["from_relative"], "old/item.bin");
    assert_eq!(copy["from_remote_path"], "/team/copy/old/item.bin");
    assert_eq!(copy["to_relative"], "new/item.bin");
    assert_eq!(copy["to_remote_path"], "/team/copy/new/item.bin");
    assert_eq!(
        copy["verified_upload_fallback"],
        "only-before-copy-task-start"
    );
    let plan_request_count = server.requests().len();

    let sync = run(&[
        "--quiet",
        "--output",
        "json",
        "sync",
        source_text,
        "/team/copy",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password_text,
        "--no-vault",
        "--allow-http",
        "--jobs",
        "1",
    ]);
    assert_success(&sync);
    let synced = stdout_json(&sync);
    assert_eq!(synced["result"]["changed"], true);
    assert_eq!(synced["result"]["uploaded"], 0);
    assert_eq!(synced["result"]["upload_bytes"], 0);
    assert_eq!(synced["result"]["server_copied"], 1);
    assert_eq!(synced["result"]["directories_created"], 1);
    assert_eq!(
        server.file_contents("/team/copy/new/item.bin"),
        Some(b"copy-me-on-nas".to_vec())
    );
    assert_eq!(
        server.file_contents("/team/copy/old/item.bin"),
        Some(b"copy-me-on-nas".to_vec()),
        "additive mode must retain the old remote path"
    );

    let all_requests = server.requests();
    let sync_requests = &all_requests[plan_request_count..];
    assert_eq!(
        sync_requests
            .iter()
            .filter(|request| request.operation() == "SYNO.FileStation.CopyMove.start")
            .count(),
        1
    );
    assert!(
        sync_requests
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Upload.upload"),
        "a safe renamed-content match must not retransmit the payload"
    );
    assert_eq!(
        sync_requests.last().map(|request| request.operation()),
        Some("SYNO.API.Auth.logout".to_owned())
    );
}

#[test]
fn destructive_sync_fails_closed_when_remote_snapshot_changes_after_planning() {
    let fixture = TestDir::new("delete-guard");
    let source = fixture.child("source");
    fs::create_dir(&source).expect("create delete-guard source");
    let keep = source.join("keep.txt");
    fs::write(&keep, b"keep").expect("write retained source file");
    let keep_mtime = modified_seconds(&keep);
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/delete");
    server.add_file("/team/delete/keep.txt", b"keep", keep_mtime);
    server.add_file("/team/delete/stale.txt", b"stale", 1_700_000_000);

    let source_text = source.to_str().expect("UTF-8 source path");
    let password_text = password.to_str().expect("UTF-8 password path");
    let common = [
        source_text,
        "/team/delete",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password_text,
        "--no-vault",
        "--allow-http",
        "--compare",
        "metadata",
        "--delete",
        "--max-delete",
        "1",
        "--jobs",
        "1",
    ];
    let mut plan_arguments = vec!["--quiet", "--output", "json", "plan"];
    plan_arguments.extend(common.iter().copied());
    let plan = run(&plan_arguments);
    assert_success(&plan);
    let planned = stdout_json(&plan);
    assert_eq!(planned["plan"]["summary"]["uploads"], 0);
    assert_eq!(planned["plan"]["summary"]["unchanged_files"], 1);
    assert_eq!(planned["plan"]["summary"]["deletions"], 1);
    assert_eq!(
        planned["plan"]["actions"]["post_deletes"][0]["remote_path"],
        "/team/delete/stale.txt"
    );
    let plan_request_count = server.requests().len();

    server.mutate_file_after_next_listing(
        "/team/delete/stale.txt",
        b"changed-concurrently",
        1_700_000_001,
    );
    let mut sync_arguments = vec!["--quiet", "--output", "json", "sync"];
    sync_arguments.extend(common.iter().copied());
    let sync = run(&sync_arguments);
    assert_eq!(sync.status.code(), Some(1));
    assert_eq!(sync.stdout, b"");
    let stderr = String::from_utf8(sync.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("changed since planning; it was preserved"),
        "unexpected stderr: {stderr:?}"
    );
    assert!(stderr.contains("/team/delete/stale.txt"));
    assert!(!stderr.contains("correct horse battery staple"));
    assert_eq!(stderr.lines().count(), 1);
    assert_eq!(
        server.file_contents("/team/delete/stale.txt"),
        Some(b"changed-concurrently".to_vec()),
        "the client must preserve the concurrently changed remote entry"
    );

    let all_requests = server.requests();
    let sync_requests = &all_requests[plan_request_count..];
    assert!(
        sync_requests
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Delete.delete"),
        "snapshot drift must abort before the delete request"
    );
    assert!(
        sync_requests
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Upload.upload")
    );
    assert_eq!(
        sync_requests.last().map(|request| request.operation()),
        Some("SYNO.API.Auth.logout".to_owned())
    );
}

#[test]
fn multi_profile_target_doctor_is_deterministic_and_keeps_sessions_separate() {
    let fixture = TestDir::new("doctor-batch");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/alpha");
    server.add_directory("/team/zeta");
    let password_path = password
        .to_str()
        .expect("UTF-8 password path")
        .replace('\\', "/");
    let config = format!(
        r#"default-profile = "zeta"

[profiles.zeta]
remote = "/team/zeta"
url = "{}"
username = "e2e-user"
password-file = "{}"
no-vault = true
compare = "metadata"
allow-http = true

[profiles.alpha]
remote = "/team/alpha"
url = "{}"
username = "e2e-user"
password-file = "{}"
no-vault = true
compare = "metadata"
allow-http = true
"#,
        server.base_url(),
        password_path,
        server.base_url(),
        password_path,
    );
    let config_path = fixture.write("config.toml", config.as_bytes());

    let output = run(&[
        "--config",
        config_path.to_str().expect("UTF-8 config path"),
        "--quiet",
        "--output",
        "ndjson",
        "doctor",
        "--all-profiles",
        "target",
    ]);
    assert_success(&output);
    let records = String::from_utf8(output.stdout)
        .expect("stdout is UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid NDJSON record"))
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["schema"], "sdsync.doctor-job.v1");
    assert_eq!(records[0]["profile"], "alpha");
    assert_eq!(records[0]["status"], "success");
    assert_eq!(records[0]["doctor"]["write_permission_path"], "/team/alpha");
    assert_eq!(records[1]["profile"], "zeta");
    assert_eq!(records[1]["doctor"]["write_permission_path"], "/team/zeta");
    assert_eq!(
        records[2],
        json!({
            "schema": "sdsync.doctor-batch.v1",
            "kind": "summary",
            "status": "success",
            "execution": "sequential",
            "write_tests_requested": false,
            "all_targets_preflighted_before_mutation": false,
            "summary": {"jobs": 2, "succeeded": 2, "preflighted": 0, "partial": 0, "failed": 0, "not_run": 0}
        })
    );

    let operations = server
        .requests()
        .iter()
        .map(|request| request.operation())
        .collect::<Vec<_>>();
    let one_target = [
        "SYNO.API.Info.query",
        "SYNO.API.Auth.login",
        "SYNO.FileStation.List.getinfo",
        "SYNO.FileStation.List.getinfo",
        "SYNO.FileStation.CheckPermission.write",
        "SYNO.FileStation.List.getinfo",
        "SYNO.FileStation.List.list",
        "SYNO.API.Auth.logout",
    ];
    assert_eq!(operations[..one_target.len()], one_target);
    assert_eq!(operations[one_target.len()..], one_target);
}

#[test]
fn write_test_batch_rejects_a_missing_destination_during_non_mutating_preflight() {
    let fixture = TestDir::new("doctor-batch-missing-destination");
    let alpha_source = fixture.child("sources/alpha");
    let beta_source = fixture.child("sources/beta");
    fs::create_dir_all(&alpha_source).expect("create alpha source");
    fs::create_dir_all(&beta_source).expect("create beta source");
    let password = fixture.write("secrets/password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team");
    server.add_directory("/team/alpha");
    let config = write_two_profile_config(
        &fixture,
        &password,
        &alpha_source,
        server.base_url(),
        "/team/alpha",
        &beta_source,
        server.base_url(),
        "/team/missing",
        "metadata",
        false,
        0,
    );

    let output = run(&[
        "--config",
        config.to_str().expect("UTF-8 config path"),
        "--quiet",
        "--output",
        "ndjson",
        "doctor",
        "--all-profiles",
        "--level",
        "extensive",
        "target",
        "--write-test",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let records = stdout_ndjson(&output);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["profile"], "alpha");
    assert_eq!(records[0]["status"], "preflighted");
    assert_eq!(records[1]["profile"], "beta");
    assert_eq!(records[1]["status"], "failed");
    assert_eq!(records[1]["doctor"]["remote_exists"], false);
    assert_eq!(
        records[1]["doctor"]["sections"][6]["id"],
        "disposable_write_verify_cleanup"
    );
    assert_eq!(records[1]["doctor"]["sections"][6]["status"], "fail");
    assert!(
        records[1]["doctor"]["sections"][6]["detail"]
            .as_str()
            .expect("probe section detail")
            .contains("requires an existing destination")
    );
    assert_eq!(records[2]["schema"], "sdsync.doctor-batch.v1");
    assert_eq!(records[2]["status"], "failed");
    assert_eq!(records[2]["write_tests_requested"], true);
    assert_eq!(records[2]["all_targets_preflighted_before_mutation"], false);
    assert!(
        server
            .requests()
            .iter()
            .all(|request| !is_mutation(&request.operation())),
        "a missing write-test destination must abort the batch before any disposable probe mutates a target"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("no disposable write probes were attempted"));
}

#[test]
fn discovery_falls_back_from_entry_cgi_to_query_cgi_without_authentication() {
    let server = MockFileStation::start();
    server.fail_entry_discovery_once(502);

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "doctor",
        "--url",
        server.base_url(),
        "--allow-http",
        "--retries",
        "0",
        "--routing-only",
    ]);
    assert_success(&output);
    let report = stdout_json(&output);
    assert_eq!(report["routing"], true);
    assert_eq!(report["api_discovery"], true);
    assert_eq!(report["authenticated"], false);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].request_path, "/prefix/webapi/entry.cgi");
    assert_eq!(requests[1].request_path, "/prefix/webapi/query.cgi");
    assert!(
        requests
            .iter()
            .all(|request| request.operation() == "SYNO.API.Info.query")
    );
    assert_eq!(server.pending_faults(), 0);
}

#[test]
fn multi_profile_plan_and_sync_preflight_every_target_before_aggregate_mutation() {
    let fixture = TestDir::new("batch-plan-sync");
    let alpha_source = fixture.child("sources/alpha");
    let beta_source = fixture.child("sources/beta");
    fs::create_dir_all(&alpha_source).expect("create alpha source");
    fs::create_dir_all(beta_source.join("nested")).expect("create beta source");
    fs::write(alpha_source.join("alpha.txt"), b"alpha").expect("write alpha payload");
    fs::write(beta_source.join("nested/beta.txt"), b"beta").expect("write beta payload");
    let password = fixture.write("secrets/password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/alpha");
    server.add_directory("/team/beta");
    let config = write_two_profile_config(
        &fixture,
        &password,
        &alpha_source,
        server.base_url(),
        "/team/alpha",
        &beta_source,
        server.base_url(),
        "/team/beta",
        "content",
        false,
        0,
    );
    let config_text = config.to_str().expect("UTF-8 config path");

    let plan = run(&[
        "--config",
        config_text,
        "--quiet",
        "--output",
        "json",
        "plan",
        "--all-profiles",
        "--max-total-delete",
        "10",
    ]);
    assert_success(&plan);
    let planned = stdout_json(&plan);
    assert_eq!(planned["schema"], "sdsync.batch.v1");
    assert_eq!(planned["kind"], "summary");
    assert_eq!(planned["mode"], "plan");
    assert_eq!(planned["status"], "success");
    assert_eq!(planned["all_targets_preflighted_before_mutation"], true);
    assert_eq!(planned["max_total_delete"], 10);
    assert_eq!(planned["preflight_deletions"], 0);
    assert!(planned["execution_reserved_deletions"].is_null());
    assert_eq!(
        planned["summary"],
        json!({"jobs": 2, "succeeded": 0, "preflighted": 2, "partial": 0, "failed": 0, "not_run": 0})
    );
    let plan_jobs = planned["jobs"].as_array().expect("batch plan jobs");
    assert_eq!(plan_jobs.len(), 2);
    assert_eq!(plan_jobs[0]["profile"], "alpha");
    assert_eq!(plan_jobs[1]["profile"], "beta");
    for job in plan_jobs {
        assert_eq!(job["status"], "preflighted");
        assert_eq!(job["mutation_authorized"], false);
        assert!(job["preflight_plan"].is_object());
        assert!(job["execution_plan"].is_null());
        assert!(job["result"].is_null());
        assert_eq!(job["preflight_plan"]["summary"]["uploads"], 1);
    }
    let plan_requests = server.requests();
    assert!(
        plan_requests
            .iter()
            .all(|request| !is_mutation(&request.operation())),
        "batch plan must remain non-mutating"
    );
    assert_eq!(
        plan_requests
            .iter()
            .filter(|request| request.operation() == "SYNO.API.Auth.login")
            .count(),
        2
    );

    let sync = run(&[
        "--config",
        config_text,
        "--quiet",
        "--output",
        "ndjson",
        "sync",
        "--all-profiles",
        "--max-total-delete",
        "10",
    ]);
    assert_success(&sync);
    let records = stdout_ndjson(&sync);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["schema"], "sdsync.batch-job.v1");
    assert_eq!(records[0]["profile"], "alpha");
    assert_eq!(records[1]["profile"], "beta");
    for record in &records[..2] {
        assert_eq!(record["status"], "success");
        assert_eq!(record["mutation_authorized"], true);
        assert!(record["preflight_plan"].is_object());
        assert!(record["execution_plan"].is_object());
        assert_eq!(record["result"]["uploaded"], 1);
        assert_eq!(record["result"]["deleted"], 0);
    }
    assert_eq!(
        records[2],
        json!({
            "schema": "sdsync.batch.v1",
            "kind": "summary",
            "mode": "sync",
            "status": "success",
            "execution": "sequential",
            "all_targets_preflighted_before_mutation": true,
            "max_total_delete": 10,
            "preflight_deletions": 0,
            "execution_reserved_deletions": 0,
            "summary": {"jobs": 2, "succeeded": 2, "preflighted": 0, "partial": 0, "failed": 0, "not_run": 0},
            "error": null
        })
    );

    let all_requests = server.requests();
    let sync_requests = &all_requests[plan_requests.len()..];
    let first_mutation = sync_requests
        .iter()
        .position(|request| is_mutation(&request.operation()))
        .expect("batch sync performs a mutation");
    let preflight_requests = &sync_requests[..first_mutation];
    assert_eq!(
        preflight_requests
            .iter()
            .filter(|request| request.operation() == "SYNO.API.Auth.login")
            .count(),
        3,
        "two completed preflight sessions plus the first execution session precede mutation"
    );
    assert_eq!(
        preflight_requests
            .iter()
            .filter(|request| request.operation() == "SYNO.API.Auth.logout")
            .count(),
        2,
        "both preflight sessions must complete before the first mutation"
    );
    for remote in ["/team/alpha", "/team/beta"] {
        let encoded = serde_json::to_string(remote).expect("encode remote path");
        assert!(preflight_requests.iter().any(|request| {
            request.operation() == "SYNO.FileStation.List.list"
                && request.fields.get("folder_path") == Some(&encoded)
        }));
    }
    assert_eq!(
        sync_requests
            .iter()
            .filter(|request| request.operation() == "SYNO.API.Auth.login")
            .count(),
        4,
        "two preflight and two execution sessions are required"
    );
    assert_eq!(
        server.file_contents("/team/alpha/alpha.txt"),
        Some(b"alpha".to_vec())
    );
    assert_eq!(
        server.file_contents("/team/beta/nested/beta.txt"),
        Some(b"beta".to_vec())
    );
    let combined_output = [plan.stdout, plan.stderr, sync.stdout, sync.stderr].concat();
    assert!(!String::from_utf8_lossy(&combined_output).contains("correct horse battery staple"));
    assert!(!String::from_utf8_lossy(&combined_output).contains("e2e-session-secret"));
}

#[test]
fn destructive_type_conflicts_delete_deepest_first_then_fully_reconcile() {
    let fixture = TestDir::new("type-conflict-success");
    let source = fixture.child("source");
    fs::create_dir_all(source.join("folder")).expect("create replacement folder");
    fs::write(source.join("node"), b"replacement").expect("write file replacing directory");
    fs::write(source.join("folder/new.txt"), b"new").expect("write nested replacement file");
    let keep = source.join("keep.txt");
    fs::write(&keep, b"keep").expect("write unchanged file");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/conflict");
    server.add_directory("/team/conflict/node");
    server.add_directory("/team/conflict/node/child");
    server.add_file("/team/conflict/node/child/old.bin", b"old", 1_700_000_000);
    server.add_file("/team/conflict/folder", b"old-file", 1_700_000_001);
    server.add_file("/team/conflict/keep.txt", b"keep", modified_seconds(&keep));
    server.add_directory("/team/conflict/stale");
    server.add_directory("/team/conflict/stale/sub");
    server.add_file("/team/conflict/stale/sub/old.bin", b"stale", 1_700_000_002);

    let source_text = source.to_str().expect("UTF-8 source path");
    let password_text = password.to_str().expect("UTF-8 password path");
    let common = [
        source_text,
        "/team/conflict",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password_text,
        "--no-vault",
        "--allow-http",
        "--compare",
        "metadata",
        "--delete",
        "--max-delete",
        "10",
        "--jobs",
        "1",
        "--retries",
        "0",
    ];
    let mut plan_arguments = vec!["--quiet", "--output", "json", "plan"];
    plan_arguments.extend(common);
    let plan = run(&plan_arguments);
    assert_success(&plan);
    let planned = stdout_json(&plan);
    assert_eq!(planned["plan"]["summary"]["uploads"], 2);
    assert_eq!(planned["plan"]["summary"]["directories"], 1);
    assert_eq!(planned["plan"]["summary"]["deletions"], 7);
    assert_eq!(planned["plan"]["summary"]["unchanged_files"], 1);
    assert_eq!(
        planned["plan"]["actions"]["pre_deletes"]
            .as_array()
            .expect("pre-delete actions")
            .iter()
            .map(|action| action["relative"].as_str().expect("relative path"))
            .collect::<Vec<_>>(),
        ["node/child/old.bin", "node/child", "folder", "node"]
    );
    assert_eq!(
        planned["plan"]["actions"]["post_deletes"]
            .as_array()
            .expect("post-delete actions")
            .iter()
            .map(|action| action["relative"].as_str().expect("relative path"))
            .collect::<Vec<_>>(),
        ["stale/sub/old.bin", "stale/sub", "stale"]
    );
    assert_eq!(
        planned["plan"]["actions"]["uploads"]
            .as_array()
            .expect("upload actions")
            .iter()
            .map(|action| {
                (
                    action["relative"].as_str().expect("relative path"),
                    action["reason"].as_str().expect("change reason"),
                )
            })
            .collect::<Vec<_>>(),
        [
            ("folder/new.txt", "missing-remote"),
            ("node", "type-replaced"),
        ]
    );
    assert_eq!(
        planned["plan"]["actions"]["creates"][0]["reason"],
        "type-replaced"
    );
    let plan_request_count = server.requests().len();

    let mut sync_arguments = vec!["--quiet", "--output", "json", "sync"];
    sync_arguments.extend(common);
    let sync = run(&sync_arguments);
    assert_success(&sync);
    let synced = stdout_json(&sync);
    assert_eq!(synced["result"]["changed"], true);
    assert_eq!(synced["result"]["uploaded"], 2);
    assert_eq!(synced["result"]["directories_created"], 1);
    assert_eq!(synced["result"]["deleted"], 7);

    let requests = server.requests();
    let sync_requests = &requests[plan_request_count..];
    let delete_requests = sync_requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.operation() == "SYNO.FileStation.Delete.delete")
        .map(|(index, request)| {
            let path = serde_json::from_str::<Vec<String>>(
                request.fields.get("path").expect("delete path field"),
            )
            .expect("delete path array")
            .remove(0);
            (index, path)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        delete_requests
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        [
            "/team/conflict/node/child/old.bin",
            "/team/conflict/node/child",
            "/team/conflict/folder",
            "/team/conflict/node",
            "/team/conflict/stale/sub/old.bin",
            "/team/conflict/stale/sub",
            "/team/conflict/stale",
        ]
    );
    let create_index = sync_requests
        .iter()
        .position(|request| request.operation() == "SYNO.FileStation.CreateFolder.create")
        .expect("replacement directory creation");
    let upload_indices = sync_requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.operation() == "SYNO.FileStation.Upload.upload")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(upload_indices.len(), 2);
    assert!(delete_requests[3].0 < create_index);
    assert!(upload_indices.iter().all(|index| create_index < *index));
    assert!(
        upload_indices
            .iter()
            .all(|index| *index < delete_requests[4].0)
    );

    assert_eq!(
        server.file_paths(),
        [
            "/team/conflict/folder/new.txt".to_owned(),
            "/team/conflict/keep.txt".to_owned(),
            "/team/conflict/node".to_owned(),
        ]
    );
    assert_eq!(
        server
            .directories()
            .into_iter()
            .filter(|path| path.starts_with("/team/conflict/"))
            .collect::<Vec<_>>(),
        ["/team/conflict/folder".to_owned()]
    );
    assert_eq!(
        server.file_contents("/team/conflict/node"),
        Some(b"replacement".to_vec())
    );
    assert_eq!(
        server.file_contents("/team/conflict/folder/new.txt"),
        Some(b"new".to_vec())
    );

    let empty_plan = run(&plan_arguments);
    assert_success(&empty_plan);
    let reconciled = stdout_json(&empty_plan);
    assert_eq!(
        reconciled["plan"]["summary"],
        json!({
            "uploads": 0,
            "upload_bytes": 0,
            "server_copy_fallback_bytes": 0,
            "server_copies": 0,
            "directories": 0,
            "deletions": 0,
            "unchanged_files": 3,
            "protected_entries": 0,
            "changes": false
        })
    );
}

#[test]
fn retryable_metadata_and_committed_upload_responses_recover_without_retransmission() {
    let fixture = TestDir::new("retry-reconciliation");
    let source = fixture.child("source");
    fs::create_dir(&source).expect("create retry source");
    fs::write(source.join("payload.bin"), b"retry-safe").expect("write retry payload");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/retry");
    server.fail_next_http_operation("SYNO.FileStation.List.getinfo", 503);
    server.fail_next_upload_response_after_commit(502);

    let output = run(&[
        "--quiet",
        "--output",
        "json",
        "sync",
        source.to_str().expect("UTF-8 source path"),
        "/team/retry",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--password-file",
        password.to_str().expect("UTF-8 password path"),
        "--no-vault",
        "--allow-http",
        "--compare",
        "content",
        "--jobs",
        "1",
        "--retries",
        "1",
    ]);
    assert_success(&output);
    let synced = stdout_json(&output);
    assert_eq!(synced["result"]["uploaded"], 1);
    assert_eq!(synced["result"]["upload_bytes"], 10);
    assert_eq!(
        server.file_contents("/team/retry/payload.bin"),
        Some(b"retry-safe".to_vec())
    );
    assert_eq!(server.pending_faults(), 0);

    let requests = server.requests();
    let getinfo = requests
        .iter()
        .filter(|request| request.operation() == "SYNO.FileStation.List.getinfo")
        .collect::<Vec<_>>();
    assert!(getinfo.len() >= 2);
    assert_eq!(getinfo[0].fields.get("path"), getinfo[1].fields.get("path"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.operation() == "SYNO.FileStation.Upload.upload")
            .count(),
        1,
        "a landed upload must be reconciled by size and the complete content fingerprint before any retransmission"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.operation() == "SYNO.FileStation.Download.download"),
        "the ambiguous upload response must trigger content reconciliation"
    );
    let output_text = String::from_utf8_lossy(&output.stdout);
    assert!(!output_text.contains("correct horse battery staple"));
    assert!(!output_text.contains("e2e-session-secret"));
}

#[test]
fn batch_preflight_failure_aborts_mutation_for_every_target() {
    let fixture = TestDir::new("batch-preflight-failure");
    let alpha_source = fixture.child("sources/alpha");
    let beta_source = fixture.child("sources/beta");
    fs::create_dir_all(&alpha_source).expect("create alpha source");
    fs::create_dir_all(&beta_source).expect("create beta source");
    fs::write(alpha_source.join("alpha.txt"), b"alpha").expect("write alpha payload");
    fs::write(beta_source.join("beta.txt"), b"beta").expect("write beta payload");
    let password = fixture.write("secrets/password", PASSWORD);
    let alpha_server = MockFileStation::start();
    alpha_server.add_directory("/team/alpha");
    let beta_server = MockFileStation::start();
    beta_server.add_directory("/team/beta");
    beta_server.fail_next_api_operation("SYNO.FileStation.List.list", 400);
    let config = write_two_profile_config(
        &fixture,
        &password,
        &alpha_source,
        alpha_server.base_url(),
        "/team/alpha",
        &beta_source,
        beta_server.base_url(),
        "/team/beta",
        "content",
        false,
        0,
    );

    let output = run(&[
        "--config",
        config.to_str().expect("UTF-8 config path"),
        "--quiet",
        "--output",
        "json",
        "sync",
        "--all-profiles",
        "--max-total-delete",
        "10",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let batch = stdout_json(&output);
    assert_eq!(batch["schema"], "sdsync.batch.v1");
    assert_eq!(batch["mode"], "sync");
    assert_eq!(batch["status"], "failed");
    assert_eq!(batch["all_targets_preflighted_before_mutation"], false);
    assert!(batch["preflight_deletions"].is_null());
    assert!(batch["execution_reserved_deletions"].is_null());
    assert_eq!(
        batch["summary"],
        json!({"jobs": 2, "succeeded": 0, "preflighted": 1, "partial": 0, "failed": 1, "not_run": 0})
    );
    let jobs = batch["jobs"].as_array().expect("batch failure jobs");
    assert_eq!(jobs[0]["profile"], "alpha");
    assert_eq!(jobs[0]["status"], "preflighted");
    assert!(jobs[0]["preflight_plan"].is_object());
    assert_eq!(jobs[1]["profile"], "beta");
    assert_eq!(jobs[1]["status"], "failed");
    assert!(jobs[1]["preflight_plan"].is_null());
    assert!(jobs[1]["error"].as_str().is_some());
    assert!(jobs.iter().all(|job| job["mutation_authorized"] == false));

    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert_eq!(
        stderr,
        "error: one or more batch preflights failed; no remote mutations were attempted\n"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    for secret in [
        "correct horse battery staple",
        "e2e-session-secret",
        "e2e-syno-token-secret",
    ] {
        assert!(!stdout.contains(secret));
        assert!(!stderr.contains(secret));
    }
    for server in [&alpha_server, &beta_server] {
        assert!(
            server
                .requests()
                .iter()
                .all(|request| !is_mutation(&request.operation())),
            "a failed batch preflight must leave every target untouched"
        );
    }
    assert_eq!(alpha_server.file_contents("/team/alpha/alpha.txt"), None);
    assert_eq!(beta_server.file_contents("/team/beta/beta.txt"), None);
    assert_eq!(beta_server.pending_faults(), 0);
}
