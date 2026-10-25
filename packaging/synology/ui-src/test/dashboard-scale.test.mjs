// SC-1 / SC-6: the snapshot is data, not state to be tracked. A deeply reactive 254-profile
// snapshot made every row read copy thousands of dependencies into the render watcher
// (414 ms per poll, quadratic in profiles); the Activity list rendered every event (1,000 events
// = 20k DOM nodes). Each test fails on the code before the change.
import assert from "node:assert/strict";
import test from "node:test";
import { appContext, appSource, loadApp } from "./helpers/load-app.mjs";
import { serviceAttentionItem } from "../src/outcomes.mjs";

globalThis.window ??= globalThis;
globalThis.document ??= { hidden: false };

const NOW = 1_700_000_000;
const NOW_MS = NOW * 1000;

function bigSnapshot(count = 254, generated = NOW) {
  const names = Array.from({ length: count }, (_, index) => `profile-${String(index + 1).padStart(3, "0")}`);
  return {
    schema: "sdsync.dsm-api.v1",
    generated_at_epoch: generated,
    capabilities: { mutations: true },
    service: { state: "running" },
    controller: { state: "running", active_pid: 0, updated_epoch: NOW },
    run: { state: "succeeded", scope: "all", operation: "sync", finished_epoch: NOW - 60 },
    profiles: names.map((name, index) => ({ name, remote: `nas:/${name}`, has_password: true, health: { state: index % 7 === 3 ? "failed" : "succeeded", checked_epoch: NOW - 60 } })),
    routines: names.map((profile) => ({ profile, enabled: true, mode: "interval", state: "succeeded" })),
    outcomes: {
      schema: "sdsync.dsm-outcomes.v1",
      run: { outcome: "succeeded", scope: "all", operation: "sync", started_epoch: NOW - 200, finished_epoch: NOW - 60, exit_code: 0, stale: { detected: false, lock: "absent" } },
      alerts: { status: "ok", failure_count: 0 },
      profiles: names.map((profile, index) => ({ profile, outcome: index % 7 === 3 ? "failed" : "succeeded", reason: index % 7 === 3 ? "core_failed" : null, status_cache: "trusted", finished_epoch: NOW - 60 - index })),
      routines: [], errors: []
    }
  };
}

async function refreshContext(documents) {
  let index = 0;
  const component = await loadApp({ overrides: { apiGet: async () => documents[Math.min(index++, documents.length - 1)] } });
  return appContext(component, {
    settings: { desktop_notifications: false, audible: false }, lastFailureKey: "", lastOutcomeFailureKey: "", failureBaselineSeen: true,
    disposed: false, snapshotRefreshBlocked: false, snapshotPromise: null, snapshotGeneration: 0, snapshotLoading: false, snapshotRefreshQueued: false,
    csrfToken: "token", auth: {}, snapshot: null, snapshotSignature: "", snapshotReceivedAtMs: 0, snapshotGeneratedAt: 0, nowMs: NOW_MS,
    snapshotFailureCode: "", snapshotFailureStreak: 0, connected: false, connectionLabel: "", freshness: "", bridgeIssue: { title: "", message: "" }, applyError: "",
    hydrateAlerts() {}, hydrateSecurityPolicy() {}, scheduleSnapshot() {}
  });
}

test("SC-1 the stored snapshot is never made reactive", async () => {
  const ctx = await refreshContext([bigSnapshot(3)]);
  assert.equal(await ctx.refreshSnapshot(false), true);
  assert.ok(Object.isFrozen(ctx.snapshot), "a frozen root is skipped by Vue's observer, so nothing under it is tracked");
  assert.equal(ctx.snapshot.profiles.length, 3);
});

test("SC-1 an identical poll never mutates the frozen snapshot; the package clock moves through its own field", async () => {
  const ctx = await refreshContext([bigSnapshot(3, NOW), bigSnapshot(3, NOW + 7200)]);
  await ctx.refreshSnapshot(false);
  const first = ctx.snapshot;
  assert.equal(ctx.snapshotGeneratedAt, NOW);
  await ctx.refreshSnapshot(false);
  assert.equal(ctx.snapshot, first, "an unchanged poll keeps the same object so no computed is invalidated");
  assert.equal(first.generated_at_epoch, NOW, "and it is not edited in place");
  assert.equal(ctx.snapshotGeneratedAt, NOW + 7200);
});

test("SC-1 a controller that stopped checking in is still seen when only generated_at_epoch advances", async () => {
  const ctx = await refreshContext([bigSnapshot(3, NOW), bigSnapshot(3, NOW + 7200)]);
  await ctx.refreshSnapshot(false);
  assert.ok(!ctx.attentionItems.some((item) => item.id === "controller-silent"));
  await ctx.refreshSnapshot(false);
  assert.ok(ctx.controllerLiveness.stale, "liveness reads the live package clock, not the frozen first one");
  const silent = ctx.attentionItems.find((item) => item.id === "controller-silent");
  assert.ok(silent, JSON.stringify(ctx.attentionItems.map((item) => item.id)));
  assert.equal(serviceAttentionItem(ctx.snapshot, "running", NOW + 7200).id, "controller-silent");
  assert.equal(serviceAttentionItem(ctx.snapshot, "running"), null, "without the live clock the frozen one says nothing");
});

test("SC-1 the per-row outcome lookup is one computed map, not a computed read per row", async () => {
  const component = await loadApp();
  const ctx = appContext(component, { snapshot: bigSnapshot(254), connected: true, freshness: "x", nowMs: NOW_MS, snapshotReceivedAtMs: NOW_MS });
  const map = ctx.profileOutcomeMap;
  assert.ok(map instanceof Map);
  assert.equal(map.size, 254);
  assert.equal(map.get("profile-004").level, "fail");
  assert.deepEqual(ctx.profileOutcome("profile-004"), map.get("profile-004"));
  assert.equal(ctx.profileOutcome("profile-001").level, "ok");
  assert.equal(ctx.profileOutcome("nobody").text, "Last run: not reported");
});

function feed(count) {
  return Array.from({ length: count }, (_, index) => ({ epoch: NOW + index, code: "routine.deferred", profile: "photos", category: "operations", level: "info", state: "ok", message: `event ${index}` }));
}

test("SC-6 only the newest events are rendered, and the rest are counted, not hidden", async () => {
  const component = await loadApp();
  const state = { activityEvents: feed(1000), activitySearchDebounced: "", activityTab: "events", activityCategory: "all", activityLevel: "all", activityRenderLimit: 200 };
  const ctx = appContext(component, state);
  assert.equal(ctx.reversedActivity.length, 1000, "the filtered total is still all of them");
  assert.equal(ctx.shownActivity.length, 200);
  assert.equal(ctx.shownActivity[0].message, "event 999", "newest first");
  assert.equal(ctx.activityHiddenCount, 800);
  assert.match(ctx.activityMoreLabel, /800 older events not shown/);
  ctx.showMoreActivity();
  assert.equal(ctx.activityRenderLimit, 400);
  assert.equal(ctx.shownActivity.length, 400);
  const few = appContext(component, { ...state, activityEvents: feed(50), activityRenderLimit: 200 });
  assert.equal(few.activityHiddenCount, 0);
  assert.equal(few.activityMoreLabel, "");
});

test("SC-6 the template renders shownActivity with an older-events control, and a tab change resets the limit", () => {
  assert.match(appSource, /v-for="event in shownActivity" :key="event\.rowKey"/);
  assert.match(appSource, /v-if="activityHiddenCount"[\s\S]*?activityMoreLabel[\s\S]*?@click="showMoreActivity"/);
  const watcher = appSource.slice(appSource.indexOf("    activityTab(value) {"), appSource.indexOf("    incidentOutcomeUnresolved(value)"));
  assert.match(watcher, /this\.activityRenderLimit = ACTIVITY_RENDER_STEP/);
});
