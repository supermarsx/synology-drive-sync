use serde_json::{Value, json};

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
