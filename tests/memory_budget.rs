//! SC-2: a tree too large for the memory a run may use is refused by name before it is listed,
//! as a lasting warning (exit 0), never killed by the OOM killer with no cause.
//!
//! `--memory-budget` stands in for the kernel's figure so the refusal is testable on every host;
//! the Linux cgroup and `MemAvailable` reading is unit-tested in `memory_budget.rs`.

#[allow(dead_code)]
mod support;

use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";

fn run(arguments: &[&str]) -> Output {
    let environment = TestDir::new("membudget-environment");
    for directory in ["home", "appdata", "local-appdata", "xdg"] {
        fs::create_dir_all(environment.child(directory)).expect("create environment directory");
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

fn arguments<'a>(
    verb: &'a str,
    source: &'a str,
    password: &'a str,
    url: &'a str,
    budget: &'a str,
) -> Vec<&'a str> {
    vec![
        "--output",
        "json",
        verb,
        source,
        "/team/budget",
        "--url",
        url,
        "--username",
        "e2e-user",
        "--password-file",
        password,
        "--no-vault",
        "--allow-http",
        "--memory-budget",
        budget,
    ]
}

struct Fixture {
    _dir: TestDir,
    source: String,
    password: String,
    server: MockFileStation,
}

fn fixture() -> Fixture {
    let dir = TestDir::new("membudget");
    for index in 0..2_000 {
        dir.write(&format!("source/d{}/f{index}.txt", index % 20), b"x");
    }
    let password = dir.write("password", PASSWORD);
    let server = MockFileStation::start();
    server.add_directory("/team/budget");
    Fixture {
        source: dir.child("source").to_str().expect("UTF-8").to_owned(),
        password: password.to_str().expect("UTF-8").to_owned(),
        _dir: dir,
        server,
    }
}

#[test]
fn a_tree_over_the_memory_budget_is_refused_by_name_before_it_is_listed() {
    let fixture = fixture();
    let output = run(&arguments(
        "sync",
        &fixture.source,
        &fixture.password,
        fixture.server.base_url(),
        "1",
    ));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "lasting, user-fixable: {stderr}"
    );
    assert!(
        stderr.contains("tree_too_large_for_memory") && stderr.contains("Split this profile"),
        "{stderr}"
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let result = &document["result"];
    assert_eq!(result["outcome"], "complete");
    assert_eq!(result["uploaded"], 0);
    let deferred = result["deferred"].as_array().expect("deferred list");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    assert_eq!(deferred[0]["path"], ".");
    assert_eq!(deferred[0]["reason"], "tree-too-large-for-memory");
    let detail = deferred[0]["detail"].as_str().expect("detail");
    assert!(
        detail.contains("2020 local") && detail.contains("MiB"),
        "names the counts and the estimate: {detail}"
    );
    assert!(
        fixture.server.requests().is_empty(),
        "refused before connecting, so nothing was listed"
    );
}

#[test]
fn a_plan_names_the_refusal_among_its_skips() {
    let fixture = fixture();
    let output = run(&arguments(
        "plan",
        &fixture.source,
        &fixture.password,
        fixture.server.base_url(),
        "1",
    ));
    assert_eq!(output.status.code(), Some(0));
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let skipped = document["plan"]["skipped"]
        .as_array()
        .expect("skipped list");
    assert!(
        skipped.iter().any(|entry| entry["path"] == "."
            && entry["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("tree-too-large-for-memory"))),
        "{skipped:?}"
    );
    assert!(fixture.server.requests().is_empty());
}

#[test]
fn a_tree_inside_the_budget_runs_as_before() {
    let fixture = fixture();
    let output = run(&arguments(
        "plan",
        &fixture.source,
        &fixture.password,
        fixture.server.base_url(),
        "4096",
    ));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["plan"]["summary"]["uploads"], 2_000);
}
