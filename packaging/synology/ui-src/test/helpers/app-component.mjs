// App.vue's <script> as a component options object with a hand-written stub table, for the tests
// that drive one method on a `this` they build. The same shape csrf-lifecycle and read-ladder-surface
// use, kept once here: `stubs` overrides or adds any binding the script imports from ./api.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import {
  WIDGET_ACTIVE_POLL_MS,
  WIDGET_BACKOFF_RAMP_MS,
  WIDGET_IDLE_POLL_MS
} from "../../src/widgetModel.mjs";
import { APP_MODULE_STUBS } from "./app-stubs.mjs";

// The sources under test. SDSYNC_UI_SRC names another checkout's src/ (request-sequencing.test.mjs
// runs the same scenarios against an older tree to print a before/after); unset, it is this one.
export const SRC = process.env.SDSYNC_UI_SRC
  ? pathToFileURL(`${resolve(process.env.SDSYNC_UI_SRC)}/`)
  : new URL("../../src/", import.meta.url);

export const appSource = await readFile(new URL("App.vue", SRC), "utf8");

export function loadAppComponent(stubs = {}) {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "App.vue script block is missing");
  const executable = script[1]
    .replace(/^import \{ ActionIcon \} from "\.\/ActionIcon";\s*/m, "")
    .replace(/^import \{ createAutosaveCoordinator \} from "\.\/autosave";\s*/m, "")
    .replace(/^import \{ installControlLayout \} from "\.\/controlLayout";\s*/m, "")
    .replace(/import \{[\s\S]*?\}\s*from "\.\/api";\s*/, "")
    .replace(/^import SecurityPanel from "\.\/SecurityPanel\.vue";\s*/m, "")
    .replace("export default {", "const AppComponent = {")
    + "\nreturn AppComponent;";
  const table = {
    ...APP_MODULE_STUBS,
    WIDGET_ACTIVE_POLL_MS,
    WIDGET_BACKOFF_RAMP_MS,
    WIDGET_IDLE_POLL_MS,
    ACTIONS: {
      configureProfile: "configure-profile", setSecret: "set-secret",
      testProfileAuth: "test-profile-auth", browseRemote: "browse-remote",
      routine: "routine", alertPolicy: "alert-policy", securityPolicy: "security-policy",
      clientEvent: "client-event", removeProfile: "remove-profile", execute: "action",
      syncStatus: "sync-status", resync: "resync", clearLogs: "clear-logs"
    },
    AUTOSAVE_API_LIMITS: Object.freeze({}),
    MAX_RESPONSE_BYTES: 1024 * 1024,
    PROGRESS_UNAVAILABLE: Object.freeze({ unavailable: true }),
    QueuedOutcomeUnknownError: class extends Error {},
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    SYNC_STATUS_MAX_LIMIT: 200,
    trustedRequestProgress: () => null,
    apiGet: async () => ({}),
    apiPost: async () => ({ ok: true }),
    probeRequestOutcome: async () => ({}),
    purgeReconciliationAuth: () => undefined,
    reconcileMutationRequest: async () => ({}),
    csrfTokenStale: () => false,
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback).slice(0, 65536),
    formatBytes: String,
    formatDate: (value) => `@${value}`,
    formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    createAutosaveCoordinator: () => ({
      cancel() {}, dispose() {}, getState: () => ({ registered: false, dirty: false }),
      hydrate() {}, setGlobalBusy() {}, setScopeBlocked() {}, update: () => ({ dirty: false })
    }),
    installControlLayout: () => () => {},
    ActionIcon: { name: "ActionIcon" },
    SecurityPanel: {},
    ...stubs
  };
  return Function(...Object.keys(table), executable)(...Object.values(table));
}

/** Browser globals a component method may touch, restored by the returned function. */
export function installBrowser(overrides = {}) {
  const previous = { window: globalThis.window, document: globalThis.document };
  const timers = [];
  globalThis.window = {
    AbortController: globalThis.AbortController,
    clearTimeout() {},
    setTimeout(callback, milliseconds) { timers.push({ callback, milliseconds }); return timers.length; },
    addEventListener() {},
    removeEventListener() {},
    matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
    ...(overrides.window || {})
  };
  globalThis.document = { hidden: false, addEventListener() {}, removeEventListener() {}, ...(overrides.document || {}) };
  const restore = () => {
    for (const [name, value] of Object.entries(previous)) {
      if (value === undefined) delete globalThis[name];
      else globalThis[name] = value;
    }
  };
  restore.timers = timers;
  return restore;
}

/** A promise settled from outside, for ordering what a method starts before anything answers. */
export function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolveFn, rejectFn) => { resolve = resolveFn; reject = rejectFn; });
  return { promise, resolve, reject };
}

export function validSnapshot(extra = {}) {
  return {
    schema: "sdsync.dsm-api.v1",
    generated_at_epoch: 1757000000,
    service: { state: "running", pid: 4242 },
    profiles: [],
    routines: [],
    capabilities: { mutations: true, secrets: true, write_test: true },
    ...extra
  };
}

// A `this` for refreshSnapshot / refreshCsrf with every collaborator stubbed. Methods named in
// `overrides` are the test's own; the rest are the component's real ones.
export function snapshotContext(component, overrides = {}) {
  const toasts = [];
  const context = {
    disposed: false,
    auth: {},
    csrfToken: "",
    csrfAttempt: null,
    snapshot: null,
    snapshotSignature: "",
    snapshotReceivedAtMs: 0,
    snapshotFailureCode: "",
    snapshotFailureStreak: 0,
    snapshotPromise: null,
    snapshotRefreshQueued: false,
    snapshotGeneration: 0,
    snapshotLoading: false,
    snapshotRefreshBlocked: false,
    connected: false,
    connectionLabel: "",
    freshness: "Waiting for status",
    bridgeIssue: { title: "", message: "" },
    canMutate: true,
    settings: { status_refresh: 5000 },
    scheduled: 0,
    toasts,
    toast(title, message, error = false) { toasts.push({ title, message, error }); },
    hydrateAlerts() {},
    hydrateSecurityPolicy() {},
    maybeNotifyFailure() {},
    scheduleSnapshot() { this.scheduled += 1; },
    ...overrides
  };
  for (const name of ["describeBridgeError", "refreshSnapshot", "refreshCsrf", "snapshotRetryDelay", "snapshotNewerThan"]) {
    if (!(name in overrides)) context[name] = (...args) => component.methods[name].call(context, ...args);
  }
  return context;
}

export function mountContext(component, overrides = {}) {
  const context = snapshotContext(component, {
    abortController: null,
    settings: { theme: "dark", status_refresh: 5000, log_refresh: 5000 },
    route: "overview",
    $el: {},
    $watch() { return () => {}; },
    connectionWatchCleanups: [],
    interfaceSettingsPayload() { return {}; },
    stopTimers() {},
    refreshLogs() {},
    ...overrides
  });
  context.installProfileWindowGuard = (...args) => component.methods.installProfileWindowGuard.apply(context, args);
  context.removeProfileWindowGuard = (...args) => component.methods.removeProfileWindowGuard.apply(context, args);
  return context;
}
