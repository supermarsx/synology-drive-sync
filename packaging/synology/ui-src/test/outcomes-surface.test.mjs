import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import * as outcomesModule from "../src/outcomes.mjs";
import {
  WIDGET_ACTIVE_POLL_MS,
  WIDGET_BACKOFF_RAMP_MS,
  WIDGET_IDLE_POLL_MS,
  widgetOverview,
  widgetProfileRows,
  widgetView
} from "../src/widgetModel.mjs";

const {
  attentionItems, failureKey, normalizeOutcomes, outcomeLabel, outcomeLevel,
  profileOutcomeRow, reasonCopy, statusCacheNote
} = outcomesModule;

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const NOW = 1_700_000_000;
const NOW_MS = NOW * 1000;

const OUTCOMES = ["succeeded", "partial", "failed", "not_run", "busy", "cancelled", "stale", "running", "unknown"];
const REASONS = [
  "preflight_config_invalid", "preflight_log_file_path", "preflight_policy", "preflight_unsafe_state",
  "no_password:photos", "lock_busy", "runner_vanished", "supervisor_refused", "dependency_unmet",
  "outside_window", "cancelled", "batch_target_failed:photos", "not_reached",
  "authentication", "network", "http_status", "timeout", "remote_response_invalid", "dsm_api_error",
  "dsm_api_unsupported", "share_not_writable", "remote_path_refused", "type_conflict",
  "protected_conflict", "empty_source_guard", "delete_limit", "source_invalid", "local_entry_unsupported",
  "local_io", "source_changed", "resync_ticket_stale", "content_verification_failed", "remote_changed",
  "server_copy_unavailable", "server_copy_failed", "reconciliation_pending", "credential_unavailable", "config_invalid",
  "https_required", "scope_not_found", "unclassified",
  "changed_during_run", "delete_bound", "server_copy_fallback", "excluded", "upload_failed"
];

function block(over = {}) {
  return {
    schema: "sdsync.dsm-outcomes.v1",
    run: {
      outcome: "succeeded", reason: null, state: "succeeded", operation: "sync", scope: "all",
      started_epoch: NOW - 100, finished_epoch: NOW - 60, exit_code: 0, deferred_files: null, skipped_files: null,
      stale: { detected: false, lock: "absent", reason: null }
    },
    alerts: { status: "ok", failure_count: 0, last_notify_epoch: 0, failure_threshold: 1, threshold_reached: false },
    profiles: [],
    errors: [],
    ...over
  };
}

function runWith(over) { return { ...block().run, ...over }; }

function profileEntry(name, over = {}) {
  return {
    profile: name, outcome: "succeeded", reason: null, action: "sync", finished_epoch: NOW - 120,
    exit_code: 0, consecutive_failures: 0, last_success_epoch: NOW - 120, status_cache: "trusted", ...over
  };
}

function snapshot(over = {}) {
  return {
    schema: "sdsync.dsm-api.v1", service: { state: "running" },
    run: { state: "succeeded", scope: "all", operation: "sync", started_epoch: 0, finished_epoch: 0 },
    profiles: [{ name: "photos", health: { state: "succeeded", checked_epoch: NOW - 60 } }],
    routines: [], outcomes: block(), ...over
  };
}

test("every outcome has a label and a level, and an unknown one is shown rather than hidden", () => {
  for (const outcome of OUTCOMES) {
    assert.ok(!outcomeLabel(outcome).startsWith("unknown"), outcome);
    assert.notEqual(outcomeLevel(outcome), "unknown", outcome);
  }
  assert.equal(outcomeLevel("failed"), "fail");
  assert.equal(outcomeLevel("stale"), "fail");
  assert.equal(outcomeLevel("succeeded"), "ok");
  assert.equal(outcomeLabel("quantum"), "unknown (quantum)");
  assert.equal(outcomeLevel("quantum"), "unknown");
  assert.equal(outcomeLabel("<script>"), "unknown (unreadable)");
  assert.equal(outcomeLabel(undefined), "unknown (unreadable)");
  assert.equal(outcomeLabel("constructor"), "unknown (constructor)");
});

test("every reason code names a cause and a next step; unknown codes are shown raw", () => {
  for (const reason of REASONS) {
    const copy = reasonCopy(reason);
    assert.equal(copy.known, true, reason);
    assert.match(copy.text, /\. [A-Z]/, `${reason} lacks a cause and a next step: ${copy.text}`);
  }
  assert.match(reasonCopy("preflight_log_file_path").text, /package paths mismatch/);
  assert.match(reasonCopy("no_password:photos").text, /\(photos\)/);
  assert.equal(reasonCopy("brand_new_reason").text, "unknown reason (brand_new_reason)");
  assert.equal(reasonCopy("brand_new:thing").known, false);
  assert.equal(reasonCopy("../etc/passwd").text, "unknown reason (unreadable)");
  assert.equal(reasonCopy(null).text, "");
  assert.equal(reasonCopy("constructor").known, false);
});

test("hostile or malformed outcomes blocks never throw", () => {
  const hostile = [null, undefined, 7, "x", [], {}, { run: 5 }, { run: { outcome: {} }, profiles: "no", errors: 3 },
    { profiles: [null, 4, {}, { profile: 9 }], alerts: [] }, { run: { outcome: "failed", exit_code: "NaN" } }];
  for (const value of hostile) {
    const normalized = normalizeOutcomes(value);
    assert.doesNotThrow(() => attentionItems(normalized, NOW));
    assert.doesNotThrow(() => failureKey(normalized));
    for (const entry of normalized ? normalized.profiles : []) assert.doesNotThrow(() => profileOutcomeRow(entry, NOW));
  }
  assert.equal(normalizeOutcomes(null), null);
});

test("attention items cover run, profile, alert and record-keeping failures", () => {
  const items = (b) => attentionItems(normalizeOutcomes(b), NOW);
  const ids = (b) => items(b).map((item) => item.id);
  assert.deepEqual(ids(block()), []);
  assert.deepEqual(ids(block({ run: runWith({ outcome: "failed", reason: "preflight_log_file_path", exit_code: 66 }) })), ["run-failed"]);
  const failed = items(block({ run: runWith({ outcome: "failed", reason: "preflight_log_file_path" }) }))[0];
  assert.equal(failed.level, "fail");
  assert.match(failed.detail, /package paths mismatch/);
  assert.deepEqual(ids(block({ run: runWith({ outcome: "stale", stale: { detected: true, lock: "dead", reason: "runner_vanished" } }) })), ["run-stale"]);
  const partial = items(block({ run: runWith({ outcome: "partial", deferred_files: 2, skipped_files: 1 }) }))[0];
  assert.equal(partial.level, "warn");
  assert.match(partial.detail, /3 files deferred or skipped/);
  assert.deepEqual(ids(block({ run: runWith({ outcome: "busy", reason: "lock_busy" }) })), ["run-busy"]);
  assert.deepEqual(ids(block({ run: runWith({ outcome: "cancelled" }) })), ["run-cancelled"]);
  const status = items(block({ run: runWith({ outcome: "unknown" }) }))[0];
  assert.equal(status.level, "fail");
  assert.equal(status.title, "Last run status unknown");
  const unknown = items(block({ run: runWith({ outcome: "exploded" }) }))[0];
  assert.equal(unknown.id, "run-unknown");
  assert.equal(unknown.title, "Last run unknown (exploded)");
  assert.equal(unknown.level, "unknown");
  assert.deepEqual(ids(block({ alerts: { status: "unreadable", failure_count: null } })), ["alerts-unreadable"]);
  assert.deepEqual(ids(block({ alerts: { status: "mystery" } })), ["alerts-unknown"]);
  const counted = items(block({ alerts: { status: "ok", failure_count: 4, failure_threshold: 3, threshold_reached: true } }))[0];
  assert.equal(counted.title, "Sync has failed 4 times in a row");
  assert.match(counted.detail, /threshold has been reached/);
  const degraded = items(block({ errors: [{ source: "run.state", code: "malformed" }] }))[0];
  assert.equal(degraded.title, "Package record-keeping is degraded");
  assert.match(degraded.detail, /run\.state malformed/);
  assert.deepEqual(ids(block({ profiles: [profileEntry("photos", {
    outcome: "failed", reason: "authentication", consecutive_failures: 3, status_cache: "last_run_failed"
  })] })), ["profile-photos"]);
});

test("profile rows carry reason, age, consecutive failures and status-cache trust", () => {
  const entry = normalizeOutcomes(block({ profiles: [profileEntry("photos", {
    outcome: "failed", reason: "authentication", consecutive_failures: 3, status_cache: "last_run_failed", finished_epoch: NOW - 7200
  })] })).profiles[0];
  const row = profileOutcomeRow(entry, NOW);
  assert.equal(row.label, "Failed");
  assert.equal(row.level, "fail");
  assert.match(row.detail, /rejected the login/);
  assert.match(row.detail, /failed 3 times in a row/);
  assert.equal(row.age, "2 h ago");
  assert.match(row.note, /^Last run failed: /);
  const note = (status_cache, over = {}) =>
    statusCacheNote(normalizeOutcomes(block({ profiles: [profileEntry("p", { status_cache, ...over })] })).profiles[0]);
  assert.equal(note("trusted"), "");
  assert.match(note("last_run_partial", { outcome: "partial" }), /some files are pending/);
  assert.match(note("no_run_recorded", { outcome: "not_run" }), /No run recorded/);
  assert.match(note("last_run_failed", { outcome: "cancelled" }), /^Last run cancelled/);
  assert.match(note("brand_new_state"), /unknown \(brand_new_state\)/);
  assert.equal(failureKey(normalizeOutcomes(block())), "");
  assert.notEqual(failureKey(normalizeOutcomes(block({ run: runWith({ outcome: "failed" }) }))), "");
});

test("a profile with no routine is not Doctor passed while its syncs fail", () => {
  const failing = snapshot({
    outcomes: block({
      run: runWith({ outcome: "failed", reason: "network" }),
      profiles: [profileEntry("photos", { outcome: "failed", reason: "network", consecutive_failures: 2, status_cache: "last_run_failed" })]
    })
  });
  const [row] = widgetProfileRows(failing, NOW_MS);
  assert.equal(row.outcome, "Failed");
  assert.equal(row.level, "fail");
  assert.match(row.detail, /could not be reached/);
  const overview = widgetOverview(failing, NOW_MS);
  assert.equal(overview.level, "fail");
  assert.equal(overview.title, "Last run failed");
  const never = snapshot({
    outcomes: block({ profiles: [profileEntry("photos", { outcome: "not_run", finished_epoch: null, status_cache: "no_run_recorded" })] })
  });
  const [idle] = widgetProfileRows(never, NOW_MS);
  assert.equal(idle.level, "idle");
  assert.match(idle.detail, /no sync recorded/);
  assert.notEqual(widgetOverview(never, NOW_MS).level, "ok");
});

test("the widget headline surfaces stale-running, partial, alerts and degraded records", () => {
  const stale = widgetOverview(snapshot({ outcomes: block({ run: runWith({ outcome: "stale", stale: { detected: true } }) }) }), NOW_MS);
  assert.equal(stale.level, "fail");
  assert.equal(stale.title, "A run died without finishing");
  const unreadable = widgetOverview(snapshot({ outcomes: block({ alerts: { status: "unreadable" } }) }), NOW_MS);
  assert.equal(unreadable.title, "Failure alerts unavailable");
  const partial = widgetOverview(snapshot({
    outcomes: block({ profiles: [profileEntry("photos", { outcome: "partial", status_cache: "last_run_partial" })] })
  }), NOW_MS);
  assert.equal(partial.level, "warn");
  const degraded = widgetOverview(snapshot({ outcomes: block({ errors: [{ source: "alerts.state", code: "unsafe_file" }] }) }), NOW_MS);
  assert.equal(degraded.title, "Package record-keeping is degraded");
  const unknownOutcome = snapshot({ outcomes: block({ profiles: [profileEntry("photos", { outcome: "zorp", reason: "weird" })] }) });
  const [row] = widgetProfileRows(unknownOutcome, NOW_MS);
  assert.equal(row.outcome, "unknown (zorp)");
  assert.notEqual(row.level, "ok");
  assert.notEqual(widgetOverview(unknownOutcome, NOW_MS).level, "ok");
  const clean = widgetOverview(snapshot({
    routines: [{ profile: "photos", state: "succeeded", last_success_epoch: NOW - 60 }],
    outcomes: block({ profiles: [profileEntry("photos")] })
  }), NOW_MS);
  assert.equal(clean.level, "ok");
});

test("a package that reports no outcomes is never shown as healthy", () => {
  const legacy = snapshot({ routines: [{ profile: "photos", state: "succeeded", last_success_epoch: NOW - 60 }] });
  delete legacy.outcomes;
  const overview = widgetOverview(legacy, NOW_MS);
  assert.equal(overview.title, "Run outcomes unavailable");
  assert.notEqual(overview.level, "ok");
});

test("failed polls make every retained row stale and grey, with the failure kept", () => {
  const healthy = snapshot({
    routines: [{ profile: "photos", state: "succeeded", last_success_epoch: NOW - 60 }],
    outcomes: block({ profiles: [profileEntry("photos")] })
  });
  assert.equal(widgetProfileRows(healthy, NOW_MS)[0].level, "ok");
  const [stale] = widgetView(healthy, NOW_MS, { stale: true }).rows;
  assert.equal(stale.level, "unknown");
  assert.equal(stale.stale, true);
  assert.equal(stale.outcome, "Succeeded (stale)");
  assert.equal(widgetView(healthy, NOW_MS, { stale: true }).overview.title, "Status is stale");
  const failing = snapshot({ outcomes: block({ profiles: [profileEntry("photos", { outcome: "failed", reason: "timeout" })] }) });
  const [kept] = widgetView(failing, NOW_MS, { stale: true }).rows;
  assert.equal(kept.level, "fail");
  assert.match(kept.outcome, /\(stale\)$/);
});

function loadApp(exports = "") {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  const executable = script[1]
    .replace(/^import \{ ActionIcon \} from "\.\/ActionIcon";\s*/m, "")
    .replace(/^import \{ createAutosaveCoordinator \} from "\.\/autosave";\s*/m, "")
    .replace(/^import \{ installControlLayout \} from "\.\/controlLayout";\s*/m, "")
    .replace(/import \{[\s\S]*?\}\s*from "\.\/api";\s*/, "")
    .replace(/^import SecurityPanel from "\.\/SecurityPanel\.vue";\s*/m, "")
    .replace("export default {", "const AppComponent = {")
    + (exports ? `\nreturn { AppComponent, ${exports} };` : "\nreturn AppComponent;");
  const stubs = {
    WIDGET_ACTIVE_POLL_MS, WIDGET_BACKOFF_RAMP_MS, WIDGET_IDLE_POLL_MS,
    OUTCOMES_MISSING_ITEM: outcomesModule.OUTCOMES_MISSING_ITEM,
    outcomeAttentionItems: outcomesModule.attentionItems,
    outcomeFailureKey: outcomesModule.failureKey,
    cacheHealthNote: outcomesModule.cacheHealthNote,
    historyNote: outcomesModule.historyNote,
    resultDisplay: outcomesModule.resultDisplay,
    warningsSentence: outcomesModule.warningsSentence,
    normalizeOutcomes, outcomeLabel, outcomeLevel, profileOutcomeRow, reasonCopy,
    ACTIONS: {}, AUTOSAVE_API_LIMITS: Object.freeze({}), MAX_RESPONSE_BYTES: 1, PROGRESS_UNAVAILABLE: {},
    QueuedOutcomeUnknownError: class extends Error {}, SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1", SYNC_STATUS_MAX_LIMIT: 200,
    trustedRequestProgress: () => null, apiGet: async () => ({}), apiPost: async () => ({}),
    probeRequestOutcome: async () => ({}), purgeReconciliationAuth() {}, reconcileMutationRequest: async () => ({}),
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback),
    formatBytes: String, formatDate: (value) => `@${value}`, formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    createAutosaveCoordinator: () => ({}), installControlLayout: () => () => {},
    ActionIcon: {}, SecurityPanel: {}
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

function appContext(component, state) {
  const context = { toasts: [], lastOutcomeFailureKey: "", runStatus: "succeeded", lastRunDetail: "@1", ...state };
  context.toast = (title, message, error) => context.toasts.push({ title, message, error });
  for (const name of Object.keys(component.computed)) {
    if (Object.prototype.hasOwnProperty.call(context, name)) continue;
    Object.defineProperty(context, name, { get: () => component.computed[name].call(context), configurable: true });
  }
  for (const name of ["announceOutcomeFailure", "profileOutcome", "rollupTrustNote", "maybeNotifyFailure"]) {
    context[name] = (...args) => component.methods[name].call(context, ...args);
  }
  return context;
}

test("the AppWindow lists every attention item and never renders a stale or missing block as green", () => {
  const component = loadApp();
  const failing = snapshot({
    outcomes: block({
      run: runWith({ outcome: "failed", reason: "preflight_log_file_path" }),
      profiles: [profileEntry("photos", { outcome: "failed", reason: "network", status_cache: "last_run_failed", consecutive_failures: 5 })]
    })
  });
  const live = appContext(component, { snapshot: failing, connected: true, freshness: "Updated now" });
  assert.deepEqual(live.attentionItems.map((item) => item.id), ["run-failed", "profile-photos"]);
  assert.equal(live.lastResult.title, "Failed");
  assert.equal(live.lastResult.level, "fail");
  assert.match(live.lastResult.detail, /package paths mismatch/);
  assert.equal(live.profileOutcome("photos").level, "fail");
  assert.match(live.profileOutcome("photos").text, /failed 5 times in a row/);
  assert.match(live.rollupTrustNote("photos"), /^Last run failed/);
  assert.equal(live.rollupTrustNote("other"), "");

  const ok = appContext(component, {
    snapshot: snapshot({ outcomes: block({ profiles: [profileEntry("photos")] }) }), connected: true, freshness: "x"
  });
  assert.deepEqual(ok.attentionItems, []);
  assert.equal(ok.profileOutcome("photos").level, "ok");
  assert.equal(ok.lastResult.level, "ok");

  const stale = appContext(component, { snapshot: ok.snapshot, connected: false, freshness: "Stale · as of 3 min ago" });
  assert.equal(stale.retainedStale, true);
  assert.equal(stale.attentionItems[0].id, "stale-poll");
  assert.equal(stale.profileOutcome("photos").level, "unknown");
  assert.match(stale.profileOutcome("photos").text, /stale reading/);
  assert.equal(stale.lastResult.level, "unknown");
  assert.match(stale.lastResult.title, /\(stale\)/);
  const staleFail = appContext(component, { snapshot: failing, connected: false, freshness: "x" });
  assert.equal(staleFail.profileOutcome("photos").level, "fail");

  const missing = appContext(component, { snapshot: snapshot({ outcomes: undefined }), connected: true, freshness: "x" });
  assert.deepEqual(missing.attentionItems.map((item) => item.id), ["outcomes-missing"]);
  assert.equal(missing.lastResult.level, "unknown");
  assert.equal(missing.profileOutcome("photos").level, "unknown");
  const none = appContext(component, { snapshot: null, connected: false, freshness: "x" });
  assert.deepEqual(none.attentionItems, []);
});

test("a failure present on load is announced once per window with its specific detail", () => {
  const component = loadApp();
  const failing = snapshot({ outcomes: block({ run: runWith({ outcome: "failed", reason: "authentication" }) }) });
  const context = appContext(component, { snapshot: failing, connected: true, freshness: "x" });
  context.announceOutcomeFailure();
  assert.equal(context.toasts.length, 1);
  assert.equal(context.toasts[0].error, true);
  assert.equal(context.toasts[0].title, "Last run failed");
  assert.match(context.toasts[0].message, /rejected the login/);
  context.announceOutcomeFailure();
  assert.equal(context.toasts.length, 1, "the same failure must not re-announce every poll");
  context.snapshot = snapshot({ outcomes: block({ run: runWith({ outcome: "failed", reason: "network", finished_epoch: NOW }) }) });
  context.announceOutcomeFailure();
  assert.equal(context.toasts.length, 2, "a different failure announces again");
  const quiet = appContext(component, { snapshot: snapshot(), connected: true, freshness: "x" });
  quiet.announceOutcomeFailure();
  assert.equal(quiet.toasts.length, 0);
});

test("the Overview, Health and Activity templates render the outcome surfaces", () => {
  assert.equal((appSource.match(/class="sdsync-attention"/g) || []).length, 2, "Overview and Activity each list attention items");
  assert.match(appSource, /<strong :class="'is-' \+ lastResult\.level">\{\{ lastResult\.title \}\}<\/strong>/);
  assert.match(appSource, /profileOutcome\(profile\.name\)\.text/);
  assert.match(appSource, /v-if="row\.trust"/);
  assert.match(appSource, /<th>Last run<\/th>/);
  assert.match(appSource, /sdsync-page-frame" :class="\{ 'is-stale-data': retainedStale/);
  assert.match(appSource, /Local source validation failed" \+ named/);
});

test("run detail, the aggregate run and not-reached profiles are never hidden", () => {
  const items = (b) => attentionItems(normalizeOutcomes(b), NOW);
  const failed = items(block({ run: runWith({ outcome: "failed", reason: "network", detail: "error: listing /team failed (DSM 408)" }) }))[0];
  assert.match(failed.detail, /Detail: error: listing \/team failed \(DSM 408\)\?\./);
  const aggregate = { outcome: "failed", reason: "batch_target_failed:photos", finished_epoch: NOW - 3600, exit_code: 1 };
  const hidden = items(block({ run: runWith({ scope: "photos", aggregate }) }));
  assert.deepEqual(hidden.map((item) => item.id), ["aggregate"]);
  assert.equal(hidden[0].level, "fail");
  assert.match(hidden[0].detail, /stopped at this profile/);
  assert.deepEqual(items(block({ run: runWith({ scope: "all", aggregate }) })).map((item) => item.id), []);
  const skipped = items(block({ profiles: [profileEntry("docs", { outcome: "not_run", reason: "not_reached", outcome_epoch: NOW - 30 })] }));
  assert.deepEqual(skipped.map((item) => item.id), ["profile-docs"]);
  assert.equal(skipped[0].level, "warn");
  assert.match(skipped[0].detail, /earlier profile failed/);
  assert.match(statusCacheNote(normalizeOutcomes(block({ profiles: [profileEntry("docs", { outcome: "not_run", status_cache: "no_run_recorded" })] })).profiles[0]), /did not reach it/);
  const [row] = widgetProfileRows(snapshot({ outcomes: block({ profiles: [profileEntry("photos", { outcome: "not_run", reason: "not_reached" })] }) }), NOW_MS);
  assert.equal(row.level, "warn");
  assert.match(row.detail, /earlier profile failed/);
});

test("v1.2 fields: degraded records, routines, status-cache health, history, runner_died, partial counts", () => {
  const items = (b) => attentionItems(normalizeOutcomes(b), NOW);
  const degraded = items(block({ run: runWith({ degraded: { sinks: ["activity", "alert"], reason: "disk_full" } }) }))[0];
  assert.equal(degraded.id, "run-degraded");
  assert.match(degraded.detail, /activity, alert \(disk_full\)/);
  assert.equal(items(block({ run: runWith({ degraded: [] }) })).length, 0);
  const routines = items(block({ routines: [
    { profile: "a", defer_reason: "dependency", last_exit_code: null, last_reason: null },
    { profile: "b", defer_reason: "window", last_exit_code: 0, last_reason: null },
    { profile: "c", defer_reason: null, last_exit_code: 66, last_reason: "preflight_log_file_path" },
    { profile: "d", defer_reason: "zzz", last_exit_code: null, last_reason: null }
  ] }));
  assert.deepEqual(routines.map((item) => item.id), ["routine-a", "routine-c", "routine-d"]);
  assert.match(routines[1].detail, /package paths mismatch/);
  assert.match(routines[2].detail, /unknown \(zzz\)/);
  const health = items(block({ status_cache_health: { state: "degraded", issue_count: 1, issues: [{ code: "rollup-corrupt", severity: "warning", cause: "rollup file is corrupt" }] } }));
  assert.equal(health[0].id, "status-cache");
  assert.equal(outcomesModule.cacheHealthNote(normalizeOutcomes(block({ status_cache_health: { state: "degraded", issues: [{ severity: "warning", cause: "bad" }] } }))), "Status figures could not be read: bad");
  assert.equal(outcomesModule.cacheHealthNote(normalizeOutcomes(block({ status_cache_health: { state: "ok" } }))), "");
  assert.match(outcomesModule.cacheHealthNote(normalizeOutcomes(block({ status_cache_health: { state: "weird" } }))), /unknown \(weird\)/);
  const fmt = (e) => `@${e}`;
  assert.equal(outcomesModule.historyNote({ files: 3, oldest_record_epoch: 99, error: null }, fmt), "History starts @99 (3 log files); anything older was rotated away.");
  assert.match(outcomesModule.historyNote({ error: "unreadable" }, fmt), /unavailable \(unreadable\)/);
  assert.match(outcomesModule.historyNote(undefined, fmt), /not reported/);
  assert.match(outcomesModule.historyNote({ files: 0, oldest_record_epoch: null }, fmt), /No activity records/);
  assert.match(reasonCopy("runner_died").text, /runner died/);
  assert.equal(reasonCopy("source-changed").known, true);
  assert.match(reasonCopy("listing-incomplete").text, /deletions in those folders are withheld/);
  assert.match(reasonCopy("authentication").text, /one-time code/);
  for (const tag of ["deferred", "link-or-reparse-point", "local-case-collision", "remote-case-collision", "case-rename-needs-delete",
    "source-changed", "source-vanished", "source-unreadable", "remote-digest-unavailable", "remote-changed", "server-copy-failed",
    "delete-failed", "unrepresentable-name", "unsupported-local-entry"]) assert.equal(reasonCopy(tag).known, true, tag);
  assert.match(reasonCopy("dehydrated-placeholder").text, /Always keep on this device/);
  assert.match(reasonCopy("windows-attributes").text, /SYSTEM or TEMPORARY/);
  assert.equal(reasonCopy("listing-incomplete").known, true);
  const partial = items(block({ run: runWith({ outcome: "partial", reason: "source-changed", deferred_files: 4, skipped_files: 0 }) }))[0];
  assert.equal(partial.level, "warn");
  assert.match(partial.detail, /4 files deferred or skipped \(files changed/);
  assert.match(items(block({ run: runWith({ outcome: "partial" }) }))[0].detail, /unrecorded number/);
  // Persistent banner: failure with a failure streak, and an aggregate failure survives a later single-profile run.
  const streak = items(block({ run: runWith({ outcome: "failed", reason: "network" }), alerts: { status: "ok", failure_count: 4, failure_threshold: 3, threshold_reached: true } }));
  assert.equal(streak[0].title, "Last run failed, 4 failures in a row");
  const aggKey = failureKey(normalizeOutcomes(block({ run: runWith({ scope: "x", aggregate: { outcome: "failed", reason: "network", finished_epoch: 5 } }) })));
  assert.match(aggKey, /^aggregate:5:network$/);
});

test("a succeeded record with warnings is never plain success", () => {
  const warned = block({
    run: runWith({ warnings: ["skipped_entries"], skipped_files: 3 }),
    profiles: [profileEntry("photos", { warnings: ["skipped_entries", "deferred_entries", "novel"], skipped_files: 3, deferred_files: 2, status_cache: "skipped_entries" })]
  });
  const normalized = normalizeOutcomes(warned);
  const items = attentionItems(normalized, NOW);
  assert.deepEqual(items.map((item) => item.id), ["run-warnings", "profile-photos"]);
  assert.equal(items[0].level, "warn");
  assert.match(items[0].detail, /3 entries skipped and will not sync until fixed/);
  assert.match(items[1].detail, /2 entries could not be synced and will be left out until fixed/);
  assert.match(items[1].detail, /unknown warning \(novel\)/);
  const row = profileOutcomeRow(normalized.profiles[0], NOW);
  assert.equal(row.label, "Succeeded with warnings");
  assert.equal(row.level, "warn");
  assert.match(row.note, /^Last run synced with warnings: /);
  assert.equal(outcomesModule.resultDisplay(normalized.run).level, "warn");
  assert.equal(outcomesModule.resultDisplay({ outcome: "succeeded", warnings: [] }).level, "ok");
  assert.equal(outcomesModule.warningsSentence({ warnings: ["skipped_entries"], skipped: 1 }), "1 entry skipped and will not sync until fixed");
  assert.equal(outcomesModule.warningsSentence({ warnings: ["keepalive_failures"], recovered: { keepaliveFailures: 1 } }), "Recovered from 1 keep-alive failure; check the network if it repeats");
  // The widget row and headline follow.
  const snap = snapshot({ routines: [{ profile: "photos", state: "succeeded", last_success_epoch: NOW - 60 }], outcomes: warned });
  const [wrow] = widgetProfileRows(snap, NOW_MS);
  assert.equal(wrow.level, "warn");
  assert.equal(wrow.outcome, "Succeeded with warnings");
  assert.equal(widgetOverview(snap, NOW_MS).level, "warn");
  // AppWindow Last result.
  const ctx = appContext(loadApp(), { snapshot: snap, connected: true, freshness: "x" });
  assert.equal(ctx.lastResult.level, "warn");
  assert.equal(ctx.lastResult.title, "Succeeded with warnings");
  // Partial names the deferred count and stays a warning.
  const partial = attentionItems(normalizeOutcomes(block({ run: runWith({ outcome: "partial", deferred_files: 2, reason: "source-changed" }) })), NOW)[0];
  assert.equal(partial.level, "warn");
  assert.match(partial.detail, /2 files deferred or skipped/);
  // core_partial is a failure.
  assert.equal(attentionItems(normalizeOutcomes(block({ run: runWith({ outcome: "failed", reason: "core_partial" }) })), NOW)[0].level, "fail");
});

test("v1.5 result facts render as warn lines on the run and on profiles", () => {
  const facts = {
    warnings: ["incomplete_listings", "mtime_not_kept", "reauthentication_failures", "server_tasks_not_stopped", "keepalive_failures", "novel_fact"],
    incomplete_listings: 7, incomplete_listing_folders: [".", "photos/2026\u0001"], mtime_not_kept: 12,
    recovered: { reauthentication_failures: 2, server_tasks_not_stopped: 1, keepalive_failures: 1 }
  };
  const normalized = normalizeOutcomes(block({ run: runWith(facts), profiles: [profileEntry("photos", facts)] }));
  const item = attentionItems(normalized, NOW).find((candidate) => candidate.id === "run-warnings");
  assert.equal(item.level, "warn");
  assert.match(item.detail, /Listing incomplete in 7 folders \(the destination root, photos\/2026\?, \.\.\.\): deletes there were held back/);
  assert.match(item.detail, /12 uploads lost their modification time; the next run will upload them again/);
  assert.match(item.detail, /2 re-login failures/);
  assert.match(item.detail, /1 server-side task could not be stopped|1 server-side task/);
  assert.match(item.detail, /Recovered from 1 keep-alive failure/);
  assert.match(item.detail, /unknown warning \(novel_fact\)/);
  const row = profileOutcomeRow(normalized.profiles[0], NOW);
  assert.equal(row.level, "warn");
  assert.equal(row.label, "Succeeded with warnings");
  assert.match(row.detail, /12 uploads lost/);
  // On a failed run the facts are still shown.
  const failed = attentionItems(normalizeOutcomes(block({ run: runWith({ outcome: "failed", warnings: ["mtime_not_kept"], mtime_not_kept: 1 }) })), NOW);
  assert.ok(failed.some((candidate) => candidate.id === "run-warnings-extra"));
  assert.doesNotThrow(() => normalizeOutcomes(block({ run: runWith({ recovered: "x", incomplete_listing_folders: [1, null, {}] }) })));
});

test("remote-unreadable and deferred_paths have copy; unrecorded counts are never shown as zero; Doctor surfaces skipped entries", () => {
  assert.match(reasonCopy("remote-unreadable").text, /Grant the sync account read permission/);
  assert.equal(reasonCopy("deferred_paths").known, true);
  const unrecorded = attentionItems(normalizeOutcomes(block({ run: runWith({ outcome: "partial", counts_available: false, reason: "deferred_paths" }) })), NOW)[0];
  assert.match(unrecorded.detail, /Entry counts were not recorded for this run/);
  assert.doesNotMatch(unrecorded.detail, /0 files?/);
  assert.equal(normalizeOutcomes(block({ run: runWith({ counts_available: "yes" }) })).run.countsAvailable, null);
  const unavailable = appContext(loadApp(), { snapshot: snapshot({ outcomes: block({ run: runWith({ counts_available: false }) }) }), connected: true, freshness: "x" });
  assert.match(unavailable.lastResult.detail, /entry counts unavailable for this run/);
  const unknownCounts = appContext(loadApp(), { snapshot: snapshot({ outcomes: block({ run: runWith({ counts_available: null }) }) }), connected: true, freshness: "x" });
  assert.doesNotMatch(unknownCounts.lastResult.detail, /counts/);
  assert.match(appSource, /id: "package_source_skipped",\s*label: "Entries a sync will skip",\s*state: "warn"/);
});

test("real `doctor source` output with skipped entries becomes a warning section, never a pass", async () => {
  // Captured from the built core: a SYSTEM-attribute file and a junction in the source.
  const raw = await readFile(new URL("./fixtures/doctor-source-skipped.json", import.meta.url), "utf8");
  const parsed = JSON.parse(raw);
  assert.equal(parsed.schema, "sdsync.source-doctor.v1");
  assert.deepEqual(parsed.source.skipped.map((entry) => entry.reason).sort(), ["link-or-reparse-point", "windows-attributes"]);
  const { doctorSourceSkippedDetail, doctorReportFromResult } = loadApp("doctorSourceSkippedDetail, doctorReportFromResult");
  for (const text of [raw, JSON.stringify(parsed)]) {
    const detail = doctorSourceSkippedDetail(text);
    assert.match(detail, /^2 entries a sync will skip and not sync until fixed: /);
    assert.match(detail, /junc · link-or-reparse-point/);
    assert.match(detail, /sysfile\.txt · windows-attributes/);
  }
  assert.equal(doctorSourceSkippedDetail(JSON.stringify({ schema: "sdsync.source-doctor.v1", source: { skipped: [] } })), "");
  assert.equal(doctorSourceSkippedDetail("not json"), "");
  const report = doctorReportFromResult({ output: JSON.stringify(parsed) }, true, "standard", false);
  const section = report.sections.find((candidate) => candidate.id === "package_source_skipped");
  assert.ok(section, "the skipped-entries section is missing");
  assert.equal(section.state, "warn");
  assert.notEqual(report.state, "ok");
});
