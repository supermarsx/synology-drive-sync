import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import {
  WIDGET_ACTIVE_POLL_MS,
  WIDGET_BACKOFF_RAMP_MS,
  WIDGET_IDLE_POLL_MS
} from "../src/widgetModel.mjs";

// Behavioral coverage for the Activity route's five subtabs: the four event
// lenses that share one feed and filter reversedActivity by category, and the
// Package logs tab that gates the expensive log read. The tab/markup contract
// itself is asserted in subtab-layout-motion.test.mjs; this file is about
// what each lens actually shows and what refreshLogs actually requests.

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

function loadAppComponent(overrides = {}) {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "App.vue script block is missing");
  let executable = script[1]
    .replace(/^import \{ ActionIcon \} from "\.\/ActionIcon";\s*/m, "")
    .replace(/^import \{ createAutosaveCoordinator \} from "\.\/autosave";\s*/m, "")
    .replace(/^import \{ installControlLayout \} from "\.\/controlLayout";\s*/m, "")
    .replace(/import \{[\s\S]*?\}\s*from "\.\/api";\s*/, "")
    .replace(/^import SecurityPanel from "\.\/SecurityPanel\.vue";\s*/m, "")
    .replace("export default {", "const AppComponent = {");
  executable += "\nreturn AppComponent;";
  const stubs = {
    // The real cadence literals, matching the pattern the other App.vue unit
    // test files use to load the component's script in isolation.
    WIDGET_ACTIVE_POLL_MS,
    WIDGET_BACKOFF_RAMP_MS,
    WIDGET_IDLE_POLL_MS,
    ACTIONS: {},
    AUTOSAVE_API_LIMITS: Object.freeze({}),
    MAX_RESPONSE_BYTES: 1024 * 1024,
    QueuedOutcomeUnknownError: class QueuedOutcomeUnknownError extends Error {},
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    PROGRESS_UNAVAILABLE: Object.freeze({ unavailable: true }),
    trustedRequestProgress: () => null,
    apiGet: async () => ({}),
    apiPost: async () => ({}),
    probeRequestOutcome: async () => ({}),
    purgeReconciliationAuth() {},
    reconcileMutationRequest: async () => ({}),
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" ? value : fallback).slice(0, 65536),
    formatBytes: String,
    formatDate: (value) => Number(value) > 0 ? `date:${value}` : "Unavailable",
    formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    createAutosaveCoordinator: () => ({}),
    installControlLayout: () => () => {},
    ActionIcon: { name: "ActionIcon" },
    SecurityPanel: {},
    ...overrides
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

const component = loadAppComponent();

function activityEvent(category, code, overrides = {}) {
  return Object.assign({
    epoch: 1, code, profile: "office", state: "succeeded", category, level: "info", message: ""
  }, overrides);
}

test("every activity category belongs to exactly one named lens, and the events lens sees them all", () => {
  const categories = component.computed.activityCategoryOptions.call({})
    .map((option) => option.value)
    .filter((value) => value !== "all");
  const events = categories.map((category) => activityEvent(category, `${category}.event`));

  const codesFor = (tab) => component.computed.reversedActivity.call({
    activityEvents: events, activitySearch: "", activityCategory: "all", activityLevel: "all", activityTab: tab
  }).map((event) => event.code).sort();

  assert.deepEqual(codesFor("events"), events.map((event) => event.code).sort(),
    "the events lens must show every category");

  const seen = new Set();
  for (const tab of ["changes", "operations", "service"]) {
    for (const code of codesFor(tab)) {
      const category = code.replace(/\.event$/, "");
      assert.equal(seen.has(category), false, `${category} appeared in more than one named lens`);
      seen.add(category);
    }
  }
  assert.equal(seen.size, categories.length, "every category must be reachable from some named lens");

  // package-logs is not a category lens: it must behave like events (no
  // restriction), since the tab never renders the feed this would filter.
  assert.deepEqual(codesFor("package-logs"), codesFor("events"));
});

test("reversedActivity with no activityTab in context behaves exactly like the events lens", () => {
  const events = [activityEvent("audit", "a"), activityEvent("sync", "b"), activityEvent("bridge", "c")];
  const withoutTab = component.computed.reversedActivity.call({
    activityEvents: events, activitySearch: "", activityCategory: "all", activityLevel: "all"
  });
  const withEventsTab = component.computed.reversedActivity.call({
    activityEvents: events, activitySearch: "", activityCategory: "all", activityLevel: "all", activityTab: "events"
  });
  assert.deepEqual(withoutTab.map((event) => event.code), withEventsTab.map((event) => event.code));
  assert.equal(withoutTab.length, 3);
});

test("reversedActivity still honors category, level, and search inside a lens", () => {
  const events = [
    activityEvent("audit", "audit.kept", { message: "policy saved" }),
    activityEvent("security", "security.kept", { level: "warn" }),
    activityEvent("sync", "sync.excluded")
  ];
  const inChanges = (activityCategory, activityLevel, activitySearch) => component.computed.reversedActivity.call({
    activityEvents: events, activitySearch, activityCategory, activityLevel, activityTab: "changes"
  }).map((event) => event.code);

  // sync is outside the Changes lens no matter what the other filters say.
  assert.deepEqual(inChanges("all", "all", ""), ["security.kept", "audit.kept"]);
  assert.deepEqual(inChanges("audit", "all", ""), ["audit.kept"]);
  assert.deepEqual(inChanges("all", "warn", ""), ["security.kept"]);
  assert.deepEqual(inChanges("all", "all", "policy saved"), ["audit.kept"]);
});

test("the Category dropdown is constrained to the active lens plus All categories", () => {
  const categoryOptions = component.computed.activityCategoryOptions.call({});
  const optionsFor = (tab) => component.computed.activityLensCategoryOptions
    .call({ activityTab: tab, activityCategoryOptions: categoryOptions })
    .map((option) => option.value);

  assert.deepEqual(optionsFor("events"), categoryOptions.map((option) => option.value),
    "the events lens must offer every category unconstrained");
  assert.deepEqual(optionsFor("package-logs"), categoryOptions.map((option) => option.value),
    "package-logs is not a category lens and must not constrain the dropdown");
  // The lens filter narrows the master list; it must not reorder it, so the
  // expected order here is derived from categoryOptions rather than retyped.
  const lensOrder = (categories) => categoryOptions
    .map((option) => option.value)
    .filter((value) => value === "all" || categories.includes(value));
  assert.deepEqual(optionsFor("changes"), lensOrder(["audit", "configuration", "secrets", "security"]));
  assert.deepEqual(optionsFor("operations"), lensOrder(["operations", "routines", "sync", "scheduler"]));
  assert.deepEqual(optionsFor("service"), lensOrder(["bridge", "authentication", "controller", "notifications"]));
});

test("switching lenses resets an out-of-lens Category selection back to all, and leaves an in-lens one alone", () => {
  const outOfLens = { activityCategory: "audit" };
  component.watch.activityTab.call(outOfLens, "operations");
  assert.equal(outOfLens.activityCategory, "all");

  const inLens = { activityCategory: "sync" };
  component.watch.activityTab.call(inLens, "operations");
  assert.equal(inLens.activityCategory, "sync");

  const alreadyAll = { activityCategory: "all" };
  component.watch.activityTab.call(alreadyAll, "changes");
  assert.equal(alreadyAll.activityCategory, "all");

  // Switching to the unconstrained events (or package-logs) lens never needs
  // to reset anything, since every category fits there.
  const toEvents = { activityCategory: "bridge" };
  component.watch.activityTab.call(toEvents, "events");
  assert.equal(toEvents.activityCategory, "bridge");
});

test("switching to Package logs shows a loading state and fetches immediately", () => {
  let refreshCalls = 0;
  const context = { activityCategory: "all", logState: "", refreshLogs() { refreshCalls += 1; } };
  component.watch.activityTab.call(context, "package-logs");
  assert.equal(context.logState, "Loading package logs…");
  assert.equal(refreshCalls, 1);
});

test("refreshLogs requests only the activity feed on an event tab, and both feeds on Package logs", async () => {
  const requestsFor = async (activityTab) => {
    const requested = [];
    const scoped = loadAppComponent({
      document: { hidden: false },
      apiGet: async (_auth, action) => {
        requested.push(action);
        return action === "logs"
          ? { schema: "sdsync.dsm-logs.v1", logs: [] }
          : { schema: "sdsync.dsm-activity.v1", events: [] };
      }
    });
    const context = {
      disposed: false, logsLoading: false, logsPaused: false, route: "activity", activityTab,
      auth: {}, logLines: 200, logSource: "all", logRecords: [], logOutput: "", activityEvents: [],
      logState: "", logsReceivedAtMs: 0, activityReceivedAtMs: 0, scheduleLogs() {}
    };
    context.logRecordsFrom = (model) => scoped.methods.logRecordsFrom.call(context, model);
    await scoped.methods.refreshLogs.call(context);
    return requested;
  };

  for (const tab of ["events", "changes", "operations", "service"]) {
    assert.deepEqual(await requestsFor(tab), ["activity"], `${tab} must not request package logs`);
  }
  assert.deepEqual(await requestsFor("package-logs"), ["logs", "activity"]);
});

test("logState stays truthful instead of claiming logs are unavailable while their tab is closed", async () => {
  const scoped = loadAppComponent({
    document: { hidden: false },
    apiGet: async (_auth, action) => action === "activity"
      ? { schema: "sdsync.dsm-activity.v1", events: [] }
      : { schema: "sdsync.dsm-logs.v1", logs: [] }
  });
  const context = {
    disposed: false, logsLoading: false, logsPaused: false, route: "activity", activityTab: "operations",
    auth: {}, logLines: 200, logSource: "all", logRecords: ["stale record from a prior Package logs visit"],
    logOutput: "", activityEvents: [], logState: "", logsReceivedAtMs: 0, activityReceivedAtMs: 0,
    scheduleLogs() {}
  };
  context.logRecordsFrom = (model) => scoped.methods.logRecordsFrom.call(context, model);
  await scoped.methods.refreshLogs.call(context);

  assert.doesNotMatch(context.logState, /unavailable/i,
    "a tab that never asked for logs must not report them as unavailable");
  assert.match(context.logState, /Activity live · package log paused while its tab is closed/);
  assert.equal(context.logsReceivedAtMs, 0, "an unpolled tab must not stamp a receipt time for logs it never fetched");
  assert.deepEqual(context.logRecords, ["stale record from a prior Package logs visit"],
    "an unpolled tab must not touch whatever the Package logs tab last held");
});

test("the empty-state message names the lens when it has never recorded an event", () => {
  const emptyTextFor = (activityTab, activityEvents = []) => component.computed.activityEmptyText.call({
    activityTab,
    activityEvents,
    activityLensTotal: component.computed.activityLensTotal.call({ activityTab, activityEvents })
  });

  assert.equal(emptyTextFor("events"), "No package events have been recorded yet.");
  assert.equal(emptyTextFor("changes"), "No configuration changes have been recorded yet.");
  assert.equal(emptyTextFor("operations"), "No operations have been recorded yet.");
  assert.equal(emptyTextFor("service"), "No service events have been recorded yet.");

  // A lens that does hold events elsewhere keeps the generic filters message,
  // even when the currently displayed page happens to be empty.
  assert.equal(emptyTextFor("changes", [activityEvent("audit", "a")]), "No package events match these filters.");
});
