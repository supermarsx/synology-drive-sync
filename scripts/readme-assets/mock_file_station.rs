//! Serves the in-tree File Station mock (tests/support/file_station_mock.rs) on a loopback port
//! until a stop file appears, so the real CLI binary can be pointed at it by capture-cli.sh.
//!
//! This file is NOT part of the normal test suite. capture-cli.sh copies it into a scratch copy
//! of the repository as tests/readme_mock_file_station.rs, so nothing here runs in CI and the
//! repository's tests/ directory is never modified.
//!
//! Environment: README_MOCK_URL_FILE receives the base URL; README_MOCK_STOP_FILE ends the run.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use support::file_station_mock::MockFileStation;

#[test]
#[ignore = "manual: started by scripts/readme-assets/capture-cli.sh"]
fn serve_demo_file_station() {
    let url_file =
        PathBuf::from(std::env::var("README_MOCK_URL_FILE").expect("README_MOCK_URL_FILE"));
    let stop_file =
        PathBuf::from(std::env::var("README_MOCK_STOP_FILE").expect("README_MOCK_STOP_FILE"));

    let server = MockFileStation::start();
    for directory in [
        "/home",
        "/home/Backup",
        "/home/Backup/documents",
        "/home/Backup/documents/reports",
    ] {
        server.add_directory(directory);
    }
    // Content the NAS already holds, so a run has something to compare against.
    let earlier = 1_767_225_600; // 2026-01-01T00:00:00Z
    server.add_file(
        "/home/Backup/documents/handbook.pdf",
        &vec![b'H'; 48 * 1024],
        earlier,
    );
    server.add_file(
        "/home/Backup/documents/reports/q1-summary.pdf",
        &vec![b'1'; 21 * 1024],
        earlier,
    );

    fs::write(&url_file, server.base_url()).expect("publish mock URL");
    while !stop_file.exists() {
        sleep(Duration::from_millis(100));
    }
}
