// What the dashboard keeps and says when a read or an action fails.
//
// - The stored totals survive a failed re-read, with the cause and their age beside them, as the
//   snapshot and the feeds already do. They used to be emptied on any transient error.
// - The four toasts that had a fixed body and swallowed the exception now carry its name and
//   message, bounded.
// - The feeds Vue 2 would walk object by object are frozen.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { installBrowser, loadAppComponent } from "./helpers/app-component.mjs";

const source = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

function rollupDocument(observed = 1757000000) {
  return {
    schema: "sdsync.status-rollup-aggregate.v1",
    complete: true,
    observed_at_epoch: observed,
    profiles_total: 1,
    profiles_observed: 1,
    profiles_never_observed: [],
    total: { in_sync: { files: 3, bytes: 30 }, would_transfer: { files: 0, bytes: 0 }, attention_entries: 0, total_entries: 3 },
    profiles: []
  };
}

function rollupContext(component, apiGet, overrides = {}) {
  const context = {
    disposed: false,
    auth: {},
    statusRollup: null,
    statusRollupError: "",
    statusRollupReceivedAtMs: 0,
    statusRollupLoading: false,
    statusRollupMessage: "Not read yet",
    nowMs: 0,
    describeBridgeError: (error) => ({ title: "x", message: `bridge says ${error.code}` }),
    ...overrides
  };
  for (const name of ["refreshStatusRollup", "statusRollupFailed"]) context[name] = (...args) => component.methods[name].call(context, ...args);
  for (const name of ["statusRollupFreshness", "statusRollupReadAge"]) {
    Object.defineProperty(context, name, { get: () => component.computed[name].call(context), configurable: true });
  }
  return context;
}

function rollupApp(apiGet) {
  return loadAppComponent({ apiGet, cacheHealthNote: () => "" });
}

test("a failed re-read keeps the last totals, names the cause and says how old they are", async () => {
  const restore = installBrowser();
  try {
    let answer = async () => rollupDocument();
    const component = rollupApp((...args) => answer(...args));
    const context = rollupContext(component, null);
    context.statusRollup = { loaded: false };
    await context.refreshStatusRollup();
    assert.equal(context.statusRollup.loaded, true);
    assert.equal(context.statusRollupError, "");
    const kept = context.statusRollup;

    answer = async () => { throw Object.assign(new Error("slow"), { status: 503, code: "client_timeout" }); };
    context.nowMs = context.statusRollupReceivedAtMs + 5 * 60 * 1000;
    await context.refreshStatusRollup();
    assert.strictEqual(context.statusRollup, kept, "the totals were not emptied");
    assert.equal(context.statusRollupError, "bridge says client_timeout", "the cause is shown");
    assert.match(context.statusRollupFreshness, /totals as read 5 minutes ago/);
    assert.equal(context.statusRollupLoading, false);

    answer = async () => rollupDocument(1757000500);
    await context.refreshStatusRollup();
    assert.equal(context.statusRollupError, "", "a good read clears the failure");
    assert.notStrictEqual(context.statusRollup, kept);
  } finally {
    restore();
  }
});

test("a document that is not the totals is a failed read too, and keeps what is held", async () => {
  const restore = installBrowser();
  try {
    let answer = async () => rollupDocument();
    const component = rollupApp((...args) => answer(...args));
    const context = rollupContext(component, null);
    context.statusRollup = { loaded: false };
    await context.refreshStatusRollup();
    const kept = context.statusRollup;
    answer = async () => ({ schema: "something-else" });
    await context.refreshStatusRollup();
    assert.strictEqual(context.statusRollup, kept);
    assert.match(context.statusRollupError, /\S/);
  } finally {
    restore();
  }
});

test("with nothing read yet, a failure is the page's own message, as before", async () => {
  const restore = installBrowser();
  try {
    const component = rollupApp(async () => { throw Object.assign(new Error("down"), { status: 503, code: "manager_unavailable" }); });
    const context = rollupContext(component, null);
    context.statusRollup = { loaded: false };
    await context.refreshStatusRollup();
    assert.equal(context.statusRollup.loaded, false);
    assert.equal(context.statusRollupError, "");
    assert.equal(context.statusRollupMessage, "bridge says manager_unavailable");
  } finally {
    restore();
  }
});

test("the page says the totals could not be re-read, with the cause, above the retained totals", () => {
  assert.match(source, /<p v-if="statusRollupError" class="sdsync-field-note" role="status">The totals could not be re-read, so the last ones read are shown\. \{\{ statusRollupError \}\}<\/p>/);
});

// ---- toasts with a cause ---------------------------------------------------------------------

test("DSM Help names why it did not open", () => {
  const toasts = [];
  const context = { route: "overview", toast: (title, message, error) => toasts.push({ title, message, error }) };
  const component = loadAppComponent();
  const previous = globalThis.SYNO;
  try {
    delete globalThis.SYNO;
    component.methods.openDsmHelp.call(context);
    assert.match(toasts[0].message, /launcher \(SYNO\.SDS\.AppLaunch\) is not available in this window/);

    globalThis.SYNO = { SDS: { AppLaunch() { throw Object.assign(new TypeError("Cannot read properties of undefined"), {}); } } };
    component.methods.openDsmHelp.call(context);
    assert.equal(toasts.length, 2);
    assert.match(toasts[1].message, /DSM refused to open its help \(TypeError: Cannot read properties of undefined\)\./);
    assert.deepEqual(toasts.map((toast) => toast.error), [true, true]);
  } finally {
    if (previous === undefined) delete globalThis.SYNO;
    else globalThis.SYNO = previous;
  }
});

test("a refused copy carries the browser's own reason, bounded", async () => {
  const toasts = [];
  const component = loadAppComponent();
  const context = {
    toast: (title, message, error) => toasts.push({ title, message, error }),
    writeTroubleshootingClipboard: async () => { throw Object.assign(new Error(`Document is not focused. ${"x".repeat(500)}`), { name: "NotAllowedError" }); }
  };
  const copied = await component.methods.copyTroubleshootingText.call(context, "some evidence", "Record");
  assert.equal(copied, false);
  assert.equal(toasts.length, 1);
  assert.equal(toasts[0].title, "Copy failed");
  assert.match(toasts[0].message, /clipboard access \(NotAllowedError: Document is not focused\./);
  assert.ok(toasts[0].message.length < 400, "the cause is bounded");
  assert.match(toasts[0].message, /copy it manually\.$/);
});

test("a sync failure with no attention item says which failure it announces", () => {
  const toasts = [];
  const component = loadAppComponent({ outcomeFailureKey: () => "run:nas:1757000000:1:source_missing" });
  const context = {
    outcomes: {},
    lastOutcomeFailureKey: "",
    attentionItems: [],
    toast: (title, message, error) => toasts.push({ title, message, error })
  };
  component.methods.announceOutcomeFailure.call(context);
  assert.equal(toasts.length, 1);
  assert.equal(toasts[0].title, "A sync failed");
  assert.match(toasts[0].message, /^Run of nas at @1757000000, exit 1, reason source_missing\. No attention item describes it\. Open Activity for details\.$/);
  assert.equal(toasts[0].error, true);

  const profile = loadAppComponent({ outcomeFailureKey: () => "profile:docs:1757000100:index_unreadable" });
  const again = { ...context, lastOutcomeFailureKey: "" };
  toasts.length = 0;
  profile.methods.announceOutcomeFailure.call(again);
  assert.match(toasts[0].message, /^Profile docs at @1757000100, reason index_unreadable\./);

  const named = { ...context, lastOutcomeFailureKey: "", attentionItems: [{ level: "fail", title: "Run failed", detail: "Because." }] };
  toasts.length = 0;
  component.methods.announceOutcomeFailure.call(named);
  assert.deepEqual(toasts.map((toast) => [toast.title, toast.message]), [["Run failed", "Because."]], "an item still speaks for itself");
});

// ---- frozen feeds ----------------------------------------------------------------------------

test("the package log records are frozen like the activity feed", () => {
  assert.match(source, /this\.logRecords = Object\.freeze\(records\);/);
  assert.match(source, /this\.activityEvents = Object\.freeze\(activity\.value\.events\);/);
  assert.match(source, /const loadedPage = Object\.freeze\(Object\.assign\(syncStatusPage\(document, profile\), \{ query \}\)\);/);
});

test("the numeric selects carry string values the page converts where it reads them", () => {
  assert.match(source, /statusRefreshOptions\(\) \{ return options\(\[\["0", "Manual only"\], \["3000"/);
  assert.match(source, /logRefreshOptions\(\) \{ return options\(\[\["0", "Manual only"\]/);
  assert.doesNotMatch(source, /\[1000, "Every second"\]/);
});
