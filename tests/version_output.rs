use std::process::Command;

#[test]
fn version_stdout_matches_the_release_probe_contract_exactly() {
    let output = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"))
        .arg("--version")
        .output()
        .expect("run the packaged binary's version command");

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("synology-drive-sync {}\n", env!("SDSYNC_VERSION")).as_bytes()
    );
    assert!(output.stderr.is_empty());
}
