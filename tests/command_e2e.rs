#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const SECRET_MARKER: &str = "TOP-SECRET-COMMAND-E2E-MARKER";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    alpha_source: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sdsync-command-e2e-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        let alpha_source = root.join("sources").join("alpha");
        let beta_source = root.join("sources").join("beta");
        fs::create_dir_all(alpha_source.join("empty-directory"))
            .expect("create isolated alpha source");
        fs::create_dir_all(beta_source.join("nested")).expect("create isolated beta source");
        fs::write(alpha_source.join("alpha.txt"), b"alpha").expect("write alpha payload");
        fs::write(beta_source.join("beta.txt"), b"beta").expect("write beta payload");
        fs::write(beta_source.join("nested").join("second.txt"), b"second")
            .expect("write nested beta payload");

        let secrets = root.join("secrets");
        fs::create_dir_all(&secrets).expect("create isolated referenced-secret directory");
        fs::write(secrets.join("password.txt"), SECRET_MARKER)
            .expect("write a marker that command output must never disclose");

        for directory in ["home", "appdata", "local-appdata", "xdg"] {
            fs::create_dir_all(root.join(directory)).expect("create isolated environment root");
        }

        let config = root.join("config.toml");
        let contents = format!(
            r#"default-profile = "alpha"

[profiles.alpha]
source = {alpha_source}
remote = "/team/alpha"
url = "https://files.example.invalid/reverse-proxy"
username = "alpha-user"
password-file = "secrets/password.txt"
no-vault = true
compare = "content"
jobs = 2
excludes = ["*.ignored"]
delete = false
retries = 0
timeout = 30
connect-timeout = 2
progress = "never"
output = "human"

[profiles.beta]
source = {beta_source}
remote = "/team/beta"
url = "https://files.example.invalid/reverse-proxy"
username = "beta-user"
no-vault = true
compare = "metadata"
jobs = 1
delete = false
retries = 0
timeout = 30
connect-timeout = 2
progress = "never"
output = "human"
"#,
            alpha_source = toml_literal(&alpha_source),
            beta_source = toml_literal(&beta_source),
        );
        fs::write(&config, contents).expect("write isolated valid configuration");

        Self {
            root,
            config,
            alpha_source,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("APPDATA", self.root.join("appdata"))
            .env("LOCALAPPDATA", self.root.join("local-appdata"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("FORCE_COLOR");

        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("SDSYNC_")
            {
                command.env_remove(name);
            }
        }
        command
    }

    fn run(&self, arguments: &[&str]) -> Captured {
        let mut command = self.command();
        command.args(arguments);
        Captured::run(command)
    }

    fn run_with_config(&self, arguments: &[&str]) -> Captured {
        let mut command = self.command();
        command.arg("--config").arg(&self.config).args(arguments);
        Captured::run(command)
    }

    fn run_source(&self, source: &Path, trailing: &[&str]) -> Captured {
        let mut command = self.command();
        command
            .args(["doctor", "source"])
            .arg(source)
            .args(trailing);
        Captured::run(command)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.root) {
            eprintln!(
                "warning: failed to remove command-E2E fixture {:?}: {error}",
                self.root
            );
        }
    }
}

#[derive(Debug)]
struct Captured {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Captured {
    fn run(mut command: Command) -> Self {
        let output = command.output().expect("run real CLI subprocess");
        Self {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("CLI stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("CLI stderr is UTF-8"),
        }
    }

    fn assert_code(&self, expected: i32) {
        assert_eq!(
            self.code,
            Some(expected),
            "unexpected exit code\nstdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
        );
    }

    fn assert_clean_success(&self) {
        self.assert_code(0);
        assert!(
            self.stderr.is_empty(),
            "successful command wrote stderr: {}",
            self.stderr
        );
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout was not one JSON document: {error}\nstdout:\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }

    fn ndjson(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|error| panic!("invalid NDJSON line {line:?}: {error}"))
            })
            .collect()
    }
}

fn toml_literal(path: &Path) -> String {
    let value = path.to_string_lossy();
    assert!(
        !value.contains('\''),
        "fixture path must be TOML-literal safe"
    );
    format!("'{value}'")
}

fn assert_config_failure(output: &Captured, expected: &str) {
    output.assert_code(2);
    assert!(output.stdout.is_empty(), "failure leaked stdout");
    assert!(
        output.stderr.starts_with("error: "),
        "failure omitted stable error prefix: {}",
        output.stderr
    );
    assert!(
        output.stderr.contains(expected),
        "failure omitted {expected:?}: {}",
        output.stderr
    );
    assert!(!output.stderr.contains(SECRET_MARKER));
}

#[test]
fn config_path_has_stable_human_json_and_ndjson_contracts() {
    let fixture = Fixture::new();

    let human = fixture.run_with_config(&["config", "path", "--output", "human"]);
    human.assert_clean_success();
    assert_eq!(human.stdout, format!("{}\n", fixture.config.display()));

    let json = fixture.run_with_config(&["config", "path", "--output", "json"]);
    json.assert_clean_success();
    let value = json.json();
    assert_eq!(value["schema"], "sdsync.config-path.v1");
    assert_eq!(value["path"], fixture.config.to_string_lossy().as_ref());
    assert!(
        json.stdout.lines().count() > 2,
        "JSON should be pretty printed"
    );

    let ndjson = fixture.run_with_config(&["config", "path", "--output", "ndjson"]);
    ndjson.assert_clean_success();
    let lines = ndjson.ndjson();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], value);
}

#[test]
fn config_init_provisions_the_platform_path_and_refuses_to_clobber() {
    let fixture = Fixture::new();
    let target = fixture.root.join("fresh").join("config.toml");
    let target_text = target.to_str().expect("UTF-8 fixture path");

    let created = fixture.run(&["config", "init", "--config", target_text]);
    created.assert_clean_success();
    assert!(
        created
            .stdout
            .starts_with("Wrote the starter configuration at ")
    );
    assert_eq!(
        fs::read_to_string(&target).expect("the starter must exist"),
        include_str!("../config.example.toml"),
        "config init must write the documented example verbatim"
    );

    // The generated file is immediately usable by the rest of the config surface.
    let validated = fixture.run(&["config", "validate", "--config", target_text]);
    validated.assert_clean_success();
    assert!(validated.stdout.contains("Configuration is valid"));

    let refused = fixture.run(&["config", "init", "--config", target_text]);
    refused.assert_code(2);
    assert!(refused.stdout.is_empty(), "a refusal leaked stdout");
    assert!(
        refused.stderr.contains("pass --force to replace it"),
        "unhelpful refusal: {}",
        refused.stderr
    );

    let edited = "default-profile = \"mine\"\n";
    fs::write(&target, edited).expect("stand in for a configuration the user edited");
    fixture
        .run(&["config", "init", "--config", target_text])
        .assert_code(2);
    assert_eq!(
        fs::read_to_string(&target).expect("read the protected configuration"),
        edited,
        "a refused init must not touch the existing file"
    );

    let forced = fixture.run(&[
        "--output",
        "json",
        "config",
        "init",
        "--force",
        "--config",
        target_text,
    ]);
    forced.assert_clean_success();
    let value = forced.json();
    assert_eq!(value["schema"], "sdsync.config-init.v1");
    assert_eq!(value["replaced"], true);
    assert_eq!(value["path"], target.to_string_lossy().as_ref());

    // Without --config the starter lands on the isolated platform-default path.
    let default_path = fixture.run(&["config", "path"]);
    default_path.assert_clean_success();
    let expected = PathBuf::from(default_path.stdout.trim_end());
    assert!(
        !expected.exists(),
        "the fixture must start without a config"
    );
    let defaulted = fixture.run(&["--output", "ndjson", "config", "init"]);
    defaulted.assert_clean_success();
    let lines = defaulted.ndjson();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["schema"], "sdsync.config-init.v1");
    assert_eq!(lines[0]["replaced"], false);
    assert!(
        expected.is_file(),
        "config init must create missing parent directories at {expected:?}"
    );
}

#[test]
fn config_validate_has_stable_human_json_and_ndjson_contracts() {
    let fixture = Fixture::new();

    let human = fixture.run_with_config(&["config", "validate", "--output", "human"]);
    human.assert_clean_success();
    assert_eq!(
        human.stdout,
        "Configuration is valid: 2 profile(s); selected \"alpha\".\n"
    );

    let json = fixture.run_with_config(&["config", "validate", "--output", "json"]);
    json.assert_clean_success();
    let value = json.json();
    assert_eq!(value["schema"], "sdsync.config-validation.v1");
    assert_eq!(value["valid"], true);
    assert_eq!(value["profiles"], 2);
    assert_eq!(value["selected_profile"], "alpha");
    assert_eq!(value["path"], fixture.config.to_string_lossy().as_ref());

    let ndjson = fixture.run_with_config(&["config", "validate", "--output", "ndjson"]);
    ndjson.assert_clean_success();
    let lines = ndjson.ndjson();
    assert_eq!(lines, [value]);
}

#[test]
fn config_show_formats_effective_values_without_reading_secret_files() {
    let fixture = Fixture::new();

    let human = fixture.run_with_config(&["config", "show", "--output", "human"]);
    human.assert_clean_success();
    assert!(human.stdout.contains("profile = \"alpha\""));
    assert!(human.stdout.contains("jobs = 2"));
    assert!(human.stdout.contains("compare = \"content\""));
    assert!(!human.stdout.contains(SECRET_MARKER));

    let json = fixture.run_with_config(&["config", "show", "--output", "json"]);
    json.assert_clean_success();
    let value = json.json();
    assert_eq!(value["profile"], "alpha");
    assert_eq!(value["jobs"], 2);
    assert_eq!(value["compare"], "content");
    assert_eq!(value["output"], "human");
    assert!(value.get("password").is_none());
    assert!(value.get("totp-secret").is_none());
    assert!(!json.stdout.contains(SECRET_MARKER));

    let ndjson = fixture.run_with_config(&["config", "show", "--output", "ndjson"]);
    ndjson.assert_clean_success();
    assert_eq!(ndjson.ndjson(), [value]);
    assert!(!ndjson.stdout.contains(SECRET_MARKER));
}

#[test]
fn every_completion_shell_emits_its_native_command_tree() {
    let fixture = Fixture::new();
    let cases = [
        ("bash", "_synology-drive-sync()"),
        ("zsh", "#compdef synology-drive-sync"),
        ("fish", "complete -c synology-drive-sync"),
        ("powershell", "Register-ArgumentCompleter"),
        (
            "elvish",
            "edit:completion:arg-completer[synology-drive-sync]",
        ),
    ];

    for (shell, signature) in cases {
        let output = fixture.run(&["completions", shell]);
        output.assert_clean_success();
        assert!(
            output.stdout.contains(signature),
            "{shell} completion omitted native signature {signature:?}"
        );
        for command in ["sync", "plan", "doctor", "config", "credentials"] {
            assert!(
                output.stdout.contains(command),
                "{shell} completion omitted {command}"
            );
        }
    }
}

#[test]
fn recursive_manpage_generation_preserves_unrelated_files_and_fails_on_a_file_path() {
    let fixture = Fixture::new();
    let directory = fixture.root.join("manual");
    fs::create_dir_all(&directory).expect("create existing manpage directory");
    let sentinel = directory.join("keep.txt");
    fs::write(&sentinel, "unrelated").expect("write unrelated manpage-directory file");

    let mut command = fixture.command();
    command.args(["manpage", "--all"]).arg(&directory);
    let generated = Captured::run(command);
    generated.assert_clean_success();
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unrelated");
    let root_page = fs::read_to_string(directory.join("synology-drive-sync.1"))
        .expect("root manpage was generated");
    let nested_page = fs::read_to_string(directory.join("synology-drive-sync-config-validate.1"))
        .expect("nested config manpage was generated");
    let generated_pages = fs::read_dir(&directory)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|value| value == "1"))
        .count();
    assert_eq!(generated_pages, 18);
    assert!(root_page.contains(".SH SUBCOMMANDS"));
    assert!(nested_page.contains(".SH NAME"));
    assert!(nested_page.contains(".SH SYNOPSIS"));

    let file_path = fixture.root.join("not-a-directory");
    fs::write(&file_path, "sentinel").expect("create manpage output collision");
    let mut command = fixture.command();
    command.args(["manpage", "--all"]).arg(&file_path);
    let rejected = Captured::run(command);
    rejected.assert_code(1);
    assert!(rejected.stdout.is_empty());
    assert!(
        rejected
            .stderr
            .contains("failed to create manpage output directory")
    );
    assert_eq!(fs::read_to_string(file_path).unwrap(), "sentinel");
}

#[test]
fn source_doctor_single_formats_real_inventory_and_uses_operational_exit_one() {
    let fixture = Fixture::new();

    let human = fixture.run_source(
        &fixture.alpha_source,
        &["--hash", "--output", "human", "--log-level", "off"],
    );
    human.assert_clean_success();
    assert!(
        human
            .stdout
            .contains("Source is healthy: 1 files, 1 directories")
    );
    assert!(human.stdout.contains("5 B across 2 entries"));
    assert!(human.stdout.contains("1 files hashed"));

    let json = fixture.run_source(
        &fixture.alpha_source,
        &["--hash", "--output", "json", "--log-level", "off"],
    );
    json.assert_clean_success();
    let value = json.json();
    assert_eq!(value["schema"], "sdsync.source-doctor.v1");
    assert_eq!(value["source"]["files"], 1);
    assert_eq!(value["source"]["directories"], 1);
    assert_eq!(value["source"]["entries"], 2);
    assert_eq!(value["source"]["bytes"], 5);
    assert_eq!(value["source"]["content_hashed"], true);
    assert_eq!(value["source"]["hashed_files"], 1);
    assert_eq!(
        PathBuf::from(value["source"]["canonical_source"].as_str().unwrap()),
        fs::canonicalize(&fixture.alpha_source).unwrap()
    );

    let ndjson = fixture.run_source(
        &fixture.alpha_source,
        &["--hash", "--output", "ndjson", "--log-level", "off"],
    );
    ndjson.assert_clean_success();
    assert_eq!(ndjson.ndjson().len(), 1);
    assert_eq!(ndjson.ndjson()[0]["source"]["bytes"], 5);

    let missing = fixture.run_source(
        &fixture.root.join("missing-source"),
        &["--output", "json", "--log-level", "off"],
    );
    missing.assert_code(1);
    assert!(missing.stdout.is_empty());
    assert!(missing.stderr.starts_with("error: failed to read"));
    assert!(missing.stderr.contains("missing-source"));
    assert!(!missing.stderr.contains(SECRET_MARKER));
}

#[test]
fn source_doctor_batch_formats_sorted_per_profile_results() {
    let fixture = Fixture::new();

    let human = fixture.run_with_config(&[
        "doctor",
        "--profiles",
        "beta,alpha",
        "source",
        "--output",
        "human",
        "--log-level",
        "off",
    ]);
    human.assert_clean_success();
    assert!(
        human
            .stdout
            .starts_with("Source diagnostic batch: 2 succeeded, 0 failed, 0 not run.\n")
    );
    let alpha_position = human.stdout.find("[alpha] healthy").unwrap();
    let beta_position = human.stdout.find("[beta] healthy").unwrap();
    assert!(
        alpha_position < beta_position,
        "batch output must be sorted"
    );

    let json = fixture.run_with_config(&[
        "doctor",
        "--profiles",
        "beta,alpha",
        "source",
        "--output",
        "json",
        "--log-level",
        "off",
    ]);
    json.assert_clean_success();
    let value = json.json();
    assert_eq!(value["schema"], "sdsync.source-doctor-batch.v1");
    assert_eq!(value["status"], "success");
    assert_eq!(value["summary"]["jobs"], 2);
    assert_eq!(value["summary"]["succeeded"], 2);
    assert_eq!(value["jobs"][0]["profile"], "alpha");
    assert_eq!(value["jobs"][1]["profile"], "beta");
    assert_eq!(value["jobs"][0]["source"]["files"], 1);
    assert_eq!(value["jobs"][1]["source"]["files"], 2);

    let ndjson = fixture.run_with_config(&[
        "doctor",
        "--profiles",
        "beta,alpha",
        "source",
        "--output",
        "ndjson",
        "--log-level",
        "off",
    ]);
    ndjson.assert_clean_success();
    let lines = ndjson.ndjson();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["schema"], "sdsync.source-doctor-job.v1");
    assert_eq!(lines[0]["profile"], "alpha");
    assert_eq!(lines[1]["profile"], "beta");
    assert_eq!(lines[2]["schema"], "sdsync.source-doctor-batch.v1");
    assert_eq!(lines[2]["status"], "success");
}

#[test]
fn early_configuration_batch_and_redaction_failures_never_contact_a_server() {
    let fixture = Fixture::new();

    let invalid_config = fixture.root.join("invalid-secret-bearing.toml");
    fs::write(
        &invalid_config,
        format!(
            "default-profile = \"broken\"\n[profiles.broken]\npassword = \"{SECRET_MARKER}\"\n"
        ),
    )
    .expect("write intentionally invalid secret-bearing config");
    let mut command = fixture.command();
    command
        .arg("--config")
        .arg(&invalid_config)
        .args(["config", "validate"]);
    assert_config_failure(
        &Captured::run(command),
        "failed to parse non-secret configuration",
    );

    let missing_config = fixture.root.join("missing-config.toml");
    let mut command = fixture.command();
    command
        .arg("--config")
        .arg(&missing_config)
        .args(["config", "show"]);
    assert_config_failure(&Captured::run(command), "failed to read configuration");

    assert_config_failure(&fixture.run(&["plan"]), "SOURCE is required");

    let mut command = fixture.command();
    command.arg("sync").arg(&fixture.alpha_source).args([
        "--url",
        "https://files.example.invalid",
        "--username",
        "tester",
        "--no-vault",
    ]);
    assert_config_failure(&Captured::run(command), "REMOTE is required");

    assert_config_failure(
        &fixture.run(&["doctor", "--routing-only"]),
        "--url is required",
    );
    assert_config_failure(
        &fixture.run(&[
            "doctor",
            "--url",
            "https://files.example.invalid",
            "--username",
            "tester",
            "--no-vault",
            "target",
            "--write-test",
        ]),
        "doctor target --write-test requires REMOTE",
    );

    let mut command = fixture.command();
    command
        .args(["doctor", "--routing-only", "source"])
        .arg(&fixture.alpha_source);
    assert_config_failure(
        &Captured::run(command),
        "doctor source cannot be combined with --routing-only",
    );

    let mut command = fixture.command();
    command
        .args(["doctor", "--level", "quick", "source"])
        .arg(&fixture.alpha_source);
    assert_config_failure(
        &Captured::run(command),
        "doctor source cannot be combined with --level",
    );

    let sensitive_url = "https://private-user:private-password@example.invalid";
    let redacted = fixture.run(&["doctor", "--routing-only", "--url", sensitive_url]);
    assert_config_failure(
        &redacted,
        "absolute HTTPS reverse-proxy URL without credentials",
    );
    assert!(!redacted.stderr.contains("private-user"));
    assert!(!redacted.stderr.contains("private-password"));

    assert_config_failure(
        &fixture.run(&["sync", "--max-total-delete", "4"]),
        "--max-total-delete requires --profiles or --all-profiles",
    );
    assert_config_failure(
        &fixture.run_with_config(&["plan", "--profiles", "alpha,alpha"]),
        "was selected more than once",
    );

    let mut command = fixture.command();
    command
        .arg("--config")
        .arg(&fixture.config)
        .args(["sync", "--profiles", "alpha,beta"])
        .arg(&fixture.alpha_source)
        .arg("/team/override");
    assert_config_failure(
        &Captured::run(command),
        "positional overrides are not allowed",
    );

    let mut command = fixture.command();
    command
        .arg("--config")
        .arg(&fixture.config)
        .args(["doctor", "--profiles", "alpha,beta", "source"])
        .arg(&fixture.alpha_source);
    assert_config_failure(
        &Captured::run(command),
        "doctor source batch jobs must take SOURCE from each selected profile",
    );

    assert_config_failure(
        &fixture.run_with_config(&["doctor", "--all-profiles", "target", "/team/override"]),
        "doctor target batch jobs must take REMOTE from each selected profile",
    );
    assert_config_failure(
        &fixture.run_with_config(&["--profile", "alpha", "plan", "--profiles", "beta"]),
        "--profile cannot be combined with --profiles or --all-profiles",
    );
    assert_config_failure(
        &fixture.run(&["doctor", "--profiles", "alpha", "source"]),
        "--profiles and --all-profiles require an existing configuration file",
    );
    assert_config_failure(
        &fixture.run_with_config(&[
            "doctor",
            "--profiles",
            "alpha,beta",
            "--max-total-delete",
            "1",
            "source",
        ]),
        "--max-total-delete applies only to batch plan and sync",
    );
}
