//! fix8 R4: Doctor keeps its structured report when its log cannot be written.
//!
//! A failed log write used to replace the whole report with one `error:` line on standard error.
//! The package's UI, finding no `sdsync.doctor.v1` document, showed every step as skipped with no
//! cause. `/dev/full` fails every write with `ENOSPC`, as a full log volume would.
#![cfg(target_os = "linux")]

#[allow(dead_code)]
mod support;

use std::process::Command;

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

#[test]
fn a_doctor_whose_log_cannot_be_written_still_prints_its_report_and_fails_by_name() {
    let environment = TestDir::new("doctor-log-full");
    let server = MockFileStation::start();
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command
        .args([
            "--log-file",
            "/dev/full",
            "--output",
            "json",
            "doctor",
            "--url",
            server.base_url(),
            "--allow-http",
            "--level",
            "quick",
        ])
        .env("HOME", environment.child("home"))
        .env("XDG_CONFIG_HOME", environment.child("xdg"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SDSYNC_") {
            command.env_remove(name);
        }
    }
    let output = command.output().expect("run synology-drive-sync doctor");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_ne!(
        output.status.code(),
        Some(0),
        "a failed log write fails the run"
    );
    assert!(
        stderr.contains("diagnostic log"),
        "the failure is named: {stderr}"
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "the structured report survives ({error}): stdout {} stderr {stderr}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(document["schema"], "sdsync.doctor.v1");
    assert_eq!(document["status"], "fail");
    let sections = document["sections"].as_array().expect("sections");
    assert_eq!(sections.len(), 16);
    for section in sections {
        let detail = section["detail"].as_str().unwrap_or_default();
        assert_ne!(detail, "not reached", "every section says why: {section}");
    }
    assert!(
        sections.iter().any(|section| section["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("not run: the diagnostic log failed ("))),
        "{sections:?}"
    );
}
