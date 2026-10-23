// ADV-U-2: the REASONS table was written against the contract document, not
// against the code that writes reasons, so `core_failed` (every failed core run)
// and 23 other tokens the runner emits read as "unknown reason". This test reads
// the writers, so a token added to the runner or the bridge without a label fails
// here, not on an operator's Overview.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
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
  for (const [token, line] of collect(controller, [new RegExp(`\\breason=(runner_died)\\b`, "g")])) state.set(token, `sdsync-controller:${line}`);
  const implied = functionBody(bridge, "fn implied_reason");
  for (const match of implied.matchAll(/Some\("([a-z_]+)"\.to_owned\(\)\)/g)) state.set(match[1], "dsm_api.rs implied_reason");
  for (const match of bridge.matchAll(/stale_reason\s*=\s*Some\("([a-z_]+)"\)/g)) state.set(match[1], "dsm_api.rs stale_reason");
  // The core's failure classes: EventReason::as_str.
  const classes = functionBody(observability.slice(observability.indexOf("impl EventReason")), "pub fn as_str");
  const coreClasses = [...classes.matchAll(/=>\s*"([a-z_]+)"/g)].map((match) => match[1]);
  for (const token of coreClasses) state.set(token, "observability.rs EventReason");
  return { state, defer, backend, coreClasses, counts: { run, controller, common } };
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
