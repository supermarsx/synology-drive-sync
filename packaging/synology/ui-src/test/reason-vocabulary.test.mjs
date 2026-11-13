// ADV-U-2: the REASONS table was written against the contract document, not
// against the code that writes reasons, so `core_failed` (every failed core run)
// and 23 other tokens the runner emits read as "unknown reason". This test reads
// the writers, so a token added to the runner or the bridge without a label fails
// here, not on an operator's Overview.
import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";
import test from "node:test";
import { attentionItems, normalizeOutcomes, profileOutcomeRow, reasonCopy } from "../src/outcomes.mjs";
import { widgetView } from "../src/widgetModel.mjs";

const ROOT = new URL("../../../../", import.meta.url);
const read = (path) => readFile(new URL(path, ROOT), "utf8");
const LIBEXEC = "packaging/synology/package/libexec/";

const NONE = new Set(["none", "ok", ""]);

function collect(source, patterns) {
  const found = new Map();
  source.split("\n").forEach((line, index) => {
    if (/^\s*#/.test(line)) return;
    for (const pattern of patterns) {
      for (const match of line.matchAll(pattern)) {
        const token = match[1];
        if (!NONE.has(token)) found.set(token, found.get(token) || index + 1);
      }
    }
  });
  return found;
}

function functionBody(source, signature) {
  const start = source.indexOf(signature);
  assert.ok(start >= 0, `cannot find ${signature}`);
  const end = source.indexOf("\n}\n", start);
  return source.slice(start, end);
}

const TOKEN = "([a-z][a-z0-9_]*)";
// Every assignment or call that puts a reason into run.state, a profile state or a
// routine state. `reason=` inside controller log events is a log field, not state.
const SHELL_STATE_REASONS = [
  new RegExp(`\\brunner_fail\\s+${TOKEN}\\b`, "g"),
  new RegExp(`\\b(?:preflight_reason|run_failure_reason|run_reason|routine_reason_value|routine_last_reason_value|scheduled_reason)=${TOKEN}\\b`, "g"),
  new RegExp(`\\brunner_profile_outcome\\s+"[^"]*"\\s+[a-z_]+\\s+\\S+\\s+${TOKEN}\\s`, "g")
];
const DEFER_REASONS = [new RegExp(`\\broutine_defer_reason_value=${TOKEN}\\b`, "g")];
const BACKEND_REASONS = [
  new RegExp(`\\b(?:realtime_watcher_reason|routine_backend_reason_value|watcher_failed_reason)=${TOKEN}\\b`, "g")
];

// RU-1: the writers that carry their reason as an argument rather than as a
// `name=token` assignment. `write_run_state STATE STARTED FINISHED EXIT SCOPE OPERATION
// REASON DETAIL` takes it as the seventh word, so a renamed token there was invisible to
// the assignment patterns, and the old `reason=runner_died` rule was satisfied by the
// controller's log lines whatever the writer said. Backslash continuations are joined and
// comment lines dropped first; a reason that is a variable ("$run_reason") is computed
// elsewhere and is covered by the assignment that fills the variable.
const SHELL_WORD = String.raw`(?:"[^"]*"|\S+)`;
const WRITE_RUN_STATE = new RegExp(String.raw`\bwrite_run_state\s+${Array(6).fill(SHELL_WORD).join(String.raw`\s+`)}\s+([a-z][a-z0-9_]*)(?=\s)`, "g");
// A record written by printf: 'outcome=unknown\nreason=runner_died\nscope=%s'.
const PRINTF_REASON = /\bprintf\s+'[^']*\\nreason=([a-z][a-z0-9_]*)\\n/g;
// reconcile_stale_run_state's default mode, which becomes the reason it writes.
const DEFAULT_MODE_REASON = /\bstale_run_mode=\$\{2:-([a-z][a-z0-9_]*)\}/g;

export function positionalReasons(source) {
  const joined = source.split("\n").filter((line) => !/^\s*#/.test(line)).join("\n").replace(/\\\r?\n\s*/g, " ");
  const found = new Set();
  for (const pattern of [WRITE_RUN_STATE, PRINTF_REASON, DEFAULT_MODE_REASON]) {
    for (const match of joined.matchAll(pattern)) if (!NONE.has(match[1])) found.add(match[1]);
  }
  return found;
}

async function writerTokens() {
  const run = await read(`${LIBEXEC}sdsync-run`);
  const controller = await read(`${LIBEXEC}sdsync-controller`);
  const common = await read(`${LIBEXEC}sdsync-common`);
  const bridge = await read("src/dsm_api.rs");
  const observability = await read("src/observability.rs");

  const state = new Map();
  const defer = new Map();
  const backend = new Map();
  for (const [file, source] of [["sdsync-run", run], ["sdsync-controller", controller], ["sdsync-common", common]]) {
    for (const [token, line] of collect(source, SHELL_STATE_REASONS)) state.set(token, `${file}:${line}`);
    for (const [token, line] of collect(source, DEFER_REASONS)) defer.set(token, `${file}:${line}`);
    for (const [token, line] of collect(source, BACKEND_REASONS)) backend.set(token, `${file}:${line}`);
  }
  // The controller rewrites a dead runner's run.state, and the bridge derives the rest.
  for (const [file, source] of [["sdsync-run", run], ["sdsync-controller", controller], ["sdsync-common", common]]) {
    for (const token of positionalReasons(source)) if (!state.has(token)) state.set(token, `${file} (positional writer)`);
  }
  const implied = functionBody(bridge, "fn implied_reason");
  for (const match of implied.matchAll(/Some\("([a-z_]+)"\.to_owned\(\)\)/g)) state.set(match[1], "dsm_api.rs implied_reason");
  for (const match of bridge.matchAll(/stale_reason\s*=\s*Some\("([a-z_]+)"\)/g)) state.set(match[1], "dsm_api.rs stale_reason");
  // The core's failure classes: EventReason::as_str.
  const classes = functionBody(observability.slice(observability.indexOf("impl EventReason")), "pub fn as_str");
  const coreClasses = [...classes.matchAll(/=>\s*"([a-z_]+)"/g)].map((match) => match[1]);
  for (const token of coreClasses) state.set(token, "observability.rs EventReason");
  return { state, defer, backend, coreClasses, counts: { run, controller, common }, sources: { run, controller, common } };
}

test("the extractor reads real writers (it would otherwise pass on an empty set)", async () => {
  const { state, defer, backend, coreClasses } = await writerTokens();
  assert.ok(state.size >= 40, `expected at least 40 state reasons, extracted ${state.size}`);
  for (const sentinel of ["core_failed", "runner_died", "profiles_skipped", "policy_http", "password_missing", "supervisor_refused", "lock_busy", "authentication"]) {
    assert.ok(state.has(sentinel), `extractor missed ${sentinel}`);
  }
  assert.ok(defer.size >= 4 && defer.has("dependency") && defer.has("source"), `defer reasons: ${[...defer.keys()]}`);
  assert.ok(backend.size >= 6, `backend reasons: ${[...backend.keys()]}`);
  assert.ok(coreClasses.length >= 30, `core classes: ${coreClasses.length}`);
});

test("every reason a writer can emit has a cause and a next step", async () => {
  const { state } = await writerTokens();
  const missing = [...state].filter(([token]) => !reasonCopy(token).known).map(([token, where]) => `${token} (${where})`);
  assert.deepEqual(missing, [], "reason tokens written by the packaging or core with no label in outcomes.mjs");
});

test("every routine defer reason has a label", async () => {
  const { defer } = await writerTokens();
  const missing = [...defer].filter(([token]) => !reasonCopy(token).known).map(([token, where]) => `${token} (${where})`);
  assert.deepEqual(missing, []);
});

test("every routine backend reason has operator guidance in the AppWindow", async () => {
  const { backend } = await writerTokens();
  const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
  const guidance = app.slice(app.indexOf("const REALTIME_REASON_GUIDANCE"), app.indexOf("const SECURITY_BOOLEAN_FIELDS"));
  const missing = [...backend].filter(([token]) => !new RegExp(`\\b${token}:`).test(guidance)).map(([token, where]) => `${token} (${where})`);
  assert.deepEqual(missing, []);
});

test("a reason this UI has never heard of still shows its raw code and a failed or warning colour", () => {
  const copy = reasonCopy("brand_new_reason");
  assert.equal(copy.known, false);
  assert.match(copy.text, /brand_new_reason/);

  const outcomes = (profile, run = {}) => normalizeOutcomes({
    schema: "sdsync.dsm-outcomes.v1",
    run: { outcome: "succeeded", stale: {}, ...run },
    alerts: { status: "ok" },
    profiles: profile ? [profile] : [],
    routines: [],
    errors: []
  });

  // A run that failed for an unknown reason is a failure that names the code.
  const failed = attentionItems(outcomes(null, { outcome: "failed", reason: "brand_new_reason" }), 1_700_000_000);
  assert.equal(failed[0].level, "fail");
  assert.match(failed[0].detail, /brand_new_reason/);

  // A profile row whose outcome looks fine but whose reason is unknown must not stay green or grey.
  const row = profileOutcomeRow(outcomes({ profile: "photos", outcome: "succeeded", reason: "brand_new_reason" }).profiles[0]);
  assert.notEqual(row.level, "ok");
  assert.notEqual(row.level, "idle");
  assert.match(row.detail, /brand_new_reason/);

  // A run recorded as succeeded with an unknown reason is surfaced, never swallowed.
  const quiet = attentionItems(outcomes(null, { outcome: "succeeded", reason: "brand_new_reason" }), 1_700_000_000);
  assert.ok(quiet.some((item) => item.level === "warn" && /brand_new_reason/.test(item.detail)), JSON.stringify(quiet));

  // An unknown outcome token takes the unknown level, which the stylesheet colours as a warning.
  const odd = profileOutcomeRow(outcomes({ profile: "photos", outcome: "exploded", reason: "x" }).profiles[0]);
  assert.equal(odd.level, "unknown");
});

test("routine defer and last reasons read as copy, not as unknown", () => {
  const snapshot = {
    schema: "sdsync.dsm-api.v1",
    service: { state: "running" },
    run: { state: "succeeded", scope: "all", operation: "sync" },
    profiles: [{ name: "photos", health: { state: "succeeded", checked_epoch: 1_699_999_000 } }],
    routines: [{ profile: "photos", state: "deferred", last_success_epoch: 1_699_999_000 }],
    outcomes: {
      schema: "sdsync.dsm-outcomes.v1",
      run: { outcome: "succeeded", stale: {} },
      alerts: { status: "ok" },
      profiles: [{ profile: "photos", outcome: "succeeded", status_cache: "trusted" }],
      routines: [{ profile: "photos", defer_reason: "dependency", last_exit_code: 1, last_reason: "core_failed" }],
      errors: []
    }
  };
  const view = widgetView(snapshot, 1_700_000_000_000);
  assert.doesNotMatch(JSON.stringify(view), /unknown reason/);
  const items = attentionItems(normalizeOutcomes(snapshot.outcomes), 1_700_000_000);
  assert.doesNotMatch(JSON.stringify(items), /unknown/);
  assert.match(JSON.stringify(items), /depends on/);
});

test("RU-1 the stale-run writers are read from the writer, so a renamed token is seen even while log lines keep the old one", async () => {
  const { sources } = await writerTokens();
  const controller = positionalReasons(sources.controller);
  assert.ok(controller.has("runner_died"), "sdsync-controller no longer writes runner_died through write_run_state");
  assert.ok(controller.has("cancelled"), "sdsync-controller no longer writes cancelled through write_run_state");
  assert.ok(positionalReasons(sources.common).has("runner_died"), "sdsync-common no longer writes runner_died into a profile record");

  // Rename the token at the controller's write_run_state call only.
  const call = /(write_run_state failed "\$stale_run_started" "\$stale_run_now" -1 "\$stale_run_scope" \\r?\n\s*"\$stale_run_operation" )runner_died/;
  assert.match(sources.controller, call, "the writer's shape changed; this test must follow it");
  const renamed = sources.controller.replace(call, "$1runner_gone");
  assert.match(renamed, /reason=runner_died/, "the controller's log lines still carry the old token, which is what hid the rename before");
  const after = positionalReasons(renamed);
  assert.ok(after.has("runner_gone"), "the renamed token is extracted from the positional writer");
  assert.ok(!reasonCopy("runner_gone").known, "and it has no label, so the vocabulary test above fails on it");
  // The same rename in the profile-record writer.
  const profileRecord = sources.common.replace(/(\\nreason=)runner_died(\\n)/, "$1runner_gone$2");
  assert.notEqual(profileRecord, sources.common);
  assert.ok(positionalReasons(profileRecord).has("runner_gone") && !positionalReasons(profileRecord).has("runner_died"));
});

// ---- the Rust daemon (src/daemon/**) ----------------------------------------------------
//
// The daemon writes the same state records the shell did, so its reasons must read as copy
// too. What is scanned, and only this (a pattern per call shape, so an ordinary Rust string
// is never mistaken for a reason):
//   1. `.reason = <expr>;` and `.last_reason = <expr>;`   (every string literal in <expr>,
//      so `if refused { "admission_refused".into() } else { "busy".into() }` yields both)
//   2. `reason: <expr>,` and `last_reason: <expr>,`       (struct fields)
//   3. `RunOutcome::X, "token",`                          (executor's ending_words tuples)
//   4. `verdict(status, outcome, <reason>, ...)`          (the third argument)
//   5. `detail: format!("token: ...` in lifecycle.rs      (the consumer deadline's prefix)
//   6. the `as_str` of DeferReason (labels) and BackendReason (operator guidance)
// Test code is cut off at the first `#[cfg(test)]`, and `*_tests.rs` files are skipped.
// Literals are matched whole, so prose (`reason = "the worker is started ..."`) never counts.
const DAEMON_DIR = "src/daemon/";
const LITERAL = /"([a-z][a-z0-9_]*)"/g;

function literals(expression) {
  return [...expression.matchAll(LITERAL)].map((match) => match[1]).filter((name) => !NONE.has(name));
}

export function daemonReasons(file, rawSource) {
  const cut = rawSource.search(/^#\[cfg\(test\)\]\s*\n\s*(?:pub(?:\([a-z]+\))? )?mod\s/m);
  const source = (cut >= 0 ? rawSource.slice(0, cut) : rawSource).split("\n").filter((line) => !/^\s*\/\//.test(line)).join("\n");
  const found = new Map();
  const add = (name, shape) => { if (!NONE.has(name) && !found.has(name)) found.set(name, `${file} (${shape})`); };
  for (const match of source.matchAll(/\.(?:last_reason|reason)\s*=(?!=)\s*([^;]+);/g)) literals(match[1]).forEach((name) => add(name, "assignment"));
  for (const match of source.matchAll(/\b(?:last_reason|reason):\s*([^,;{}]*(?:\{[^{}]*\}[^,;{}]*)*),/g)) literals(match[1]).forEach((name) => add(name, "field"));
  for (const match of source.matchAll(/RunOutcome::\w+,\s*"([a-z][a-z0-9_]*)",/g)) add(match[1], "ending");
  for (const match of source.matchAll(/\bverdict\(\s*[^,()]+,\s*[^,()]+,\s*([^,]+),/g)) literals(match[1]).forEach((name) => add(name, "verdict"));
  if (file.endsWith("lifecycle.rs")) {
    for (const match of source.matchAll(/detail:\s*format!\(\s*"([a-z][a-z0-9_]*): /g)) add(match[1], "detail prefix");
  }
  return found;
}

async function daemonSources() {
  const root = new URL(DAEMON_DIR, ROOT);
  const names = (await readdir(root, { recursive: true })).map((name) => name.replaceAll("\\", "/"))
    .filter((name) => name.endsWith(".rs") && !name.endsWith("_tests.rs")).sort();
  const sources = new Map();
  for (const name of names) sources.set(name, await readFile(new URL(name, root), "utf8"));
  return sources;
}

async function daemonTokens() {
  const sources = await daemonSources();
  const reasons = new Map();
  for (const [name, source] of sources) {
    for (const [reason, where] of daemonReasons(`${DAEMON_DIR}${name}`, source)) if (!reasons.has(reason)) reasons.set(reason, where);
  }
  const routines = sources.get("routines.rs");
  const arms = (enumName) => {
    const body = functionBody(routines.slice(routines.indexOf(`impl ${enumName}`)), "pub(crate) fn as_str");
    return [...body.matchAll(/=>\s*"([a-z_]+)"/g)].map((match) => match[1]).filter((name) => !NONE.has(name));
  };
  return { sources, reasons, defer: arms("DeferReason"), backend: arms("BackendReason") };
}

test("the daemon extractor reads real writers (it would otherwise pass on an empty set)", async () => {
  const { sources, reasons, defer, backend } = await daemonTokens();
  assert.ok(sources.size >= 10, `expected the daemon sources, read ${sources.size}`);
  for (const sentinel of ["run_stalled", "launch_failed", "state_unwritable", "core_oom_killed", "core_killed", "core_failed", "supervisor_refused", "runner_died", "cancelled", "deferred_paths", "busy", "admission_refused", "interrupted", "consumer_timeout"]) {
    assert.ok(reasons.has(sentinel), `daemon extractor missed ${sentinel}: ${[...reasons.keys()].join(", ")}`);
  }
  assert.ok(defer.includes("delete_bound") && defer.includes("dependency"), `defer: ${defer}`);
  assert.ok(backend.length >= 8 && backend.includes("watch_limit"), `backend: ${backend}`);
  // Test code and prose are not reasons.
  for (const never of ["brand_new_reason", "bogus", "new_reason", "the_worker_is_started"]) assert.ok(!reasons.has(never), never);
});

test("every reason the daemon writes has a cause and a next step", async () => {
  const { reasons } = await daemonTokens();
  const missing = [...reasons].filter(([name]) => !reasonCopy(name).known).map(([name, where]) => `${name} (${where})`);
  assert.deepEqual(missing, [], "reason tokens written by src/daemon with no label in outcomes.mjs");
});

test("every routine defer and backend reason the daemon writes has copy and operator guidance", async () => {
  const { defer, backend } = await daemonTokens();
  assert.deepEqual(defer.filter((name) => !reasonCopy(name).known), []);
  const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
  const guidance = app.slice(app.indexOf("const REALTIME_REASON_GUIDANCE"), app.indexOf("const SECURITY_BOOLEAN_FIELDS"));
  assert.deepEqual(backend.filter((name) => !new RegExp(`\\b${name}:`).test(guidance)), []);
});

test("the daemon extractor sees a renamed token in each call shape", () => {
  const sample = [
    'state.last_reason = if x { "admission_refused".into() } else { "busy".into() };',
    'record.reason = "runner_gone".into();',
    'Foo { reason: "field_reason".into(), other: 1 }',
    '(RunOutcome::Failed, "ending_reason", String::new(), false)',
    'verdict(RunStatus::Failed, RunOutcome::Failed, if p { "kill_a" } else { "kill_b" }, d, false, true)',
    "#[cfg(test)]",
    "mod tests {",
    '  fn t() { state.last_reason = "only_in_tests".into(); } }'
  ].join("\n");
  const found = [...daemonReasons("x.rs", sample).keys()];
  assert.deepEqual(found.sort(), ["admission_refused", "busy", "ending_reason", "field_reason", "kill_a", "kill_b", "runner_gone"]);
  const lifecycle = daemonReasons("src/daemon/lifecycle.rs", 'detail: format!(\n  "consumer_timeout: {} did not finish",\n  x)');
  assert.ok(lifecycle.has("consumer_timeout"));
});

// fix9: the daemon scopes a realtime run to the folders that changed, so `scope_not_found` is no
// longer only a missing profile: a folder made and removed within the quiet period is on neither
// side when the run starts. The copy must not blame the profile name for that.
test("scope_not_found names both the profile and the scoped folder, and says nothing is missing", () => {
  const copy = reasonCopy("scope_not_found");
  assert.equal(copy.known, true);
  assert.match(copy.text, /profile/);
  assert.match(copy.text, /folder this run was scoped to/);
  assert.match(copy.text, /daily full pass/);
});
