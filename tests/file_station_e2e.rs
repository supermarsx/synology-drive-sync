mod support;

use std::fs;
use std::process::{Command, Output};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};
use support::TestDir;
use support::file_station_mock::{CapturedRequest, MockFileStation};

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

/// One section of a doctor document, addressed by its stable id rather than by its position.
///
/// Display order is part of the report contract, and it is pinned once and completely in
/// `routing_only_doctor_stops_after_reverse_proxy_discovery`. Everywhere else, addressing a
/// section by index only means every assertion in this file has to be renumbered whenever a
/// section is added -- churn that hides which assertions actually changed meaning.
fn section<'a>(document: &'a Value, id: &str) -> &'a Value {
    document["sections"]
        .as_array()
        .expect("doctor document carries a section array")
        .iter()
        .find(|section| section["id"] == id)
        .unwrap_or_else(|| panic!("doctor document has no {id} section"))
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
    // The three transport sections pass here: loopback resolves to one address, nothing
    // announces itself in front of the mock, and no cookie is set. Capability enumeration passes
    // too: it needs no session, which is exactly why it is the one new check quick still buys.
    assert_eq!(
        actual["summary"],
        json!({"pass":6,"warn":1,"fail":0,"skip":9})
    );
    let sections = actual["sections"].as_array().expect("section array");
    // The whole display order is pinned here, so a section added or moved anywhere in the report
    // has to be a deliberate change to this list rather than a silent reshuffle.
    assert_eq!(
        sections
            .iter()
            .map(|section| section["id"].as_str().expect("section id"))
            .collect::<Vec<_>>(),
        [
            "network_reachability",
            "routing_tls",
            "dsm_api_discovery",
            "capability_enumeration",
            "intermediary_transport",
            "dsm_session_auth",
            "session_channel_ablation",
            "session_concurrency",
            "session_cookie_ledger",
            "file_station_capabilities",
            "capability_diagnosis",
            "destination_path_resolution",
            "destination_permissions",
            "destination_inventory",
            "disposable_write_verify_cleanup",
            "session_logout",
        ]
    );
    // Execution order is not display order, and the report states the real one.
    assert_eq!(
        sections
            .iter()
            .map(|section| section["step"].as_u64().expect("section step"))
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 15, 6, 7, 8, 16, 5, 9, 10, 11, 12, 13, 14]
    );
    assert_eq!(section(&actual, "network_reachability")["status"], "pass");
    assert_eq!(section(&actual, "routing_tls")["status"], "warn");
    assert_eq!(section(&actual, "dsm_api_discovery")["status"], "pass");
    assert_eq!(section(&actual, "capability_enumeration")["status"], "pass");
    assert_eq!(section(&actual, "intermediary_transport")["status"], "pass");
    assert_eq!(section(&actual, "dsm_session_auth")["status"], "skip");
    assert_eq!(section(&actual, "session_cookie_ledger")["status"], "pass");
    // Quick is unauthenticated, so the session probes have nothing to present and say so.
    assert_eq!(
        section(&actual, "session_channel_ablation")["status"],
        "skip"
    );
    assert_eq!(section(&actual, "capability_diagnosis")["status"], "skip");

    // Every API DSM advertises, with the version this tool asks of each. No session was needed.
    // The enumeration sees more than the ten-name discovery query does, which is the point of it.
    let capabilities = &actual["capabilities"];
    assert_eq!(capabilities["advertised_apis"], 16);
    assert_eq!(capabilities["unusable_entries"], 0);
    assert!(
        capabilities["requirements"]
            .as_array()
            .expect("requirement matrix")
            .iter()
            .all(|verdict| verdict["satisfied"] == true),
        "the mock advertises every version this tool asks for: {capabilities}"
    );
    // Tier 1 lists the File Station surface in full, including the APIs this tool never calls.
    let file_station = capabilities["file_station"]
        .as_array()
        .expect("File Station tier");
    assert_eq!(file_station.len(), 12);
    let rename = file_station
        .iter()
        .find(|api| api["name"] == "SYNO.FileStation.Rename")
        .expect("an advertised API this tool does not use");
    assert_eq!(rename["used"], false);
    assert_eq!(rename["required_version"], Value::Null);
    // Tier 3 counts everything else by namespace rather than naming it.
    assert_eq!(
        capabilities["namespaces"]
            .as_array()
            .expect("namespace tier")
            .iter()
            .map(|namespace| (
                namespace["namespace"].as_str().expect("namespace name"),
                namespace["apis"].as_u64().expect("namespace count")
            ))
            .collect::<Vec<_>>(),
        [
            ("SYNO.Core.*", 2),
            ("SYNO.API.*", 1),
            ("SYNO.DownloadStation.*", 1)
        ]
    );

    let requests = server.requests();
    // Two: the ten-API discovery that feeds every later call, and the separate `query=all` read
    // that feeds the report. They are deliberately not the same request.
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.request_path, "/prefix/webapi/entry.cgi");
        assert_eq!(request.operation(), "SYNO.API.Info.query");
    }
    assert_eq!(
        requests[1].fields.get("query").map(String::as_str),
        Some("all")
    );

    // The unauthenticated transport probe really did run, and really did make its own requests
    // against the discovery route rather than borrowing the client's.
    let probes = server
        .connections()
        .into_iter()
        .filter(|request| request.api.is_empty())
        .collect::<Vec<_>>();
    assert!(
        !probes.is_empty(),
        "the transport probe should have issued its own HTTP samples"
    );
    // `entry.cgi` is what API discovery opens with, so it is what the probe must time. Pinning
    // the probe to the `query.cgi` fallback measured a route the run itself never used, and on a
    // host that serves only one of the two it measured a handler that does not exist.
    for probe in &probes {
        assert!(
            probe.request_path.starts_with("/prefix/webapi/entry.cgi"),
            "the probe should measure the route discovery actually uses, not {:?}",
            probe.request_path
        );
    }
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
    assert_eq!(section(&result, "dsm_session_auth")["status"], "skip");
    // Two unauthenticated reads and nothing else: the ten-API discovery, and the `query=all`
    // capability enumeration. Neither touches the destination, which is the point of quick.
    assert_eq!(
        server
            .requests()
            .iter()
            .map(|request| request.operation())
            .collect::<Vec<_>>(),
        ["SYNO.API.Info.query", "SYNO.API.Info.query"]
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
    assert_eq!(section(&result, "routing_tls")["status"], "warn");
    assert_eq!(section(&result, "dsm_api_discovery")["status"], "fail");
    assert!(
        section(&result, "file_station_capabilities")["detail"]
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
    // The whole request sequence a standard run makes, in order. Reading top to bottom: the two
    // unauthenticated discovery reads, login, the session confirmation, the four session-channel
    // ablation variants, the capability diagnosis bracketed by its two host reads, the
    // destination walk and permission check, the bounded inventory, and logout.
    //
    // The walk's second `getinfo` is the destination's own, and it establishes that the path
    // exists, is a directory, and is not a mount boundary. The inventory therefore opens at its
    // listing rather than repeating that request: a run against a healthy target pays for the
    // destination `getinfo` exactly once. A destination the walk did not resolve keeps the
    // inventory's own `getinfo`, which is what turns an absent path into `absent_root`.
    assert_eq!(
        operations,
        [
            "SYNO.API.Info.query",
            "SYNO.API.Info.query",
            "SYNO.API.Auth.login",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.Info.get",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.VirtualFolder.list",
            "SYNO.FileStation.BackgroundTask.list",
            "SYNO.FileStation.Info.get",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.List.getinfo",
            "SYNO.FileStation.CheckPermission.write",
            "SYNO.FileStation.List.list",
            "SYNO.API.Auth.logout",
        ]
    );
    // The destination is inspected once, not twice.
    assert_eq!(
        operations
            .iter()
            .filter(|operation| operation.as_str() == "SYNO.FileStation.List.getinfo")
            .count(),
        2,
        "one `getinfo` per path component, and none repeated for the inventory"
    );
    assert_eq!(
        requests[2].fields.get("account").map(String::as_str),
        Some("e2e-user")
    );

    // The destination walked one component at a time, and every capability probed for this
    // account. Both come from requests the run was already making.
    let resolution = &actual["path_resolution"];
    assert_eq!(resolution["total_components"], 2);
    assert_eq!(resolution["fully_resolved"], true);
    assert_eq!(resolution["segments"][0]["path"], "/team");
    assert_eq!(resolution["segments"][1]["path"], "/team/target");
    assert_eq!(
        section(&actual, "destination_path_resolution")["status"],
        "pass"
    );

    let diagnosis = &actual["capability_diagnosis"];
    assert_eq!(diagnosis["session_aborted"], false);
    assert_eq!(diagnosis["hostname_changed"], false);
    assert_eq!(diagnosis["host"], "MOCKSTATION");
    assert!(
        diagnosis["capabilities"]
            .as_array()
            .expect("capability matrix")
            .iter()
            .all(|record| record["verdict"] == "works"),
        "every probed capability works against the mock: {diagnosis}"
    );

    // The ablation reproduces the client's own behaviour first, then removes one channel at a
    // time. The mock resolves the session from `_sid`, so cookie-only is rejected.
    let channels = actual["session_channels"]
        .as_array()
        .expect("session channel probes");
    assert_eq!(
        channels
            .iter()
            .map(|probe| probe["channels"].as_str().expect("channel name"))
            .collect::<Vec<_>>(),
        [
            "all",
            "sid-field-only",
            "cookie-only",
            "token-header-only",
            "sid-field-only-tokenless-login"
        ]
    );
    assert_eq!(channels[0]["outcome"], "ok");
    assert_eq!(channels[1]["outcome"], "ok");
    assert_eq!(channels[2]["dsm_code"], 119);
    assert_eq!(channels[3]["dsm_code"], 119);
    assert_eq!(
        section(&actual, "session_channel_ablation")["status"],
        "pass"
    );
    assert_eq!(
        requests
            .last()
            .and_then(|request| request.fields.get("_sid")),
        Some(&"e2e-session-secret".to_owned())
    );
}

#[test]
fn doctor_rejects_unusable_post_login_sessions_and_always_logs_out() {
    for code in [106, 107, 119] {
        let fixture = TestDir::new(&format!("target-doctor-session-rejection-{code}"));
        let password = fixture.write("password", PASSWORD);
        let server = MockFileStation::start();
        server.add_directory("/team/target");
        server.fail_next_api_operation("SYNO.FileStation.List.list_share", code);

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

        let actual = stdout_json(&output);
        assert_eq!(actual["status"], "fail");
        assert_eq!(actual["authenticated"], false);
        assert_eq!(actual["remote_checked"], false);
        assert_eq!(section(&actual, "dsm_session_auth")["status"], "fail");
        assert!(
            section(&actual, "dsm_session_auth")["detail"]
                .as_str()
                .expect("session confirmation detail")
                .contains(&format!("code {code}"))
        );
        assert_eq!(
            section(&actual, "destination_permissions")["status"],
            "skip"
        );
        assert_eq!(section(&actual, "destination_inventory")["status"], "skip");
        assert_eq!(section(&actual, "session_logout")["status"], "pass");

        assert_eq!(
            server
                .requests()
                .iter()
                .map(|request| request.operation())
                .collect::<Vec<_>>(),
            [
                "SYNO.API.Info.query",
                "SYNO.API.Info.query",
                "SYNO.API.Auth.login",
                "SYNO.FileStation.List.list_share",
                "SYNO.API.Auth.logout",
            ]
        );
    }
}

/// A session DSM has already rejected must not be asked to enumerate.
///
/// This reproduces a reported live failure: `list_share` succeeds, the very next authenticated
/// call returns 119, and the diagnostic used to fall through into the inventory anyway. That
/// second request could never have succeeded, and reporting its failure separately presented one
/// dead session as two independent problems.
#[test]
fn a_rejected_session_stops_the_diagnostic_instead_of_enumerating() {
    for code in [106, 107, 119] {
        let fixture = TestDir::new(&format!("target-doctor-permission-session-{code}"));
        let password = fixture.write("password", PASSWORD);
        let server = MockFileStation::start();
        server.add_directory("/team/target");
        // The permission check's first request, immediately after session confirmation passed.
        server.fail_next_api_operation("SYNO.FileStation.List.getinfo", code);

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

        let actual = stdout_json(&output);
        assert_eq!(actual["status"], "fail");
        assert_eq!(
            section(&actual, "destination_permissions")["status"],
            "fail"
        );
        assert_eq!(section(&actual, "destination_inventory")["status"], "skip");
        assert_eq!(
            section(&actual, "destination_inventory")["detail"],
            "not attempted; the DSM session was already rejected by the permission check"
        );
        // The specific reason must survive `explain_dependent_skips`, which runs first and would
        // otherwise leave the vaguer "not run because Destination permissions failed".
        assert!(
            !section(&actual, "destination_inventory")["detail"]
                .as_str()
                .expect("inventory detail")
                .contains("not run because")
        );
        // Logout still happens, so the run leaves no session behind.
        assert_eq!(section(&actual, "session_logout")["status"], "pass");

        // The decisive assertion: no enumeration request was made after the rejection.
        let operations = server
            .requests()
            .iter()
            .map(|request| request.operation())
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            [
                "SYNO.API.Info.query",
                "SYNO.API.Info.query",
                "SYNO.API.Auth.login",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.Info.get",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.VirtualFolder.list",
                "SYNO.FileStation.BackgroundTask.list",
                "SYNO.FileStation.Info.get",
                "SYNO.FileStation.List.getinfo",
                "SYNO.API.Auth.logout",
            ],
            "a rejected session must not be asked to enumerate"
        );

        // The report names a concrete next step rather than restating the code.
        let remediation = section(&actual, "destination_permissions")["remediation"]
            .as_str()
            .expect("a rejected session has a remediation hint");
        assert!(
            remediation.contains("same DSM host"),
            "unexpected remediation: {remediation}"
        );

        // Each section reports the requests it is responsible for.
        let permission_calls = section(&actual, "destination_permissions")["calls"]
            .as_array()
            .expect("permission calls");
        assert_eq!(permission_calls.len(), 1);
        assert_eq!(permission_calls[0]["api"], "SYNO.FileStation.List");
        assert_eq!(permission_calls[0]["method"], "getinfo");
        assert_eq!(permission_calls[0]["dsm_code"], code);
        assert_eq!(permission_calls[0]["outcome"], "dsm-error");

        // The resolution section renders the walk the permission check performed rather than
        // repeating it, so the report says exactly how far the path got before the session died.
        let resolution = &actual["path_resolution"];
        assert_eq!(resolution["total_components"], 2);
        assert_eq!(resolution["fully_resolved"], false);
        assert_eq!(resolution["first_missing"], Value::Null);
        assert_eq!(resolution["segments"][0]["path"], "/team");
        assert_eq!(resolution["segments"][0]["exists"], false);
        assert_eq!(resolution["segments"][0]["dsm_code"], code);
        assert!(
            section(&actual, "destination_path_resolution")["calls"]
                .as_array()
                .expect("resolution calls")
                .is_empty(),
            "the walk's requests belong to the permission check that issued them"
        );
    }
}

/// The ablation settles the reported live failure: login works, the first authenticated call
/// works, and everything after it answers 119.
///
/// This is the one diagnostic that can attribute that to the client rather than to the path. The
/// mock models a DSM that discards a session presented as an `id` cookie -- which a `format=sid`
/// login is documented never to have issued -- and the report has to name the cookie, not the
/// network.
#[test]
fn the_channel_ablation_names_the_cookie_when_only_the_sid_field_is_accepted() {
    let fixture = TestDir::new("target-doctor-cookie-poisons-session");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    server.reject_cookie_sessions_after_first_use();

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
    let actual = stdout_json(&output);

    // The session confirmation carries the cookie, so it is what kills the session. Everything
    // after it fails, and the ablation is what says which channel did it.
    let ablation = section(&actual, "session_channel_ablation");
    assert_eq!(ablation["status"], "fail");
    let detail = ablation["detail"].as_str().expect("ablation detail");
    assert!(
        detail.contains("accepted with the _sid request field alone"),
        "the ablation must attribute the failure to the channel, not the path: {detail}"
    );
    let remediation = ablation["remediation"]
        .as_str()
        .expect("a confirmed cookie fault names its fix");
    assert!(
        remediation.contains("Stop sending the cookie header for sid-format logins"),
        "unexpected remediation: {remediation}"
    );

    let channels = actual["session_channels"]
        .as_array()
        .expect("session channel probes");
    assert_eq!(channels[0]["channels"], "all");
    assert_eq!(channels[0]["dsm_code"], 119);
    assert_eq!(channels[1]["channels"], "sid-field-only");
    assert_eq!(channels[1]["outcome"], "ok");
    assert_eq!(channels[2]["dsm_code"], 119);

    // The capability matrix must not be painted red by a dead session: a 119 stops the probing
    // and is reported as a session verdict, not as fifteen broken capabilities.
    let diagnosis = &actual["capability_diagnosis"];
    assert_eq!(diagnosis["session_aborted"], true);
    assert_eq!(diagnosis["working"], 0);
    assert_eq!(
        diagnosis["capabilities"][0]["verdict"], "not probed",
        "a session error is not a capability verdict"
    );
    let capability_section = section(&actual, "capability_diagnosis");
    assert_eq!(capability_section["status"], "warn");
    assert!(
        capability_section["detail"]
            .as_str()
            .expect("capability detail")
            .contains("describe the session rather than the capabilities")
    );

    let human = String::from_utf8(
        run(&[
            "--quiet",
            "--output",
            "human",
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
        ])
        .stdout,
    )
    .expect("UTF-8 human report");
    assert!(
        human.contains("DSM session channel ablation:"),
        "the human report should carry the ablation block:\n{human}"
    );
    assert!(
        human.contains("sid-field-only") && human.contains("cookie-only"),
        "every variant should be named:\n{human}"
    );
    assert!(
        !human.contains("e2e-session-secret") && !human.contains("e2e-syno-token-secret"),
        "the ablation varies session channels and must still publish no session value"
    );
}

/// Two host names inside one run is the only positive proof that requests reached two hosts.
#[test]
fn two_host_names_in_one_run_are_reported_as_a_path_that_does_not_reach_one_host() {
    let fixture = TestDir::new("target-doctor-two-hosts");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    server.change_hostname_after_next_info_read("OTHERSTATION");

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
    let actual = stdout_json(&output);

    let diagnosis = &actual["capability_diagnosis"];
    assert_eq!(diagnosis["hostname_changed"], true);
    let capability_section = section(&actual, "capability_diagnosis");
    assert_eq!(capability_section["status"], "fail");
    assert!(
        capability_section["detail"]
            .as_str()
            .expect("capability detail")
            .contains("two different host names")
    );
    assert!(
        capability_section["remediation"]
            .as_str()
            .expect("a two-host finding names its next step")
            .contains("direct address"),
        "the fix for a path that reaches two hosts is a different path"
    );
}

/// A destination whose interior components are missing is a different fault from a missing share,
/// and the report has to say which one it is.
#[test]
fn the_destination_walk_names_the_component_where_resolution_stops() {
    let fixture = TestDir::new("target-doctor-missing-component");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/present");

    let doctor = |remote: &str| {
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
            remote,
        ]);
        stdout_json(&output)
    };

    // The share and its child exist; only the last component does not. Sync can create that, so
    // the section warns rather than failing and says so.
    let missing_leaf = doctor("/team/present/absent");
    let resolution = &missing_leaf["path_resolution"];
    assert_eq!(resolution["total_components"], 3);
    assert_eq!(resolution["first_missing"], 3);
    assert_eq!(resolution["fully_resolved"], false);
    assert_eq!(
        resolution["segments"]
            .as_array()
            .expect("walked segments")
            .iter()
            .map(|segment| (
                segment["path"].as_str().expect("segment path"),
                segment["exists"].as_bool().expect("segment existence")
            ))
            .collect::<Vec<_>>(),
        [
            ("/team", true),
            ("/team/present", true),
            ("/team/present/absent", false)
        ]
    );
    let leaf_section = section(&missing_leaf, "destination_path_resolution");
    assert_eq!(leaf_section["status"], "warn");
    assert!(
        leaf_section["detail"]
            .as_str()
            .expect("resolution detail")
            .contains("stops at component 3 of 3")
    );

    // The shared folder itself is absent. No amount of creating directories fixes that, and the
    // remediation points at the account's visible roots instead.
    let missing_share = doctor("/absent/target");
    let share_section = section(&missing_share, "destination_path_resolution");
    assert_eq!(share_section["status"], "fail");
    assert_eq!(missing_share["path_resolution"]["first_missing"], 1);
    assert!(
        share_section["remediation"]
            .as_str()
            .expect("a missing share names its next step")
            .contains("shared folder named by the first path component")
    );
}

/// A malformed entry in `query=all` must cost its own line of output, not the enumeration.
///
/// `query=all` returns entries authored by whoever wrote each installed package. The strict type
/// that feeds `required_spec` would fail the whole map on one bad entry, which is exactly why the
/// diagnostic read has its own lenient one.
#[test]
fn capability_enumeration_survives_an_entry_the_documented_api_map_cannot_describe() {
    let server = MockFileStation::start();
    server.advertise_malformed_api();

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

    let capabilities = &actual["capabilities"];
    assert_eq!(capabilities["advertised_apis"], 16);
    assert_eq!(capabilities["unusable_entries"], 1);
    let enumeration = section(&actual, "capability_enumeration");
    assert_eq!(enumeration["status"], "warn");
    assert!(
        enumeration["detail"]
            .as_str()
            .expect("enumeration detail")
            .contains("1 advertised entries could not be read")
    );
    // The connection itself is untouched: discovery still validated its ten APIs strictly.
    assert_eq!(actual["api_discovery"], true);
    assert_eq!(section(&actual, "dsm_api_discovery")["status"], "pass");

    let human = String::from_utf8(
        run(&[
            "--quiet",
            "--output",
            "human",
            "doctor",
            "--url",
            server.base_url(),
            "--allow-http",
            "--routing-only",
        ])
        .stdout,
    )
    .expect("UTF-8 human report");
    assert!(
        human.contains("DSM capability enumeration:"),
        "the human report should carry the enumeration block:\n{human}"
    );
    assert!(
        human.contains("SYNO.FileStation.Rename") && human.contains("unused by this tool"),
        "the File Station tier should name what DSM offers and this tool does not use:\n{human}"
    );
    assert!(
        human.contains("SYNO.Core.* (2)"),
        "other namespaces should be counted rather than listed:\n{human}"
    );
}

/// A single trace-level run must explain a live failure without a second round trip.
///
/// This is the shape of log a user is asked to attach: it has to name the build, the endpoint,
/// every request with its DSM code and latency, and which session channels were attached — while
/// containing no credential material at all.
#[test]
fn one_trace_run_explains_a_rejected_session_without_leaking_credentials() {
    let fixture = TestDir::new("target-doctor-trace-diagnostics");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    server.fail_next_api_operation("SYNO.FileStation.List.getinfo", 119);

    let output = run(&[
        "--log-level",
        "trace",
        "--log-format",
        "json",
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

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
    let records = stderr
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str::<Value>(line).expect("one JSON record per line"))
        .collect::<Vec<_>>();
    let event_of = |name: &str| {
        records
            .iter()
            .find(|record| record["event"] == name)
            .unwrap_or_else(|| panic!("no {name} record in:\n{stderr}"))
            .clone()
    };

    // The build banner is the very first thing any sink receives.
    assert_eq!(records[0]["event"], "run.build");
    assert_eq!(records[0]["build"]["name"], "synology-drive-sync");
    assert_eq!(records[0]["build"]["version"], env!("SDSYNC_VERSION"));

    // The endpoint is named once, with its redirect and certificate policy.
    let connection = event_of("connection.established");
    assert_eq!(connection["connection"]["scheme"], "http");
    assert_eq!(connection["connection"]["host"], "127.0.0.1");
    assert_eq!(connection["connection"]["redirects"], "refused");

    // The login shape is what distinguishes a mis-carried session from a bad credential.
    let session = event_of("session.established");
    assert_eq!(session["session"]["login_format"], "sid");
    assert!(
        session["session"]["sid_length"]
            .as_u64()
            .is_some_and(|length| length > 0)
    );

    // Every request is attributable, and the failing one carries its DSM code and description.
    let calls = records
        .iter()
        .filter(|record| record["event"] == "api_call.completed")
        .collect::<Vec<_>>();
    assert!(
        calls.len() >= 4,
        "expected a record per round trip, got {}:\n{stderr}",
        calls.len()
    );
    let getinfo = calls
        .iter()
        .find(|record| record["call"]["method"] == "getinfo")
        .expect("the failing call is recorded");
    assert_eq!(getinfo["level"], "debug", "a failed call must reach debug");
    assert_eq!(getinfo["call"]["api"], "SYNO.FileStation.List");
    assert_eq!(getinfo["call"]["dsm_code"], 119);
    assert_eq!(
        getinfo["call"]["dsm_description"],
        "session is invalid; rerun to authenticate again"
    );
    assert_eq!(getinfo["call"]["http_status"], 200);
    assert_eq!(getinfo["call"]["outcome"], "dsm-error");
    // Which session channels were on the wire, as booleans and never values.
    assert_eq!(getinfo["call"]["session"]["cookie_header"], true);
    assert_eq!(getinfo["call"]["session"]["sid_field"], true);

    // The same session succeeded moments earlier; that contrast is the whole diagnosis.
    let list_share = calls
        .iter()
        .find(|record| record["call"]["method"] == "list_share")
        .expect("the preceding successful call is recorded");
    assert_eq!(list_share["call"]["outcome"], "ok");
    assert_eq!(
        list_share["level"], "trace",
        "a healthy call stays at trace"
    );

    // Nothing in the entire stream may carry credential material.
    let secrets = [
        "correct horse battery staple",
        "e2e-session-secret",
        "passwd",
        "otp_code",
        "SynoToken",
    ];
    for secret in secrets {
        assert!(
            !stderr.contains(secret),
            "trace diagnostics leaked {secret:?}:\n{stderr}"
        );
    }
}

/// A response that sets a cookie is reported by name, never by value.
///
/// Whether DSM rotates the session on a *successful* response is the discriminator between a
/// session the client may keep reusing and one it has already invalidated by continuing to send the
/// previous identifier. The names make that visible; the values must never leave the process.
#[test]
fn responses_that_set_cookies_are_reported_by_name_only() {
    let fixture = TestDir::new("target-doctor-set-cookie-names");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    let cookie_value = "proxy-cookie-must-not-be-used";
    server.require_header_session_transport(&format!(
        "id={cookie_value}; Path=/; HttpOnly; SameSite=Strict"
    ));

    let output = run(&[
        "--log-level",
        "trace",
        "--log-format",
        "json",
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

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
    let records = stderr
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str::<Value>(line).expect("one JSON record per line"))
        .collect::<Vec<_>>();

    // Completion records only: a start record is emitted before any response exists.
    let completed = |method: &str| {
        records
            .iter()
            .find(|record| {
                record["event"] == "api_call.completed" && record["call"]["method"] == method
            })
            .unwrap_or_else(|| panic!("no completed {method} record in:\n{stderr}"))
            .clone()
    };

    let login = completed("login");
    assert_eq!(login["call"]["set_cookie_count"], 1);
    assert_eq!(login["call"]["set_cookie_names"], "id");

    // Calls that set nothing say so, which is what makes a rotation stand out.
    let list_share = completed("list_share");
    assert_eq!(list_share["call"]["set_cookie_count"], 0);
    assert_eq!(list_share["call"]["set_cookie_names"], "");

    // The value behind the reported name must not appear anywhere in the stream.
    assert!(
        !stderr.contains(cookie_value),
        "cookie reporting leaked its value:\n{stderr}"
    );
}

/// A session rotated on an ordinary authenticated response is reported on *that* response.
///
/// This is the decisive case. Capturing `Set-Cookie` only on the login response would leave the
/// question unanswered, because the fact that matters is whether DSM rotates the session on a
/// later successful call — after which a client still sending the previous identifier is stale.
#[test]
fn a_session_rotated_mid_run_is_reported_on_the_call_that_rotated_it() {
    let fixture = TestDir::new("target-doctor-mid-run-rotation");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    let rotated_value = "rotated-session-value-must-not-leak";
    // Not the login response: an ordinary authenticated call, exactly as DSM would.
    server.rotate_session_on(
        "SYNO.FileStation.List.list_share",
        &format!("id={rotated_value}; Path=/; HttpOnly"),
    );

    let output = run(&[
        "--log-level",
        "trace",
        "--log-format",
        "json",
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

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
    let records = stderr
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str::<Value>(line).expect("one JSON record per line"))
        .collect::<Vec<_>>();
    let completed = |method: &str| {
        records
            .iter()
            .find(|record| {
                record["event"] == "api_call.completed" && record["call"]["method"] == method
            })
            .unwrap_or_else(|| panic!("no completed {method} record in:\n{stderr}"))
            .clone()
    };

    // The rotation is attributed to the successful call that performed it.
    let list_share = completed("list_share");
    assert_eq!(list_share["call"]["outcome"], "ok");
    assert_eq!(list_share["call"]["set_cookie_count"], 1);
    assert_eq!(list_share["call"]["set_cookie_names"], "id");

    // The login response set nothing, so the two are distinguishable in one log.
    let login = completed("login");
    assert_eq!(login["call"]["set_cookie_count"], 0);
    assert_eq!(
        records
            .iter()
            .find(|record| record["event"] == "session.established")
            .expect("a session record")["session"]["server_set_cookie"],
        false
    );

    // A later call still reports its own rotation state, so staleness is traceable forward.
    let getinfo = completed("getinfo");
    assert_eq!(getinfo["call"]["set_cookie_count"], 0);

    assert!(
        !stderr.contains(rotated_value),
        "rotation reporting leaked the new session value:\n{stderr}"
    );
}

/// The cookie ledger names the rotation loudly and still publishes no cookie value.
///
/// This is the whole point of the permanence check and of the redaction rule at once. A cookie
/// re-issued on a *successful* call, by a client that keeps no cookie jar, is the observation that
/// separates "this client is presenting the session wrongly" from "the path is not stable". The
/// operator has to be able to read that off the report, and the report has to reach an issue
/// tracker without carrying a live session identifier into it.
#[test]
fn a_rotated_cookie_is_reported_in_full_without_publishing_any_cookie_value() {
    let fixture = TestDir::new("target-doctor-cookie-ledger");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    let login_value = "hgU9TnMzBqXwLpR7vKd2eFsA4Yj6Nc1QoZi8Wm3Xb5";
    let rotated_value = "Rk4pLs7Wq2Zx9Tb6Nv3Hc8Md1Ug5Yf0Ea7Jr4Ki2Po";
    server.require_header_session_transport(&format!(
        "id={login_value}; Path=/; HttpOnly; SameSite=Lax"
    ));
    server.rotate_session_on(
        "SYNO.FileStation.List.list_share",
        &format!("id={rotated_value}; Path=/; HttpOnly; SameSite=Lax"),
    );

    let arguments = |format: &str| {
        vec![
            "--log-level".to_owned(),
            "trace".to_owned(),
            "--log-format".to_owned(),
            "json".to_owned(),
            "--output".to_owned(),
            format.to_owned(),
            "doctor".to_owned(),
            "--url".to_owned(),
            server.base_url().to_owned(),
            "--username".to_owned(),
            "e2e-user".to_owned(),
            "--password-file".to_owned(),
            password.to_str().expect("UTF-8 password path").to_owned(),
            "--no-vault".to_owned(),
            "--allow-http".to_owned(),
            "target".to_owned(),
            "/team/target".to_owned(),
        ]
    };
    let run_with = |format: &str| {
        let owned = arguments(format);
        let borrowed = owned.iter().map(String::as_str).collect::<Vec<_>>();
        let output = run(&borrowed);
        (
            String::from_utf8(output.stdout).expect("UTF-8 report"),
            String::from_utf8(output.stderr).expect("UTF-8 diagnostics"),
        )
    };

    let (json_stdout, json_stderr) = run_with("json");
    let actual: Value = serde_json::from_str(&json_stdout).expect("a JSON doctor report");

    let ledger_section = actual["sections"]
        .as_array()
        .expect("section array")
        .iter()
        .find(|section| section["id"] == "session_cookie_ledger")
        .expect("the cookie ledger section")
        .clone();
    assert_eq!(ledger_section["status"], "warn");
    let detail = ledger_section["detail"].as_str().expect("a ledger detail");
    assert!(
        detail.contains("NEW value for cookie id"),
        "the rotation should be unmissable in the section detail: {detail}"
    );
    assert!(
        ledger_section["remediation"]
            .as_str()
            .expect("a ledger remediation")
            .contains("format=cookie"),
        "the ledger should name the two coherent session transports"
    );

    let cookies = &actual["transport"]["cookies"];
    assert_eq!(cookies["rotated_on_success"], "id");
    // The client installs no cookie jar, so a rotated value is observed and then discarded. That
    // fact is read from the code rather than asserted in prose, and the report carries it.
    assert_eq!(cookies["client_maintains_cookie_jar"], false);
    let id = cookies["cookies"]
        .as_array()
        .expect("cookie entries")
        .iter()
        .find(|entry| entry["name"] == "id")
        .expect("the id cookie")
        .clone();
    assert_eq!(id["dsm_cookie"], true);
    assert_eq!(id["set_count"], 2);
    assert_eq!(id["distinct_values"], 2);
    assert_eq!(id["rotated_on_success"], true);
    // Attributes are reported by name; Path and Domain by presence only.
    assert_eq!(id["persistence"], "session");
    assert_eq!(id["http_only"], true);
    assert_eq!(id["same_site"], "lax");
    assert_eq!(id["path_present"], true);
    assert_eq!(id["domain_present"], false);
    assert_eq!(id["value_length"], rotated_value.len());
    // The rotation is attributed to the successful call that carried it.
    let rotation = id["rotations"]
        .as_array()
        .expect("rotation list")
        .first()
        .expect("one rotation")
        .clone();
    assert_eq!(rotation["on_success"], true);
    assert_eq!(rotation["at"]["method"], "list_share");

    // Reachability is TCP, and the payload says so rather than leaving "ping" to be read as ICMP.
    assert_eq!(actual["transport"]["probe_method"], "tcp-connect");
    assert_eq!(actual["transport"]["reachability"]["method"], "tcp-connect");

    let (human_stdout, human_stderr) = run_with("human");
    assert!(
        human_stdout.contains("Cookie permanence across the run:"),
        "the human report should carry the ledger:\n{human_stdout}"
    );
    assert!(
        human_stdout.contains("ROTATED ON A SUCCESSFUL RESPONSE"),
        "the human report should shout about the rotation:\n{human_stdout}"
    );
    assert!(
        human_stdout.contains("keeps no cookie jar"),
        "the human report should state what happened to the rotated value:\n{human_stdout}"
    );
    assert!(
        human_stdout.contains("not by ICMP"),
        "the human report should not let \"ping\" be read as ICMP:\n{human_stdout}"
    );

    // The decisive assertion, across every surface the report reaches: two renderers and the
    // structured log stream, and neither cookie value anywhere in any of them.
    for (label, rendered) in [
        ("the JSON report", &json_stdout),
        ("the JSON log stream", &json_stderr),
        ("the human report", &human_stdout),
        ("the human run's log stream", &human_stderr),
    ] {
        for secret in [login_value, rotated_value] {
            assert!(
                !rendered.contains(secret),
                "{label} leaked a cookie value:\n{rendered}"
            );
        }
        // Not just the whole value: no leading fragment of one either.
        for secret in [&login_value[..12], &rotated_value[..12]] {
            assert!(
                !rendered.contains(secret),
                "{label} leaked part of a cookie value:\n{rendered}"
            );
        }
    }
}

/// A permission refusal is not a session failure, so enumeration still runs.
///
/// "Can read but cannot write" is a real and useful diagnosis, and stopping on every permission
/// error would throw it away.
#[test]
fn a_permission_refusal_still_enumerates_the_destination() {
    let fixture = TestDir::new("target-doctor-permission-refused");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    // 105 is "session does not have permission": the session itself remains valid.
    server.fail_next_api_operation("SYNO.FileStation.CheckPermission.write", 105);

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

    let actual = stdout_json(&output);
    assert_eq!(
        section(&actual, "destination_permissions")["status"],
        "fail"
    );
    assert_eq!(
        section(&actual, "destination_inventory")["status"],
        "pass",
        "a permission refusal must not suppress the inventory"
    );
    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.operation() == "SYNO.FileStation.List.list"),
        "the destination should still have been enumerated"
    );
}

#[test]
fn dsm7_reverse_proxy_receives_explicit_session_headers_and_body_fields() {
    let fixture = TestDir::new("target-doctor-explicit-sid");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/target");
    server.require_header_session_transport(
        "id=proxy-cookie-must-not-be-used; Path=/; HttpOnly; SameSite=Strict",
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
        "--write-test",
    ]);
    assert_success(&output);

    let requests = server.requests();
    let login = requests
        .iter()
        .find(|request| request.operation() == "SYNO.API.Auth.login")
        .expect("login request");
    assert_eq!(
        login.fields.get("session").map(String::as_str),
        Some("FileStation")
    );
    assert_eq!(login.fields.get("format").map(String::as_str), Some("sid"));
    assert_eq!(
        login.fields.get("enable_syno_token").map(String::as_str),
        Some("yes")
    );
    assert!(!login.fields.contains_key("_sid"));
    assert!(!login.fields.contains_key("SynoToken"));
    assert!(!login.headers.contains_key("cookie"));
    assert!(!login.headers.contains_key("x-syno-token"));

    let discovery = requests
        .iter()
        .find(|request| request.operation() == "SYNO.API.Info.query")
        .expect("discovery request");
    assert!(!discovery.headers.contains_key("cookie"));
    assert!(!discovery.headers.contains_key("x-syno-token"));

    let authenticated = requests
        .iter()
        .filter(|request| {
            request.operation() != "SYNO.API.Info.query"
                && request.operation() != "SYNO.API.Auth.login"
        })
        .collect::<Vec<_>>();
    assert!(!authenticated.is_empty());
    assert_eq!(
        authenticated.last().map(|request| request.operation()),
        Some("SYNO.API.Auth.logout".to_owned())
    );
    for operation in [
        "SYNO.FileStation.Upload.upload",
        "SYNO.FileStation.Download.download",
        "SYNO.FileStation.Delete.delete",
    ] {
        assert!(
            authenticated
                .iter()
                .any(|request| request.operation() == operation),
            "extensive doctor must exercise {operation}"
        );
    }
    // The channel-ablation probe is the one place a request is *meant* to carry less than the
    // full set of session channels: presenting them one at a time is the whole point of it. Those
    // variants are identified by shape and pinned to the exact combinations the probe claims, so
    // "some requests carry fewer channels" can never quietly become "the client stopped sending
    // one". Every other authenticated request still carries all four, with the right values.
    let channel_shape = |request: &CapturedRequest| {
        (
            request.fields.contains_key("_sid"),
            request.fields.contains_key("SynoToken"),
            request.headers.contains_key("cookie"),
            request.headers.contains_key("x-syno-token"),
        )
    };
    let (ablation, ordinary): (Vec<_>, Vec<_>) = authenticated
        .iter()
        .copied()
        .partition(|request| channel_shape(request) != (true, true, true, true));
    assert_eq!(
        ablation
            .iter()
            .map(|request| channel_shape(request))
            .collect::<Vec<_>>(),
        [
            (true, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
            // The tokenless-login variant: the documented `_sid` field alone, against a session
            // established by a second login that omitted `enable_syno_token`.
            (true, false, false, false),
            // That second session logging itself out. It carries no `SynoToken` because DSM never
            // issued it one, which is the whole point of the variant -- and its presence here is
            // the proof that the extra session does not outlive the probe that needed it.
            (true, false, true, false),
        ],
        "only the reduced ablation variants and the second session's logout may present a \
         partial session"
    );
    assert!(ordinary.iter().all(|request| {
        request.fields.get("_sid").map(String::as_str) == Some("e2e-session-secret")
            && request.fields.get("SynoToken").map(String::as_str) == Some("e2e-syno-token-secret")
            && request.headers.get("cookie").map(String::as_str) == Some("id=e2e-session-secret")
            && request.headers.get("x-syno-token").map(String::as_str)
                == Some("e2e-syno-token-secret")
    }));

    // A server that demands every channel at once rejects the documented `_sid` field on its own,
    // and the ablation reports that rather than a healthy session.
    let actual = stdout_json(&output);
    assert_eq!(
        section(&actual, "session_channel_ablation")["status"],
        "warn"
    );
    assert_eq!(actual["session_channels"][0]["outcome"], "ok");
    assert_eq!(actual["session_channels"][1]["dsm_code"], 119);
    assert_eq!(actual["session_channels"][2]["dsm_code"], 119);
    // Cookie-only was rejected as well, so no single channel carries this session and the verdict
    // must say the *combination* is what DSM accepts. Reading the `_sid` rejection on its own as
    // "DSM resolves the session from the cookie" pointed the operator at dropping the cookie --
    // the one change that would break a server behaving like this one.
    let ablation_section = section(&actual, "session_channel_ablation");
    let detail = ablation_section["detail"]
        .as_str()
        .expect("ablation detail");
    assert!(
        detail.contains("only the full combination of channels was accepted"),
        "the ablation should name what the server actually honoured: {detail}"
    );
    assert!(
        !detail.contains("resolving the session from the cookie"),
        "a rejected cookie-only variant cannot support that reading: {detail}"
    );
    // The tokenless second login is rejected by this server too, which is itself the finding.
    assert_eq!(
        actual["session_channels"][4]["channels"],
        "sid-field-only-tokenless-login"
    );
    assert_eq!(actual["session_channels"][4]["ran"], true);
    assert!(
        detail.contains("regardless of how the session was created"),
        "the tokenless variant's answer belongs in the verdict: {detail}"
    );
    let remediation = ablation_section["remediation"]
        .as_str()
        .expect("ablation remediation");
    assert!(
        remediation.contains("Keep sending both channels"),
        "the remediation must not tell an operator to remove a channel this server requires: \
         {remediation}"
    );

    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in [
        "correct horse battery staple",
        "e2e-session-secret",
        "e2e-syno-token-secret",
        "proxy-cookie-must-not-be-used",
    ] {
        assert!(!rendered.contains(secret));
    }
}

#[test]
fn untargeted_doctor_discovers_bounded_visible_shared_folders_without_selecting_one() {
    let fixture = TestDir::new("untargeted-doctor");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    for share in [
        "alpha", "beta", "delta", "epsilon", "gamma", "omega", "zeta",
    ] {
        server.add_directory(&format!("/{share}"));
    }
    // list_share is authenticated discovery, not a browse/write permission
    // assertion. Preserve a reported root even when File Station marks child
    // listing unavailable.
    server.disable_directory_listing("/alpha");
    let password_path = password.to_str().expect("UTF-8 password path");
    let invoke = |format| {
        run(&[
            "--quiet",
            "--output",
            format,
            "doctor",
            "--url",
            server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            password_path,
            "--no-vault",
            "--allow-http",
            "target",
        ])
    };

    let json_output = invoke("json");
    assert_success(&json_output);
    let actual = stdout_json(&json_output);
    assert_eq!(actual["level"], "standard");
    assert_eq!(actual["authenticated"], true);
    assert_eq!(actual["remote_checked"], false);
    assert_eq!(actual["remote_exists"], Value::Null);
    assert_eq!(actual["remote_entries"], Value::Null);
    assert_eq!(actual["write_permission_scope"], Value::Null);
    assert_eq!(actual["write_permission_path"], Value::Null);
    assert_eq!(
        section(&actual, "destination_permissions")["status"],
        "skip"
    );
    assert!(
        section(&actual, "destination_permissions")["detail"]
            .as_str()
            .expect("permission skip detail")
            .contains("no destination was selected")
    );
    assert_eq!(section(&actual, "destination_inventory")["status"], "pass");
    assert_eq!(
        actual["remote_inventory"]["scope"],
        "visible_shared_folders"
    );
    assert_eq!(actual["remote_inventory"]["root_exists"], true);
    assert_eq!(actual["remote_inventory"]["total_entries"], 8);
    assert_eq!(actual["remote_inventory"]["sample_count"], 5);
    assert_eq!(actual["remote_inventory"]["sample_limit"], 5);
    assert_eq!(actual["remote_inventory"]["truncated"], true);
    assert_eq!(actual["remote_inventory"]["truncated_count"], 3);
    assert_eq!(
        actual["remote_inventory"]["truncated_reason"],
        "sample_limit"
    );
    assert_eq!(actual["remote_inventory"]["budget"]["pages_requested"], 1);
    assert_eq!(actual["remote_inventory"]["budget"]["traversal_depth"], 0);
    assert_eq!(actual["remote_inventory"]["budget"]["deadline_ms"], 5_000);
    assert_eq!(
        actual["remote_inventory"]["sample"]
            .as_array()
            .expect("shared-folder sample")
            .iter()
            .map(|entry| entry["relative_path"].as_str().expect("logical share path"))
            .collect::<Vec<_>>(),
        ["/alpha", "/beta", "/delta", "/epsilon", "/gamma"]
    );
    assert!(
        actual["remote_inventory"]["sample"]
            .as_array()
            .expect("shared-folder sample")
            .iter()
            .all(|entry| {
                entry["kind"] == "directory"
                    && entry["size_bytes"].is_null()
                    && entry["mtime_seconds"].is_null()
                    && entry["mount_boundary"] == false
            })
    );

    let ndjson_output = invoke("ndjson");
    assert_success(&ndjson_output);
    let records = stdout_ndjson(&ndjson_output);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["remote_inventory"], actual["remote_inventory"]);
    assert_eq!(records[0]["remote_checked"], false);
    assert_eq!(records[0]["remote_exists"], Value::Null);

    let human_output = invoke("human");
    assert_success(&human_output);
    let human = String::from_utf8(human_output.stdout.clone()).expect("human output is UTF-8");
    assert!(human.contains("8 visible shared-folder roots; 5 sampled; 3 truncated"));
    assert!(human.contains("path=/alpha; name=alpha; kind=directory"));
    assert!(human.contains("no destination was selected or permission-checked"));

    for output in [&json_output, &ndjson_output, &human_output] {
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!rendered.contains("correct horse battery staple"));
        assert!(!rendered.contains("e2e-session-secret"));
        assert!(!rendered.contains("e2e-syno-token-secret"));
    }

    let requests = server.requests();
    // Three identical runs of fifteen requests each. The sequence is pinned once, in
    // `authenticated_target_doctor_checks_exact_destination_and_logs_out`; what matters here is
    // that all three runs make exactly the same requests in exactly the same order.
    assert_eq!(requests.len(), 45);
    for request_set in requests.as_chunks::<15>().0 {
        assert_eq!(
            request_set
                .iter()
                .map(|request| request.operation())
                .collect::<Vec<_>>(),
            [
                "SYNO.API.Info.query",
                "SYNO.API.Info.query",
                "SYNO.API.Auth.login",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.Info.get",
                "SYNO.FileStation.List.list_share",
                "SYNO.FileStation.VirtualFolder.list",
                "SYNO.FileStation.BackgroundTask.list",
                "SYNO.FileStation.Info.get",
                "SYNO.FileStation.List.list_share",
                "SYNO.API.Auth.logout",
            ]
        );
        let confirmation = &request_set[3];
        assert_eq!(
            confirmation.fields.get("offset").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            confirmation.fields.get("limit").map(String::as_str),
            Some("1")
        );
        let listing = &request_set[13];
        assert_eq!(listing.fields.get("offset").map(String::as_str), Some("0"));
        assert_eq!(listing.fields.get("limit").map(String::as_str), Some("6"));
        assert_eq!(
            listing.fields.get("additional").map(String::as_str),
            Some("[\"perm\"]")
        );
    }
    assert!(
        requests
            .iter()
            .all(|request| !is_mutation(&request.operation()))
    );
    assert!(requests.iter().all(|request| {
        request.operation() != "SYNO.FileStation.List.getinfo"
            && request.operation() != "SYNO.FileStation.List.list"
    }));
}

#[test]
fn untargeted_doctor_preserves_explicit_zero_shared_folder_evidence() {
    let fixture = TestDir::new("untargeted-doctor-empty");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.remove_directory("/team");

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
    ]);

    assert_success(&output);
    let actual = stdout_json(&output);
    assert_eq!(actual["status"], "warn");
    assert_eq!(actual["remote_checked"], false);
    assert_eq!(actual["remote_exists"], Value::Null);
    assert_eq!(
        section(&actual, "destination_permissions")["status"],
        "skip"
    );
    assert_eq!(section(&actual, "destination_inventory")["status"], "pass");
    assert_eq!(
        actual["remote_inventory"]["scope"],
        "visible_shared_folders"
    );
    assert_eq!(actual["remote_inventory"]["total_entries"], 0);
    assert_eq!(actual["remote_inventory"]["sample_count"], 0);
    assert_eq!(actual["remote_inventory"]["sample"], json!([]));
    assert_eq!(actual["remote_inventory"]["truncated"], false);
    assert_eq!(actual["remote_inventory"]["truncated_count"], 0);
    assert_eq!(actual["remote_inventory"]["truncated_reason"], Value::Null);

    assert_eq!(
        server
            .requests()
            .iter()
            .map(|request| request.operation())
            .collect::<Vec<_>>(),
        [
            "SYNO.API.Info.query",
            "SYNO.API.Info.query",
            "SYNO.API.Auth.login",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.Info.get",
            "SYNO.FileStation.List.list_share",
            "SYNO.FileStation.VirtualFolder.list",
            "SYNO.FileStation.BackgroundTask.list",
            "SYNO.FileStation.Info.get",
            "SYNO.FileStation.List.list_share",
            "SYNO.API.Auth.logout",
        ]
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
    assert_eq!(section(&result, "dsm_session_auth")["status"], "fail");
    assert_eq!(
        section(&result, "destination_permissions")["status"],
        "skip"
    );
    assert!(
        section(&result, "destination_permissions")["detail"]
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
        [
            "SYNO.API.Info.query",
            "SYNO.API.Info.query",
            "SYNO.API.Auth.login"
        ]
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
    assert_eq!(section(&result, "dsm_session_auth")["status"], "fail");
    assert_eq!(
        section(&result, "disposable_write_verify_cleanup")["status"],
        "skip"
    );
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

    // Index 0 and 1 are the two unauthenticated discovery reads: the ten-name query and the
    // `query=all` capability enumeration.
    let requests = server.requests();
    assert_eq!(requests[2].operation(), "SYNO.API.Auth.login");
    assert!(!requests[2].fields.contains_key("otp_code"));
    assert_eq!(requests[3].operation(), "SYNO.API.Auth.login");
    let generated = requests[3]
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

/// The ablation's second login must not be able to break the run it is diagnosing.
///
/// Both this client's logins and its logouts name `session=FileStation`. DSM binds one session per
/// (account, session name) and answers a collision with `107`, "session interrupted by duplicate
/// login" -- a code `session_is_unusable` treats as fatal. A second login taken at step 7 could
/// therefore invalidate the run's own session and abort steps 8 through 16, which for a user whose
/// reported problem *is* a session that stops being recognised would mean the diagnostic
/// manufacturing the symptom it was asked to explain.
///
/// The variant is deferred to after the primary logout, so this asserts two things: the duplicate
/// login really does arrive last, and every section that depends on the primary session completed
/// normally against a server that punishes duplicate logins.
#[test]
fn the_second_ablation_login_cannot_disturb_the_run_it_diagnoses() {
    let fixture = TestDir::new("duplicate-login");
    let password = fixture.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/probe");
    server.invalidate_session_on_duplicate_login();

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
        "/team/probe",
        "--write-test",
    ]);

    assert_success(&output);
    let result = stdout_json(&output);
    assert_eq!(result["level"], "extensive");

    // Everything from the ablation onwards runs on the primary session. None of it may fail.
    for id in [
        "session_channel_ablation",
        "session_concurrency",
        "capability_diagnosis",
        "destination_path_resolution",
        "destination_permissions",
        "destination_inventory",
        "disposable_write_verify_cleanup",
        "session_logout",
    ] {
        assert_ne!(
            section(&result, id)["status"],
            "fail",
            "{id} failed on a server that invalidates a session on duplicate login: {}",
            section(&result, id)["detail"]
        );
    }
    assert_eq!(section(&result, "session_logout")["status"], "pass");
    assert_eq!(
        section(&result, "disposable_write_verify_cleanup")["status"],
        "pass"
    );
    assert_eq!(result["write_test"]["status"], "success");

    // The variant did run -- this test would otherwise pass by never taking the risk at all.
    assert_eq!(server.logins_seen(), 2, "the second login must be made");
    let channels = result["session_channels"]
        .as_array()
        .expect("session channel probes");
    assert_eq!(
        channels[4]["channels"], "sid-field-only-tokenless-login",
        "the deferred variant is the last one"
    );
    assert_eq!(channels[4]["ran"], true);

    // And it ran *after* the primary session was closed, which is what makes it safe rather than
    // lucky. A regression that moved it back into step 7 fails here.
    let operations = server
        .requests()
        .iter()
        .map(|request| request.operation())
        .collect::<Vec<_>>();
    let logins = operations
        .iter()
        .enumerate()
        .filter(|(_, operation)| operation.as_str() == "SYNO.API.Auth.login")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let logouts = operations
        .iter()
        .enumerate()
        .filter(|(_, operation)| operation.as_str() == "SYNO.API.Auth.logout")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(logins.len(), 2, "one primary login and one probe login");
    assert_eq!(logouts.len(), 2, "each session logs itself out");
    assert!(
        logins[1] > logouts[0],
        "the duplicate login must arrive after the primary session is closed: \
         logins at {logins:?}, logouts at {logouts:?}"
    );
    assert!(
        logouts[1] > logins[1],
        "the second session must log itself out: logouts at {logouts:?}"
    );
    assert_eq!(
        operations.last().map(String::as_str),
        Some("SYNO.API.Auth.logout"),
        "the run ends with a logout, not with a live extra session"
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
        section(&result, "disposable_write_verify_cleanup")["status"],
        "pass"
    );
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
    assert_eq!(
        section(&result, "disposable_write_verify_cleanup")["status"],
        "skip"
    );
    assert!(
        section(&result, "disposable_write_verify_cleanup")["detail"]
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
        "SYNO.API.Info.query",
        "SYNO.API.Auth.login",
        "SYNO.FileStation.List.list_share",
        "SYNO.FileStation.List.list_share",
        "SYNO.FileStation.List.list_share",
        "SYNO.FileStation.List.list_share",
        "SYNO.FileStation.List.list_share",
        "SYNO.FileStation.Info.get",
        "SYNO.FileStation.List.list_share",
        "SYNO.FileStation.VirtualFolder.list",
        "SYNO.FileStation.BackgroundTask.list",
        "SYNO.FileStation.Info.get",
        "SYNO.FileStation.List.getinfo",
        "SYNO.FileStation.List.getinfo",
        "SYNO.FileStation.CheckPermission.write",
        // No third `getinfo`: the walk already established that the destination exists, is a
        // directory, and is not a mount boundary, so the inventory opens at its listing.
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
    // A destination the walk could not resolve keeps the inventory's own `getinfo`. That request
    // is what reports the target as absent rather than as an error, so the round trip a resolved
    // destination no longer pays for must still be made here. `remote_exists: false` above is
    // reachable only through it; this names the request so a regression says which one went
    // missing.
    // Counted, not merely present: the walk issues its own `getinfo` for this path on the way to
    // discovering the 408, so `any` would pass on the walk's request alone and prove nothing. Two
    // is the walk's and the inventory's.
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| {
                request.operation() == "SYNO.FileStation.List.getinfo"
                    && request
                        .fields
                        .get("path")
                        .is_some_and(|path| path.contains("/team/missing"))
            })
            .count(),
        2,
        "the absent destination must still be inspected by the inventory's own getinfo"
    );
    assert_eq!(
        section(&records[1]["doctor"], "disposable_write_verify_cleanup")["status"],
        "fail"
    );
    assert!(
        section(&records[1]["doctor"], "disposable_write_verify_cleanup")["detail"]
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
    // Discovery falls back from entry.cgi to query.cgi, and the capability enumeration then takes
    // entry.cgi again: it reuses the same fallback rather than assuming the route discovery
    // settled on, because a proxy that misroutes one of these can misroute the other.
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].request_path, "/prefix/webapi/entry.cgi");
    assert_eq!(requests[1].request_path, "/prefix/webapi/query.cgi");
    assert_eq!(requests[2].request_path, "/prefix/webapi/entry.cgi");
    assert_eq!(
        requests[2].fields.get("query").map(String::as_str),
        Some("all")
    );
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
