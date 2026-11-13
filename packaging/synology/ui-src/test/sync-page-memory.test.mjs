// Sync status pages this window already fetched are shown again from memory, with their age.
//
// The per-file check keeps no index: every page is a queued job that walks both trees. Going back
// to a page the person was just reading re-ran that walk, with the audit rows and the NAS load it
// costs. Now Previous (and Next to a page already fetched) reuse the page while it is under five
// minutes old, nothing it compared against has changed and the query is the same; Check status is
// always a new walk and forgets every remembered page.
import assert from "node:assert/strict";
import test from "node:test";
import { installBrowser, loadAppComponent } from "./helpers/app-component.mjs";

const FIVE_MINUTES = 5 * 60 * 1000;

function pageDocument(profile, cursor, next, rows) {
  return {
    schema: "sdsync.status.v1",
    scope: "",
    compare: "size and mtime",
    limit: 100,
    truncated: Boolean(next),
    next_cursor: next || "",
    entries: Array.from({ length: rows }, (_, index) => ({ relative: `${cursor || "p0"}/file-${index}`, state: "differs" })),
    stats: { complete: true, total: 250 },
    profile
  };
}

function setup(overrides = {}, stubs = {}) {
  const posts = [];
  const holder = {};
  const component = loadAppComponent({
    apiPost: (...args) => holder.context.apiPost(...args),
    ...stubs
  });
  const context = {
    disposed: false,
    auth: {},
    csrfToken: "token",
    syncStatusReady: true,
    syncStatusForm: { profile: "nas", state: "attention", filter: "", scope: "", include_excluded: false, limit: 100 },
    syncStatusEvidenceStamp: "stamp-1",
    syncStatusResult: { loaded: false, entries: [] },
    syncStatusCursors: [],
    syncStatusPageNumber: 0,
    syncStatusPhase: "idle",
    syncStatusMessage: "",
    syncStatusBusy: false,
    syncStatusCheckedAtMs: 0,
    syncStatusFromMemory: false,
    liveProgress: { active: false },
    nowMs: Date.now(),
    toasts: [],
    toast(title, message, error) { this.toasts.push({ title, message, error }); },
    posts,
    // Page N of the walk: the cursor names the page after it.
    apiPost(_auth, _token, _action, payload) {
      posts.push(payload);
      const index = Number((payload.cursor || "c0").slice(1));
      return Promise.resolve({ output: JSON.stringify(pageDocument("nas", payload.cursor, `c${index + 1}`, 2)) });
    },
    ...overrides
  };
  for (const name of Object.keys(component.computed)) {
    if (!(name in overrides) && !(name in context)) {
      Object.defineProperty(context, name, { get: () => component.computed[name].call(context), configurable: true });
    }
  }
  for (const name of Object.keys(component.methods)) {
    if (!(name in context)) context[name] = (...args) => component.methods[name].call(context, ...args);
  }
  holder.context = context;
  return { component, context };
}

test("Previous returns to a fetched page from memory, with no request, labelled with its age", async () => {
  const restore = installBrowser();
  try {
    const { context } = setup();
    await context.checkSyncStatus();
    await context.syncStatusNextPage();
    assert.equal(context.posts.length, 2, "two pages, two walks");
    assert.equal(context.syncStatusPageNumber, 2);

    await context.syncStatusPreviousPage();
    assert.equal(context.posts.length, 2, "going back asked nobody");
    assert.equal(context.syncStatusPageNumber, 1);
    assert.equal(context.syncStatusResult.entries[0].relative, "p0/file-0");
    assert.equal(context.syncStatusFromMemory, true);
    assert.equal(context.syncStatusBusy, false);
    assert.equal(context.syncStatusPhase, "ready");
    assert.match(context.syncStatusCheckedLabel, /^Checked moments ago, shown from this window's memory \(no new check\)\. Select Check status to compare again\.$/);
    assert.equal(context.syncStatusHasPrevious, false);
    assert.equal(context.syncStatusHasNext, true);

    await context.syncStatusNextPage();
    assert.equal(context.posts.length, 2, "forward to a page already fetched is memory too");
    assert.equal(context.syncStatusPageNumber, 2);
    assert.equal(context.syncStatusHasPrevious, true, "the cursor trail is kept");

    await context.syncStatusNextPage();
    assert.equal(context.posts.length, 3, "a page not yet fetched is a walk, as before");
    assert.equal(context.syncStatusFromMemory, false);
    assert.match(context.syncStatusCheckedLabel, /^Checked moments ago$/);
  } finally {
    restore();
  }
});

test("Check status is always a new walk and forgets every remembered page", async () => {
  const restore = installBrowser();
  try {
    const { context } = setup();
    await context.checkSyncStatus();
    await context.syncStatusNextPage();
    await context.checkSyncStatus();
    assert.equal(context.posts.length, 3);
    assert.equal(context.syncStatusPageNumber, 1);
    await context.syncStatusNextPage();
    assert.equal(context.posts.length, 4, "page 2 was forgotten with the old check");
  } finally {
    restore();
  }
});

test("a page is only as old as five minutes", async () => {
  const restore = installBrowser();
  const realNow = Date.now;
  let clock = 1_800_000_000_000;
  Date.now = () => clock;
  try {
    const { context } = setup();
    await context.checkSyncStatus();
    await context.syncStatusNextPage();
    clock += FIVE_MINUTES - 1;
    await context.syncStatusPreviousPage();
    assert.equal(context.posts.length, 2, "4 minutes 59 seconds: memory");
    await context.syncStatusNextPage();
    clock += 2;
    await context.syncStatusPreviousPage();
    assert.equal(context.posts.length, 3, "past five minutes: the walk runs again");
    assert.equal(context.syncStatusFromMemory, false);
    assert.equal(context.syncStatusPageNumber, 1);
  } finally {
    Date.now = realNow;
    restore();
  }
});

test("changing any field of the query, the page size or the profile never shows another query's page", async () => {
  const restore = installBrowser();
  try {
    const changes = [
      (form) => { form.state = "differs"; },
      (form) => { form.filter = "invoice"; },
      (form) => { form.scope = "reports"; },
      (form) => { form.include_excluded = true; },
      (form) => { form.limit = 50; },
      (form) => { form.profile = "other"; }
    ];
    for (const change of changes) {
      const { context } = setup();
      await context.checkSyncStatus();
      await context.syncStatusNextPage();
      change(context.syncStatusForm);
      // The same cursor under another query is a different page.
      await context.loadSyncStatus("", 1);
      assert.equal(context.posts.length, 3, `${JSON.stringify(context.syncStatusForm)} walked`);
      assert.equal(context.syncStatusFromMemory, false);
    }
  } finally {
    restore();
  }
});

test("the query-change and profile-change handlers drop everything remembered", async () => {
  const restore = installBrowser();
  try {
    const { context } = setup();
    await context.checkSyncStatus();
    await context.syncStatusNextPage();
    context.onSyncQueryChanged();
    assert.equal(context.syncStatusResult.loaded, false);
    assert.equal(context.syncStatusCheckedAtMs, 0);
    await context.loadSyncStatus("c0", 1);
    assert.equal(context.posts.length, 3, "the first page was forgotten");
  } finally {
    restore();
  }
});

test("a run finishing, or any profile outcome moving, ends the remembered pages", async () => {
  const restore = installBrowser();
  try {
    const { context } = setup();
    await context.checkSyncStatus();
    await context.syncStatusNextPage();
    context.syncStatusEvidenceStamp = "stamp-2";
    await context.syncStatusPreviousPage();
    assert.equal(context.posts.length, 3, "what the page compared against changed, so it is compared again");
    assert.equal(context.syncStatusFromMemory, false);
  } finally {
    restore();
  }
});

test("the evidence stamp moves with the run and with each profile's outcome", () => {
  const component = loadAppComponent();
  const stamp = (outcomes) => component.computed.syncStatusEvidenceStamp.call({ outcomes });
  const base = { run: { outcome: "succeeded", startedEpoch: 10, finishedEpoch: 20 }, profiles: [{ profile: "nas", finishedEpoch: 20, outcomeEpoch: 20 }] };
  assert.equal(stamp(null), "none");
  assert.equal(stamp(base), stamp(JSON.parse(JSON.stringify(base))), "the same state is the same stamp");
  assert.notEqual(stamp(base), stamp({ ...base, run: { ...base.run, outcome: "running", finishedEpoch: 0 } }));
  assert.notEqual(stamp(base), stamp({ ...base, run: { ...base.run, finishedEpoch: 30 } }));
  assert.notEqual(stamp(base), stamp({ ...base, profiles: [{ profile: "nas", finishedEpoch: 40, outcomeEpoch: 40 }] }));
});

test("deleting a profile and confirming a re-upload drop that profile's pages, and only its pages", async () => {
  const restore = installBrowser();
  try {
    const { context } = setup();
    await context.checkSyncStatus();
    await context.syncStatusNextPage();
    context.syncStatusForm.profile = "other";
    await context.loadSyncStatus("", 1);
    assert.equal(context.posts.length, 3);

    // The window's own code path: a profile's removal drops the pages of that profile, nothing else.
    const component = loadAppComponent({ apiPost: async () => ({}) });
    const removing = {
      disposed: false,
      auth: {},
      csrfToken: "token",
      selectedProfile: "nas",
      canChangeProfiles: true,
      operationBusy: false,
      syncStatusPageCache: context.syncStatusPageCache,
      toast() {},
      confirmAction: async () => true,
      clearAutosaveFailure() {},
      closeProfile() {},
      refreshSnapshot: async () => true,
      pauseAutosave() {},
      reportMutationError() {}
    };
    await component.methods.removeProfile.call(removing);
    context.syncStatusForm.profile = "nas";
    await context.loadSyncStatus("c0", 2);
    assert.equal(context.posts.length, 4, "nas's page was dropped");
    context.syncStatusForm.profile = "other";
    await context.loadSyncStatus("", 1);
    assert.equal(context.posts.length, 4, "other's page was kept");
  } finally {
    restore();
  }
});

test("a failed walk leaves nothing remembered and no age on screen", async () => {
  const restore = installBrowser();
  try {
    let fail = false;
    const { context } = setup({
      apiPost(_auth, _token, _action, payload) {
        this.posts.push(payload);
        if (fail) return Promise.reject(Object.assign(new Error("walk failed"), { status: 500, code: "operation_failed" }));
        return Promise.resolve({ output: JSON.stringify(pageDocument("nas", "", "c1", 1)) });
      }
    });
    await context.checkSyncStatus();
    fail = true;
    await context.syncStatusNextPage();
    assert.equal(context.syncStatusPhase, "failed");
    assert.equal(context.syncStatusCheckedAtMs, 0);
    assert.equal(context.syncStatusCheckedLabel, "");
    fail = false;
    await context.checkSyncStatus();
    assert.equal(context.syncStatusPhase, "ready");
  } finally {
    restore();
  }
});

test("a loaded page is frozen, so Vue never walks its rows", async () => {
  const restore = installBrowser();
  try {
    const { context } = setup();
    await context.checkSyncStatus();
    assert.equal(Object.isFrozen(context.syncStatusResult), true);
    assert.equal(Object.isFrozen(context.syncStatusResult.entries), false, "the page object is the root Vue skips; its rows are plain data");
    assert.equal(context.syncStatusResult.entries.length, 2);
  } finally {
    restore();
  }
});

test("the page names its age in the heading and the help says what is and is not remembered", async () => {
  const { readFile } = await import("node:fs/promises");
  const source = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
  assert.match(source, /Compared by \{\{ syncStatusResult\.compare \}\}<template v-if="syncStatusCheckedLabel"> · \{\{ syncStatusCheckedLabel \}\}<\/template>/);
  const help = await readFile(new URL("../../package/ui/help/enu/sync.html", import.meta.url), "utf8");
  assert.doesNotMatch(help, /walked again every time/);
  assert.match(help, /five minutes/);
  assert.match(help, /Check status/);
});
