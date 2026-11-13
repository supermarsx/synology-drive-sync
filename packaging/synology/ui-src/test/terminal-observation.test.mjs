// How a terminal operation (Doctor, a status walk, a re-upload) is watched.
//
// Hidden tab: the result is asked for every 15 s instead of on the 150 ms..2 s ramp, because each
// ask is a CGI process on the NAS and the page is not on screen; showing the tab ends the wait.
// Detach: the person can stop watching one operation after ten silent minutes. That ends the poll
// alone, cancels nothing on the NAS, locks nothing, and is not mistaken for a failure.
import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { installBrowser, loadAppComponent } from "./helpers/app-component.mjs";

const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
const JOB = "c".repeat(48);

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function jsonResponse(model, status = 200) {
  return {
    redirected: false,
    status,
    ok: status >= 200 && status < 300,
    headers: { get: (name) => name.toLowerCase() === "content-type" ? "application/json" : null },
    async text() { return JSON.stringify(model); }
  };
}

// A browser whose timers the test fires and whose document the test hides and shows.
function fakeBrowser({ hidden = false } = {}) {
  const previous = { window: globalThis.window, document: globalThis.document, fetch: globalThis.fetch };
  const timers = [];
  const listeners = new Map();
  let nextId = 1;
  const document = {
    hidden,
    addEventListener(name, listener) { listeners.set(name, [...(listeners.get(name) || []), listener]); },
    removeEventListener(name, listener) { listeners.set(name, (listeners.get(name) || []).filter((item) => item !== listener)); },
    listenerCount(name) { return (listeners.get(name) || []).length; },
    show() { document.hidden = false; for (const listener of [...(listeners.get("visibilitychange") || [])]) listener(); },
    hide() { document.hidden = true; for (const listener of [...(listeners.get("visibilitychange") || [])]) listener(); }
  };
  globalThis.window = {
    crypto: globalThis.crypto || webcrypto,
    TextEncoder: globalThis.TextEncoder,
    AbortController: globalThis.AbortController,
    setTimeout(callback, milliseconds) { const id = nextId++; timers.push({ id, callback, milliseconds }); return id; },
    clearTimeout(id) { const index = timers.findIndex((timer) => timer.id === id); if (index >= 0) timers.splice(index, 1); }
  };
  globalThis.document = document;
  const restore = () => {
    for (const [name, value] of Object.entries(previous)) {
      if (value === undefined) delete globalThis[name];
      else globalThis[name] = value;
    }
  };
  return { timers, document, restore, listeners };
}

const settle = async (times = 30) => { for (let index = 0; index < times; index += 1) await new Promise((resolve) => setImmediate(resolve)); };

// The ask-for-the-result waits among all the timers (request timeouts are timers too).
const waits = (timers) => timers.map((timer) => timer.milliseconds).filter((milliseconds) => milliseconds < 10000 || milliseconds === 15000);

function installBridge(api, { onRead } = {}) {
  const state = { reads: 0, requestId: "" };
  globalThis.fetch = (url, options) => {
    if (url === api.DSM_TOKEN_URL) return Promise.reject(new TypeError("no DSM token in this test"));
    if (options.method === "POST") {
      const request = JSON.parse(options.body);
      state.requestId = request.request_id;
      return Promise.resolve(jsonResponse({ schema: api.QUEUED_SCHEMA, ok: true, state: "queued", request_id: request.request_id, job_id: JOB }));
    }
    state.reads += 1;
    if (onRead) return onRead(state, options);
    return Promise.resolve(jsonResponse({ schema: api.RESULT_STATUS_SCHEMA, ok: true, state: "pending", job_id: JOB }));
  };
  return state;
}

const SYNC_STATUS = { cursor: "", filter: "", include_excluded: false, limit: 100, profile: "nas", scope: "", state: "attention" };

function start(api, auth, extra = {}) {
  return api.apiPost(auth, "csrf-token", "sync-status", SYNC_STATUS, true, undefined, extra.options, extra.progress || null);
}

test("a visible tab asks on the ramp; a hidden tab asks every fifteen seconds", async () => {
  const browser = fakeBrowser();
  try {
    const api = await loadApi();
    installBridge(api);
    const auth = { signal: new AbortController().signal };
    const watching = start(api, auth);
    watching.catch(() => {});
    await settle();
    assert.deepEqual(waits(browser.timers), [150], "the first wait is the first rung of the ramp");
    browser.timers.splice(0).forEach((timer) => timer.callback());
    await settle();
    assert.deepEqual(waits(browser.timers), [300]);

    browser.document.hide();
    browser.timers.splice(0).forEach((timer) => timer.callback());
    await settle();
    assert.deepEqual(waits(browser.timers), [api.HIDDEN_RESULT_POLL_INTERVAL_MS], "hidden: one ask per fifteen seconds");
    assert.equal(api.HIDDEN_RESULT_POLL_INTERVAL_MS, 15000);
    browser.timers.splice(0).forEach((timer) => timer.callback());
    await settle();
    assert.deepEqual(waits(browser.timers), [15000], "and it stays there while the tab stays hidden");
  } finally {
    browser.restore();
  }
});

test("showing the tab again ends the long wait at once and returns to the ramp", async () => {
  const browser = fakeBrowser({ hidden: true });
  try {
    const api = await loadApi();
    const state = installBridge(api);
    const watching = start(api, { signal: new AbortController().signal });
    watching.catch(() => {});
    await settle();
    assert.deepEqual(waits(browser.timers), [15000]);
    assert.equal(state.reads, 1);
    assert.equal(browser.document.listenerCount("visibilitychange"), 1, "the wait is listening for the tab to be shown");

    browser.document.show();
    await settle();
    assert.equal(state.reads, 2, "the next read happened without waiting out the fifteen seconds");
    assert.equal(waits(browser.timers).includes(15000), false, "the abandoned long timer was cleared");
    assert.equal(browser.document.listenerCount("visibilitychange"), 0, "and the listener is gone");
    assert.deepEqual(waits(browser.timers), [300], "visible again: the ramp continues");
  } finally {
    browser.restore();
  }
});

test("a bounded (autosave) observation is not slowed by a hidden tab", async () => {
  const browser = fakeBrowser({ hidden: true });
  try {
    const api = await loadApi();
    installBridge(api);
    const options = { ...api.AUTOSAVE_API_LIMITS, setTimeout: window.setTimeout, clearTimeout: window.clearTimeout };
    const watching = api.apiPost({ signal: new AbortController().signal }, "csrf-token", "sync-status", SYNC_STATUS, true, undefined, options);
    watching.catch(() => {});
    await settle();
    assert.equal(waits(browser.timers).includes(15000), false, "an autosave has its own thirty-second ceiling");
    assert.ok(waits(browser.timers).includes(150));
  } finally {
    browser.restore();
  }
});

test("detaching ends that operation's poll with a named, non-failing error and no further reads", async () => {
  const browser = fakeBrowser();
  try {
    const api = await loadApi();
    const state = installBridge(api);
    const windowAbort = new AbortController();
    const detach = new AbortController();
    const progress = () => {};
    progress.detachSignal = detach.signal;
    const watching = start(api, { signal: windowAbort.signal }, { progress });
    let outcome;
    watching.then(() => { outcome = { ok: true }; }, (error) => { outcome = { error }; });
    await settle();
    assert.equal(state.reads, 1);

    detach.abort();
    await settle();
    assert.ok(outcome && outcome.error, "the operation ended");
    const error = outcome.error;
    assert.equal(error.name, "ObservationDetachedError");
    assert.equal(error.detached, true);
    assert.equal(error.accepted, true, "the job was accepted");
    assert.equal(error.outcomeUnknown, false, "so nothing is locked behind an unknown outcome");
    assert.equal(error.trustedJobId, true);
    assert.equal(error.jobId, JOB);
    assert.match(error.message, /not told to cancel it and may still run it/);
    assert.match(error.message, /Activity/);

    const reads = state.reads;
    browser.timers.splice(0).forEach((timer) => timer.callback());
    await settle();
    assert.equal(state.reads, reads, "no read after detaching");
    assert.equal(windowAbort.signal.aborted, false, "the window's other requests are untouched");
  } finally {
    browser.restore();
  }
});

test("detaching while a read is in flight ends it too, and an already-detached signal never starts a read", async () => {
  const browser = fakeBrowser();
  try {
    const api = await loadApi();
    const held = installBridge(api, {
      onRead: (_state, options) => new Promise((_resolve, reject) => {
        options.signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), { once: true });
      })
    });
    const detach = new AbortController();
    const progress = () => {};
    progress.detachSignal = detach.signal;
    const watching = start(api, { signal: new AbortController().signal }, { progress });
    let outcome;
    watching.then(() => { outcome = { ok: true }; }, (error) => { outcome = { error }; });
    await settle();
    assert.equal(held.reads, 1, "a read is in flight");
    detach.abort();
    await settle();
    assert.equal(outcome.error.name, "ObservationDetachedError");

    const before = held.reads;
    const detached = new AbortController();
    detached.abort();
    const early = () => {};
    early.detachSignal = detached.signal;
    await assert.rejects(() => start(api, { signal: new AbortController().signal }, { progress: early }), (error) => error.detached === true);
    assert.equal(held.reads, before, "no result read was made for an operation already detached");
  } finally {
    browser.restore();
  }
});

test("closing the window is still the ordinary cancellation, not a detach", async () => {
  const browser = fakeBrowser();
  try {
    const api = await loadApi();
    installBridge(api);
    const windowAbort = new AbortController();
    const detach = new AbortController();
    const progress = () => {};
    progress.detachSignal = detach.signal;
    const watching = start(api, { signal: windowAbort.signal }, { progress });
    let outcome;
    watching.then(() => { outcome = { ok: true }; }, (error) => { outcome = { error }; });
    await settle();
    windowAbort.abort();
    await settle();
    assert.ok(outcome.error);
    assert.notEqual(outcome.error.name, "ObservationDetachedError");
    assert.equal(outcome.error.detached, undefined);
  } finally {
    browser.restore();
  }
});

test("an operation with no detach signal behaves exactly as before", async () => {
  const browser = fakeBrowser();
  try {
    const api = await loadApi();
    const state = installBridge(api, {
      onRead: (_state, _options) => Promise.resolve(jsonResponse({
        schema: api.RESULT_STATUS_SCHEMA, ok: true, state: "complete", job_id: JOB, client_request_id: state.requestId,
        result: { schema: api.RESULT_SCHEMA, ok: true, status: "succeeded", message: "done", output: "x", exit_code: 0 }
      }))
    });
    const result = await start(api, { signal: new AbortController().signal }, { progress: () => {} });
    assert.equal(result.output, "x");
  } finally {
    browser.restore();
  }
});

// ---- the window's side ----------------------------------------------------------------------

function stalledContext(component, overrides = {}) {
  const context = {
    disposed: false,
    nowMs: 0,
    liveProgress: { active: true, operation: "sync-status", progress: null, startedAtMs: 1_000_000 },
    liveProgressDetach: new AbortController(),
    controllerLiveness: { known: true, service: "running", activePid: 0, stale: false, updatedEpoch: 0, packageEpoch: 0 },
    run: {},
    runActive: false,
    ...overrides
  };
  for (const name of Object.keys(component.computed)) {
    if (!Object.prototype.hasOwnProperty.call(overrides, name)) {
      Object.defineProperty(context, name, { get: () => component.computed[name].call(context), configurable: true });
    }
  }
  for (const name of ["detachLiveOperation"]) context[name] = (...args) => component.methods[name].call(context, ...args);
  return context;
}

test("an operation watched for ten minutes with no phase published is named stalled, and a phase clears it", () => {
  const restore = installBrowser();
  try {
    const component = loadAppComponent();
    const tenMinutes = 10 * 60 * 1000;
    const context = stalledContext(component, { nowMs: 1_000_000 + tenMinutes - 1 });
    assert.equal(context.liveProgressStalled, false, "one millisecond short");
    context.nowMs = 1_000_000 + tenMinutes;
    assert.equal(context.liveProgressStalled, true);
    assert.match(context.liveProgressStalledText, /no progress on this for 10 minutes/);
    assert.match(context.liveProgressStalledText, /cancels nothing: the NAS may still run it/);
    assert.match(context.liveProgressStalledText, /Activity/);
    context.liveProgress = { ...context.liveProgress, progress: { phase: "walking" } };
    assert.equal(context.liveProgressStalled, false, "an operation that reports phases is running, not stalled");
    context.liveProgress = { active: false, operation: "", progress: null, startedAtMs: 0 };
    assert.equal(context.liveProgressStalled, false, "nothing to stop waiting for");
  } finally {
    restore();
  }
});

test("stopping the wait aborts only that operation's signal", () => {
  const restore = installBrowser();
  try {
    const component = loadAppComponent();
    const context = stalledContext(component);
    const signal = context.liveProgressDetach.signal;
    assert.equal(signal.aborted, false);
    context.detachLiveOperation();
    assert.equal(signal.aborted, true);
    context.liveProgressDetach = null;
    context.liveProgress = { active: false, operation: "", progress: null, startedAtMs: 0 };
    assert.doesNotThrow(() => context.detachLiveOperation(), "nothing in flight is not an error");
    // The button was offered for an operation this browser gave no way to stop: say so.
    const toasts = [];
    context.toast = (title, message, isError) => toasts.push({ title, message, isError });
    context.liveProgress = { active: true, operation: "doctor", progress: null, startedAtMs: 1 };
    context.detachLiveOperation();
    assert.deepEqual(toasts.map((toast) => [toast.title, toast.isError]), [["Cannot stop waiting", true]]);
  } finally {
    restore();
  }
});

test("the status walk, the re-upload and Doctor each offer Stop waiting, and a detach is reported but never an incident", async () => {
  const source = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
  for (const flag of ["syncStatusBusy", "resyncBusy", "doctorProgress.active"]) {
    assert.ok(
      source.includes(`<div v-if="${flag} && liveProgressStalled" class="sdsync-warning sdsync-live-stalled"`),
      `${flag} offers the stalled notice`
    );
  }
  assert.equal([...source.matchAll(/@click="detachLiveOperation"/g)].length, 3);
  assert.match(source, /if \(error && error\.detached === true\) \{[\s\S]*?this\.toast\("Stopped waiting", stopped, false\);[\s\S]*?return \{ unknown: false, inspection: false, detached: true/);
  const component = loadAppComponent();
  const toasts = [];
  const context = { toasts, toast: (title, message, isError) => toasts.push({ title, message, isError }) };
  const report = component.methods.reportMutationError.call(
    context,
    Object.assign(new Error("This window stopped waiting for the result."), {
      detached: true, accepted: true, outcomeUnknown: false, trustedJobId: true, jobId: JOB
    }),
    "Sync status failed", "Sync status outcome unknown", "The package could not complete the status query."
  );
  assert.equal(report.detached, true);
  assert.equal(report.unknown, false);
  assert.equal(report.inspection, false);
  assert.deepEqual(toasts.map((toast) => [toast.title, toast.isError]), [["Stopped waiting", false]]);
  assert.match(toasts[0].message, new RegExp(`Queued job ID: ${JOB}`));
});
