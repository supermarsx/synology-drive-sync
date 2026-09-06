use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn run(arguments: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command.args(arguments);
    // The ambient environment must not steer the binary: `SDSYNC_*` names are the documented
    // configuration surface, and a leaked one would silently change what is being tested.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SDSYNC_") {
            command.env_remove(name);
        }
    }
    command.output().expect("run the packaged binary")
}

/// A throwaway local tree for the one subcommand that logs without touching a network or a vault.
fn local_source(label: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("sdsync-banner-{label}"));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("create the fixture source directory");
    fs::write(directory.join("payload.txt"), b"banner fixture\n").expect("write a payload file");
    directory
}

/// Run a command that builds a logger.
///
/// `config` and `completions` deliberately emit no structured events at all, so they are not
/// evidence either way; a local source diagnostic is the cheapest command that does log.
fn run_logging(label: &str, arguments: &[&str]) -> std::process::Output {
    let source = local_source(label);
    let mut full = arguments.to_vec();
    full.extend(["doctor", "source"]);
    let source_argument = source.to_str().expect("UTF-8 fixture path").to_owned();
    full.push(&source_argument);
    run(&full)
}

#[test]
fn version_stdout_matches_the_release_probe_contract_exactly() {
    let output = run(&["--version"]);

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("synology-drive-sync {}\n", env!("SDSYNC_VERSION")).as_bytes()
    );
    // The build banner is a log record, so it must never reach the release probe's channels.
    assert!(output.stderr.is_empty());
}

/// Every run identifies the build that produced it, on the log channel.
///
/// A pasted log or a rotated log file is only actionable if it names its own version, and the
/// banner is emitted where every subcommand builds its logger rather than per command.
#[test]
fn every_logged_run_opens_with_the_build_banner() {
    let output = run_logging("human", &["--log-level", "info"]);
    assert!(output.status.success(), "{output:?}");

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
    let banner = stderr
        .lines()
        .find(|line| line.contains("INFO  build"))
        .unwrap_or_else(|| panic!("no build banner in:\n{stderr}"));
    for expected in [
        "synology-drive-sync",
        env!("SDSYNC_VERSION"),
        env!("SDSYNC_BUILD_TARGET"),
        env!("SDSYNC_BUILD_PROFILE"),
        env!("SDSYNC_BUILD_COMMIT"),
    ] {
        assert!(
            banner.contains(expected),
            "banner {banner:?} omits {expected:?}"
        );
    }
    // The banner is diagnostics, never command output.
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synology-drive-sync 0"));
}

#[test]
fn the_build_banner_is_json_when_logs_are_json() {
    let output = run_logging("json", &["--log-format", "json"]);
    assert!(output.status.success(), "{output:?}");

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
    let record = stderr
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("one record per line"))
        .find(|record| record["event"] == "run.build")
        .unwrap_or_else(|| panic!("no run.build record in:\n{stderr}"));

    assert_eq!(record["schema"], "sdsync.log.v1");
    assert_eq!(record["level"], "info");
    assert_eq!(record["build"]["name"], "synology-drive-sync");
    assert_eq!(record["build"]["version"], env!("SDSYNC_VERSION"));
    assert_eq!(record["build"]["target"], env!("SDSYNC_BUILD_TARGET"));
}

/// `--log-level off` disables structured emission, and the banner is not an exception.
#[test]
fn the_build_banner_respects_log_level_off() {
    let output = run_logging("off", &["--log-level", "off"]);
    assert!(output.status.success(), "{output:?}");

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
    assert!(
        !stderr.contains("build"),
        "log-level off still emitted a banner:\n{stderr}"
    );
}
