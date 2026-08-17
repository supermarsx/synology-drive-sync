#![forbid(unsafe_code)]

//! An option that overrides a lower layer must stay reachable when that layer is the environment.
//!
//! Clap enforces `conflicts_with` against environment-sourced values and counts even
//! `SDSYNC_DELETE=false` as present, so declaring the negation flags as conflicting made every
//! shipped wrapper — each of which passes `--no-delete` next to an environment file that sets
//! `SDSYNC_DELETE` — abort with a usage error before doing any work. These tests drive the real
//! binary with the environment variable set and the negating flag passed, and assert on the
//! resulting behavior rather than on the exit status alone. Precedence itself lives in the
//! resolvers (`resolve_safety`, `resolve_output`, `resolve_authentication`) and in `credentials`.

/// Only the mock's listing and upload surface is exercised here; the rest belongs to the other
/// suites that share this module.
#[allow(dead_code)]
mod support;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::UNIX_EPOCH;

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";

/// Run the built binary with an isolated home, no ambient `SDSYNC_*`, and exactly the variables
/// under test. `stdin` is closed unless a payload is supplied, so nothing here can block.
fn run(variables: &[(&str, &str)], arguments: &[&str], stdin: Option<&[u8]>) -> Output {
    let environment = TestDir::new("env-precedence");
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
        .env("XDG_CONFIG_HOME", environment.child("xdg"))
        .env("NO_COLOR", "1");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SDSYNC_") {
            command.env_remove(name);
        }
    }
    for (name, value) in variables {
        command.env(name, value);
    }

    let Some(payload) = stdin else {
        command.stdin(Stdio::null());
        return command.output().expect("run synology-drive-sync");
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn synology-drive-sync");
    child
        .stdin
        .take()
        .expect("piped standard input")
        .write_all(payload)
        .expect("write the standard-input secret");
    child.wait_with_output().expect("run synology-drive-sync")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The regression itself: an environment-sourced value must never be reported as a conflicting
/// command-line argument, and must never turn into the usage/configuration exit code.
fn assert_no_environment_conflict(label: &str, output: &Output) {
    let stderr = stderr(output);
    assert!(
        !stderr.contains("cannot be used with"),
        "{label}: an environment-sourced value raised a command-line conflict:\n{stderr}"
    );
    assert_ne!(
        output.status.code(),
        Some(2),
        "{label}: exited with the usage/configuration code:\n{stderr}"
    );
}

fn assert_planned(label: &str, output: &Output) -> Value {
    assert_no_environment_conflict(label, output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{label}: plan did not succeed:\n{}",
        stderr(output)
    );
    serde_json::from_slice(&output.stdout).expect("plan stdout is one JSON document")
}

fn modified_seconds(path: &Path) -> i64 {
    i64::try_from(
        fs::metadata(path)
            .expect("fixture metadata")
            .modified()
            .expect("fixture modification time")
            .duration_since(UNIX_EPOCH)
            .expect("fixture predates the Unix epoch")
            .as_secs(),
    )
    .expect("fixture timestamp fits i64")
}

fn text(path: &Path) -> &str {
    path.to_str().expect("UTF-8 fixture path")
}

/// A source whose single file already matches the remote, plus one remote-only file that a
/// destructive plan would delete. The deletion count is the observable for `--no-delete`.
struct DeletionFixture {
    _directory: TestDir,
    server: MockFileStation,
    source: std::path::PathBuf,
    password: std::path::PathBuf,
}

impl DeletionFixture {
    fn new(label: &str) -> Self {
        let directory = TestDir::new(label);
        let source = directory.child("source");
        fs::create_dir(&source).expect("create fixture source");
        let keep = source.join("keep.txt");
        fs::write(&keep, b"keep").expect("write the retained source file");
        let password = directory.write("password", PASSWORD);

        let server = MockFileStation::start();
        server.add_directory("/team/export");
        server.add_file("/team/export/keep.txt", b"keep", modified_seconds(&keep));
        server.add_file("/team/export/stale.txt", b"stale", 1_700_000_000);

        Self {
            _directory: directory,
            server,
            source,
            password,
        }
    }

    fn plan(&self, variables: &[(&str, &str)], trailing: &[&str]) -> Output {
        let mut arguments = vec![
            "--output",
            "json",
            "plan",
            text(&self.source),
            "/team/export",
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            text(&self.password),
            "--no-vault",
            "--allow-http",
            "--compare",
            "metadata",
            "--max-delete",
            "5",
            "--jobs",
            "1",
        ];
        arguments.extend(trailing.iter().copied());
        run(variables, &arguments, None)
    }
}

#[test]
fn no_delete_overrides_a_falsey_and_a_truthy_delete_environment() {
    let fixture = DeletionFixture::new("no-delete-env");

    // Control: the environment variable really is being read, so the assertions below are not
    // passing merely because deletion was never selected in the first place.
    let enabled = fixture.plan(&[("SDSYNC_DELETE", "true")], &[]);
    let planned = assert_planned("SDSYNC_DELETE=true", &enabled);
    assert_eq!(planned["plan"]["summary"]["deletions"], 1);
    assert_eq!(
        planned["plan"]["actions"]["post_deletes"][0]["remote_path"],
        "/team/export/stale.txt"
    );

    // Both spellings failed before the fix: clap treated a falsey environment value as present.
    for value in ["false", "true"] {
        let output = fixture.plan(&[("SDSYNC_DELETE", value)], &["--no-delete"]);
        let label = format!("SDSYNC_DELETE={value} --no-delete");
        let planned = assert_planned(&label, &output);
        assert_eq!(
            planned["plan"]["summary"]["deletions"], 0,
            "{label}: --no-delete did not win over the environment"
        );
        assert!(
            planned["plan"]["actions"]["post_deletes"]
                .as_array()
                .expect("post_deletes is an array")
                .is_empty(),
            "{label}: a deletion survived --no-delete"
        );
    }
}

#[test]
fn no_quiet_re_enables_terminal_diagnostics_over_a_quiet_environment() {
    let fixture = DeletionFixture::new("no-quiet-env");

    let quiet = fixture.plan(&[("SDSYNC_QUIET", "true")], &[]);
    assert_planned("SDSYNC_QUIET=true", &quiet);
    assert!(
        quiet.stderr.is_empty(),
        "SDSYNC_QUIET=true left terminal diagnostics on: {}",
        stderr(&quiet)
    );

    let restored = fixture.plan(&[("SDSYNC_QUIET", "true")], &["--no-quiet"]);
    assert_planned("SDSYNC_QUIET=true --no-quiet", &restored);
    assert!(
        stderr(&restored).contains("INFO"),
        "--no-quiet did not restore terminal diagnostics over the environment: {}",
        stderr(&restored)
    );
}

#[test]
fn verbosity_raises_severity_under_a_quiet_environment_without_unsilencing_the_terminal() {
    // `--quiet` is a terminal sink policy and `-v` is a severity policy, so the two compose
    // instead of overriding each other. The durable log is the only place that difference shows.
    for (label, verbosity, expect_debug) in
        [("baseline", None, false), ("verbose", Some("-vv"), true)]
    {
        let directory = TestDir::new(&format!("verbose-quiet-env-{label}"));
        let source = directory.child("source");
        fs::create_dir(&source).expect("create fixture source");
        fs::write(source.join("payload.txt"), b"payload").expect("write the uploaded source file");
        let password = directory.write("password", PASSWORD);
        let log = directory.child("run.jsonl");

        let server = MockFileStation::start();
        server.add_directory("/team/export");

        let mut arguments = vec![
            "--output",
            "json",
            "--log-format",
            "json",
            "--log-file",
            text(&log),
        ];
        arguments.extend(verbosity);
        arguments.extend([
            "sync",
            text(&source),
            "/team/export",
            "--url",
            server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            text(&password),
            "--no-vault",
            "--allow-http",
            "--jobs",
            "1",
        ]);
        let output = run(&[("SDSYNC_QUIET", "true")], &arguments, None);

        assert_no_environment_conflict(label, &output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{label}: sync did not succeed:\n{}",
            stderr(&output)
        );
        assert!(
            output.stderr.is_empty(),
            "{label}: -vv must not defeat the quiet terminal policy: {}",
            stderr(&output)
        );

        let recorded = fs::read_to_string(&log).expect("the durable log survives a quiet run");
        assert!(
            !recorded.is_empty(),
            "{label}: quiet must not disable file logging"
        );
        assert_eq!(
            recorded.contains(r#""level":"debug""#),
            expect_debug,
            "{label}: unexpected severity in the durable log:\n{recorded}"
        );
    }
}

#[test]
fn vault_lookup_stays_selectable_over_a_no_vault_environment() {
    // The resolved `no-vault` value is not externally observable without writing to the real OS
    // credential store, so this asserts what can be checked offline on every platform: the
    // override is accepted next to the environment variable and the run reaches real work. The
    // precedence itself (`--vault` clears `no-vault`) is covered by the `resolve_authentication`
    // unit tests in `src/config.rs`.
    let fixture = DeletionFixture::new("vault-env");

    let output = fixture.plan(&[("SDSYNC_NO_VAULT", "true")], &["--vault"]);
    let planned = assert_planned("SDSYNC_NO_VAULT=true --vault", &output);
    assert_eq!(planned["schema"], "sdsync.plan.v1");
}

#[test]
fn password_stdin_overrides_a_password_file_environment() {
    let directory = TestDir::new("password-source-env");
    let source = directory.child("source");
    fs::create_dir(&source).expect("create fixture source");
    fs::write(source.join("payload.txt"), b"payload").expect("write the source file");
    let absent = directory.child("absent-password");

    let server = MockFileStation::start();
    server.add_directory("/team/export");

    let arguments = [
        "--quiet",
        "--output",
        "json",
        "plan",
        text(&source),
        "/team/export",
        "--url",
        server.base_url(),
        "--username",
        "e2e-user",
        "--no-vault",
        "--allow-http",
        "--jobs",
        "1",
    ];
    let variables = [("SDSYNC_PASSWORD_FILE", text(&absent))];

    // The environment names a file that does not exist, so reaching a plan at all proves the
    // command-line source displaced it rather than merely coexisting with it.
    let mut with_stdin = arguments.to_vec();
    with_stdin.push("--password-stdin");
    let output = run(&variables, &with_stdin, Some(PASSWORD));
    let planned = assert_planned("SDSYNC_PASSWORD_FILE + --password-stdin", &output);
    assert_eq!(planned["schema"], "sdsync.plan.v1");

    // Control: without the override the environment's file is used, and it cannot be read.
    let fallback = run(&variables, &arguments, None);
    assert_ne!(fallback.status.code(), Some(0));
    assert!(
        stderr(&fallback).contains("absent-password"),
        "the environment password file was not consulted: {}",
        stderr(&fallback)
    );
}

#[test]
fn a_command_line_remote_log_token_file_overrides_a_token_environment_variable() {
    let directory = TestDir::new("remote-log-token-env");
    let token = directory.write("token", b"collector-token\n");
    let source = directory.child("source");
    fs::create_dir(&source).expect("create fixture source");

    // An environment-variable *name* containing whitespace is rejected during resolution, so the
    // absence of that rejection proves the command-line file source displaced it.
    let variables = [("SDSYNC_REMOTE_LOG_TOKEN_ENV", "not a variable name")];
    let arguments = [
        "--quiet",
        "plan",
        text(&source),
        "/team/export",
        "--url",
        "https://files.example.invalid/reverse-proxy",
        "--username",
        "e2e-user",
        "--no-vault",
        "--connect-timeout",
        "1",
        "--retries",
        "0",
        "--remote-log-url",
        "https://collector.example.invalid/ingest",
    ];

    let mut with_file = arguments.to_vec();
    with_file.extend(["--remote-log-token-file", text(&token)]);
    let output = run(&variables, &with_file, None);
    assert_no_environment_conflict(
        "SDSYNC_REMOTE_LOG_TOKEN_ENV + --remote-log-token-file",
        &output,
    );

    // Control: left to itself the environment value is resolved, and rejected.
    let fallback = run(&variables, &arguments, None);
    assert_eq!(fallback.status.code(), Some(2));
    assert!(
        stderr(&fallback).contains("remote-log-token-env"),
        "the environment token source was not consulted: {}",
        stderr(&fallback)
    );
}

#[test]
fn credential_stdin_sources_stay_selectable_over_a_secret_file_environment() {
    let directory = TestDir::new("credentials-source-env");
    let absent = directory.child("absent-secret");
    let profile = [
        "--url",
        "https://files.example.invalid/reverse-proxy",
        "--username",
        "e2e-user",
    ];

    // Both actions point the environment at a file that does not exist, so nothing here can reach
    // the real OS credential store: the empty standard input fails first.
    for (variable, action, flag) in [
        ("SDSYNC_PASSWORD_FILE", "set-password", "--password-stdin"),
        ("SDSYNC_TOTP_SECRET_FILE", "set-totp", "--secret-stdin"),
    ] {
        let mut arguments = vec!["--quiet", "credentials", action];
        arguments.extend(profile);
        arguments.push(flag);
        let output = run(&[(variable, text(&absent))], &arguments, None);

        let label = format!("{variable} + {flag}");
        assert_no_environment_conflict(&label, &output);
        assert!(
            !stderr(&output).contains("absent-secret"),
            "{label}: the environment file displaced the command-line source: {}",
            stderr(&output)
        );
    }
}
