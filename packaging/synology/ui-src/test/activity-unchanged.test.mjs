// The Activity poll asks "has the feed changed since this version" and keeps its rows on "no".
//
// The package re-read, re-tailed and re-sanitised up to a megabyte of the same activity log every
// five seconds for a feed that changes a few times an hour, and the page threw the identical answer
// away. A full answer now names its version; the timer's next read sends it back as `since`, and an
// `unchanged` answer costs a hundred bytes. These tests fix when `since` may be sent (the timer's
// read only, same line count, no failure since) and what each kind of answer does to what is shown.
import assert from "node:assert/strict";
import test from "node:test";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import { ACTIVITY_VERSION_PATTERN, activityIsUnchanged, activityVersionOf } from "../src/feedModel.mjs";
import { installBrowser, loadAppComponent } from "./helpers/app-component.mjs";

const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
const VERSION = "1a2b:3c4d:5e6f-0";
const OTHER = "ffff:0:1-1";

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function event(code, epoch = 1757000000) {
  return { epoch, code, profile: "nas", category: "sync", level: "info", state: "ok", message: code, client_request_id: "" };
}

function fullFeed(events, version = VERSION, extra = {}) {
  return { schema: "sdsync.dsm-activity.v1", events, history: { files: 1, oldest_epoch: 1757000000 }, version, ...extra };
}

function unchanged(version = VERSION) {
  return { schema: "sdsync.dsm-activity.v1", unchanged: true, version };
}

function logsContext(component, answers, overrides = {}) {
  const asked = [];
  const context = {
    disposed: false,
    logsLoading: false,
    logsPaused: false,
    route: "activity",
    activityTab: "events",
    logLines: 200,
    logSource: "all",
    auth: {},
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
    connected: true,
    snapshot: {},
    snapshotLoading: false,
    snapshotPromise: null,
    asked,
    scheduled: 0,
    scheduleLogs() { this.scheduled += 1; },
    ...overrides
  };
  context.apiGet = (_auth, action, parameters) => {
    asked.push({ action, parameters });
    const answer = answers.shift();
    if (answer instanceof Error || (answer && answer.__reject)) return Promise.reject(answer.__reject || answer);
    return Promise.resolve(answer);
  };
  context.refreshLogs = (...args) => component.methods.refreshLogs.call(context, ...args);
  return context;
}

async function pair(answers, overrides) {
  const holder = {};
  const component = loadAppComponent({ apiGet: (...args) => holder.context.apiGet(...args) });
  holder.context = logsContext(component, answers, overrides);
  return { component, context: holder.context };
}

test("the version pattern in the request validator and in the feed model are the same pattern", () => {
  const declared = apiSource.match(/export const ACTIVITY_VERSION_PATTERN = (\/.*\/);/);
  assert.ok(declared, "api.js declares the pattern");
  assert.equal(declared[1], String(ACTIVITY_VERSION_PATTERN));
});

test("an Activity read may carry `since`, and nothing else may ride in the query", async () => {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  const seen = [];
  globalThis.window = {
    crypto: webcrypto, TextEncoder: globalThis.TextEncoder, setTimeout: globalThis.setTimeout, clearTimeout: globalThis.clearTimeout
  };
  globalThis.fetch = async (url) => {
    seen.push(String(url));
    return {
      redirected: false, status: 200, ok: true,
      headers: { get: () => "application/json" },
      async text() { return JSON.stringify(url.includes("api=") ? { schema: "sdsync.dsm-token.v1", success: true, data: { synotoken: "t" } } : unchanged()); }
    };
  };
  try {
    const api = await loadApi();
    const read = (parameters) => api.apiGet({}, "activity", parameters);
    const model = await read({ lines: 200, since: VERSION });
    assert.equal(model.unchanged, true);
    assert.ok(seen.some((url) => url.includes("action=activity") && url.includes(`since=${encodeURIComponent(VERSION)}`)));
    await assert.rejects(() => read({ lines: 200, since: "not a version!" }), /reviewed bridge contract/);
    await assert.rejects(() => read({ lines: 200, since: "A".repeat(10) }), /reviewed bridge contract/, "upper case is not the package's alphabet");
    await assert.rejects(() => read({ lines: 200, since: "a".repeat(97) }), /reviewed bridge contract/, "bounded at 96");
    await assert.rejects(() => read({ lines: 200, since: 12 }), /reviewed bridge contract/);
    await assert.rejects(() => read({ lines: 200, after: VERSION }), /reviewed bridge contract/);
    await assert.rejects(() => api.apiGet({}, "snapshot", { since: VERSION }), /reviewed bridge contract/, "`since` belongs to Activity alone");
    await assert.rejects(() => api.apiGet({}, "logs", { lines: 10, source: "all", since: VERSION }), /reviewed bridge contract/);
  } finally {
    for (const [name, value] of Object.entries(previous)) {
      if (value === undefined) delete globalThis[name];
      else globalThis[name] = value;
    }
  }
});

test("a full answer names its version, and only a well-formed one is kept", () => {
  assert.equal(activityVersionOf(fullFeed([])), VERSION);
  assert.equal(activityVersionOf({ schema: "x", events: [] }), "", "an older service names none");
  assert.equal(activityVersionOf(fullFeed([], "BAD VERSION")), "");
  assert.equal(activityVersionOf(fullFeed([], 7)), "");
  assert.equal(activityVersionOf(null), "");
});

test("`unchanged` confirms only the version this window sent, and only without a feed attached", () => {
  assert.equal(activityIsUnchanged(unchanged(), VERSION), true);
  assert.equal(activityIsUnchanged(unchanged(OTHER), VERSION), false, "another version is not a confirmation");
  assert.equal(activityIsUnchanged(unchanged(), ""), false, "nothing was sent, so nothing can be confirmed");
  assert.equal(activityIsUnchanged({ ...unchanged(), events: [] }, VERSION), false, "a feed is a feed");
  assert.equal(activityIsUnchanged({ ...unchanged(), schema: "other" }, VERSION), false);
});

test("the timer's read sends the held version, and `unchanged` keeps the rows, the history and the version", async () => {
  const restore = installBrowser();
  try {
    const rows = [event("a"), event("b")];
    const { context } = await pair([fullFeed(rows), unchanged()]);
    await context.refreshLogs();                       // opening the page: whole
    assert.deepEqual(context.asked[0].parameters, { lines: 200 }, "a person's read never sends a version");
    assert.equal(context.activityVersion, VERSION);
    assert.equal(context.activityVersionLines, 200);
    const held = context.activityEvents;
    const history = context.activityHistory;
    assert.equal(Object.isFrozen(held), true, "the feed is data replaced whole, not made reactive");

    const before = context.activityReceivedAtMs;
    await new Promise((resolve) => setTimeout(resolve, 3));
    await context.refreshLogs({ poll: true });         // the timer: asks
    assert.deepEqual(context.asked[1].parameters, { lines: 200, since: VERSION });
    assert.strictEqual(context.activityEvents, held, "the same array: no computed that reads it is invalidated");
    assert.strictEqual(context.activityHistory, history, "the history note is untouched");
    assert.equal(context.activityVersion, VERSION);
    assert.equal(context.activityFeedError, "");
    assert.ok(context.activityReceivedAtMs > before, "the answer still counts as a successful read");
    assert.equal(context.logsFailureStreak, 0);
    assert.equal(context.scheduled, 2);
  } finally {
    restore();
  }
});

test("a changed feed replaces the rows and the version", async () => {
  const restore = installBrowser();
  try {
    const { context } = await pair([fullFeed([event("a")]), fullFeed([event("a"), event("c")], OTHER)]);
    await context.refreshLogs();
    await context.refreshLogs({ poll: true });
    assert.equal(context.asked[1].parameters.since, VERSION);
    assert.equal(context.activityEvents.length, 2);
    assert.equal(context.activityVersion, OTHER);
  } finally {
    restore();
  }
});

test("`since` is never sent for another line count, a person's read, or after a failure", async () => {
  const restore = installBrowser();
  try {
    const failure = Object.assign(new Error("slow"), { status: 503, code: "client_timeout" });
    const { context } = await pair([
      fullFeed([event("a")]),     // 1 whole, learns VERSION at 200
      fullFeed([event("a")]),     // 2 line count changed: whole
      { __reject: failure },      // 3 poll fails
      fullFeed([event("a")]),     // 4 poll after the failure: whole
      fullFeed([event("a")]),     // 5 a person's read: whole
      unchanged()                 // 6 the next poll asks again
    ]);
    await context.refreshLogs({ poll: true });
    assert.equal(context.asked[0].parameters.since, undefined, "nothing held yet");
    context.logLines = 500;
    await context.refreshLogs({ poll: true });
    assert.deepEqual(context.asked[1].parameters, { lines: 500 }, "a version belongs to the line count it was read with");
    await context.refreshLogs({ poll: true });         // sends since (500), the read fails
    assert.equal(context.asked[2].parameters.since, VERSION);
    assert.notEqual(context.activityFeedError, "");
    assert.equal(context.activityVersion, "", "a failure forgets the version");
    await context.refreshLogs({ poll: true });
    assert.deepEqual(context.asked[3].parameters, { lines: 500 }, "the first read after a failure is whole");
    await context.refreshLogs();
    assert.deepEqual(context.asked[4].parameters, { lines: 500 }, "an explicit refresh is whole");
    await context.refreshLogs({ poll: true });
    assert.equal(context.asked[5].parameters.since, VERSION);
    assert.equal(context.activityFeedError, "");
  } finally {
    restore();
  }
});

test("a service that names no version is read whole every time, exactly as before", async () => {
  const restore = installBrowser();
  try {
    const old = (events) => ({ schema: "sdsync.dsm-activity.v1", events, history: null });
    const { context } = await pair([old([event("a")]), old([event("a")])]);
    await context.refreshLogs({ poll: true });
    await context.refreshLogs({ poll: true });
    assert.deepEqual(context.asked.map((read) => read.parameters), [{ lines: 200 }, { lines: 200 }]);
    assert.equal(context.activityVersion, "");
    assert.equal(context.activityEvents.length, 1);
  } finally {
    restore();
  }
});

test("an `unchanged` answer nobody asked for is a named failure, not an empty feed", async () => {
  const restore = installBrowser();
  try {
    const { context } = await pair([fullFeed([event("a")]), unchanged()]);
    await context.refreshLogs();
    await context.refreshLogs();                       // a person's read: no version sent
    assert.match(context.activityFeedError, /said nothing changed, but this window sent no version/);
    assert.equal(context.activityEvents.length, 1, "the rows already held are kept");
    assert.equal(context.activityVersion, "");
  } finally {
    restore();
  }
});

test("an `unchanged` answer naming another version is a failure too", async () => {
  const restore = installBrowser();
  try {
    const { context } = await pair([fullFeed([event("a")]), unchanged(OTHER)]);
    await context.refreshLogs();
    await context.refreshLogs({ poll: true });
    assert.notEqual(context.activityFeedError, "");
    assert.equal(context.activityVersion, "");
  } finally {
    restore();
  }
});

test("only the timer's own read asks; every other caller passes no trigger", async () => {
  const { readFile: read } = await import("node:fs/promises");
  const source = await read(new URL("../src/App.vue", import.meta.url), "utf8");
  assert.match(source, /this\.logTimer = window\.setTimeout\(\(\) => this\.refreshLogs\(\{ poll: true \}\), logsPollDelay\(this, interval\)\)/);
  const callers = [...source.matchAll(/refreshLogs\(([^)]*)\)/g)].map((match) => match[1]).filter((argument) => argument && argument !== "trigger");
  assert.deepEqual([...new Set(callers)], ["{ poll: true }"], "no other call site names a trigger");
});
