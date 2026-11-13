// What the dashboard asks the package for, how many times, in what order, and how many bytes come
// back: page load, a save, the steady poll, a long operation in a hidden tab, and paging through the
// sync status. The real api.js and the real App.vue methods run against a fake bridge that answers
// after a fixed latency, so the numbers are the browser's own request pattern and nothing else.
//
//   node --test test/request-sequencing.test.mjs                       asserts the pattern of this tree
//   SDSYNC_UI_SRC=/path/to/older/src node --test test/request-sequencing.test.mjs
//                                                                       prints the same measures for that tree
//
// Every run prints `MEASURE {json}` lines. With SDSYNC_UI_SRC set nothing is asserted: it is how the
// "before" column of the report was produced from the tree before these changes.
//
// What this cannot show: the cost of a request on the NAS (a process, a DSM user-service call, a
// relay). Only the count and the order are the browser's to know; the latency here is a stand-in.
import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { SRC, loadAppComponent, mountContext, snapshotContext, validSnapshot } from "./helpers/app-component.mjs";

const COMPARE_ONLY = Boolean(process.env.SDSYNC_UI_SRC);
const LATENCY_MS = 40;
const JOB = "d".repeat(48);
const VERSION = "5a5a:6b6b:7c7c-1";
const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
const settle = async (times = 40) => { for (let index = 0; index < times; index += 1) await new Promise((resolve) => setImmediate(resolve)); };

const apiSource = await readFile(new URL("api.js", SRC), "utf8");
async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function report(name, measures) {
  console.log(`MEASURE ${JSON.stringify({ scenario: name, tree: COMPARE_ONLY ? "compared" : "current", ...measures })}`);
}

function feed(events) {
  return Array.from({ length: events }, (_, index) => ({
    epoch: 1757000000 + index, code: `sync.module.${index % 7}`, profile: "nas", category: "sync", level: "info",
    state: "ok", message: `Synced reports/quarter-${index}/summary-${index}.pdf to the File Station destination`, client_request_id: ""
  }));
}

// A browser, and a bridge that answers each request after LATENCY_MS and keeps the log.
function fakeWorld({ hidden = false, virtualTime = false } = {}) {
  const previous = { window: globalThis.window, document: globalThis.document, fetch: globalThis.fetch };
  const log = [];
  const timers = [];
  const live = new Set();       // real timers the window under test armed (the 30 s clock tick, ...)
  const listeners = new Map();
  let nextTimer = 1;
  let virtualNow = 0;
  const document = {
    hidden,
    addEventListener(name, listener) { listeners.set(name, [...(listeners.get(name) || []), listener]); },
    removeEventListener(name, listener) { listeners.set(name, (listeners.get(name) || []).filter((item) => item !== listener)); }
  };
  const state = { resultReads: 0, activityFull: 0, activityUnchanged: 0, requestId: "", csrf: 0, snapshots: 0 };
  const fetchMock = async (urlText, options = {}) => {
    const url = new URL(String(urlText), "http://dsm.local");
    const action = url.searchParams.get("action") || (url.searchParams.get("api") ? "dsm-token" : "unknown");
    const entry = { at: virtualTime ? virtualNow : performance.now(), method: options.method || "GET", action, bytes: 0 };
    log.push(entry);
    if (!virtualTime) await sleep(LATENCY_MS);
    let model;
    if (action === "dsm-token") model = { success: true, data: { synotoken: "dsm-token-1" } };
    else if (action === "csrf") {
      state.csrf += 1;
      const issued = 1757000000;
      model = { schema: "sdsync.dsm-csrf.v1", csrf_token: `v1.${issued}.${issued + 300}.${"n".repeat(8)}.${"m".repeat(16)}`, expires_at_epoch: issued + 300 };
    } else if (action === "snapshot") {
      state.snapshots += 1;
      model = validSnapshot();
    } else if (action === "activity") {
      if (url.searchParams.get("since") === VERSION) {
        state.activityUnchanged += 1;
        model = { schema: "sdsync.dsm-activity.v1", unchanged: true, version: VERSION };
      } else {
        state.activityFull += 1;
        model = { schema: "sdsync.dsm-activity.v1", events: feed(200), history: { files: 1, oldest_epoch: 1757000000 }, version: VERSION };
      }
    } else if (entry.method === "POST") {
      const request = JSON.parse(options.body);
      state.requestId = request.request_id;
      model = { schema: "sdsync.dsm-queued.v1", ok: true, state: "queued", request_id: request.request_id, job_id: JOB };
    } else if (action === "result") {
      state.resultReads += 1;
      model = state.resultReads < state.completeAfter
        ? { schema: "sdsync.dsm-result-status.v1", ok: true, state: "pending", job_id: JOB }
        : {
          schema: "sdsync.dsm-result-status.v1", ok: true, state: "complete", job_id: JOB, client_request_id: state.requestId,
          result: { schema: "sdsync.dsm-result.v1", ok: true, status: "succeeded", message: "done", output: "{}", exit_code: 0 }
        };
    } else model = { schema: "unexpected" };
    const body = JSON.stringify(model);
    entry.bytes = Buffer.byteLength(body);
    return {
      redirected: false, status: 200, ok: true,
      headers: { get: (name) => name.toLowerCase() === "content-type" ? "application/json" : null },
      async text() { return body; }
    };
  };
  state.completeAfter = 3;
  globalThis.window = {
    crypto: globalThis.crypto || webcrypto,
    TextEncoder: globalThis.TextEncoder,
    AbortController: globalThis.AbortController,
    fetch: fetchMock,
    addEventListener() {},
    removeEventListener() {},
    matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
    setTimeout: virtualTime
      ? (callback, milliseconds) => { const id = nextTimer++; timers.push({ id, callback, at: virtualNow + milliseconds }); return id; }
      : (callback, milliseconds) => {
        const id = setTimeout(() => { live.delete(id); callback(); }, milliseconds);
        live.add(id);
        return id;
      },
    clearTimeout: virtualTime
      ? (id) => { const index = timers.findIndex((timer) => timer.id === id); if (index >= 0) timers.splice(index, 1); }
      : (id) => { live.delete(id); clearTimeout(id); }
  };
  globalThis.document = document;
  globalThis.fetch = fetchMock;
  return {
    log, state, document, timers,
    // Move the virtual clock forward, running every timer that falls due, letting each settle.
    async advance(untilMs) {
      for (;;) {
        await settle(10);
        const due = timers.filter((timer) => timer.at <= untilMs).sort((a, b) => a.at - b.at)[0];
        if (!due) break;
        timers.splice(timers.indexOf(due), 1);
        virtualNow = due.at;
        due.callback();
      }
      virtualNow = untilMs;
      await settle(10);
    },
    nowVirtual: () => virtualNow,
    show() { document.hidden = false; for (const listener of [...(listeners.get("visibilitychange") || [])]) listener(); },
    restore() {
      for (const id of live) clearTimeout(id);
      live.clear();
      for (const [name, value] of Object.entries(previous)) {
        if (value === undefined) delete globalThis[name];
        else globalThis[name] = value;
      }
    }
  };
}

// Round trips in a row: requests that start together are one wave, and a later wave starts only
// after the answers it waited for. The longest chain of dependent requests is the wave count.
function waves(log) {
  let count = 0;
  let last = -Infinity;
  for (const entry of log.slice().sort((a, b) => a.at - b.at)) {
    if (entry.at - last >= LATENCY_MS / 2) count += 1;
    last = entry.at;
  }
  return count;
}

test("page load: requests and the longest chain of them", async () => {
  const world = fakeWorld();
  try {
    const api = await loadApi();
    const component = loadAppComponent({ ...api });
    const context = mountContext(component, { auth: { signal: new AbortController().signal } });
    const started = performance.now();
    await component.mounted.call(context);
    const wall = performance.now() - started;
    const depth = waves(world.log);
    const actions = world.log.map((entry) => entry.action);
    const startsMs = world.log.map((entry) => Math.round(entry.at - started));
    report("page-load", { requests: world.log.length, actions, startsMs, serialRoundTrips: depth, wallMs: Math.round(wall), latencyMs: LATENCY_MS, bytes: world.log.reduce((sum, entry) => sum + entry.bytes, 0) });
    assert.equal(context.connected, true);
    assert.ok(context.snapshot && context.csrfToken);
    if (!COMPARE_ONLY) {
      assert.deepEqual(actions.slice().sort(), ["csrf", "dsm-token", "snapshot"]);
      assert.equal(depth, 2, "the DSM token, then the package token and the snapshot together");
      assert.ok(world.log.find((entry) => entry.action === "csrf").at - world.log.find((entry) => entry.action === "snapshot").at < LATENCY_MS / 2, "issued in the same tick");
    }
  } finally {
    world.restore();
  }
});

test("recovery after a failure that cleared the token", async () => {
  const world = fakeWorld();
  try {
    const api = await loadApi();
    const component = loadAppComponent({ ...api });
    const context = snapshotContext(component, { auth: { signal: new AbortController().signal }, csrfToken: "" });
    await api.apiGet(context.auth, "snapshot");           // the DSM token is already held
    world.log.length = 0;
    const started = performance.now();
    await context.refreshSnapshot(false);
    const wall = performance.now() - started;
    const depth = waves(world.log);
    report("recovery", { requests: world.log.length, actions: world.log.map((entry) => entry.action), serialRoundTrips: depth, wallMs: Math.round(wall) });
    if (!COMPARE_ONLY) assert.equal(depth, 1);
  } finally {
    world.restore();
  }
});

test("steady poll on the Activity page: requests and bytes per cycle", async () => {
  const world = fakeWorld();
  try {
    const api = await loadApi();
    const component = loadAppComponent({ ...api });
    const context = snapshotContext(component, {
      auth: { signal: new AbortController().signal },
      csrfToken: "held",
      route: "activity",
      activityTab: "events",
      logLines: 200,
      logSource: "all",
      logsLoading: false,
      logsPaused: false,
      activityEvents: [],
      activitySignature: "",
      activityVersion: "",
      activityVersionLines: 0,
      activityHistory: null,
      activityReceivedAtMs: 0,
      activityFeedError: "",
      logsFeedError: "",
      logsFailureStreak: 0,
      logsLastDurationMs: 0,
      logsReceivedAtMs: 0,
      logState: "",
      scheduleLogs() {}
    });
    context.refreshLogs = (...args) => component.methods.refreshLogs.call(context, ...args);
    await context.refreshSnapshot(false);                 // warms the DSM token
    await context.refreshLogs();                          // opening the page: the whole feed
    world.log.length = 0;
    const cycles = 4;
    const started = performance.now();
    for (let index = 0; index < cycles; index += 1) {
      await context.refreshSnapshot(false);
      await context.refreshLogs({ poll: true });
    }
    const wall = performance.now() - started;
    const bytes = world.log.reduce((sum, entry) => sum + entry.bytes, 0);
    const perCycle = world.log.length / cycles;
    report("steady-poll-activity-page", {
      cycles,
      requestsPerCycle: perCycle,
      bytesPerCycle: Math.round(bytes / cycles),
      activityBytesPerCycle: Math.round(world.log.filter((entry) => entry.action === "activity").reduce((sum, entry) => sum + entry.bytes, 0) / cycles),
      activityFull: world.state.activityFull - 1,
      activityUnchanged: world.state.activityUnchanged,
      wallMsPerCycle: Math.round(wall / cycles)
    });
    if (!COMPARE_ONLY) {
      assert.equal(perCycle, 2, "one snapshot and one activity read per cycle: the count did not grow");
      assert.equal(world.state.activityUnchanged, cycles);
      assert.ok(bytes / cycles < 20000, "an unchanged activity answer is small");
    }
  } finally {
    world.restore();
  }
});

test("a save: the same POST and result observation as ever", async () => {
  const world = fakeWorld();
  try {
    const api = await loadApi();
    world.state.completeAfter = 3;
    const auth = { signal: new AbortController().signal };
    await api.apiGet(auth, "csrf");
    world.log.length = 0;
    const started = performance.now();
    await api.apiPost(auth, "v1.1757000000.1757000300.nnnnnnnn.mmmmmmmmmmmmmmmm", "set-default", { name: "nas" }, true, 0);
    const wall = performance.now() - started;
    report("save", { requests: world.log.length, actions: world.log.map((entry) => `${entry.method}:${entry.action}`), wallMs: Math.round(wall) });
    if (!COMPARE_ONLY) assert.deepEqual(world.log.map((entry) => entry.method), ["POST", "GET", "GET", "GET"]);
  } finally {
    world.restore();
  }
});

test("a long operation watched for a minute: result reads, tab visible and tab hidden", async () => {
  const counts = {};
  for (const hidden of [false, true]) {
    const world = fakeWorld({ hidden, virtualTime: true });
    try {
      const api = await loadApi();
      world.state.completeAfter = 100000;                // never finishes inside the minute
      const auth = { signal: new AbortController().signal };
      const watching = api.apiPost(auth, "v1.1757000000.1757000300.nnnnnnnn.mmmmmmmmmmmmmmmm", "sync-status",
        { cursor: "", filter: "", include_excluded: false, limit: 100, profile: "nas", scope: "", state: "attention" }, true);
      watching.catch(() => {});
      await world.advance(60000);
      counts[hidden ? "hidden" : "visible"] = world.state.resultReads;
    } finally {
      world.restore();
    }
  }
  report("terminal-operation-one-minute", { resultReadsVisibleTab: counts.visible, resultReadsHiddenTab: counts.hidden });
  if (!COMPARE_ONLY) {
    assert.ok(counts.visible >= 25, `visible: ${counts.visible}`);
    assert.ok(counts.hidden <= 5, `hidden: ${counts.hidden}`);
  }
});

test("paging the sync status: walks for Next, Previous and Next again", async () => {
  const world = fakeWorld();
  try {
    const walks = [];
    const holder = {};
    const component = loadAppComponent({
      apiPost: (_auth, _token, _action, payload) => {
        walks.push(payload.cursor);
        const index = Number((payload.cursor || "c0").slice(1));
        return Promise.resolve({ output: JSON.stringify({ schema: "sdsync.status.v1", scope: "", compare: "size", limit: 100, truncated: true, next_cursor: `c${index + 1}`, entries: [], stats: { complete: true } }) });
      }
    });
    const context = {
      disposed: false, auth: {}, csrfToken: "t", syncStatusReady: true,
      syncStatusForm: { profile: "nas", state: "attention", filter: "", scope: "", include_excluded: false, limit: 100 },
      syncStatusEvidenceStamp: "s",
      syncStatusResult: { loaded: false, entries: [] }, syncStatusCursors: [], syncStatusPageNumber: 0, syncStatusPhase: "idle",
      syncStatusMessage: "", syncStatusBusy: false, syncStatusCheckedAtMs: 0, syncStatusFromMemory: false, liveProgress: { active: false },
      toast() {}
    };
    for (const name of Object.keys(component.computed)) {
      if (!(name in context)) Object.defineProperty(context, name, { get: () => component.computed[name].call(context), configurable: true });
    }
    for (const name of Object.keys(component.methods)) if (!(name in context)) context[name] = (...args) => component.methods[name].call(context, ...args);
    holder.context = context;
    await context.checkSyncStatus();
    await context.syncStatusNextPage();
    await context.syncStatusPreviousPage();
    await context.syncStatusNextPage();
    report("sync-status-paging", { walks: walks.length, cursors: walks });
    if (!COMPARE_ONLY) assert.equal(walks.length, 2, "Check status and Next walked; Previous and Next again did not");
  } finally {
    world.restore();
  }
});
