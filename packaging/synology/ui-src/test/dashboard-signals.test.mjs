// Fix round 3 (D3-UI-1 .. D3-UI-11): the dashboard says what is wrong on every page,
// says "not read" instead of "empty" when it has read nothing, recovers quickly after an
// outage, and does not offer a second full sync without saying one is already going.
// Each test fails on the code before the round-3 change; the finding ids are in the names.
import assert from "node:assert/strict";
import test from "node:test";
import { appContext, appSource, loadApp } from "./helpers/load-app.mjs";
import { attentionItems, normalizeOutcomes, reasonCopy, serviceAttentionItem } from "../src/outcomes.mjs";
import { WIDGET_BACKOFF_RAMP_MS, widgetView } from "../src/widgetModel.mjs";

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

const failedBlock = () => block({
  run: { outcome: "failed", reason: "core_failed", scope: "all", operation: "sync", started_epoch: NOW - 120, finished_epoch: NOW - 60, exit_code: 1, stale: {} },
  profiles: [{ profile: "photos", outcome: "failed", reason: "core_failed", status_cache: "last_run_failed", finished_epoch: NOW - 60 }]
});
const failedSnapshot = () => snapshot(failedBlock(), { run: { state: "failed", scope: "all", operation: "sync", finished_epoch: NOW - 60, exit_code: 1 } });

const live = { connected: true, freshness: "x", nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS };

// ---------------------------------------------------------------------------
// D3-UI-1: a failure is visible on every page, and a failure toast stays
// ---------------------------------------------------------------------------

test("D3-UI-1 a failed run raises one topbar chip that counts the run and its profile as one failure", async () => {
  const component = await loadApp();
  const ctx = appContext(component, { ...live, snapshot: failedSnapshot() });
  const chip = ctx.attentionChip;
  assert.ok(chip, "a failed run must raise the chip");
  assert.equal(chip.level, "fail");
  assert.equal(chip.text, "1 failing", "the run and the profile it failed are one failure");
  assert.match(chip.title, /Last run failed/);
});

test("D3-UI-1 a healthy estate has no chip, and a stale reading alone is not counted twice", async () => {
  const component = await loadApp();
  assert.equal(appContext(component, { ...live, snapshot: snapshot(block()) }).attentionChip, null);
  const stale = appContext(component, { ...live, connected: false, snapshot: snapshot(block()) });
  assert.equal(stale.attentionChip, null, "the stale banner and the connection label already say so");
  assert.equal(stale.attentionItems[0].id, "stale-poll");
});

test("D3-UI-1 two unrelated failures are counted as two, and a warning-only estate says 'needs attention'", async () => {
  const component = await loadApp();
  const two = failedBlock();
  two.profiles.push({ profile: "docs", outcome: "failed", reason: "network", status_cache: "last_run_failed", finished_epoch: NOW - 90_000 });
  const chip = appContext(component, { ...live, snapshot: snapshot(two) }).attentionChip;
  assert.equal(chip.text, "2 failing", "a profile that failed in an earlier run is its own failure");
  const warn = block({ run: { outcome: "partial", reason: "deferred_paths", scope: "all", operation: "sync", started_epoch: NOW - 100, finished_epoch: NOW - 60, stale: {} } });
  const quiet = appContext(component, { ...live, snapshot: snapshot(warn) }).attentionChip;
  assert.equal(quiet.level, "warn");
  assert.match(quiet.text, /needs? attention/);
});

test("D3-UI-1 the chip lives in the topbar, outside every page, and opens the Overview", () => {
  const topbar = appSource.slice(appSource.indexOf('<header class="sdsync-topbar">'), appSource.indexOf("</header>"));
  assert.match(topbar, /class="sdsync-attention-chip"/);
  assert.match(topbar, /v-if="attentionChip"/);
  assert.match(topbar, /@click="navigate\('overview'\)"/);
  assert.ok(topbar.indexOf("sdsync-attention-chip") < topbar.indexOf("sdsync-freshness"), "the chip sits beside the freshness text");
  const stage = appSource.slice(appSource.indexOf('<div class="sdsync-page-stage">'));
  assert.doesNotMatch(stage.slice(0, stage.indexOf("</main>")), /sdsync-attention-chip/, "one chip, not one per page");
});

test("D3-UI-1 a failure toast stays until it is dismissed; an ordinary toast still fades", async () => {
  const component = await loadApp();
  const timers = [];
  const clear = [];
  const realSet = window.setTimeout;
  const realClear = window.clearTimeout;
  window.setTimeout = (fn, ms) => { timers.push({ fn, ms }); return timers.length; };
  window.clearTimeout = (id) => { clear.push(id); };
  try {
    const ctx = appContext(component, { toasts: [], toastSequence: 0, toastTimers: [], disposed: false, toast: undefined });
    delete ctx.toast;
    ctx.toast = (...args) => component.methods.toast.call(ctx, ...args);
    ctx.toast("Saved", "fine", false);
    ctx.toast("Run failed", "nothing synced", true);
    assert.equal(ctx.toasts.length, 2);
    assert.equal(timers.length, 1, "only the ordinary toast gets a timer");
    assert.equal(timers[0].ms, 6000);
    ctx.toast("Run failed", "nothing synced", true);
    assert.equal(ctx.toasts.filter((item) => item.title === "Run failed").length, 1, "the same failure is not stacked");
    for (let index = 0; index < 8; index += 1) ctx.toast(`Failure ${index}`, "x", true);
    assert.ok(ctx.toasts.length <= 5, `the stack is bounded, was ${ctx.toasts.length}`);
    assert.equal(ctx.toasts[ctx.toasts.length - 1].title, "Failure 7", "the newest failure is kept");
    const id = ctx.toasts[0].id;
    ctx.dismissToast(id);
    assert.ok(!ctx.toasts.some((item) => item.id === id));
  } finally {
    window.setTimeout = realSet;
    window.clearTimeout = realClear;
  }
  const item = appSource.match(/<div v-for="toastItem in toasts"[\s\S]*?<\/div>/)[0];
  assert.match(item, /@click="dismissToast\(toastItem\.id\)"/, "a failure toast has a dismiss control");
  assert.doesNotMatch(item, /role=/, "no second live region beside the container's");
});

// ---------------------------------------------------------------------------
// D3-UI-2: a stopped or untrusted service and a controller that stopped checking in
// ---------------------------------------------------------------------------

test("D3-UI-2 a stopped service is an attention item, not a neutral pill beside a green 'Succeeded'", async () => {
  const component = await loadApp();
  const ctx = appContext(component, { ...live, snapshot: snapshot(block(), { service: { state: "stopped" } }) });
  const item = ctx.attentionItems.find((candidate) => candidate.id === "service-down");
  assert.ok(item, JSON.stringify(ctx.attentionItems));
  assert.equal(item.level, "warn");
  assert.match(item.title, /Package service stopped/);
  assert.match(item.detail, /Package Center/);
  assert.equal(ctx.attentionChip.level, "warn");
});

test("D3-UI-2 an untrusted service identity is a failure", async () => {
  const component = await loadApp();
  const ctx = appContext(component, { ...live, snapshot: snapshot(block(), { service: { state: "untrusted" } }) });
  const item = ctx.attentionItems.find((candidate) => candidate.id === "service-untrusted");
  assert.ok(item);
  assert.equal(item.level, "fail");
  assert.equal(ctx.attentionChip.level, "fail");
});

test("D3-UI-2 a running controller that has stopped checking in is flagged; a long job with a fresh heartbeat is not", async () => {
  const component = await loadApp();
  const wedged = snapshot(block(), { controller: { state: "running", active_pid: 0, updated_epoch: NOW - 7200 } });
  const item = appContext(component, { ...live, snapshot: wedged }).attentionItems.find((candidate) => candidate.id === "controller-silent");
  assert.ok(item, "a controller silent for two hours with nothing running is wedged");
  assert.equal(item.level, "warn");
  assert.match(item.title, /2 h/);
  const longJob = snapshot(block(), { controller: { state: "running", active_pid: 4242, updated_epoch: NOW - 3 * 3600 } });
  assert.ok(!appContext(component, { ...live, snapshot: longJob }).attentionItems.some((candidate) => candidate.id === "controller-silent"), "a controller executing a long job is not wedged");
  const fresh = snapshot(block(), { controller: { state: "running", active_pid: 0, updated_epoch: NOW - 20 } });
  assert.ok(!appContext(component, { ...live, snapshot: fresh }).attentionItems.some((candidate) => candidate.id === "controller-silent"));
  const none = snapshot(block());
  assert.ok(!appContext(component, { ...live, snapshot: none }).attentionItems.some((candidate) => candidate.id === "controller-silent"), "no controller record is not a wedge");
});

test("D3-UI-2 the desktop widget reads the same rule: a wedged controller is not 'All profiles healthy'", () => {
  const wedged = snapshot(block(), { controller: { state: "running", active_pid: 0, updated_epoch: NOW - 7200 } });
  const view = widgetView(wedged, NOW_MS);
  assert.notEqual(view.overview.level, "ok");
  assert.match(view.overview.title, /controller/i);
  assert.equal(widgetView(snapshot(block()), NOW_MS).overview.level, "ok");
  assert.equal(serviceAttentionItem(wedged, "running").id, "controller-silent");
  assert.equal(serviceAttentionItem(snapshot(block()), "running"), null);
});

// ---------------------------------------------------------------------------
// D3-UI-3: nothing read is "not read", never "empty" or "off"
// ---------------------------------------------------------------------------

test("D3-UI-3 before any snapshot the empty states say 'not read', not 'none configured'", async () => {
  const component = await loadApp();
  const cold = appContext(component, { snapshot: null, connected: false, freshness: "Status unavailable", nowMs: NOW_MS, snapshotReceivedAtMs: 0 });
  assert.equal(cold.snapshotRead, false);
  assert.match(cold.profilesEmptyText, /not (been )?(read|loaded)/i);
  assert.doesNotMatch(cold.profilesEmptyText, /No configured/);
  assert.match(cold.routinesEmptyText, /not (been )?(read|loaded)/i);
  assert.doesNotMatch(cold.routinesEmptyText, /No configured/);
  const warm = appContext(component, { ...live, snapshot: snapshot(block(), { profiles: [], routines: [] }) });
  assert.equal(warm.snapshotRead, true);
  assert.equal(warm.profilesEmptyText, "No configured profiles.");
  assert.match(warm.routinesEmptyText, /^No configured routines/);
});

test("D3-UI-3 the Overview tiles, run status and alert pill are gated on a read snapshot in the template", () => {
  const overview = appSource.slice(appSource.indexOf('<div class="sdsync-metrics"'), appSource.indexOf('<div class="sdsync-two-column">'));
  assert.match(overview, /<strong>\{\{ snapshotRead \? profiles\.length : 'Not read' \}\}<\/strong>/);
  assert.match(overview, /snapshotRead \? \(runActive \? runScope : 'Idle'\) : 'Not read'/);
  assert.match(overview, /snapshotRead \? \(realtimeRoutines\.length \? realtimeRoutines\.length \+ ' active' : 'Off'\) : 'Not read'/);
  assert.match(appSource, /<p v-if="!profiles\.length" class="sdsync-empty">\{\{ profilesEmptyText \}\}<\/p>/);
  assert.match(appSource, /<p v-if="!routines\.length" class="sdsync-empty">\{\{ routinesEmptyText \}\}<\/p>/);
  assert.match(appSource, /alertPolicyPill/);
});

test("D3-UI-3 the alert policy pill does not say Disabled before the policy was read", async () => {
  const component = await loadApp();
  const cold = appContext(component, { snapshot: null, alertForm: { enabled: false } });
  assert.deepEqual({ ...cold.alertPolicyPill }, { state: "unknown", text: "Not read" });
  const warm = appContext(component, { snapshot: snapshot(block(), { alerts: { enabled: false } }), alertForm: { enabled: false } });
  assert.equal(warm.alertPolicyPill.text, "Disabled");
  assert.equal(appContext(component, { snapshot: snapshot(block(), { alerts: { enabled: true } }), alertForm: { enabled: true } }).alertPolicyPill.text, "Enabled");
});

// ---------------------------------------------------------------------------
// D3-UI-4: recovery after an outage
// ---------------------------------------------------------------------------

test("D3-UI-4 the AppWindow's status poll never backs off past one minute", async () => {
  const component = await loadApp();
  const delay = (state, interval = 5000) => appContext(component, state).snapshotRetryDelay(interval);
  const base = { snapshotFailureCode: "", windowHidden: false, snapshotFailureStatus: 0 };
  for (const streak of [2, 3, 4, 5, 9]) {
    assert.ok(delay({ ...base, snapshotFailureStreak: streak }) <= 60_000, `streak ${streak}`);
  }
  assert.equal(delay({ ...base, snapshotFailureStreak: 9 }), WIDGET_BACKOFF_RAMP_MS[0]);
  assert.equal(delay({ ...base, snapshotFailureCode: "runtime_upgrading", snapshotFailureStatus: 503, snapshotFailureStreak: 9 }) <= 60_000, true);
  assert.equal(delay({ ...base, snapshotFailureCode: "manager_unsafe", snapshotFailureStatus: 503, snapshotFailureStreak: 1 }), 0, "a stop code still stops");
});

test("D3-UI-4 a Logs or Activity read that succeeds while the snapshot is stale probes the snapshot at once", async () => {
  let reads = 0;
  const component = await loadApp({ overrides: { apiGet: async () => { reads += 1; return { schema: "sdsync.dsm-activity.v1", events: [] }; } } });
  const probes = [];
  const ctx = appContext(component, {
    disposed: false, logsLoading: false, logsPaused: false, route: "activity", activityTab: "events", logLines: 200, logSource: "all",
    auth: {}, activityEvents: [], activityHistory: null, logRecords: [], logOutput: "", logState: "", logsReceivedAtMs: 0, activityReceivedAtMs: 0,
    activityFeedError: "", logsFeedError: "", activitySearchDebounced: "", activityCategory: "all", activityLevel: "all",
    snapshot: snapshot(block()), connected: false, snapshotFailureStreak: 5, snapshotLoading: false, snapshotPromise: null,
    scheduleLogs() {}, refreshSnapshot: (manual) => { probes.push(manual); return Promise.resolve(true); }
  });
  await ctx.refreshLogs();
  assert.ok(reads > 0);
  assert.deepEqual(probes, [false], "the bridge answered, so ask for the snapshot now");
  assert.equal(ctx.snapshotFailureStreak, 0, "the failure streak restarts");
  probes.length = 0;
  ctx.connected = true;
  await ctx.refreshLogs();
  assert.deepEqual(probes, [], "a healthy connection needs no extra probe");
});

// ---------------------------------------------------------------------------
// D3-UI-6: a second full sync
// ---------------------------------------------------------------------------

function quickRunContext(component, over = {}) {
  const asked = [];
  const ctx = appContext(component, {
    ...live, snapshot: snapshot(block()), operationBusy: false, canRunOperations: true, runQueuedAtMs: 0,
    confirmAction: (title, message, button) => { asked.push({ title, message, button }); return Promise.resolve(false); },
    ...over
  });
  ctx.canRunOperations = true;
  return { ctx, asked };
}

test("D3-UI-6 Run all while a run is active says so and offers 'Queue another'", async () => {
  const component = await loadApp();
  const running = snapshot(
    block({ run: { outcome: "running", scope: "all", operation: "sync", started_epoch: NOW - 720, stale: { detected: false, lock: "live" } } }),
    { run: { state: "running", scope: "all", operation: "sync", started_epoch: NOW - 720 } }
  );
  const { ctx, asked } = quickRunContext(component, { snapshot: running });
  await ctx.quickRun();
  assert.equal(asked.length, 1);
  assert.match(asked[0].message, /already running/i);
  assert.match(asked[0].message, /12 min/);
  assert.equal(asked[0].button, "Queue another");
});

test("D3-UI-6 Run all after this window queued a run says one is probably still waiting", async () => {
  const component = await loadApp();
  const { ctx, asked } = quickRunContext(component, { runQueuedAtMs: Date.now() - 90_000 });
  await ctx.quickRun();
  assert.match(asked[0].message, /queued|waiting/i);
  assert.equal(asked[0].button, "Queue another");
  const idle = quickRunContext(component, { runQueuedAtMs: Date.now() - 3_600_000 });
  await idle.ctx.quickRun();
  assert.equal(idle.asked[0].button, "Run all", "an hour-old queue entry is long finished");
  assert.doesNotMatch(idle.asked[0].message, /already/);
});

test("D3-UI-6 a quiet estate keeps the original confirmation", async () => {
  const component = await loadApp();
  const { ctx, asked } = quickRunContext(component);
  await ctx.quickRun();
  assert.equal(asked[0].title, "Run all configured profiles?");
  assert.match(asked[0].message, /starts a real one-way sync/);
  assert.equal(asked[0].button, "Run all");
});

// ---------------------------------------------------------------------------
// D3-UI-8: mutations are not offered beside a stale page
// ---------------------------------------------------------------------------

test("D3-UI-8 a stale reading disables mutations even when a CSRF read has succeeded again", async () => {
  const component = await loadApp();
  const base = { snapshot: snapshot(block(), { capabilities: { mutations: true } }), csrfToken: "token" };
  assert.equal(appContext(component, { ...base, connected: true }).canMutate, true);
  const stale = appContext(component, { ...base, connected: false });
  assert.equal(stale.retainedStale, true);
  assert.equal(stale.canMutate, false, "the page is dimmed and says stale; the buttons must agree");
});

// ---------------------------------------------------------------------------
// D3-UI-9: non-object members
// ---------------------------------------------------------------------------

test("D3-UI-9 null and non-object members of profiles, routines and health are dropped, not thrown on", async () => {
  const component = await loadApp();
  const hostile = snapshot(block(), {
    profiles: [null, { name: "photos", has_password: true }, 7, "x", [1], undefined],
    routines: [null, { profile: "photos", enabled: true }, false],
    health: [null, { profile: "photos", last_check_epoch: NOW }, 3]
  });
  const ctx = appContext(component, { ...live, snapshot: hostile, profileFilter: "", profileFilterStatus: "all" });
  assert.deepEqual(ctx.profiles.map((item) => item.name), ["photos"]);
  assert.deepEqual(ctx.routines.map((item) => item.profile), ["photos"]);
  assert.equal(ctx.enabledRoutines.length, 1);
  assert.equal(ctx.readyProfileCount, 1);
  assert.equal(ctx.filteredProfiles.length, 1);
  assert.equal(ctx.healthRows.length, 1);
  assert.doesNotThrow(() => ctx.healthFreshness);
});

// ---------------------------------------------------------------------------
// D3-UI-10: a window DSM removed without destroying it
// ---------------------------------------------------------------------------

test("D3-UI-10 a window whose element was removed from the document destroys itself after two quiet ticks", async () => {
  const component = await loadApp();
  let destroyed = 0;
  const element = { nodeType: 1, isConnected: true };
  const ctx = appContext(component, { $el: element, wasAttached: false, detachedTicks: 0, disposed: false, $destroy() { destroyed += 1; } });
  ctx.clockWatchdog();
  assert.equal(ctx.wasAttached, true);
  element.isConnected = false;
  ctx.clockWatchdog();
  assert.equal(destroyed, 0, "one tick detached could be DSM moving the window");
  ctx.clockWatchdog();
  assert.equal(destroyed, 1);
  const revived = appContext(component, { $el: element, wasAttached: true, detachedTicks: 1, disposed: false, $destroy() { destroyed += 1; } });
  element.isConnected = true;
  revived.clockWatchdog();
  assert.equal(revived.detachedTicks, 0, "re-attached resets the count");
});

test("D3-UI-10 a window that was never attached, or a comment-node root, is never destroyed by the watchdog", async () => {
  const component = await loadApp();
  let destroyed = 0;
  const never = appContext(component, { $el: { nodeType: 1, isConnected: false }, wasAttached: false, detachedTicks: 0, disposed: false, $destroy() { destroyed += 1; } });
  for (let tick = 0; tick < 6; tick += 1) never.clockWatchdog();
  const comment = appContext(component, { $el: { nodeType: 8 }, wasAttached: false, detachedTicks: 0, disposed: false, $destroy() { destroyed += 1; } });
  for (let tick = 0; tick < 6; tick += 1) comment.clockWatchdog();
  const gone = appContext(component, { $el: undefined, wasAttached: false, detachedTicks: 0, disposed: false, $destroy() { destroyed += 1; } });
  for (let tick = 0; tick < 6; tick += 1) gone.clockWatchdog();
  assert.equal(destroyed, 0);
});

test("D3-UI-10 the clock timer runs the watchdog on every tick", () => {
  const scheduler = appSource.slice(appSource.indexOf("function scheduleClock"), appSource.indexOf("// A minimised DSM window is not document.hidden"));
  assert.match(scheduler, /clockWatchdog\(vm\)/);
});

// ---------------------------------------------------------------------------
// D3-UI-5: the evidence age is measured on the package's clock
// ---------------------------------------------------------------------------

test("D3-UI-5 'Observed' ages follow the snapshot's clock, not the browser's", async () => {
  const component = await loadApp();
  const rollup = {
    loaded: true, complete: true, observedAtEpoch: NOW - 85, overlapping: false, profilesTotal: 1, profilesObserved: 1, neverObserved: [], total: null, totalUnavailableReason: "",
    profiles: [{ profile: "photos", observedAtEpoch: NOW - 85, complete: true, state: { inSync: { files: 1, bytes: 1 }, wouldTransfer: { files: 0, bytes: 0 }, attention: 0, totalEntries: 1 } }]
  };
  // The browser clock is two hours behind the NAS; the evidence is 85 seconds old on the NAS.
  const behind = appContext(component, { snapshot: snapshot(block()), statusRollup: rollup, nowMs: NOW_MS - 7200_000, snapshotReceivedAtMs: NOW_MS - 7200_000, connected: true, freshness: "x" });
  assert.equal(behind.statusRollupRows[0].observed, "moments ago");
  assert.match(behind.statusRollupFreshness, /moments ago/);
  const ahead = appContext(component, { snapshot: snapshot(block()), statusRollup: rollup, nowMs: NOW_MS + 7200_000, snapshotReceivedAtMs: NOW_MS + 7200_000, connected: true, freshness: "x" });
  assert.equal(ahead.statusRollupRows[0].observed, "moments ago");
  assert.doesNotMatch(appSource, /Math\.floor\(Date\.now\(\) \/ 1000\) - observed/);
});

// ---------------------------------------------------------------------------
// D3-UI-11: wording
// ---------------------------------------------------------------------------

test("D3-UI-11 a busy refusal does not promise a retry for a run the user started", () => {
  for (const code of ["busy", "lock_busy"]) {
    const text = reasonCopy(code).text;
    assert.doesNotMatch(text, /Nothing to do; the run is retried/, code);
    assert.match(text, /routine/i, `${code} names who retries`);
    assert.match(text, /run it again|start it again|run again/i, `${code} tells a user-started run to be started again`);
  }
});

test("D3-UI-11 a cancelled run says whether a routine reruns it", () => {
  const text = reasonCopy("cancelled").text;
  assert.match(text, /routine/i);
  assert.match(text, /run it again|run again|start it again/i);
  const item = attentionItems(normalizeOutcomes(block({ run: { outcome: "cancelled", scope: "all", operation: "sync", started_epoch: NOW - 100, finished_epoch: NOW - 60, stale: {} } })), NOW).find((candidate) => candidate.id === "run-cancelled");
  assert.match(item.detail, /routine/i);
});

test("D3-UI-11 no instruction says 'restart' where Package Center only offers Stop and Run", async () => {
  for (const code of ["pid_unreadable", "identity_refused", "identity_changed", "signal_failed"]) {
    const text = reasonCopy(code).text;
    assert.match(text, /Stop the package in Package Center, then Run it/, code);
    assert.doesNotMatch(text, /Restart/, code);
  }
  const reload = attentionItems(normalizeOutcomes(block({ controller_reload: { pending: true, epoch: NOW - 600, reason: "identity_refused", pid: 1, operation: "configure-routine" } })), NOW).find((candidate) => candidate.id === "controller-reload");
  assert.match(reload.detail, /Stop the package in Package Center, then Run it/);
  assert.doesNotMatch(`${reload.title} ${reload.detail}`, /[Rr]estart/);
  const widget = await import("../src/widgetModel.mjs");
  const outcomes = await import("../src/outcomes.mjs");
  const shipped = [appSource, await (await import("node:fs/promises")).readFile(new URL("../src/widgetModel.mjs", import.meta.url), "utf8"), await (await import("node:fs/promises")).readFile(new URL("../src/outcomes.mjs", import.meta.url), "utf8")].join("\n");
  assert.doesNotMatch(shipped, /Restart (?:Synology Drive Sync|the package(?: service)?)(?: in Package Center)?[.,]/, "no instruction to restart the package from Package Center");
  assert.ok(widget && outcomes);
});

test("D3-UI-1 a run without a start time still folds in the profiles it just failed", async () => {
  const component = await loadApp();
  const noStart = failedBlock();
  delete noStart.run.started_epoch;
  assert.equal(appContext(component, { ...live, snapshot: snapshot(noStart) }).attentionChip.text, "1 failing");
});

test("D3-UI-10 the first visibility report counts as 'was attached', so a window closed within thirty seconds is still caught", () => {
  const observer = appSource.slice(appSource.indexOf("function observeWindowVisibility"), appSource.indexOf("export default {"));
  assert.match(observer, /clockWatchdog\(vm\);/);
});

// ---------------------------------------------------------------------------
// D3-UI-7: a cancelled run is "cancelled" in Activity
// ---------------------------------------------------------------------------

test("D3-UI-7 a run.cancelled record shows state cancelled, and a cancel is not a failure to announce", async () => {
  const component = await loadApp();
  const events = [
    { epoch: NOW, code: "run.cancelled", profile: "photos", category: "operations", level: "info", state: "cancelled", message: "Package action cancelled before completion" },
    // Written before the writer knew run.cancelled: a run.failed record at info level that says it was cancelled.
    { epoch: NOW + 1, code: "run.failed", profile: "photos", category: "operations", level: "info", state: "failed", message: "Package action cancelled before completion" },
    { epoch: NOW + 2, code: "run.failed", profile: "photos", category: "operations", level: "error", state: "failed", message: "the sync core exited with an error" }
  ];
  const ctx = appContext(component, { activityEvents: events, activitySearchDebounced: "", activityTab: "events", activityCategory: "all", activityLevel: "all", activityRenderLimit: 200 });
  const rows = ctx.reversedActivity;
  assert.equal(rows[0].state, "failed", "a real failure stays failed");
  assert.equal(rows[1].state, "cancelled", "the legacy cancel record reads as cancelled");
  assert.equal(rows[2].state, "cancelled");
  assert.equal(rows[2].code, "run.cancelled");
  // The failure announcer reads the package's outcome verdict, not Activity rows, and a cancelled
  // outcome is not a failure key.
  const outcomes = normalizeOutcomes(block({ run: { outcome: "cancelled", scope: "all", operation: "sync", started_epoch: NOW - 100, finished_epoch: NOW - 60, stale: {} } }));
  const { failureKey } = await import("../src/outcomes.mjs");
  assert.equal(failureKey(outcomes), "");
});

// ---------------------------------------------------------------------------
// D3-UI-5 (bridge half): the NAS clock the snapshot reports
// ---------------------------------------------------------------------------

test("D3-UI-5 the NAS zone is labelled whenever it differs from the browser's, and never guessed", async () => {
  const component = await loadApp();
  const nas = (nas_clock) => appContext(component, { snapshot: snapshot(block(), { nas_clock }), connected: true, freshness: "x", nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS, browserOffsetSeconds: -5 * 3600 });
  const cet = nas({ now_epoch: NOW, utc_offset_seconds: 3600, zone: "CET" });
  assert.equal(cet.nasClock.label, "UTC+01:00 (CET)");
  assert.equal(cet.nasClock.differs, true);
  assert.match(cet.nasTimeNote, /NAS time is UTC\+01:00 \(CET\); this browser is UTC-05:00\./);
  assert.equal(cet.nasTimeSuffix(NOW), ` (NAS ${new Date((NOW + 3600) * 1000).toISOString().slice(0, 16).replace("T", " ")})`);
  const same = appContext(component, { snapshot: snapshot(block(), { nas_clock: { now_epoch: NOW, utc_offset_seconds: -5 * 3600, zone: "EST" } }), connected: true, freshness: "x", nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS, browserOffsetSeconds: -5 * 3600 });
  assert.equal(same.nasTimeNote, "");
  assert.equal(same.nasTimeSuffix(NOW), "");
  for (const bad of [{ now_epoch: null, utc_offset_seconds: null, zone: null, error: "timezone_unavailable" }, undefined, null, "x", { utc_offset_seconds: "3600" }]) {
    const unknown = nas(bad);
    assert.equal(unknown.nasClock.known, false);
    assert.equal(unknown.nasTimeSuffix(NOW), "");
    assert.doesNotMatch(unknown.nasTimeNote, /NAS time is UTC/);
  }
  assert.match(nas({ now_epoch: null, utc_offset_seconds: null, zone: null, error: "timezone_unavailable" }).nasTimeNote, /The NAS time zone is unavailable/);
});

test("a log source the read deadline cut says it ran out of time", async () => {
  const component = await loadApp();
  const ctx = appContext(component, { logSource: "sync" });
  const records = ctx.logRecordsFrom({
    schema: "sdsync.dsm-logs.v1",
    logs: [
      { source: "sync", scan_limited: true, deadline_reached: true, lines: ['{"level":"error"}'] },
      { source: "api", scan_limited: true, lines: ['{"level":"info"}'] },
      { source: "audit", deadline_reached: true, lines: ['{"level":"info"}'] }
    ]
  });
  assert.equal(records[0].deadlineReached, true);
  assert.equal(records[1].deadlineReached, undefined, "a size-limited source did not run out of time");
  assert.equal(records[2].deadlineReached, undefined, "the deadline flag only counts together with scan_limited");
  assert.match(appSource, /v-if="record\.deadlineReached">the read ran out of time; older lines were not scanned/);
  // A line the bridge withheld is shown as written, never blanked.
  const withheld = ctx.logRecordsFrom({ schema: "sdsync.dsm-logs.v1", logs: [{ source: "api", lines: ["[line withheld: could not be redacted]"] }] });
  assert.match(withheld[0].text, /\[line withheld: could not be redacted\]/);
  const long = ctx.logRecordsFrom({ schema: "sdsync.dsm-logs.v1", logs: [{ source: "api", lines: ["[line too long: 40000 bytes, withheld]"] }] });
  assert.match(long[0].text, /\[line too long: 40000 bytes, withheld\]/);
});

// ---------------------------------------------------------------------------
// fix3-bridge additions: control_lock_busy, time-limited directory listings, clock_skew
// ---------------------------------------------------------------------------

test("a control_lock_busy 503 on a change is named, and says a retry is safe", async () => {
  const { DsmApiError } = await import("../src/api.js");
  const component = await loadApp();
  const ctx = appContext(component, {});
  const error = Object.assign(new DsmApiError("lock held", 503, "control_lock_busy"), { trustedRejection: true });
  const issue = ctx.describeBridgeError(error, "status");
  assert.equal(issue.title, "Package is busy recording another change");
  assert.match(issue.message, /safe/);
  const report = ctx.reportMutationError(error, "Profile not saved", "Profile outcome unknown", "The package rejected the profile.");
  assert.match(`${report.title || ""} ${report.message || ""}`, /recording another change|safe/i, JSON.stringify(report));
  assert.doesNotMatch(report.message || "", /lock held/, "the envelope's own words are not rendered");
  const { widgetBridgeIssue } = await import("../src/widgetModel.mjs");
  assert.equal(widgetBridgeIssue({ status: 503, code: "control_lock_busy" }).title, "Package is busy recording another change");
});

test("a directory listing the 6 s budget cut says it took too long, not just 'truncated'", async () => {
  assert.match(appSource, /pathBrowser\.timeLimited/);
  assert.match(appSource, /the folder took too long to list; not every child is shown/);
  assert.match(appSource, /this\.pathBrowser\.timeLimited = result\.time_limited === true && result\.truncated === true/);
});
