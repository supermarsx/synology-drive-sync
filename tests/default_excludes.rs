//! fix8 R7b: the office lock and temporary files, and the OS thumbnail and folder metadata files,
//! excluded by default are neither uploaded nor, under `--delete`, deleted from the NAS: a copy
//! already there stays.

#[allow(dead_code)]
mod support;

use std::fs;
use std::process::Command;

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const ROOT: &str = "/team/office";

#[test]
fn office_lock_files_are_not_uploaded_and_their_nas_copies_are_never_deleted() {
    let fixture = TestDir::new("default-excludes-office");
    let source = fixture.child("source");
    fs::create_dir_all(&source).expect("create source");
    fs::write(source.join("report.odt"), b"the document").expect("write document");
    // Open documents' companions, locally.
    for name in [
        ".~lock.report.odt#",
        "~WRL0001.tmp",
        "orders.laccdb",
        "orders.ldb",
        "~$report.odt",
        "Thumbs.db",
        ".DS_Store",
    ] {
        fs::write(source.join(name), b"lock").expect("write lock file");
    }
    let server = MockFileStation::start();
    server.add_directory(ROOT);
    // Left on the NAS by an earlier version that carried them, with no local counterpart now.
    let stale = [
        ".~lock.old.odt#",
        "~DF1A2B.TMP",
        "stock.LACCDB",
        "legacy.ldb",
        "~$old.docx",
        "ehthumbs.db",
    ];
    for name in stale {
        server.add_file(&format!("{ROOT}/{name}"), b"stale lock", 1_700_000_000);
    }
    let password = fixture.write("password", PASSWORD);

    let environment = TestDir::new("default-excludes-environment");
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command
        .args([
            "--quiet",
            "--output",
            "json",
            "sync",
            source.to_str().unwrap(),
            ROOT,
            "--url",
            server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            password.to_str().unwrap(),
            "--no-vault",
            "--allow-http",
            "--delete",
            "--jobs",
            "1",
        ])
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
    let output = command.output().expect("run synology-drive-sync");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["result"]["uploaded"], 1, "{document}");
    assert_eq!(document["result"]["deleted"], 0, "{document}");
    assert_eq!(
        server.file_contents(&format!("{ROOT}/report.odt")),
        Some(b"the document".to_vec())
    );
    for name in [
        ".~lock.report.odt#",
        "~WRL0001.tmp",
        "orders.laccdb",
        "orders.ldb",
        "~$report.odt",
        "Thumbs.db",
        ".DS_Store",
    ] {
        assert_eq!(
            server.file_contents(&format!("{ROOT}/{name}")),
            None,
            "{name} is excluded, never uploaded"
        );
    }
    for name in stale {
        assert_eq!(
            server.file_contents(&format!("{ROOT}/{name}")),
            Some(b"stale lock".to_vec()),
            "{name} is excluded, so a mirror run never deletes it"
        );
    }
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Delete.delete"),
        "no deletion was even attempted"
    );
}
