// The shared mock exposes one API surface for the whole suite; this binary drives only the
// slice it needs to reach every schema-governed emitter offline.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};
use support::TestDir;
use support::file_station_mock::MockFileStation;

const SCHEMA_TEXT: &str = include_str!("../docs/observability.schema.json");
const DIGEST: &str = "d41d8cd98f00b204e9800998ecf8427e";

fn validate(root: &Value, schema: &Value, instance: &Value, path: &str) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| format!("unsupported non-local reference {reference:?}"))?;
        let target = root
            .pointer(pointer)
            .ok_or_else(|| format!("schema reference {reference:?} does not resolve"))?;
        return validate(root, target, instance, path);
    }

    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let outcomes = branches
            .iter()
            .map(|branch| validate(root, branch, instance, path))
            .collect::<Vec<_>>();
        let matches = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
        return if matches == 1 {
            Ok(())
        } else {
            Err(format!(
                "{path}: expected exactly one oneOf branch, matched {matches}; {outcomes:?}"
            ))
        };
    }

    if let Some(expected) = schema.get("const")
        && expected != instance
    {
        return Err(format!(
            "{path}: expected constant {expected}, got {instance}"
        ));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(instance)
    {
        return Err(format!("{path}: {instance} is not in enum {allowed:?}"));
    }

    if let Some(expected_type) = schema.get("type").and_then(Value::as_str) {
        let matches = match expected_type {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
            "number" => instance.is_number(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            other => return Err(format!("{path}: unsupported schema type {other:?}")),
        };
        if !matches {
            return Err(format!(
                "{path}: expected type {expected_type}, got {instance}"
            ));
        }
    }

    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && instance.as_f64().is_some_and(|value| value < minimum)
    {
        return Err(format!("{path}: number is below minimum {minimum}"));
    }
    if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
        && instance
            .as_str()
            .is_some_and(|value| value.chars().count() < minimum as usize)
    {
        return Err(format!("{path}: string is shorter than {minimum}"));
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
        let value = instance
            .as_str()
            .ok_or_else(|| format!("{path}: pattern applies to a non-string"))?;
        let matches = match pattern {
            "^/" => value.starts_with('/'),
            "^[0-9a-f]{32}$" => {
                value.len() == 32
                    && value
                        .as_bytes()
                        .iter()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            }
            other => return Err(format!("{path}: unsupported schema pattern {other:?}")),
        };
        if !matches {
            return Err(format!("{path}: {value:?} does not match {pattern:?}"));
        }
    }

    if let Some(object) = instance.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!("{path}: missing required property {key:?}"));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            for key in object.keys() {
                if !properties.is_some_and(|known| known.contains_key(key)) {
                    return Err(format!("{path}: unexpected property {key:?}"));
                }
            }
        }
        if let Some(properties) = properties {
            for (key, child_schema) in properties {
                if let Some(child) = object.get(key) {
                    validate(root, child_schema, child, &format!("{path}/{key}"))?;
                }
            }
        }
    }

    if let Some(array) = instance.as_array()
        && let Some(item_schema) = schema.get("items")
    {
        for (index, item) in array.iter().enumerate() {
            validate(root, item_schema, item, &format!("{path}/{index}"))?;
        }
    }
    Ok(())
}

fn snapshot(kind: &str, size: u64, digest: Option<&str>, require_mtime: bool) -> Value {
    json!({
        "entry_kind": kind,
        "size": size,
        "mtime_seconds": 1_785_769_200,
        "content_md5": digest,
        "require_mtime": require_mtime,
    })
}

fn destination_guard() -> Value {
    json!({
        "remote_path": "/team/export/new/report.bin",
        "local_relative": "new/report.bin",
        "expected_size": 4096,
        "expected_mtime_seconds": 1_785_769_200,
        "content_md5": DIGEST,
    })
}

fn copy_fields() -> Value {
    json!({
        "from_relative": "old/report.bin",
        "from_remote_path": "/team/export/old/report.bin",
        "to_relative": "new/report.bin",
        "to_remote_path": "/team/export/new/report.bin",
        "expected_size": 4096,
        "expected_mtime_seconds": 1_785_769_200,
        "content_md5": DIGEST,
        "source_snapshot_guard": snapshot("file", 4096, Some(DIGEST), true),
        "verified_upload_fallback": "only-before-copy-task-start",
    })
}

fn representative_plan() -> Value {
    json!({
        "summary": {
            "uploads": 1,
            "upload_bytes": 8192,
            "server_copy_fallback_bytes": 4096,
            "server_copies": 1,
            "directories": 1,
            "deletions": 2,
            "unchanged_files": 3,
            "protected_entries": 0,
            "changes": true,
        },
        "actions": {
            "pre_deletes": [{
                "relative": "conflict",
                "remote_path": "/team/export/conflict",
                "entry_kind": "directory",
                "type_conflict": true,
                "snapshot_guard": snapshot("directory", 0, None, false),
            }],
            "creates": [{
                "relative": "",
                "remote_path": "/team/export",
                "reason": "missing-remote",
            }],
            "copies": [copy_fields()],
            "uploads": [{
                "relative": "payload.bin",
                "remote_path": "/team/export/payload.bin",
                "bytes": 8192,
                "mtime_ms": 1_785_769_200_000_u64,
                "reason": "content-differs",
            }],
            "post_deletes": [{
                "relative": "old/report.bin",
                "remote_path": "/team/export/old/report.bin",
                "entry_kind": "file",
                "type_conflict": false,
                "snapshot_guard": snapshot("file", 4096, Some(DIGEST), true),
                "destination_guard": destination_guard(),
            }],
        },
    })
}

fn assert_valid(schema: &Value, record: &Value) {
    if let Err(error) = validate(schema, schema, record, "$") {
        panic!("record did not validate against observability.schema.json: {error}\n{record}");
    }
}

#[test]
fn strict_schema_accepts_runtime_server_copy_and_guard_shapes() {
    let schema: Value = serde_json::from_str(SCHEMA_TEXT).expect("schema must be valid JSON");
    let plan = representative_plan();

    assert_valid(
        &schema,
        &json!({"schema": "sdsync.plan.v1", "plan": plan.clone()}),
    );
    let mut plan_without_copy_destination_guard = representative_plan();
    plan_without_copy_destination_guard["actions"]["post_deletes"][0]["destination_guard"] =
        Value::Null;
    assert_valid(
        &schema,
        &json!({
            "schema": "sdsync.plan.v1",
            "plan": plan_without_copy_destination_guard,
        }),
    );
    assert_valid(
        &schema,
        &json!({
            "schema": "sdsync.sync.v1",
            "plan": plan,
            "result": {
                "changed": true,
                "uploaded": 1,
                "server_copied": 1,
                "upload_bytes": 8192,
                "directories_created": 1,
                "deleted": 2,
                "elapsed_ms": 42,
            },
        }),
    );

    for record in [
        json!({
            "schema": "sdsync.plan.v1",
            "kind": "summary",
            "uploads": 1,
            "upload_bytes": 8192,
            "server_copy_fallback_bytes": 4096,
            "server_copies": 1,
            "directories": 1,
            "deletions": 2,
            "unchanged_files": 3,
            "protected_entries": 0,
            "changes": true,
        }),
        json!({
            "schema": "sdsync.plan-action.v1",
            "action": "delete-conflict",
            "relative": "conflict",
            "remote_path": "/team/export/conflict",
            "entry_kind": "directory",
            "snapshot_guard": snapshot("directory", 0, None, false),
        }),
        json!({
            "schema": "sdsync.plan-action.v1",
            "action": "create-directory",
            "relative": "",
            "remote_path": "/team/export",
            "reason": "missing-remote",
        }),
        {
            let mut record = copy_fields();
            record["schema"] = json!("sdsync.plan-action.v1");
            record["action"] = json!("copy-remote-content");
            record
        },
        json!({
            "schema": "sdsync.plan-action.v1",
            "action": "upload",
            "relative": "payload.bin",
            "remote_path": "/team/export/payload.bin",
            "bytes": 8192,
            "mtime_ms": 1_785_769_200_000_u64,
            "reason": "content-differs",
        }),
        json!({
            "schema": "sdsync.plan-action.v1",
            "action": "delete",
            "relative": "old/report.bin",
            "remote_path": "/team/export/old/report.bin",
            "entry_kind": "file",
            "snapshot_guard": snapshot("file", 4096, Some(DIGEST), true),
            "destination_guard": destination_guard(),
        }),
        json!({
            "schema": "sdsync.output.v1",
            "kind": "completion",
            "result": {
                "changed": true,
                "uploaded": 1,
                "server_copied": 1,
                "upload_bytes": 8192,
                "directories_created": 1,
                "deleted": 2,
                "elapsed_ms": 42,
            },
        }),
    ] {
        assert_valid(&schema, &record);
    }
}

#[test]
fn strict_schema_rejects_missing_or_unknown_server_copy_fields() {
    let schema: Value = serde_json::from_str(SCHEMA_TEXT).expect("schema must be valid JSON");
    let mut missing = json!({"schema": "sdsync.plan.v1", "plan": representative_plan()});
    missing["plan"]["summary"]
        .as_object_mut()
        .unwrap()
        .remove("server_copies");
    assert!(validate(&schema, &schema, &missing, "$").is_err());

    let mut unknown = json!({"schema": "sdsync.plan.v1", "plan": representative_plan()});
    unknown["plan"]["summary"]["server_copy_mode"] = json!("implicit");
    assert!(validate(&schema, &schema, &unknown, "$").is_err());
}

#[test]
fn strict_schema_requires_a_known_change_reason_on_creates_and_uploads() {
    let schema: Value = serde_json::from_str(SCHEMA_TEXT).expect("schema must be valid JSON");

    for reason in [
        "missing-remote",
        "size-differs",
        "mtime-differs",
        "content-differs",
        "type-replaced",
    ] {
        let mut plan = representative_plan();
        plan["actions"]["uploads"][0]["reason"] = json!(reason);
        plan["actions"]["creates"][0]["reason"] = json!(reason);
        assert_valid(&schema, &json!({"schema": "sdsync.plan.v1", "plan": plan}));
    }

    let mut invented = json!({"schema": "sdsync.plan.v1", "plan": representative_plan()});
    invented["plan"]["actions"]["uploads"][0]["reason"] = json!("vibes-differ");
    assert!(validate(&schema, &schema, &invented, "$").is_err());

    let mut missing = json!({"schema": "sdsync.plan.v1", "plan": representative_plan()});
    missing["plan"]["actions"]["creates"][0]
        .as_object_mut()
        .unwrap()
        .remove("reason");
    assert!(validate(&schema, &schema, &missing, "$").is_err());
}

// ---------------------------------------------------------------------------
// Real-output conformance.
//
// Everything above validates hand-written fixtures, which certifies only that the schema
// accepts what a test author believed the CLI emits. The tests below spawn the real binary
// against the hand-rolled File Station mock and feed its actual stdout and stderr through the
// same shipped schema customers validate against, so production emitters and
// `docs/observability.schema.json` cannot drift apart in either direction unnoticed.
// ---------------------------------------------------------------------------

const MOCK_ACCOUNT: &str = "e2e-user";
const MOCK_PASSWORD: &[u8] = b"correct horse battery staple\n";

fn parsed_schema() -> Value {
    serde_json::from_str(SCHEMA_TEXT).expect("schema must be valid JSON")
}

/// Fail with the verbatim validator error, the offending record, and the command that
/// produced it: a drift report is only actionable if it names all three.
fn assert_real_output_valid(schema: &Value, record: &Value, source: &str) {
    if let Err(error) = validate(schema, schema, record, "$") {
        panic!(
            "real CLI output from `{source}` violates the shipped docs/observability.schema.json\n\
             validator error: {error}\n\
             record: {record}\n\
             Fix the emitter or the schema deliberately; never relax the schema to silence this."
        );
    }
}

fn json_document(stdout: &str, source: &str) -> Value {
    serde_json::from_str(stdout).unwrap_or_else(|error| {
        panic!("`{source}` stdout was not one JSON document: {error}\nstdout:\n{stdout}")
    })
}

fn json_records(stream: &str, source: &str) -> Vec<Value> {
    stream
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("`{source}` emitted a non-JSON record {line:?}: {error}")
            })
        })
        .collect()
}

/// Standard error interleaves human diagnostics (the plain-HTTP warning, for one) with the
/// structured stream, so keep only record lines — after proving no structured record hides in
/// what the filter drops.
fn structured_records(stream: &str, source: &str) -> Vec<Value> {
    let (records, diagnostics): (Vec<_>, Vec<_>) = stream
        .lines()
        .filter(|line| !line.trim().is_empty())
        .partition(|line| line.starts_with('{'));
    for line in diagnostics {
        assert!(
            !line.contains("sdsync."),
            "`{source}` emitted a structured record this filter would skip: {line:?}"
        );
    }
    json_records(&records.join("\n"), source)
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

/// Spawn the real binary with every ambient influence removed: a redirected home, config, and
/// cache root per platform, no colour, and no inherited `SDSYNC_*` variable. Output shape must
/// depend only on the arguments, otherwise a conformance sample proves nothing.
fn run_cli(environment: &Path, arguments: &[&str]) -> (String, String) {
    run_cli_expecting(environment, arguments, 0)
}

fn run_cli_expecting(environment: &Path, arguments: &[&str], expected: i32) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command
        .args(arguments)
        .stdin(Stdio::null())
        .env("HOME", environment.join("home"))
        .env("USERPROFILE", environment.join("home"))
        .env("APPDATA", environment.join("appdata"))
        .env("LOCALAPPDATA", environment.join("local-appdata"))
        .env("XDG_CONFIG_HOME", environment.join("xdg"))
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

    let output = command.output().expect("run the real CLI subprocess");
    let stdout = String::from_utf8(output.stdout).expect("CLI stdout is UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("CLI stderr is UTF-8");
    assert_eq!(
        output.status.code(),
        Some(expected),
        "the CLI took an unexpected exit path, so its output is not the intended conformance \
         sample: {arguments:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    (stdout, stderr)
}

fn hermetic_environment(fixture: &TestDir) -> &Path {
    for directory in ["home", "appdata", "local-appdata", "xdg"] {
        fs::create_dir_all(fixture.child(directory)).expect("create isolated environment root");
    }
    fixture.path()
}

struct Scenario {
    fixture: TestDir,
    server: MockFileStation,
    source: String,
    remote: String,
    password: String,
    retries: String,
}

impl Scenario {
    /// One offline mirror that provokes every plan action kind at once: a remote directory
    /// standing where a file belongs, a remote file standing where a directory belongs, a
    /// cross-parent rename the planner can satisfy with a verified server copy, a stale remote
    /// subtree, and one already-matching file.
    fn every_plan_action(label: &str) -> Self {
        let fixture = TestDir::new(label);
        let server = MockFileStation::start();
        let remote = "/team/conformance";

        let keep = fixture.write("source/keep.txt", b"keep");
        fixture.write("source/node", b"replacement");
        fixture.write("source/folder/new.txt", b"new");
        let renamed = fixture.write("source/new/item.bin", b"copy-me-on-nas");

        server.add_directory(remote);
        server.add_file(
            &format!("{remote}/keep.txt"),
            b"keep",
            modified_seconds(&keep),
        );
        server.add_directory(&format!("{remote}/node"));
        server.add_directory(&format!("{remote}/node/child"));
        server.add_file(
            &format!("{remote}/node/child/old.bin"),
            b"old",
            1_700_000_000,
        );
        server.add_file(&format!("{remote}/folder"), b"old-file", 1_700_000_001);
        server.add_directory(&format!("{remote}/old"));
        server.add_file(
            &format!("{remote}/old/item.bin"),
            b"copy-me-on-nas",
            modified_seconds(&renamed),
        );
        server.add_directory(&format!("{remote}/stale"));
        server.add_file(&format!("{remote}/stale/gone.bin"), b"stale", 1_700_000_002);

        Self::assemble(fixture, server, remote)
    }

    /// A purely additive mirror. Progress rendering is suppressed whenever command output is
    /// machine readable, so the progress sample needs its own run with real work to do; running
    /// it twice also reaches both the executed and the unchanged sync-document branches.
    fn additive(label: &str) -> Self {
        let fixture = TestDir::new(label);
        let server = MockFileStation::start();
        let remote = "/team/progress";

        fixture.write("source/one.txt", b"first payload");
        fixture.write("source/nested/two.bin", b"second payload");
        server.add_directory(remote);

        Self::assemble(fixture, server, remote)
    }

    fn assemble(fixture: TestDir, server: MockFileStation, remote: &str) -> Self {
        hermetic_environment(&fixture);
        let password = fixture.write("password", MOCK_PASSWORD);
        let source = fixture
            .child("source")
            .to_str()
            .expect("UTF-8 fixture source path")
            .to_owned();
        let password = password
            .to_str()
            .expect("UTF-8 fixture password path")
            .to_owned();
        Self {
            fixture,
            server,
            source,
            remote: remote.to_owned(),
            password,
            retries: "0".to_owned(),
        }
    }

    fn retrying(mut self, retries: u8) -> Self {
        self.retries = retries.to_string();
        self
    }

    /// `leading` carries global flags and the subcommand; `trailing` carries per-command flags.
    fn run(&self, leading: &[&str], trailing: &[&str]) -> (String, String) {
        self.run_expecting(leading, trailing, 0)
    }

    fn run_expecting(
        &self,
        leading: &[&str],
        trailing: &[&str],
        expected: i32,
    ) -> (String, String) {
        let mut arguments = leading.to_vec();
        arguments.extend([
            self.source.as_str(),
            self.remote.as_str(),
            "--url",
            self.server.base_url(),
            "--username",
            MOCK_ACCOUNT,
            "--password-file",
            self.password.as_str(),
            "--no-vault",
            "--allow-http",
            // Content comparison is what lets the planner recognise the cross-parent rename and
            // emit a verified server copy, so the sample keeps covering that record.
            "--compare",
            "content",
            "--jobs",
            "1",
            "--retries",
            self.retries.as_str(),
        ]);
        arguments.extend(trailing);
        run_cli_expecting(self.fixture.path(), &arguments, expected)
    }
}

fn assert_every_action_kind_is_present(actions: &Value, document: &Value) {
    for kind in [
        "pre_deletes",
        "creates",
        "copies",
        "uploads",
        "post_deletes",
    ] {
        assert!(
            !actions[kind]
                .as_array()
                .unwrap_or_else(|| panic!("plan actions must expose {kind} as an array"))
                .is_empty(),
            "the conformance scenario stopped exercising {kind}, so the schema check for it \
             became vacuous: {document}"
        );
    }
    assert!(
        actions["post_deletes"]
            .as_array()
            .expect("post-delete actions")
            .iter()
            .any(|action| !action["destination_guard"].is_null()),
        "the scenario must keep producing a populated destination_guard: {document}"
    );
}

#[test]
fn real_plan_json_and_ndjson_stdout_conform_to_the_shipped_schema() {
    let schema = parsed_schema();
    let scenario = Scenario::every_plan_action("schema-real-plan");
    let destructive = ["--delete", "--max-delete", "20"];

    let source = "plan --output json";
    let (stdout, _) = scenario.run(
        &["--quiet", "--progress", "never", "--output", "json", "plan"],
        &destructive,
    );
    let document = json_document(&stdout, source);
    assert_real_output_valid(&schema, &document, source);
    assert_eq!(document["schema"], "sdsync.plan.v1");
    assert_every_action_kind_is_present(&document["plan"]["actions"], &document);

    let source = "plan --output ndjson";
    let (stdout, _) = scenario.run(
        &[
            "--quiet",
            "--progress",
            "never",
            "--output",
            "ndjson",
            "plan",
        ],
        &destructive,
    );
    let records = json_records(&stdout, source);
    for record in &records {
        assert_real_output_valid(&schema, record, source);
    }
    assert_eq!(records[0]["schema"], "sdsync.plan.v1");
    assert_eq!(records[0]["kind"], "summary");

    let observed = records
        .iter()
        .filter_map(|record| record["action"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        observed,
        std::collections::BTreeSet::from([
            "copy-remote-content",
            "create-directory",
            "delete",
            "delete-conflict",
            "upload",
        ]),
        "the NDJSON conformance sample stopped covering every action record: {records:?}"
    );
}

#[test]
fn real_sync_stdout_and_json_logs_conform_to_the_shipped_schema() {
    let schema = parsed_schema();
    let scenario = Scenario::every_plan_action("schema-real-sync");

    // The command result lands on stdout while structured logs land on stderr, so one run
    // samples both governed streams.
    let source = "sync --output ndjson --log-format json";
    let (stdout, stderr) = scenario.run(
        &[
            "--output",
            "ndjson",
            "--log-format",
            "json",
            "--log-level",
            "trace",
            "--progress",
            "never",
            "sync",
        ],
        &["--delete", "--max-delete", "20"],
    );

    let stdout_records = json_records(&stdout, source);
    for record in &stdout_records {
        assert_real_output_valid(&schema, record, source);
    }
    let completion = stdout_records.last().expect("a completion record");
    assert_eq!(completion["schema"], "sdsync.output.v1");
    assert_eq!(completion["kind"], "completion");
    assert_eq!(completion["result"]["changed"], true);

    let stderr_records = structured_records(&stderr, source);
    for record in &stderr_records {
        assert_real_output_valid(&schema, record, source);
    }
    let identifiers = stderr_records
        .iter()
        .filter_map(|record| record["schema"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        identifiers.contains("sdsync.log.v1"),
        "structured logging produced no log event to validate: {stderr}"
    );

    // A second identical run now has nothing to do, which reaches the schema's separate
    // unchanged-completion branch through the same real emitter.
    let source = "sync --output ndjson (already reconciled)";
    let (stdout, _) = scenario.run(
        &[
            "--quiet",
            "--progress",
            "never",
            "--output",
            "ndjson",
            "sync",
        ],
        &["--delete", "--max-delete", "20"],
    );
    let records = json_records(&stdout, source);
    for record in &records {
        assert_real_output_valid(&schema, record, source);
    }
    let completion = records.last().expect("a completion record");
    assert_eq!(completion["schema"], "sdsync.output.v1");
    assert_eq!(completion["changed"], false);
}

/// `terminal_progress_enabled` suppresses the progress stream whenever command output is
/// machine readable, so the only way to sample real progress records is a human-output sync
/// with JSON logging, which switches the renderer to its NDJSON form on stderr.
#[test]
fn real_progress_records_conform_to_the_shipped_schema() {
    let schema = parsed_schema();
    let scenario = Scenario::additive("schema-real-progress");

    let source = "sync --output human --log-format json --progress always";
    let (_, stderr) = scenario.run(
        &[
            "--output",
            "human",
            "--log-format",
            "json",
            "--log-level",
            "trace",
            "--progress",
            "always",
            "sync",
        ],
        &[],
    );

    let records = structured_records(&stderr, source);
    for record in &records {
        assert_real_output_valid(&schema, record, source);
    }
    let progress = records
        .iter()
        .filter(|record| record["schema"] == "sdsync.progress.v1")
        .collect::<Vec<_>>();
    assert!(
        !progress.is_empty(),
        "progress rendering produced no record to validate: {stderr}"
    );
    assert!(
        progress
            .iter()
            .any(|record| record.get("update").is_some_and(|update| !update.is_null())),
        "no progress record carried the optional per-operation update: {stderr}"
    );
}

/// Happy-path runs never reach the failure half of the `logEvent` and `progressUpdate` enums.
/// Injected transport faults do, and those records ship to customers exactly like the rest.
#[test]
fn real_retry_and_failure_records_conform_to_the_shipped_schema() {
    let schema = parsed_schema();
    let scenario = Scenario::additive("schema-real-retry").retrying(2);
    scenario
        .server
        .fail_next_http_operation("SYNO.FileStation.Upload.upload", 503);

    let source = "sync with an injected retryable upload failure";
    let (_, stderr) = scenario.run(
        &[
            "--output",
            "human",
            "--log-format",
            "json",
            "--log-level",
            "trace",
            "--progress",
            "always",
            "sync",
        ],
        &[],
    );
    let records = structured_records(&stderr, source);
    for record in &records {
        assert_real_output_valid(&schema, record, source);
    }
    assert!(
        records
            .iter()
            .any(|record| record["event"] == "retry.scheduled"),
        "the injected fault produced no retry.scheduled record to validate: {stderr}"
    );
    assert_eq!(
        scenario.server.pending_faults(),
        0,
        "the injected fault never fired, so the failure records were never exercised"
    );

    // A run that exhausts its budget emits the terminal `upload.failed` and `run.failed`
    // events and a failed progress update, then exits non-zero. That stream is governed by the
    // same schema as a successful one.
    let failing = Scenario::additive("schema-real-run-failed");
    failing
        .server
        .fail_next_http_operation("SYNO.FileStation.Upload.upload", 503);
    let source = "sync with an unrecoverable upload failure";
    let (_, stderr) = failing.run_expecting(
        &[
            "--output",
            "human",
            "--log-format",
            "json",
            "--log-level",
            "trace",
            "--progress",
            "always",
            "sync",
        ],
        &[],
        1,
    );
    let records = structured_records(&stderr, source);
    for record in &records {
        assert_real_output_valid(&schema, record, source);
    }
    let events = records
        .iter()
        .filter_map(|record| record["event"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for event in ["upload.failed", "run.failed"] {
        assert!(
            events.contains(event),
            "the failing run emitted no {event} record to validate: {stderr}"
        );
    }
    assert!(
        records
            .iter()
            .any(|record| record["update"]["kind"] == "failed"),
        "the failing run emitted no failed progress update to validate: {stderr}"
    );
}

/// `sdsync.sync.v1` carries two mutually exclusive result shapes. Reconciling the same mirror
/// twice reaches both from the real emitter: an execution report, then the unchanged form.
#[test]
fn real_sync_json_documents_conform_to_the_shipped_schema() {
    let schema = parsed_schema();
    let scenario = Scenario::additive("schema-real-sync-document");
    let machine = ["--quiet", "--progress", "never", "--output", "json", "sync"];

    let source = "sync --output json";
    let (stdout, _) = scenario.run(&machine, &[]);
    let document = json_document(&stdout, source);
    assert_real_output_valid(&schema, &document, source);
    assert_eq!(document["schema"], "sdsync.sync.v1");
    assert_eq!(document["result"]["changed"], true);
    assert!(
        document["result"]["elapsed_ms"].is_number(),
        "the executed branch must carry a full execution report: {document}"
    );

    let source = "sync --output json (already reconciled)";
    let (stdout, _) = scenario.run(&machine, &[]);
    let document = json_document(&stdout, source);
    assert_real_output_valid(&schema, &document, source);
    assert_eq!(document["schema"], "sdsync.sync.v1");
    assert_eq!(document["result"], json!({"changed": false}));
    assert_eq!(document["plan"]["summary"]["changes"], false);
}

/// The config surface emits its own record identifiers that this schema deliberately does not
/// govern. Pinning that boundary means adding one of them to `oneOf` fails here until real
/// output is fed through the validator too, instead of being silently assumed conformant.
#[test]
fn config_records_stay_outside_the_governed_schema_until_output_is_validated() {
    let schema = parsed_schema();
    let fixture = TestDir::new("schema-config-boundary");
    let environment = hermetic_environment(&fixture).to_owned();
    let source = fixture.child("source");
    fs::create_dir_all(&source).expect("create configured source directory");
    let config = fixture.write(
        "config.toml",
        format!(
            r#"default-profile = "alpha"

[profiles.alpha]
source = '{}'
remote = "/team/alpha"
url = "https://files.example.invalid/reverse-proxy"
username = "alpha-user"
no-vault = true
"#,
            source.to_str().expect("UTF-8 configured source path")
        )
        .as_bytes(),
    );
    let config = config.to_str().expect("UTF-8 config path");

    for command in [
        ["config", "path"],
        ["config", "validate"],
        ["config", "show"],
    ] {
        let label = command.join(" ");
        let (stdout, _) = run_cli(
            &environment,
            &[
                "--config", config, "--output", "json", command[0], command[1],
            ],
        );
        let document = json_document(&stdout, &label);
        let identifier = document["schema"].as_str().unwrap_or_default().to_owned();
        assert!(
            validate(&schema, &schema, &document, "$").is_err(),
            "`{label}` emits {identifier:?}, which docs/observability.schema.json now accepts. \
             Governed records must be validated from real output: add this command to the \
             real-output conformance tests rather than leaving it unchecked."
        );
    }
}
