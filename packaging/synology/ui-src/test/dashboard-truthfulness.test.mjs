// Round-2 adversarial findings against the DSM dashboard (ADV-U-3 .. ADV-U-6, ADV-S-2,
// and the cheap S3s). Each test fails on the pre-fix code; the finding ids are in the names.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { appContext, appSource, loadApp } from "./helpers/load-app.mjs";
import { DsmApiError } from "../src/api.js";
import { attentionItems, normalizeOutcomes } from "../src/outcomes.mjs";
import {
  WIDGET_BACKOFF_RAMP_MS,
  WIDGET_IDLE_POLL_MS,
  widgetBridgeIssue,
  widgetOverview,
  widgetRun,
  widgetView
} from "../src/widgetModel.mjs";

globalThis.window ??= globalThis;
globalThis.document ??= { hidden: false };

const NOW = 1_700_000_000;
const NOW_MS = NOW * 1000;

function block(over = {}) {
  return {
    schema: "sdsync.dsm-outcomes.v1",
    run: { outcome: "succeeded", reason: null, scope: "all", operation: "sync", started_epoch: NOW - 100, finished_epoch: NOW - 60, exit_code: 0, stale: { detected: false, lock: "absent" } },
    alerts: { status: "ok", failure_count: 0, failure_threshold: 1, threshold_reached: false },
    profiles: [{ profile: "photos", outcome: "succeeded", status_cache: "trusted", finished_epoch: NOW - 60 }],
    routines: [],
    status_cache_health: { state: "ok", issues: [] },
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
    routines: [{ profile: "photos", state: "succeeded", last_success_epoch: NOW - 60 }],
    outcomes,
    ...over
  };
}

const staleOutcomes = () => block({
  run: { outcome: "stale", reason: "runner_died", state: "running", scope: "all", operation: "sync", started_epoch: NOW - 900, stale: { detected: true, lock: "dead" } },
  profiles: [{ profile: "photos", outcome: "succeeded", status_cache: "last_run_failed", reason: "runner_died", finished_epoch: NOW - 4000 }]
});
const staleSnapshot = () => snapshot(staleOutcomes(), { run: { state: "running", scope: "all", operation: "sync" } });

// ---------------------------------------------------------------------------
// ADV-U-3: the stale verdict wins everywhere
// ---------------------------------------------------------------------------

test("ADV-U-3 a dead runner is not 'Sync running' on the widget, in its rows, or in its poll cadence", () => {
  const run = widgetRun(staleSnapshot());
  assert.equal(run.active, false);
  assert.equal(run.label, "");
  const view = widgetView(staleSnapshot(), NOW_MS);
  assert.equal(view.overview.level, "fail");
  assert.match(view.overview.title, /died/i);
  assert.equal(view.rows[0].running, false);
  assert.notEqual(view.rows[0].outcome, "Running");
  assert.equal(view.rows[0].level, "fail", "a profile covered by a dead run is not shown as healthy or running");
});

test("ADV-U-3 a live run still reads as running, with or without an outcomes block", () => {
  const live = snapshot(block({ run: { outcome: "running", scope: "all", operation: "sync", stale: { detected: false, lock: "live" } } }), { run: { state: "running", scope: "all", operation: "sync" } });
  assert.equal(widgetRun(live).active, true);
  assert.equal(widgetView(live, NOW_MS).rows[0].outcome, "Running");
  const legacy = snapshot(undefined, { run: { state: "running", scope: "all", operation: "sync" } });
  assert.equal(widgetRun(legacy).active, true);
});

test("ADV-U-3 the AppWindow's Overview tiles say the run died, not that it is running", async () => {
  const component = await loadApp();
  const ctx = appContext(component, { snapshot: staleSnapshot(), connected: true, freshness: "x" });
  assert.equal(ctx.runActive, false);
  assert.equal(ctx.runStatus, "stale");
  assert.equal(ctx.attentionItems[0].id, "run-stale");
  assert.equal(ctx.profileOutcome("photos").level, "fail");
  assert.match(appSource, /runActive \? runScope : 'Idle'/);
  assert.doesNotMatch(appSource, /runStatus === 'running' \? runScope/);
});

// ---------------------------------------------------------------------------
// ADV-U-4: a block of the wrong shape or schema is "status unavailable", never healthy
// ---------------------------------------------------------------------------

const MALFORMED = {
  "empty object": {},
  "another schema": block({ schema: "sdsync.dsm-outcomes.v2" }),
  "no schema": (() => { const copy = block(); delete copy.schema; return copy; })(),
  "run.outcome null": block({ run: { outcome: null } }),
  "run.outcome number": block({ run: { outcome: 5 } }),
  "run.outcome array": block({ run: { outcome: ["failed"] } }),
  "no run": (() => { const copy = block(); delete copy.run; return copy; })(),
  "no profiles list": (() => { const copy = block(); delete copy.profiles; return copy; })(),
  "profiles not a list": block({ profiles: { photos: {} } }),
  "no alerts": (() => { const copy = block(); delete copy.alerts; return copy; })()
};

for (const [name, outcomes] of Object.entries(MALFORMED)) {
  test(`ADV-U-4 outcomes with ${name} reads as status unavailable, in the widget and the AppWindow`, async () => {
    assert.equal(normalizeOutcomes(outcomes), null);
    const overview = widgetOverview(snapshot(outcomes), NOW_MS);
    assert.notEqual(overview.level, "ok");
    assert.equal(overview.title, "Run outcomes unavailable");
    assert.match(overview.detail, /^Status unavailable: .+/);
    const component = await loadApp();
    const ctx = appContext(component, { snapshot: snapshot(outcomes), connected: true, freshness: "x" });
    assert.deepEqual(ctx.attentionItems.map((item) => item.id), ["outcomes-missing"]);
    assert.match(ctx.attentionItems[0].detail, /^Status unavailable: /);
    assert.equal(ctx.lastResult.level, "unknown");
  });
}

test("ADV-U-4 a profile whose outcome is the wrong type is unknown, not 'not run'", () => {
  const outcomes = block({ profiles: [{ profile: "photos", outcome: { x: 1 }, status_cache: "trusted" }] });
  const normalized = normalizeOutcomes(outcomes);
  assert.equal(normalized.profiles[0].outcome, "unknown");
  const items = attentionItems(normalized, NOW);
  assert.ok(items.some((item) => item.id === "profile-photos" && item.level !== "ok"));
});

test("ADV-U-4 an absent block keeps its distinct wording and a valid block still reads healthy", () => {
  const absent = widgetOverview(snapshot(undefined), NOW_MS);
  assert.equal(absent.title, "Run outcomes unavailable");
  assert.match(absent.detail, /^Status unavailable: /);
  assert.equal(widgetOverview(snapshot(block()), NOW_MS).level, "ok");
});

// ---------------------------------------------------------------------------
// ADV-U-5: notifications
// ---------------------------------------------------------------------------

function notifier(initial = {}) {
  const sent = [];
  globalThis.Notification = class { constructor(title) { sent.push(title); } static permission = "granted"; };
  window.Notification = globalThis.Notification;
  return { sent, state: { settings: { desktop_notifications: true, audible: false }, lastFailureKey: "", lastOutcomeFailureKey: "", failureBaselineSeen: false, connected: true, freshness: "x", playCue() {}, ...initial } };
}

const failedOutcomes = (finished, reason = "core_failed") => block({ run: { outcome: "failed", reason, scope: "all", operation: "sync", started_epoch: finished - 5, finished_epoch: finished, exit_code: 1, stale: {} } });
const failedSnapshot = (finished) => snapshot(failedOutcomes(finished), { run: { state: "failed", scope: "all", operation: "sync", finished_epoch: finished, exit_code: 1 } });

test("ADV-U-5 the first failure after a clean baseline notifies once, and a repeat poll does not", async () => {
  const component = await loadApp();
  const { sent, state } = notifier();
  const ctx = appContext(component, { ...state, snapshot: snapshot(block()) });
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 0, "a clean baseline is silent");
  ctx.snapshot = failedSnapshot(NOW + 600);
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 1, "the first failure must notify");
  ctx.maybeNotifyFailure();
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 1, "polling the same failure again must not notify");
  ctx.snapshot = failedSnapshot(NOW + 1200);
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 2, "a new failure notifies");
});

test("ADV-U-5 a failure already on screen when the window opens is the baseline, not a notification", async () => {
  const component = await loadApp();
  const { sent, state } = notifier();
  const ctx = appContext(component, { ...state, snapshot: failedSnapshot(NOW) });
  ctx.maybeNotifyFailure();
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 0);
  assert.equal(ctx.toasts.filter((toast) => toast.error).length, 1, "the toast still announces it once");
});

test("ADV-U-5 a dead runner and an unknown outcome notify like any failure", async () => {
  const component = await loadApp();
  const { sent, state } = notifier();
  const ctx = appContext(component, { ...state, snapshot: snapshot(block()) });
  ctx.maybeNotifyFailure();
  ctx.snapshot = staleSnapshot();
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 1, "stale must notify");
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 1);
  ctx.snapshot = snapshot(block({ run: { outcome: "unknown", scope: "all", stale: {}, started_epoch: NOW + 5000 } }));
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 2, "unknown must notify");
});

test("ADV-U-5 a partial run does not notify", async () => {
  const component = await loadApp();
  const { sent, state } = notifier();
  const ctx = appContext(component, { ...state, snapshot: snapshot(block()) });
  ctx.maybeNotifyFailure();
  ctx.snapshot = snapshot(block({ run: { outcome: "partial", reason: "deferred_paths", scope: "all", stale: {}, finished_epoch: NOW + 50 } }));
  ctx.maybeNotifyFailure();
  assert.equal(sent.length, 0);
});

test("ADV-U-15 a notification that throws is reported and never turns a good snapshot into 'bridge unavailable'", async () => {
  globalThis.Notification = class { constructor() { throw new Error("notifications are blocked by policy"); } static permission = "granted"; };
  window.Notification = globalThis.Notification;
  const component = await loadApp({
    overrides: { apiGet: async () => snapshot(failedOutcomes(NOW + 99), { run: { state: "failed", scope: "all", operation: "sync", finished_epoch: NOW + 99, exit_code: 1 } }) }
  });
  const ctx = appContext(component, {
    settings: { desktop_notifications: true, audible: false }, lastFailureKey: "", lastOutcomeFailureKey: "", failureBaselineSeen: true,
    disposed: false, snapshotRefreshBlocked: false, snapshotPromise: null, snapshotGeneration: 0, snapshotLoading: false, snapshotRefreshQueued: false,
    csrfToken: "token", auth: {}, snapshot: snapshot(block()), snapshotSignature: "", snapshotReceivedAtMs: 0, snapshotFailureCode: "", snapshotFailureStreak: 0,
    connected: false, connectionLabel: "", freshness: "", bridgeIssue: { title: "", message: "" },
    hydrateAlerts() {}, hydrateSecurityPolicy() {}, scheduleSnapshot() {}, playCue() {}
  });
  const succeeded = await ctx.refreshSnapshot(false);
  assert.equal(succeeded, true);
  assert.ok(ctx.applyError === "", "a notification failure is a toast, not a persistent apply error");
  assert.equal(ctx.connected, true);
  assert.equal(ctx.bridgeIssue.title, "");
  assert.ok(ctx.toasts.some((toast) => toast.error && /notification/i.test(toast.title + toast.message) && /blocked by policy/.test(toast.message)), JSON.stringify(ctx.toasts));
});

// ---------------------------------------------------------------------------
// ADV-U-6: the cause of a failed read is shown
// ---------------------------------------------------------------------------

test("ADV-U-6 every failed read shows its status, code and message instead of 'could not be reached'", async () => {
  const component = await loadApp();
  const ctx = appContext(component, {});
  const trusted = (status, code, message) => Object.assign(new DsmApiError(message, status, code), { trustedRejection: true });
  const cases = [
    [trusted(500, "internal_error", "internal error while composing the snapshot"), [/500/, /internal_error/, /composing the snapshot/]],
    [trusted(429, "rate_limited", "too many requests"), [/429/, /rate_limited/, /too many requests/]],
    [new DsmApiError("API did not return JSON", 502, "non_json_response"), [/502/, /non_json_response/]],
    [new DsmApiError("API response exceeded the client limit", 200, "response_too_large"), [/response_too_large/, /exceeded the client limit/]],
    [new Error("API returned forbidden secret material"), [/forbidden secret material/]],
    [new Error("API response is too complex"), [/too complex/]],
    [Object.assign(new DsmApiError("timed out", 0, "client_timeout"), { clientTimeout: true }), [/client_timeout/]],
    [new TypeError("Failed to fetch"), [/Failed to fetch/]],
    [trusted(503, "manager_busy", "the package helper is busy"), [/503/, /manager_busy/]]
  ];
  for (const [error, patterns] of cases) {
    const issue = ctx.describeBridgeError(error, "status");
    const shown = `${issue.title} ${issue.message}`;
    for (const pattern of patterns) assert.match(shown, pattern, `${error.message}: ${shown}`);
  }
  const serverAnswered = ctx.describeBridgeError(trusted(500, "internal_error", "boom"), "status");
  assert.doesNotMatch(serverAnswered.message, /could not be reached/, "the server did answer; do not claim it was unreachable");
  const unreachable = ctx.describeBridgeError(new TypeError("Failed to fetch"), "status");
  assert.match(unreachable.message, /could not be reached/);
});

test("ADV-U-6 an untrusted server message is not rendered, only its status and code", async () => {
  const component = await loadApp();
  const ctx = appContext(component, {});
  const issue = ctx.describeBridgeError(new DsmApiError("<script>alert(1)</script> rm -rf", 500, "http_500"), "status");
  assert.doesNotMatch(`${issue.title} ${issue.message}`, /script|rm -rf/);
  assert.match(issue.message, /500/);
});

test("ADV-U-6 the widget names the status and code too, without breaking its titles", () => {
  const issue = widgetBridgeIssue(Object.assign(new DsmApiError("boom", 500, "internal_error"), { trustedRejection: true }));
  assert.match(issue.detail, /500/);
  assert.match(issue.detail, /internal_error/);
  assert.ok(issue.detail.length <= 110, "bounded for a 318px card");
  assert.equal(widgetBridgeIssue({ status: 503, code: "manager_busy" }).title, "Package is busy");
  assert.equal(widgetBridgeIssue({ status: 401, code: "x" }).title, "DSM session expired");
});

// ---------------------------------------------------------------------------
// ADV-S-2 / ADV-S-21: a dead Activity feed is not an empty one
// ---------------------------------------------------------------------------

function feedContext(component, over = {}) {
  return appContext(component, {
    disposed: false, logsLoading: false, logsPaused: false, route: "activity", activityTab: "events", logLines: 200, logSource: "all",
    auth: {}, activityEvents: [], activityHistory: null, logRecords: [], logOutput: "No log data yet.", logState: "", logsReceivedAtMs: 0, activityReceivedAtMs: 0,
    activityFeedError: "", logsFeedError: "", activitySearchDebounced: "", activityCategory: "all", activityLevel: "all",
    scheduleLogs() {}, ...over
  });
}

test("ADV-S-2 a failed first Activity read says why, on every tab, and never claims nothing was recorded", async () => {
  const failing = new DsmApiError("activity log is unreadable", 503, "corrupt_state");
  const component = await loadApp({ overrides: { apiGet: async () => { throw Object.assign(failing, { trustedRejection: true }); } } });
  for (const tab of ["events", "changes", "operations", "service", "package-logs"]) {
    const ctx = feedContext(component, { activityTab: tab });
    await ctx.refreshLogs();
    assert.match(ctx.activityFeedError, /503/, tab);
    assert.match(ctx.activityFeedError, /corrupt_state/, tab);
    assert.match(ctx.feedProblem, /corrupt_state/, `${tab}: the problem must be visible outside the Package logs panel`);
    assert.doesNotMatch(ctx.activityEmptyText, /have been recorded yet/, tab);
    assert.match(ctx.activityEmptyText, /could not be read/, tab);
  }
});

test("ADV-S-2 the feed problem is rendered above the Activity tabs, not inside the Package logs tab", () => {
  const section = appSource.slice(appSource.indexOf("route === 'activity'"));
  const tabs = section.indexOf("data-subtabs=\"activity\"");
  const problem = section.indexOf("feedProblem");
  assert.ok(problem > 0 && problem < tabs, "feedProblem must precede the activity sub-tabs");
});

test("ADV-S-2 a successful empty read still says nothing was recorded, and a pending first read says it is loading", async () => {
  const empty = await loadApp({ overrides: { apiGet: async () => ({ schema: "sdsync.dsm-activity.v1", events: [] }) } });
  const ctx = feedContext(empty);
  assert.match(ctx.activityEmptyText, /Reading package events/);
  await ctx.refreshLogs();
  assert.equal(ctx.activityFeedError, "");
  assert.equal(ctx.feedProblem, "");
  assert.match(ctx.activityEmptyText, /No package events have been recorded yet/);
});

test("ADV-U-4 an Activity document with the wrong schema or no events list is a failure, not an empty feed", async () => {
  for (const document of [{ schema: "sdsync.dsm-activity.v2", events: [] }, { events: [] }, { schema: "sdsync.dsm-activity.v1" }, { schema: "sdsync.dsm-activity.v1", events: "none" }]) {
    const component = await loadApp({ overrides: { apiGet: async () => document } });
    const ctx = feedContext(component);
    await ctx.refreshLogs();
    assert.match(ctx.activityFeedError, /activity/i, JSON.stringify(document));
    assert.match(ctx.activityFeedError, /schema|events/i);
    assert.equal(ctx.activityReceivedAtMs, 0, "a malformed document is not a receipt");
  }
});

test("ADV-U-4 a Logs document with the wrong schema is a failure on the Package logs tab", async () => {
  const component = await loadApp({
    overrides: { apiGet: async (_auth, action) => (action === "logs" ? { schema: "sdsync.dsm-logs.v9", logs: [] } : { schema: "sdsync.dsm-activity.v1", events: [] }) }
  });
  const ctx = feedContext(component, { activityTab: "package-logs" });
  await ctx.refreshLogs();
  assert.match(ctx.logsFeedError, /schema/i);
  assert.match(ctx.feedProblem, /package log/i);
});

test("ADV-S-2 a feed that recovers clears its problem", async () => {
  let fail = true;
  const component = await loadApp({ overrides: { apiGet: async () => { if (fail) throw new Error("API did not return JSON"); return { schema: "sdsync.dsm-activity.v1", events: [] }; } } });
  const ctx = feedContext(component);
  await ctx.refreshLogs();
  assert.notEqual(ctx.feedProblem, "");
  fail = false;
  await ctx.refreshLogs();
  assert.equal(ctx.feedProblem, "");
  assert.equal(ctx.activityFeedError, "");
});

// ---------------------------------------------------------------------------
// S3s
// ---------------------------------------------------------------------------

test("ADV-U-7 relative ages follow a clock tick instead of freezing at 'moments ago'", async () => {
  const component = await loadApp();
  const failing = snapshot(failedOutcomes(NOW - 10), { generated_at_epoch: NOW });
  const ctx = appContext(component, { snapshot: failing, connected: true, freshness: "x", nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS });
  assert.match(ctx.attentionItems[0].detail, /moments ago/);
  ctx.nowMs = NOW_MS + 6 * 3600 * 1000;
  assert.match(ctx.attentionItems[0].detail, /6 h ago/);
  assert.match(ctx.profileOutcome("photos").text, /Last run/);
});

test("ADV-U-12 ages are measured on the package's clock, not the browser's", async () => {
  const component = await loadApp();
  const failing = snapshot(failedOutcomes(NOW - 7200), { generated_at_epoch: NOW });
  // The browser clock is two hours fast; the package says the failure was two hours before *its* now.
  const ctx = appContext(component, { snapshot: failing, connected: true, freshness: "x", nowMs: NOW_MS + 7200 * 1000, snapshotReceivedAtMs: NOW_MS + 7200 * 1000 });
  assert.match(ctx.attentionItems[0].detail, /2 h ago/);
  const skewed = appContext(component, { snapshot: failedSnapshot(NOW - 10), connected: true, freshness: "x", nowMs: NOW_MS - 7200 * 1000, snapshotReceivedAtMs: NOW_MS - 7200 * 1000 });
  assert.match(skewed.attentionItems[0].detail, /moments ago/);
});

test("ADV-U-8 the stale-reading text keeps ageing after refresh has stopped", async () => {
  const component = await loadApp();
  const ctx = appContext(component, {
    snapshot: snapshot(block()), connected: false, nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS - 30_000, staleFreshnessCode: "manager_unsafe",
    freshness: "Stale · as of less than a minute ago; the package could not refresh it.", disposed: false
  });
  ctx.tickClock(NOW_MS + 8 * 3600 * 1000);
  assert.equal(ctx.nowMs, NOW_MS + 8 * 3600 * 1000);
  assert.doesNotMatch(ctx.freshness, /less than a minute/);
  assert.match(ctx.freshness, /Unanswered|8 h|hour/);
  const live = appContext(component, { snapshot: snapshot(block()), connected: true, nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS, staleFreshnessCode: null, freshness: "Updated 12:00:00", disposed: false });
  live.tickClock(NOW_MS + 60_000);
  assert.equal(live.freshness, "Updated 12:00:00", "a live reading keeps its own text");
});

test("ADV-U-9 the widget does not turn a routine that is waiting for its window or a busy lock amber", () => {
  const waiting = (reason) => snapshot(
    block({ routines: [{ profile: "photos", defer_reason: reason, last_exit_code: null, last_reason: null }] }),
    { routines: [{ profile: "photos", state: "deferred", last_success_epoch: NOW - 60 }] }
  );
  for (const reason of ["window", "busy"]) {
    const view = widgetView(waiting(reason), NOW_MS);
    assert.notEqual(view.rows[0].level, "warn", reason);
    assert.notEqual(view.overview.level, "warn", reason);
    assert.doesNotMatch(view.overview.title, /need attention/, reason);
  }
  for (const reason of ["dependency", "source"]) {
    const view = widgetView(waiting(reason), NOW_MS);
    assert.equal(view.rows[0].level, "warn", reason);
    assert.match(view.rows[0].detail, /\S/);
    assert.doesNotMatch(view.rows[0].detail, /unknown/, reason);
  }
});

test("ADV-U-10 an identical Activity poll does not replace the feed array or re-normalise it", async () => {
  const events = Array.from({ length: 50 }, (_, index) => ({ epoch: NOW + index, code: "run.finished", profile: "photos", category: "operations", level: "info", state: "ok", message: `m${index}` }));
  const component = await loadApp({ overrides: { apiGet: async () => ({ schema: "sdsync.dsm-activity.v1", events: JSON.parse(JSON.stringify(events)) }) } });
  const ctx = feedContext(component);
  await ctx.refreshLogs();
  const first = ctx.activityEvents;
  assert.equal(first.length, 50);
  await ctx.refreshLogs();
  assert.equal(ctx.activityEvents, first, "byte-identical feed must keep the same array so computeds are not invalidated");
  assert.ok(ctx.activityReceivedAtMs > 0);
});

test("ADV-U-11 unnamed failures back off after the second consecutive one, and a hidden window slows to the idle cadence", async () => {
  const component = await loadApp();
  const delay = (state, interval = 5000) => appContext(component, state).snapshotRetryDelay(interval);
  const base = { snapshotFailureCode: "", windowHidden: false };
  assert.equal(delay({ ...base, snapshotFailureStatus: 401, snapshotFailureStreak: 1 }), 5000, "one failed read is not a verdict");
  assert.equal(delay({ ...base, snapshotFailureStatus: 401, snapshotFailureStreak: 2 }), WIDGET_BACKOFF_RAMP_MS[0]);
  assert.equal(delay({ ...base, snapshotFailureStatus: 0, snapshotFailureStreak: 4 }), WIDGET_BACKOFF_RAMP_MS[2]);
  assert.equal(delay({ ...base, snapshotFailureStatus: 502, snapshotFailureStreak: 9 }), WIDGET_BACKOFF_RAMP_MS[3]);
  assert.equal(delay({ ...base, snapshotFailureStatus: 400, snapshotFailureStreak: 9 }), 5000, "a request error is not a transport outage");
  assert.equal(delay({ ...base, snapshotFailureStatus: 0, snapshotFailureStreak: 0 }), 5000, "healthy cadence is untouched");
  assert.equal(delay({ ...base, windowHidden: true, snapshotFailureStatus: 0, snapshotFailureStreak: 0 }), WIDGET_IDLE_POLL_MS);
  assert.equal(delay({ ...base, snapshotFailureCode: "manager_unsafe", windowHidden: true, snapshotFailureStatus: 503, snapshotFailureStreak: 1 }), 0, "a stop code still stops");
});

test("ADV-U-14 the freshness text is not a live region that re-announces on every poll", () => {
  assert.doesNotMatch(appSource, /class="sdsync-freshness" aria-live="polite">\{\{ freshness \}\}/);
  assert.match(appSource, /<span aria-live="polite">\{\{ connectionLabel \}\}<\/span>/, "connection transitions are still announced");
});

test("ADV-U-13 names with bidi and zero-width controls are shown escaped, not rendered", async () => {
  const { visibleText } = await import("../src/textSafety.mjs");
  // The escaped form is a backslash, "u" and the braced code point; built here so no
  // invisible or escape-looking text sits in this source file.
  const ESC = (hex) => String.fromCharCode(92) + "u{" + hex + "}";
  const RLO = String.fromCharCode(0x202e);
  const ZWSP = String.fromCharCode(0x200b);
  const BOM = String.fromCharCode(0xfeff);
  assert.equal(visibleText("photos"), "photos");
  assert.equal(visibleText(`a${RLO}b`), "a" + ESC("202E") + "b");
  assert.equal(visibleText(`a${ZWSP}b`), "a" + ESC("200B") + "b");
  assert.equal(visibleText(`${BOM}name`), "" + ESC("FEFF") + "name");
  assert.equal(visibleText("  padded  "), "padded", "leading and trailing spaces are not significant to a reader");
  const hostile = normalizeOutcomes(block({ profiles: [{ profile: `ph${RLO}otos`, outcome: "failed", reason: "core_failed", status_cache: "last_run_failed" }] }));
  const items = attentionItems(hostile, NOW);
  assert.ok(items.some((item) => item.title.includes(ESC("202E"))), JSON.stringify(items));
  assert.ok(!items.some((item) => (item.title + item.detail).includes(RLO)), "names are shown escaped in every rendered field");
  const view = widgetView(snapshot(block(), { profiles: [{ name: `ph${RLO}otos`, health: {} }], routines: [] }), NOW_MS);
  assert.ok(view.rows[0].displayName.includes(ESC("202E")));
  assert.ok(!view.rows[0].displayName.includes(RLO));
  const panel = await readFile(new URL("../src/WidgetPanel.vue", import.meta.url), "utf8");
  assert.match(panel, /<bdi>/, "the widget isolates names from the surrounding text direction");
  const component = await loadApp();
  const normalized = appContext(component, { activityEvents: [{ epoch: NOW, code: "x", profile: `ph${RLO}otos`, category: "operations", level: "info", message: `copy ${RLO}gpj.exe` }], activitySearchDebounced: "", activityTab: "events", activityCategory: "all", activityLevel: "all" }).reversedActivity[0];
  assert.ok(!normalized.profile.includes(RLO) && !normalized.message.includes(RLO));
});

test("ADV-S-20 a step that fails while applying a good snapshot is a persistent warning, and the connection stays up", async () => {
  const component = await loadApp({ overrides: { apiGet: async () => snapshot(block()) } });
  const ctx = appContext(component, {
    settings: { desktop_notifications: false, audible: false }, lastFailureKey: "", lastOutcomeFailureKey: "", failureBaselineSeen: true,
    disposed: false, snapshotRefreshBlocked: false, snapshotPromise: null, snapshotGeneration: 0, snapshotLoading: false, snapshotRefreshQueued: false,
    csrfToken: "token", auth: {}, snapshot: null, snapshotSignature: "", snapshotReceivedAtMs: 0, snapshotFailureCode: "", snapshotFailureStreak: 0,
    connected: false, connectionLabel: "", freshness: "", bridgeIssue: { title: "", message: "" }, applyError: "",
    hydrateAlerts() { throw new Error("alerts document is malformed"); }, hydrateSecurityPolicy() {}, scheduleSnapshot() {}
  });
  const originalError = console.error;
  console.error = () => {};
  try {
    assert.equal(await ctx.refreshSnapshot(false), true);
  } finally {
    console.error = originalError;
  }
  assert.equal(ctx.connected, true);
  assert.match(ctx.applyError, /alert settings: alerts document is malformed/);
  assert.ok(ctx.attentionItems.some((item) => item.id === "ui-apply-error" && /malformed/.test(item.detail)));
  assert.ok(ctx.toasts.some((toast) => toast.error && /Dashboard error/.test(toast.title)));
});

