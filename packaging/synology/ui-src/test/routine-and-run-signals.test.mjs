// Contract C-B1 .. C-B6 and the engine's new wire names, rendered: a blocked routine,
// a run that never ends, the core's own error output, "saved, but ..." notices,
// and a feed that is slow or failing being asked for less often.
import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { appContext, appSource, loadApp } from "./helpers/load-app.mjs";
import { runStderrBlock } from "../src/feedModel.mjs";
import { attentionItems, normalizeOutcomes, reasonCopy, routineDeferCopy } from "../src/outcomes.mjs";
import { WIDGET_BACKOFF_RAMP_MS, widgetRun, widgetView } from "../src/widgetModel.mjs";

globalThis.window ??= globalThis;
globalThis.document ??= { hidden: false };

const NOW = 1_700_000_000;
const NOW_MS = NOW * 1000;

function block(over = {}) {
  return {
    schema: "sdsync.dsm-outcomes.v1",
    run: { outcome: "succeeded", scope: "all", operation: "sync", started_epoch: NOW - 100, finished_epoch: NOW - 60, exit_code: 0, stale: { detected: false, lock: "absent" } },
    alerts: { status: "ok", failure_count: 0 },
    profiles: [{ profile: "photos", outcome: "succeeded", status_cache: "trusted", finished_epoch: NOW - 60 }],
    routines: [],
    errors: [],
    ...over
  };
}

function snapshot(outcomes, over = {}) {
  return {
    schema: "sdsync.dsm-api.v1",
    generated_at_epoch: NOW,
    service: { state: "running" },
    run: { state: "succeeded", scope: "all", operation: "sync", finished_epoch: NOW - 60 },
    profiles: [{ name: "photos", health: { state: "succeeded", checked_epoch: NOW - 60 } }],
    routines: [{ profile: "photos", state: "deferred", last_success_epoch: NOW - 60 }],
    outcomes,
    ...over
  };
}

const routineDeferred = (reason) => block({ routines: [{ profile: "photos", defer_reason: reason, last_exit_code: null, last_reason: null }] });

// ---------------------------------------------------------------------------
// The engine's new wire names have copy, and the extractor reads the engine
// ---------------------------------------------------------------------------

const ROOT = new URL("../../../../", import.meta.url);

function asStrArms(source, anchor) {
  const start = source.indexOf(anchor);
  assert.ok(start >= 0, `cannot find ${anchor}`);
  const body = source.slice(start, source.indexOf("\n    }\n", start));
  return [...body.matchAll(/=>\s*"([a-z][a-z-]*)"/g)].map((match) => match[1]);
}

test("every deferral and skip wire name the engine can emit has a label", async () => {
  const sync = await readFile(new URL("src/sync.rs", ROOT), "utf8");
  const local = await readFile(new URL("src/local.rs", ROOT), "utf8");
  const deferrals = asStrArms(sync.slice(sync.indexOf("impl DeferReason")), "pub fn as_str");
  const skips = asStrArms(local.slice(local.indexOf("impl SkipReason")), "pub fn as_str");
  assert.ok(deferrals.length >= 15 && deferrals.includes("remote-refused"), `deferral names: ${deferrals}`);
  assert.ok(skips.length >= 7 && skips.includes("type-conflict"), `skip names: ${skips}`);
  const missing = [...deferrals, ...skips].filter((name) => !reasonCopy(name).known);
  assert.deepEqual(missing, [], "engine wire names with no label in outcomes.mjs");
});

// ---------------------------------------------------------------------------
// ADV-B-7: a delete_bound routine can never run
// ---------------------------------------------------------------------------

test("ADV-B-7 a routine deferred by delete_bound is blocked and needs action, not 'deleted next run'", () => {
  const copy = routineDeferCopy("delete_bound");
  assert.equal(copy.blocked, true);
  assert.equal(copy.level, "fail");
  assert.match(copy.text, /can never run/);
  assert.doesNotMatch(copy.text, /next run/);
  const items = attentionItems(normalizeOutcomes(routineDeferred("delete_bound")), NOW);
  const item = items.find((candidate) => candidate.id === "routine-photos");
  assert.equal(item.level, "fail");
  assert.match(item.title, /blocked/);
  assert.match(item.detail, /deletion limit/);
  // The core's per-run meaning of the same word is unchanged.
  assert.match(reasonCopy("delete_bound").text, /bounded/);
  const view = widgetView(snapshot(routineDeferred("delete_bound")), NOW_MS);
  assert.equal(view.rows[0].level, "fail");
  assert.equal(view.rows[0].outcome, "Blocked");
  assert.equal(view.overview.level, "fail");
  assert.match(view.overview.title, /blocked/);
});

// ---------------------------------------------------------------------------
// ADV-B-6: a run that never ends
// ---------------------------------------------------------------------------

const runningBlock = (seconds, long) => block({
  run: { outcome: "running", scope: "all", operation: "sync", started_epoch: NOW - seconds, exit_code: null, running_for_seconds: seconds, long_running: long, warnings: long ? ["long_running"] : [], stale: { detected: false, lock: "live" } }
});

test("ADV-B-6 a long-running run is a warning with its age, and says to check whether it is stuck", () => {
  const items = attentionItems(normalizeOutcomes(runningBlock(7 * 3600, true)), NOW);
  const item = items.find((candidate) => candidate.id === "run-long");
  assert.equal(item.level, "warn");
  assert.match(item.title, /7 h/);
  assert.match(item.detail, /stuck/);
  assert.equal(items.filter((candidate) => /warnings/.test(candidate.title)).length, 0, "no duplicate warnings item");
  const normal = attentionItems(normalizeOutcomes(runningBlock(600, false)), NOW);
  assert.deepEqual(normal.map((candidate) => candidate.id), []);
});

test("ADV-B-6 the widget shows the running age and never shows busy waiting as green", () => {
  const run = widgetRun(snapshot(runningBlock(7 * 3600, true), { run: { state: "running", scope: "all", operation: "sync" } }));
  assert.equal(run.active, true);
  assert.equal(run.longRunning, true);
  assert.match(run.label, /7 h/);

  const busy = (outcomes) => widgetView(snapshot(outcomes, { run: { state: "running", scope: "all", operation: "sync" } }), NOW_MS);
  const ordinary = busy({ ...runningBlock(600, false), routines: [{ profile: "photos", defer_reason: "busy" }] });
  assert.notEqual(ordinary.rows[0].level, "ok", "busy waiting is not green");
  const stuck = busy({ ...runningBlock(7 * 3600, true), routines: [{ profile: "photos", defer_reason: "busy" }] });
  assert.notEqual(stuck.overview.level, "ok");
  const waiting = widgetView(snapshot(routineDeferred("busy")), NOW_MS);
  assert.notEqual(waiting.rows[0].level, "ok");
  assert.notEqual(waiting.overview.level, "ok");
  assert.doesNotMatch(waiting.overview.title, /first run/, "waiting is not 'waiting for the first run'");
});

test("ADV-B-6 the Overview names how long the active run has been going", async () => {
  const component = await loadApp();
  const ctx = appContext(component, {
    snapshot: snapshot(runningBlock(7 * 3600, true), { run: { state: "running", scope: "all", operation: "sync" } }),
    connected: true, freshness: "x", nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS
  });
  assert.equal(ctx.runActive, true);
  assert.match(ctx.runningForSuffix, /running for 7 h.*stuck/);
});

// ---------------------------------------------------------------------------
// ADV-B-12: a cancelled run is not a failure to announce
// ---------------------------------------------------------------------------

test("ADV-B-12 a cancelled run does not raise the failure notification", async () => {
  const sent = [];
  globalThis.Notification = class { constructor(title) { sent.push(title); } static permission = "granted"; };
  window.Notification = globalThis.Notification;
  const component = await loadApp();
  // A failure was already seen (lastFailureKey set, baseline taken), so only the
  // cancelled run's own classification decides whether this one notifies.
  const base = { settings: { desktop_notifications: true, audible: false }, lastFailureKey: "prior:failure", lastOutcomeFailureKey: "", failureBaselineSeen: true, connected: true, freshness: "x", playCue() {} };
  const cancelledOutcomes = block({ run: { outcome: "cancelled", reason: "cancelled", scope: "all", finished_epoch: NOW + 5, exit_code: 130, stale: {} } });
  const ctx = appContext(component, { ...base, snapshot: snapshot(cancelledOutcomes, { run: { state: "failed", scope: "all", operation: "sync", finished_epoch: NOW + 5, exit_code: 130 } }) });
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 0, "a cancelled run with an outcomes block");
  // An older package with no outcomes block has only the legacy state; exit 130/143 is still a stop.
  const legacy = appContext(component, { ...base, snapshot: snapshot(undefined, { run: { state: "failed", scope: "all", operation: "sync", finished_epoch: NOW + 9, exit_code: 143 } }) });
  legacy.maybeNotifyFailure();
  assert.equal(sent.length, 0, "a legacy run stopped by SIGTERM");
  legacy.snapshot = snapshot(undefined, { run: { state: "failed", scope: "all", operation: "sync", finished_epoch: NOW + 20, exit_code: 1 } });
  legacy.maybeNotifyFailure();
  assert.equal(sent.length, 1, "a genuine failure still notifies");
});

// ---------------------------------------------------------------------------
// C-B3: the core's own error output
// ---------------------------------------------------------------------------

const stderrLines = [
  "# epoch=1699990000 scope=all operation=sync exit=1",
  "error: older failure",
  `# epoch=${NOW - 60} scope=all operation=sync exit=1`,
  "error: listing /team failed (DSM 408)",
  "caused by: connection reset",
  `# epoch=${NOW + 500} scope=photos operation=sync exit=1`,
  "error: a newer run"
];

test("C-B3 runStderrBlock returns only the lines under the matching header", () => {
  assert.deepEqual(runStderrBlock(stderrLines, NOW - 60), { found: true, lines: ["error: listing /team failed (DSM 408)", "caused by: connection reset"] });
  assert.equal(runStderrBlock(stderrLines, 42).found, false, "a rotated-away run is not 'no output'");
  assert.deepEqual(runStderrBlock([`# epoch=${NOW}`], NOW), { found: true, lines: [] });
});

const failedWithStderr = (over = {}) => block({
  run: { outcome: "failed", reason: "core_failed", scope: "all", finished_epoch: NOW - 60, exit_code: 1, stale: {}, stderr_log: { source: "run-stderr", finished_epoch: NOW - 60, exit_code: 1 }, ...over }
});

function stderrContext(component, over = {}) {
  return appContext(component, {
    disposed: false, auth: {}, runStderr: { epoch: 0, lines: [], error: "", fetchedAtMs: 0 }, runStderrLoading: false,
    snapshot: snapshot(failedWithStderr()), connected: true, freshness: "x", nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS,
    ...over
  });
}

async function refreshStderr(component, apiGet, ctx) {
  // refreshRunStderr is module-level; reach it through refreshSnapshot's apply step.
  const loaded = await loadApp({ overrides: { apiGet }, exports: "refreshRunStderr" });
  await loaded.refreshRunStderr(ctx);
  void component;
}

test("C-B3 a failed run's core error output is fetched once, shown, and not re-fetched", async () => {
  let reads = 0;
  const apiGet = async (_auth, action, query) => {
    reads += 1;
    assert.equal(action, "logs");
    assert.equal(query.source, "run-stderr");
    return { schema: "sdsync.dsm-logs.v1", logs: [{ source: "run-stderr", lines: stderrLines }] };
  };
  const component = await loadApp({ overrides: { apiGet } });
  const ctx = stderrContext(component);
  await refreshStderr(component, apiGet, ctx);
  assert.equal(reads, 1);
  assert.deepEqual(ctx.runStderrView, { error: "", text: "error: listing /team failed (DSM 408)\ncaused by: connection reset" });
  await refreshStderr(component, apiGet, ctx);
  assert.equal(reads, 1, "the same run is not fetched again");
  assert.match(appSource, /<details v-if="runStderrView" class="sdsync-run-stderr">/);
});

test("C-B3 a failed read of the error output says so and is retried no faster than once a minute", async () => {
  let reads = 0;
  const apiGet = async () => { reads += 1; throw new Error("API did not return JSON"); };
  const component = await loadApp({ overrides: { apiGet } });
  const ctx = stderrContext(component);
  await refreshStderr(component, apiGet, ctx);
  assert.match(ctx.runStderrView.error, /did not return JSON/);
  await refreshStderr(component, apiGet, ctx);
  assert.equal(reads, 1, "no retry inside a minute");
  ctx.runStderr = { ...ctx.runStderr, fetchedAtMs: Date.now() - 61_000 };
  await refreshStderr(component, apiGet, ctx);
  assert.equal(reads, 2);
});

test("C-B3 nothing is fetched for a succeeded run or a run with no stderr pointer, and a stale pointer is cleared", async () => {
  let reads = 0;
  const apiGet = async () => { reads += 1; return { schema: "sdsync.dsm-logs.v1", logs: [] }; };
  const component = await loadApp({ overrides: { apiGet } });
  const ok = stderrContext(component, { snapshot: snapshot(block()) });
  await refreshStderr(component, apiGet, ok);
  const noPointer = stderrContext(component, { snapshot: snapshot(failedWithStderr({ stderr_log: null })) });
  await refreshStderr(component, apiGet, noPointer);
  assert.equal(reads, 0);
  const cleared = stderrContext(component, { snapshot: snapshot(block()), runStderr: { epoch: 5, lines: ["x"], error: "", fetchedAtMs: 1 } });
  await refreshStderr(component, apiGet, cleared);
  assert.equal(cleared.runStderrView, null);
});

// ---------------------------------------------------------------------------
// C-B1: "saved, but ..."
// ---------------------------------------------------------------------------

function installBrowser(packageFetch) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = {
    AbortController: globalThis.AbortController,
    TextEncoder: globalThis.TextEncoder,
    crypto: globalThis.crypto || webcrypto,
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout
  };
  globalThis.fetch = packageFetch;
  return () => {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  };
}

const jsonResponse = (model, status = 200) => ({
  redirected: false,
  status,
  ok: status >= 200 && status < 300,
  headers: { get: (name) => (name.toLowerCase() === "content-type" ? "application/json" : null) },
  text: () => Promise.resolve(JSON.stringify(model))
});

async function saveRoutine(result, auth) {
  const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
  const api = await import(`data:text/javascript;base64,${Buffer.from(apiSource).toString("base64")}#${Math.random()}`);
  const jobId = "c".repeat(48);
  let requestId = "";
  const restore = installBrowser((url, options) => {
    if (options.method === "POST") {
      requestId = JSON.parse(options.body).request_id;
      return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-queued.v1", ok: true, state: "queued", request_id: requestId, job_id: jobId }, 202));
    }
    return Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-result-status.v1", ok: true, state: "complete", job_id: jobId, client_request_id: requestId,
      result: { schema: "sdsync.dsm-result.v1", ok: true, message: "Routine removed", ...result }
    }));
  });
  try {
    return await api.apiPost(auth, "csrf-token", api.ACTIONS.removeRoutine, { name: "photos" });
  } finally {
    restore();
  }
}

test("C-B1/C-B7 a save with warnings reaches the window's notice as a list, and a clean save clears it", async () => {
  const seen = [];
  const auth = { onMutationWarning: (warnings) => seen.push(warnings) };
  const warned = await saveRoutine({
    warnings: [
      { code: "controller_reload_failed", message: "configuration was saved but the running controller was not reloaded: no controller pid" },
      { code: "manager_warning", message: "something else" }
    ]
  }, auth);
  assert.equal(warned.ok, true, "the save itself still succeeds");
  assert.deepEqual(seen[0], [
    { code: "controller_reload_failed", text: "configuration was saved but the running controller was not reloaded: no controller pid" },
    { code: "manager_warning", text: "something else" }
  ]);
  await saveRoutine({}, auth);
  assert.equal(seen[1], null);
  // An older bridge's single pair is still read, so it is not silent.
  await saveRoutine({ warning_code: "controller_reload_failed", warning: "pair form" }, auth);
  assert.deepEqual(seen[2], [{ code: "controller_reload_failed", text: "pair form" }]);
  // Control characters and an over-long line are bounded; an entry with no message still produces a notice.
  await saveRoutine({ warnings: [{ code: "controller_reload_failed", message: `line one\nline two ${"x".repeat(900)}` }] }, auth);
  assert.ok(seen[3][0].text.length <= 400 && !seen[3][0].text.includes("\n"));
  await saveRoutine({ warnings: [{ code: "controller_reload_failed" }] }, auth);
  assert.match(seen[4][0].text, /saved with a warning/);
});

test("C-B1 the window shows the notices as a banner and a toast, and Dismiss clears them", async () => {
  const component = await loadApp();
  const ctx = appContext(component, { disposed: false, mutationWarning: null });
  ctx.noteMutationWarning([{ code: "controller_reload_failed", text: "saved but not reloaded" }, { code: "manager_warning", text: "second" }]);
  assert.equal(ctx.mutationWarning.length, 2);
  assert.ok(ctx.toasts.some((toast) => toast.error && /not fully in effect/.test(toast.title) && /saved but not reloaded second/.test(toast.message)));
  ctx.noteMutationWarning(null);
  assert.equal(ctx.mutationWarning, null);
  assert.match(appSource, /v-if="mutationWarning" class="sdsync-banner sdsync-mutation-warning" role="alert"/);
  assert.match(appSource, /v-for="\(warning, index\) in mutationWarning"/);
  assert.match(appSource, /onMutationWarning: \(warning\)/);
});

// ---------------------------------------------------------------------------
// B-4: do not hammer a slow or failing feed
// ---------------------------------------------------------------------------

test("B-4 the Logs/Activity poll waits for three times a slow read and ramps after repeated failure", async () => {
  const loaded = await loadApp({ exports: "logsPollDelay" });
  const delay = (state, interval = 5000) => loaded.logsPollDelay({ windowHidden: false, logsLastDurationMs: 0, logsFailureStreak: 0, ...state }, interval);
  assert.equal(delay({}), 5000);
  assert.equal(delay({ logsLastDurationMs: 800 }), 5000, "a fast read keeps the operator's cadence");
  assert.equal(delay({ logsLastDurationMs: 9000 }), 27_000, "a 9 s scan is not asked for again for 27 s");
  assert.equal(delay({ logsFailureStreak: 1 }), 5000, "one failure is not a verdict");
  assert.equal(delay({ logsFailureStreak: 2 }), WIDGET_BACKOFF_RAMP_MS[0]);
  assert.equal(delay({ logsFailureStreak: 9 }), WIDGET_BACKOFF_RAMP_MS[3]);
  assert.equal(delay({ windowHidden: true }), 60_000);
});

test("B-4 refreshLogs records how long a read took and how many failed in a row", async () => {
  let fail = true;
  const component = await loadApp({ overrides: { apiGet: async () => { if (fail) throw new Error("API did not return JSON"); return { schema: "sdsync.dsm-activity.v1", events: [] }; } } });
  const ctx = appContext(component, {
    disposed: false, logsLoading: false, logsPaused: false, route: "activity", activityTab: "events", logLines: 200, logSource: "all", auth: {},
    activityEvents: [], activityHistory: null, logRecords: [], logOutput: "", logState: "", logsReceivedAtMs: 0, activityReceivedAtMs: 0,
    activityFeedError: "", logsFeedError: "", activitySignature: "", logsLastDurationMs: 0, logsFailureStreak: 0, scheduleLogs() {}
  });
  await ctx.refreshLogs();
  await ctx.refreshLogs();
  assert.equal(ctx.logsFailureStreak, 2);
  fail = false;
  await ctx.refreshLogs();
  assert.equal(ctx.logsFailureStreak, 0);
  assert.ok(ctx.logsLastDurationMs >= 0);
});

test("C-E6 lasting refusals say the entry is not synced until the user acts, never 'retried'", () => {
  for (const name of [
    "remote-refused", "remote-skipped", "unsupported-entry", "case-rename-pending", "delete-not-permitted",
    "local-permission-denied", "type-conflict", "protected-descendant", "remote-link"
  ]) {
    const { text, known } = reasonCopy(name);
    assert.equal(known, true, name);
    assert.match(text, /^not synced until you fix this: /, name);
    assert.doesNotMatch(text, /retried/, name);
  }
  assert.match(reasonCopy("remote-real-path-unusable").text, /nothing was synced/);
});
