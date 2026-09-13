import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import {
  WIDGET_ACTIVE_POLL_MS,
  WIDGET_BACKOFF_RAMP_MS,
  WIDGET_IDLE_POLL_MS,
  widgetBridgeIssue
} from "../src/widgetModel.mjs";

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

/**
 * The named in-service read failures, exactly as this release ships them.
 *
 * Every row used to render "Package service unavailable · Restart Synology Drive
 * Sync", because a semantic 503 was all the browser ever saw. Restating the
 * whole table here rather than deriving it from the source is the point: a
 * change to any operator-visible sentence, to any retry decision, or to the set
 * of codes that have copy at all has to be made twice, deliberately.
 *
 * `kind` is the service's own classification. It is not rendered; it is asserted
 * against, because the entire reason the service separates the two is that an
 * Unavailable clears on its own and an UnsafeRuntime does not.
 */
const SHIPPED_COPY = Object.freeze([
  Object.freeze({
    code: "runtime_upgrading", kind: "Unavailable", retry: "auto",
    title: "Package is upgrading",
    message: "Synology Drive Sync is upgrading. This page will retry.",
    cause: "the package is upgrading"
  }),
  Object.freeze({
    code: "runtime_uninstalling", kind: "Unavailable", retry: "stop",
    title: "Package is being removed",
    message: "Synology Drive Sync is being removed. Nothing further will be read until it is installed again.",
    cause: "the package is being removed"
  }),
  Object.freeze({
    code: "runtime_closed", kind: "Unavailable", retry: "auto",
    title: "Package is stopping or upgrading",
    message: "The package is stopping or upgrading. This page will retry.",
    cause: "the package is stopping or upgrading"
  }),
  Object.freeze({
    code: "runtime_marker_unsafe", kind: "UnsafeRuntime", retry: "stop",
    title: "Package state unconfirmed",
    message: "The package could not confirm it is running normally. Inspect the package API log; do not restart it in the hope the marker clears.",
    cause: "the package cannot confirm it is running normally"
  }),
  Object.freeze({
    code: "policy_unreadable", kind: "UnsafeRuntime", retry: "stop",
    title: "Security policy unreadable",
    message: "The package could not read its security policy. Repair or reinstall the latest complete package release, then reopen this app.",
    cause: "the package cannot read its security policy"
  }),
  Object.freeze({
    code: "manager_busy", kind: "Unavailable", retry: "backoff",
    title: "Package is busy",
    message: "The package is busy answering other windows. Retrying; close AppWindows you are not using if this persists.",
    cause: "the package is busy"
  }),
  Object.freeze({
    code: "manager_lane_poisoned", kind: "UnsafeRuntime", retry: "stop",
    title: "Package service needs a restart",
    message: "The package service cannot serve further reads in this state. Restart Synology Drive Sync in Package Center.",
    cause: "the package service needs a restart"
  }),
  Object.freeze({
    code: "manager_unsafe", kind: "UnsafeRuntime", retry: "stop",
    title: "Package files are not in a safe state",
    message: "The package files are not in a safe state. Repair or reinstall the latest complete package release; do not change ownership or permissions manually.",
    cause: "the package files are not in a safe state"
  }),
  Object.freeze({
    code: "manager_spawn_failed", kind: "Unavailable", retry: "backoff",
    title: "Package helper could not start",
    message: "The package could not start its helper. Retrying; inspect the package API log if this persists.",
    cause: "the package could not start its helper"
  }),
  Object.freeze({
    code: "manager_timeout", kind: "Unavailable", retry: "backoff",
    title: "Package took too long to answer",
    message: "The package took too long to answer. Retrying; a busy NAS or a large log set can do this.",
    cause: "the package took too long to answer"
  }),
  Object.freeze({
    code: "manager_output_too_large", kind: "Unavailable", retry: "backoff",
    title: "Package answer was too large",
    message: "The package produced more data than this page can read. Retrying; inspect the package API log.",
    cause: "the package produced more data than this page can read"
  }),
  Object.freeze({
    code: "manager_exit_status", kind: "Unavailable", retry: "backoff",
    title: "Package could not assemble this view",
    message: "The package could not assemble this view. Retrying; inspect the package API log.",
    cause: "the package could not assemble this view"
  }),
  Object.freeze({
    code: "manager_output_invalid", kind: "Unavailable", retry: "backoff",
    title: "Package answer could not be read",
    message: "The package returned data this page cannot read. Retrying; inspect the package API log.",
    cause: "the package returned data this page cannot read"
  }),
  Object.freeze({
    code: "manager_output_schema", kind: "Unavailable", retry: "backoff",
    title: "UI and package versions differ",
    message: "Repair or reinstall one complete release so the AppWindow and package API use the same schema. Retrying meanwhile.",
    cause: "the UI and package versions differ"
  }),
  Object.freeze({
    code: "config_file_unsafe", kind: "UnsafeRuntime", retry: "stop",
    title: "Package file permissions are unsafe",
    message: "A package file has unexpected ownership or permissions. Repair the package rather than changing the file yourself.",
    cause: "a package file has unexpected ownership or permissions"
  }),
  // Unavailable, yet not retried: the record stays unparseable until the file
  // rotates or is cleared, so there is nothing for a poll to discover.
  Object.freeze({
    code: "package_state_corrupt", kind: "Unavailable", retry: "stop",
    title: "Package record is corrupt",
    message: "A stored package record could not be read. Inspect Logs and Activity for the affected file; restarting does not repair it.",
    cause: "a stored package record could not be read"
  }),
  Object.freeze({
    code: "clock_unavailable", kind: "UnsafeRuntime", retry: "stop",
    title: "NAS clock is not set",
    message: "The NAS clock is not set correctly. Correct the system time, then retry.",
    cause: "the NAS clock is not set correctly"
  })
]);

// The sentence the bare 503 branch has always rendered, and must keep rendering
// for a 503 this release has never heard of.
const CATCH_ALL = Object.freeze({
  title: "Package service unavailable",
  message: "Restart Synology Drive Sync and inspect its API log if the package bridge does not recover."
});

function loadAppComponent({ apiGet = async () => ({}) } = {}) {
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
  const stubs = {
    // The real cadence literals, not stand-ins: App.vue's retry ladder and
    // stale-age escalation are only meaningful against the ramp the widget
    // actually ships and validate_spk.py actually pins.
    WIDGET_ACTIVE_POLL_MS,
    WIDGET_BACKOFF_RAMP_MS,
    WIDGET_IDLE_POLL_MS,
    ACTIONS: {
      configureProfile: "configure-profile", setSecret: "set-secret",
      testProfileAuth: "test-profile-auth", browseRemote: "browse-remote",
      routine: "routine", alertPolicy: "alert-policy", securityPolicy: "security-policy",
      clientEvent: "client-event", removeProfile: "remove-profile", execute: "action"
    },
    AUTOSAVE_API_LIMITS: Object.freeze({}),
    MAX_RESPONSE_BYTES: 1024 * 1024,
    PROGRESS_UNAVAILABLE: Object.freeze({ unavailable: true }),
    QueuedOutcomeUnknownError: class extends Error {},
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    SYNC_STATUS_MAX_LIMIT: 200,
    trustedRequestProgress: () => null,
    apiGet,
    apiPost: async () => ({ ok: true }),
    probeRequestOutcome: async () => ({}),
    purgeReconciliationAuth: () => undefined,
    reconcileMutationRequest: async () => ({}),
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
    SecurityPanel: {}
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

function describe(component, error) {
  return component.methods.describeBridgeError.call({}, error, "status");
}

/**
 * A `this` for refreshSnapshot with every collaborator it touches stubbed and
 * the two clocks it consults under the test's control.
 */
function snapshotContext(component, overrides = {}) {
  const toasts = [];
  const context = {
    disposed: false,
    auth: {},
    csrfToken: "token",
    snapshot: null,
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
    scheduled: [],
    toasts,
    toast(title, message, error = false) { toasts.push({ title, message, error }); },
    async refreshCsrf() {},
    hydrateAlerts() {},
    hydrateSecurityPolicy() {},
    maybeNotifyFailure() {},
    scheduleSnapshot() { this.scheduled.push(this.snapshotRetryDelay(Number(this.settings.status_refresh))); },
    ...overrides
  };
  for (const name of ["describeBridgeError", "refreshSnapshot", "snapshotRetryDelay", "snapshotNewerThan"]) {
    context[name] = (...args) => component.methods[name].call(context, ...args);
  }
  return context;
}

async function withBrowserGlobals(run) {
  const previousWindow = globalThis.window;
  const previousDocument = globalThis.document;
  globalThis.window = { clearTimeout() {}, setTimeout() { return 1; } };
  globalThis.document = { hidden: false };
  try {
    return await run();
  } finally {
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
    if (previousDocument === undefined) delete globalThis.document;
    else globalThis.document = previousDocument;
  }
}

function validSnapshot(extra = {}) {
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

// §4.3 gave every in-service read failure a name; this is the set the AppWindow
// has copy for. Asserting the exact set, not a subset, is what makes adding a
// code to the service without adding copy for it a visible omission rather than
// a silent fall-through to "Restart Synology Drive Sync".
test("every named in-service read failure has its own copy, and only those do", () => {
  const table = appSource.match(/const BRIDGE_FAILURE_COPY = Object\.freeze\(\{([\s\S]*?)\n\}\);/);
  assert.ok(table, "BRIDGE_FAILURE_COPY is missing");
  const declared = [...table[1].matchAll(/^ {2}([a-z_]+): Object\.freeze\(\{/gm)].map((match) => match[1]);
  assert.equal(declared.length, SHIPPED_COPY.length, "the shipped table and this test's copy of it disagree in size");
  assert.deepEqual(declared, SHIPPED_COPY.map((row) => row.code));
});

test("each named failure renders its own title and text, never the bare 503 catch-all", () => {
  const component = loadAppComponent();
  for (const row of SHIPPED_COPY) {
    const issue = describe(component, { status: 503, code: row.code });
    assert.equal(issue.title, row.title, row.code);
    assert.equal(issue.message, row.message, row.code);
    assert.notEqual(issue.title, CATCH_ALL.title, `${row.code} fell through to the catch-all`);
    // The envelope's own message is discarded on purpose: this window renders
    // only copy it owns. A branch that leaked server text would show it here.
    const withServerText = describe(component, {
      status: 503, code: row.code, message: "server-supplied text that must not be rendered"
    });
    assert.equal(withServerText.message, row.message, `${row.code} rendered the envelope's message`);
  }
});

test("the bare 503 catch-all survives for an unnamed code", () => {
  const component = loadAppComponent();
  for (const code of ["", "service_unavailable", "bridge_protocol_unavailable", "a_code_from_a_newer_package"]) {
    const issue = describe(component, { status: 503, code });
    assert.equal(issue.title, CATCH_ALL.title, code);
    assert.equal(issue.message, CATCH_ALL.message, code);
  }
  // And the authentication branches keep their precedence over the new table.
  assert.equal(
    describe(component, { status: 503, code: "dsm_authentication_helper_unavailable" }).title,
    "DSM authentication helper unavailable"
  );
  assert.equal(describe(component, { status: 401, code: "manager_busy" }).title, "DSM session expired");
  assert.equal(describe(component, { status: 403, code: "manager_unsafe" }).title, "DSM access denied");
});

// The failure stage is appended by the shared `issue` helper, so a new branch
// that built its own object would silently drop it.
test("named failures still carry the failure stage when the envelope names one", () => {
  const component = loadAppComponent();
  const issue = describe(component, { status: 503, code: "manager_timeout", stage: "service_request" });
  assert.match(issue.message, /Failure stage: service_request\.$/);
});

/**
 * S4. widgetModel.mjs's docblock has always claimed its titles are "the exact
 * ones App.vue's describeBridgeError produces for the same conditions", and
 * nothing checked it. Two surfaces naming the same fault differently is the
 * failure this prevents: the desktop card and the window are looked at within
 * seconds of each other by the same person.
 */
test("the desktop card and the AppWindow name every named failure identically", () => {
  const component = loadAppComponent();
  for (const row of SHIPPED_COPY) {
    const error = { status: 503, code: row.code };
    assert.equal(
      widgetBridgeIssue(error).title,
      describe(component, error).title,
      `${row.code} is named differently by the widget and the AppWindow`
    );
  }
  // The pre-existing shared titles must not have been disturbed on the way.
  for (const [status, code, title] of [
    [401, "", "DSM session expired"],
    [403, "", "DSM access denied"],
    [503, "service_unavailable", "Package service unavailable"],
    [404, "non_json_response", "Package UI route unavailable"]
  ]) {
    const error = { status, code };
    assert.equal(widgetBridgeIssue(error).title, describe(component, error).title, `${status} ${code}`);
    assert.equal(widgetBridgeIssue(error).title, title);
  }
});

// The whole reason the service separates Unavailable from UnsafeRuntime is that
// one clears on its own and the other does not. If the copy and the polling
// ladder do not carry that distinction, the separation is invisible and the
// operator is told to wait out a fault that will never clear.
test("a runtime the package refuses to vouch for is never polled through, and says so", () => {
  const promisesRetry = (message) => /This page will retry\.|Retrying/.test(message);
  for (const row of SHIPPED_COPY) {
    if (row.kind === "UnsafeRuntime") {
      assert.equal(row.retry, "stop", `${row.code} keeps polling an unverifiable runtime`);
      assert.match(
        row.message,
        /inspect|repair|reinstall|restart|correct the system time/i,
        `${row.code} does not tell the operator what to do`
      );
    }
    assert.equal(
      promisesRetry(row.message),
      row.retry !== "stop",
      `${row.code} promises a retry it does not perform, or performs one it does not promise`
    );
  }
});

/**
 * Row 12. Three ladders, chosen per code.
 *
 * The numbers are the widget's, deliberately: WIDGET_BACKOFF_RAMP_MS and
 * WIDGET_ACTIVE_POLL_MS are reviewed literals that validate_spk.py already
 * holds, and a second ramp would be a second thing to keep in step.
 */
test("each named failure selects its own bounded retry ladder", () => {
  const component = loadAppComponent();
  const delay = (code, streak, interval = 5000) => component.methods.snapshotRetryDelay.call(
    { snapshotFailureCode: code, snapshotFailureStreak: streak },
    interval
  );

  // No failure, and an unnamed one: today's behaviour exactly, the configured
  // cadence with no ladder at all.
  assert.equal(delay("", 0), 5000);
  assert.equal(delay("service_unavailable", 6), 5000);

  for (const row of SHIPPED_COPY) {
    if (row.retry !== "stop") continue;
    assert.equal(delay(row.code, 1), 0, `${row.code} scheduled another read`);
    assert.equal(delay(row.code, 9), 0, `${row.code} scheduled another read`);
  }

  for (const row of SHIPPED_COPY) {
    if (row.retry !== "auto") continue;
    assert.equal(delay(row.code, 1), WIDGET_ACTIVE_POLL_MS, row.code);
    assert.equal(delay(row.code, 9), WIDGET_ACTIVE_POLL_MS, row.code);
    // An operator who has already chosen a slower cadence keeps it.
    assert.equal(delay(row.code, 3, 30000), 30000, row.code);
  }

  for (const row of SHIPPED_COPY) {
    if (row.retry !== "backoff") continue;
    // One failed read is not a verdict, exactly as the desktop card holds.
    assert.equal(delay(row.code, 1), 5000, `${row.code} backed off on a single failure`);
    const walked = [2, 3, 4, 5, 6, 7].map((streak) => delay(row.code, streak));
    assert.deepEqual(walked, [...WIDGET_BACKOFF_RAMP_MS, WIDGET_BACKOFF_RAMP_MS[3], WIDGET_BACKOFF_RAMP_MS[3]], row.code);
  }
});

test("a stood-down ladder schedules nothing, and a manual retry re-arms it", async () => {
  const component = loadAppComponent({
    apiGet: async () => { throw Object.assign(new Error("unsafe"), { status: 503, code: "manager_unsafe" }); }
  });
  await withBrowserGlobals(async () => {
    const context = snapshotContext(component, { snapshot: validSnapshot(), snapshotReceivedAtMs: Date.now() });

    await context.refreshSnapshot(false);
    assert.equal(context.snapshotFailureCode, "manager_unsafe");
    assert.deepEqual(context.scheduled, [0], "an unsafe package was polled again");

    // The Retry button is the operator saying "now". It clears the stand-down so
    // the ladder can re-arm; without this the button would perform exactly one
    // read and never schedule another for the rest of the session.
    context.scheduled.length = 0;
    await context.refreshSnapshot(true);
    assert.equal(context.snapshotFailureStreak, 1, "a manual retry did not restart the streak");
    assert.deepEqual(context.scheduled, [0]);

    // A success clears both, and the ordinary cadence returns.
    context.snapshotFailureCode = "manager_timeout";
    context.snapshotFailureStreak = 4;
    assert.equal(context.snapshotRetryDelay(5000), WIDGET_BACKOFF_RAMP_MS[2]);
    context.snapshotFailureCode = "";
    context.snapshotFailureStreak = 0;
    assert.equal(context.snapshotRetryDelay(5000), 5000);
  });
});

test("scheduleSnapshot honours the ladder, Manual only, and the profile-draft block", () => {
  const component = loadAppComponent();
  const scheduled = [];
  const previousWindow = globalThis.window;
  const previousDocument = globalThis.document;
  globalThis.window = {
    clearTimeout() {},
    setTimeout(callback, delay) { scheduled.push(delay); return scheduled.length; }
  };
  globalThis.document = { hidden: false };
  try {
    const context = {
      snapshotTimer: 0,
      disposed: false,
      snapshotRefreshBlocked: false,
      settings: { status_refresh: 5000 },
      snapshotFailureCode: "",
      snapshotFailureStreak: 0,
      refreshSnapshot() {}
    };
    context.snapshotRetryDelay = (interval) => component.methods.snapshotRetryDelay.call(context, interval);
    const schedule = () => component.methods.scheduleSnapshot.call(context);

    schedule();
    assert.deepEqual(scheduled, [5000]);

    context.snapshotFailureCode = "config_file_unsafe";
    context.snapshotFailureStreak = 1;
    schedule();
    assert.deepEqual(scheduled, [5000], "a stood-down ladder created a timer");
    assert.equal(context.snapshotTimer, 0);

    context.snapshotFailureCode = "manager_busy";
    context.snapshotFailureStreak = 3;
    schedule();
    assert.deepEqual(scheduled, [5000, WIDGET_BACKOFF_RAMP_MS[1]]);

    // Manual only and an open profile draft both still win outright.
    context.settings.status_refresh = 0;
    schedule();
    context.settings.status_refresh = 5000;
    context.snapshotRefreshBlocked = true;
    schedule();
    assert.equal(scheduled.length, 2, "a paused or manual window scheduled a background read");
  } finally {
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
    if (previousDocument === undefined) delete globalThis.document;
    else globalThis.document = previousDocument;
  }
});

/**
 * §3.4. docs/dsm/dashboard.md has promised since the first release that stale
 * values are never silently treated as current. "Stale · last successful
 * snapshot retained" kept the letter of that and none of its use: it could not
 * separate a snapshot four seconds old from one four hours old, nor a package
 * that was busy from one whose files were unsafe.
 */
test("the stale header carries the retained document's age and the named cause", async () => {
  const failures = [];
  const component = loadAppComponent({
    apiGet: async () => { throw failures.shift(); }
  });
  await withBrowserGlobals(async () => {
    const received = Date.now() - 45000;
    for (const [code, expected] of [
      ["manager_busy", /^Stale · as of less than a minute ago; the package could not refresh it: the package is busy\.$/],
      ["manager_timeout", /the package took too long to answer\.$/],
      ["config_file_unsafe", /a package file has unexpected ownership or permissions\. Automatic refresh has stopped; select Retry once the package is repaired\.$/]
    ]) {
      const context = snapshotContext(component, { snapshot: validSnapshot(), snapshotReceivedAtMs: received });
      failures.push(Object.assign(new Error(code), { status: 503, code }));
      await context.refreshSnapshot(false);
      assert.match(context.freshness, expected, code);
      assert.equal(context.snapshot !== null, true, "the retained document was discarded");
    }

    // An unnamed code still gets the age, because the age is this window's own
    // evidence and never depended on the service naming anything.
    const context = snapshotContext(component, { snapshot: validSnapshot(), snapshotReceivedAtMs: received });
    failures.push(Object.assign(new Error("x"), { status: 503, code: "service_unavailable" }));
    await context.refreshSnapshot(false);
    assert.equal(context.freshness, "Stale · as of less than a minute ago; the package could not refresh it.");

    // Nothing retained is still "Status unavailable": there is no age to give.
    const empty = snapshotContext(component);
    failures.push(Object.assign(new Error("x"), { status: 503, code: "manager_busy" }));
    await empty.refreshSnapshot(false);
    assert.equal(empty.freshness, "Status unavailable");
  });
});

test("past the escalation threshold the rows stop being presented as evidence", async () => {
  const component = loadAppComponent({
    apiGet: async () => { throw Object.assign(new Error("busy"), { status: 503, code: "manager_busy" }); }
  });
  await withBrowserGlobals(async () => {
    const escalation = WIDGET_BACKOFF_RAMP_MS[WIDGET_BACKOFF_RAMP_MS.length - 1];

    const below = snapshotContext(component, {
      snapshot: validSnapshot(), snapshotReceivedAtMs: Date.now() - (escalation - 30000)
    });
    await below.refreshSnapshot(false);
    assert.match(below.freshness, /^Stale · as of 4 minutes ago;/);

    const above = snapshotContext(component, {
      snapshot: validSnapshot(), snapshotReceivedAtMs: Date.now() - (escalation + 60000)
    });
    await above.refreshSnapshot(false);
    assert.match(
      above.freshness,
      /^Unanswered · the package has not answered for 6 minutes: the package is busy\. These values are no longer current evidence\.$/
    );
  });
});

// Below the widget's resting cadence the exact number decides nothing, so it is
// not given. Above it the unit has to change or "412 minutes" is what an
// overnight outage renders.
test("the retained age is described in units an operator acts on", async () => {
  const component = loadAppComponent({
    apiGet: async () => { throw Object.assign(new Error("busy"), { status: 503, code: "manager_busy" }); }
  });
  await withBrowserGlobals(async () => {
    const phrase = async (ageMs) => {
      const context = snapshotContext(component, {
        snapshot: validSnapshot(), snapshotReceivedAtMs: Date.now() - ageMs
      });
      await context.refreshSnapshot(false);
      return context.freshness;
    };
    assert.match(await phrase(0), /as of less than a minute ago/);
    assert.match(await phrase(WIDGET_IDLE_POLL_MS - 1000), /as of less than a minute ago/);
    assert.match(await phrase(WIDGET_IDLE_POLL_MS), /as of 1 minute ago/);
    assert.match(await phrase(2 * WIDGET_IDLE_POLL_MS), /as of 2 minutes ago/);
    // Past the escalation the sentence changes shape, and the unit has to change
    // with the age or an overnight outage renders as "412 minutes".
    assert.match(await phrase(4 * 3600000), /has not answered for 4 hours/);
    assert.match(await phrase(3 * 86400000), /has not answered for 3 days/);
  });
});

/**
 * Row 13. A snapshot carrying a key this release has never heard of is accepted
 * today: App.vue checks the schema string and nothing else, and only
 * `request-status` is exact-keyed anywhere in the bundle.
 *
 * This pins the tolerant behaviour on purpose. It is not an endorsement — a
 * wrapping envelope would be accepted wholesale by this same check — but a
 * future exact-key change must be a deliberate edit with this test in front of
 * it, not a surprise discovered on a NAS.
 */
test("a snapshot with a stray key is accepted, as it is today", async () => {
  const stray = validSnapshot({ an_unknown_key: { nested: [1, 2, 3] }, another: "value" });
  const component = loadAppComponent({ apiGet: async () => stray });
  await withBrowserGlobals(async () => {
    const context = snapshotContext(component);
    const result = await context.refreshSnapshot(false);
    assert.equal(result, true);
    assert.strictEqual(context.snapshot, stray);
    assert.equal(context.snapshot.an_unknown_key.nested[1], 2, "the extra key was not stripped");
    assert.equal(context.connected, true);
    assert.equal(context.snapshotReceivedAtMs > 0, true);
    assert.match(context.freshness, /^Updated /);
  });

  // The schema string is still the one thing that is checked, and it still
  // fails closed.
  const wrong = loadAppComponent({ apiGet: async () => validSnapshot({ schema: "sdsync.dsm-api.v2" }) });
  await withBrowserGlobals(async () => {
    const context = snapshotContext(wrong);
    assert.equal(await context.refreshSnapshot(false), false);
    assert.equal(context.snapshot, null);
    assert.equal(context.connected, false);
  });
});

/**
 * Row 14. Retaining a read document is only safe because of where it is kept: a
 * tab is one DSM session, one package UID and one argument set, the bound is one
 * document per read kind, and eviction is the window closing. Nothing is keyed,
 * nothing is shared and nothing outlives the instance.
 */
test("retained read documents are per-component state with no cross-session retention", () => {
  const component = loadAppComponent();
  const first = component.data();
  const second = component.data();
  assert.notStrictEqual(first, second, "two windows would share one state object");
  for (const field of ["snapshot"]) assert.equal(first[field], null, field);
  for (const field of ["snapshotReceivedAtMs", "logsReceivedAtMs", "activityReceivedAtMs"]) {
    assert.equal(first[field], 0, field);
    assert.equal(second[field], 0, field);
  }
  first.snapshot = validSnapshot();
  first.snapshotReceivedAtMs = 12345;
  assert.equal(component.data().snapshot, null, "a retained document leaked into the next window");
  assert.equal(component.data().snapshotReceivedAtMs, 0);

  // Nothing a read returns is ever written to browser storage, so closing the
  // window is genuinely eviction. Every storage call in the source is the
  // interface-preferences key and no other.
  const storageCalls = [...appSource.matchAll(/window\.localStorage\.\w+\(([^,)]*)/g)].map((match) => match[1].trim());
  assert.equal(storageCalls.length > 0, true, "the storage scan matched nothing and proved nothing");
  for (const argument of storageCalls) assert.equal(argument, "SETTINGS_KEY", argument);
  assert.doesNotMatch(appSource, /sessionStorage/);

  // And teardown stops the poll rather than leaving a timer holding the
  // instance alive.
  const cleared = [];
  const previousWindow = globalThis.window;
  globalThis.window = { clearTimeout(timer) { cleared.push(timer); } };
  try {
    const context = {
      disposed: false, auth: {}, autosaveCoordinator: null, abortController: null,
      snapshotTimer: 7, logTimer: 8, incidentProbeTimer: 9,
      snapshot: validSnapshot(), snapshotReceivedAtMs: 12345,
      controlLayoutCleanup: null, beforeUnloadHandler: null, visibilityHandler: null,
      connectionProofTimer: 0
    };
    context.stopTimers = () => component.methods.stopTimers.call(context);
    component.methods.stopTimers.call(context);
    assert.deepEqual(cleared, [7, 8, 9]);
    assert.deepEqual([context.snapshotTimer, context.logTimer, context.incidentProbeTimer], [0, 0, 0]);
  } finally {
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
  }
});

// The Activity list is the first place anyone looks when diagnosing the very
// failure that froze it, so its rows need a date for the same reason the status
// header does.
test("a failed log or activity feed dates the rows it leaves on screen", async () => {
  const failing = (action) => loadAppComponent({
    apiGet: async (_auth, requested) => {
      if (requested === action) throw Object.assign(new Error(action), { status: 503, code: "manager_timeout" });
      return requested === "logs"
        ? { schema: "sdsync.dsm-logs.v1", logs: [{ source: "api", lines: ["a retained line"] }] }
        : { schema: "sdsync.dsm-activity.v1", events: [] };
    }
  });
  const logContext = (component, overrides) => {
    const context = {
      disposed: false, auth: {}, route: "activity", logsPaused: false, logsLoading: false,
      logLines: 200, logSource: "all", logRecords: [], logOutput: "", activityEvents: [],
      logState: "", logsReceivedAtMs: 0, activityReceivedAtMs: 0,
      scheduleLogs() {},
      ...overrides
    };
    context.logRecordsFrom = (model) => component.methods.logRecordsFrom.call(context, model);
    return context;
  };

  await withBrowserGlobals(async () => {
    // Activity failed. Its previous rows are still on screen and they are now
    // dated; the log feed that did answer is untouched.
    const activityDown = failing("activity");
    const one = logContext(activityDown, { activityReceivedAtMs: Date.now() - 4 * 3600000 });
    await activityDown.methods.refreshLogs.call(one);
    assert.equal(one.logsReceivedAtMs > 0, true, "a successful log read left no receipt");
    assert.match(one.logState, /activity feed unavailable, retained rows are 4 hours old$/);

    // The package log failed instead. Same treatment, other half.
    const logsDown = failing("logs");
    const two = logContext(logsDown, { logsReceivedAtMs: Date.now() - 2 * 60000 });
    await logsDown.methods.refreshLogs.call(two);
    assert.equal(two.activityReceivedAtMs > 0, true, "a successful activity read left no receipt");
    assert.match(two.logState, /package log read unavailable, retained rows are 2 minutes old$/);

    // Both failed and nothing was ever retained: say that rather than dating
    // rows that do not exist.
    const bothDown = loadAppComponent({
      apiGet: async () => { throw Object.assign(new Error("down"), { status: 503, code: "manager_busy" }); }
    });
    const three = logContext(bothDown);
    await bothDown.methods.refreshLogs.call(three);
    assert.equal(three.logState, "Logs unavailable · nothing retained");

    // Both failed with rows retained: the staler half is what the age reports.
    const four = logContext(bothDown, {
      logsReceivedAtMs: Date.now() - 3 * 3600000,
      activityReceivedAtMs: Date.now() - 2 * 60000
    });
    await bothDown.methods.refreshLogs.call(four);
    assert.equal(four.logState, "Logs unavailable · retained rows are 3 hours old");
    assert.equal(four.logsReceivedAtMs > 0, true, "a failed read discarded a receipt");
  });

  // Both feeds start undated, exactly as the snapshot does.
  const data = loadAppComponent().data();
  assert.equal(data.logsReceivedAtMs, 0);
  assert.equal(data.activityReceivedAtMs, 0);
});
