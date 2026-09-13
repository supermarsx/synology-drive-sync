import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import {
  WIDGET_ACTIVE_POLL_MS,
  WIDGET_BACKOFF_RAMP_MS,
  WIDGET_IDLE_POLL_MS
} from "../src/widgetModel.mjs";

// Component loader copied from reconciliation-barrier.test.mjs, for the reason
// that file gives for copying its own harness: a self-contained copy cannot
// collide with concurrent edits to the existing suite.

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

function loadAppComponent() {
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
    ACTIONS: { syncStatus: "sync-status", resync: "resync", execute: "action" },
    AUTOSAVE_API_LIMITS: Object.freeze({}),
    MAX_RESPONSE_BYTES: 1024 * 1024,
    PROGRESS_UNAVAILABLE: Object.freeze({ unavailable: true }),
    QueuedOutcomeUnknownError: class extends Error {},
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    SYNC_STATUS_MAX_LIMIT: 200,
    apiGet: async () => ({}),
    apiPost: async () => ({ ok: true }),
    trustedRequestProgress: (value) => {
      if (!value || typeof value !== "object" || Array.isArray(value)) return null;
      const keys = Object.keys(value).sort().join(",");
      const counted = keys === "count,label,step,total,unit,updated_at";
      if (!counted && keys !== "label,step,total,updated_at") return null;
      return {
        step: Number(value.step),
        total: Number(value.total),
        label: value.label,
        updatedAt: Number(value.updated_at),
        unit: counted ? value.unit : "",
        count: counted ? Number(value.count) : 0
      };
    },
    probeRequestOutcome: async () => ({}),
    purgeReconciliationAuth: () => undefined,
    reconcileMutationRequest: async () => ({}),
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback).slice(0, 65536),
    formatBytes: (value) => `${value} B`,
    formatDate: (value) => Number(value) > 0 ? `@${value}` : "Unavailable",
    formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    createAutosaveCoordinator: () => ({}),
    installControlLayout: () => () => {},
    ActionIcon: { name: "ActionIcon" },
    SecurityPanel: {}
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

const component = loadAppComponent();
const { computed, methods } = component;

// A context carrying only what the controller-liveness join reads. Every field
// is already published by the snapshot endpoint; none of it was ever correlated
// with a queued job before.
function snapshotContext({ service = "running", controller = {}, run = {}, liveProgress = null } = {}) {
  const snapshot = {
    generated_at_epoch: 1757548800,
    service: { state: service, pid: service === "stopped" ? 0 : 4242 },
    controller: controller === null ? undefined : {
      state: "running",
      pid: 4242,
      next_run_epoch: 0,
      active_pid: 0,
      updated_epoch: 1757548790,
      ...controller
    },
    run: { state: "never", started_epoch: 0, finished_epoch: 0, exit_code: null, scope: "none", operation: "none", ...run }
  };
  const context = {
    snapshot: controller === null && service === null ? null : snapshot,
    liveProgress: liveProgress || { active: false, operation: "", progress: null }
  };
  context.serviceState = computed.serviceState.call(context);
  context.run = computed.run.call(context);
  context.runStatus = computed.runStatus.call(context);
  context.controllerLiveness = computed.controllerLiveness.call(context);
  context.queuedWaitDetail = computed.queuedWaitDetail.call(context);
  context.liveProgressDetail = computed.liveProgressDetail.call(context);
  return context;
}

function countedRecord(overrides = {}) {
  return {
    step: 5,
    total: 7,
    label: "Comparing file contents",
    updatedAt: 1757548790,
    unit: "files",
    count: 12480,
    ...overrides
  };
}

// The join, in the user's words: a job pending behind a stopped controller and a
// job pending behind a running sync are different facts calling for different
// actions, and both used to render as the single word "pending".
test("a queued job behind a stopped controller says so, and says when it will run", () => {
  const context = snapshotContext({
    service: "stopped",
    controller: { state: "stopped", pid: 0, updated_epoch: 1757548000 }
  });
  assert.equal(context.queuedWaitDetail.kind, "stopped");
  assert.match(context.queuedWaitDetail.text, /package controller is stopped/);
  assert.match(context.queuedWaitDetail.text, /@1757548000/, "the time it stopped must be named");
  assert.match(context.queuedWaitDetail.text, /runs when the package is started in Package Center/);
  // Never the bare word on its own.
  assert.doesNotMatch(context.queuedWaitDetail.text, /^pending$/i);
});

test("a queued job behind a running operation names what it is waiting for", () => {
  const context = snapshotContext({
    controller: { active_pid: 9001 },
    run: { state: "running", operation: "sync", scope: "archive", started_epoch: 1757548100 }
  });
  assert.equal(context.queuedWaitDetail.kind, "behind");
  assert.match(context.queuedWaitDetail.text, /already running the sync of archive/);
  assert.match(context.queuedWaitDetail.text, /@1757548100/);
  assert.match(context.queuedWaitDetail.text, /so this starts as soon as that finishes/);
});

// The controller's own state file is not evidence about the controller. A queued
// control job is run by a child that never writes run state, so "active but no
// named run" is a real and ordinary case, not a data fault.
test("an active controller with no named run still reports that something is ahead", () => {
  const context = snapshotContext({ controller: { active_pid: 9001 } });
  assert.equal(context.queuedWaitDetail.kind, "behind");
  assert.match(context.queuedWaitDetail.text, /another operation the controller has already started/);
  assert.doesNotMatch(context.queuedWaitDetail.text, /\bnone\b/);
});

test("a controller that has stopped checking in is reported as possibly wedged", () => {
  // Three minutes past its last report with nothing active. Its longest idle
  // sleep is thirty seconds, so this is six missed ticks.
  const context = snapshotContext({ controller: { updated_epoch: 1757548800 - 181 } });
  assert.equal(context.queuedWaitDetail.kind, "stalled");
  assert.match(context.queuedWaitDetail.text, /has not checked in since/);
  assert.match(context.queuedWaitDetail.text, /may be wedged/);
  assert.match(context.queuedWaitDetail.text, /Review Logs/);
});

test("staleness is not read while the controller is executing a job", () => {
  // The same stale timestamp, but with a job in flight. The controller publishes
  // its active PID and then blocks for as long as the job takes, so treating
  // this as evidence would have every long sync report its own controller dead.
  const context = snapshotContext({
    controller: { updated_epoch: 1757548800 - 4000, active_pid: 9001 },
    run: { state: "running", operation: "sync", scope: "archive", started_epoch: 1757544800 }
  });
  assert.equal(context.queuedWaitDetail.kind, "behind");
  assert.doesNotMatch(context.queuedWaitDetail.text, /wedged/);
});

test("a live PID that is not the controller is not reported as a healthy queue", () => {
  const context = snapshotContext({ service: "untrusted", controller: { state: "running" } });
  assert.equal(context.queuedWaitDetail.kind, "untrusted");
  assert.match(context.queuedWaitDetail.text, /is not the controller/);
  assert.match(context.queuedWaitDetail.text, /Restart the package/);
});

test("a running idle controller promises the queue will drain", () => {
  const context = snapshotContext();
  assert.equal(context.queuedWaitDetail.kind, "queued");
  assert.match(context.queuedWaitDetail.text, /starts this as soon as the work ahead of it finishes/);
});

test("an unavailable snapshot claims nothing about why a job is waiting", () => {
  const context = snapshotContext({ service: null, controller: null });
  assert.equal(context.queuedWaitDetail.kind, "unknown");
  assert.match(context.queuedWaitDetail.text, /cannot be shown/);
  // The one thing it must not do is guess.
  assert.doesNotMatch(context.queuedWaitDetail.text, /stopped|wedged|already running/);
});

test("a snapshot without a controller block falls back rather than inventing liveness", () => {
  const context = snapshotContext({ controller: null });
  assert.equal(context.controllerLiveness.known, false);
  assert.equal(context.queuedWaitDetail.kind, "unknown");
});

// The shape the contract asks for: a determinate phase counter, an honest
// running count, and no percentage, because the denominator is unknown.
test("a counted phase renders its step, label and running count without a percentage", () => {
  const context = snapshotContext({
    liveProgress: { active: true, operation: "sync-status", progress: countedRecord() }
  });
  assert.equal(
    context.liveProgressDetail,
    "Step 5 of 7: Comparing file contents — 12,480 files so far."
  );
  assert.doesNotMatch(context.liveProgressDetail, /%/);
  assert.doesNotMatch(context.liveProgressDetail, /\b\d+\s*\/\s*\d+\b/);
});

test("a determinate phase renders as a phase alone", () => {
  const context = snapshotContext({
    liveProgress: {
      active: true,
      operation: "sync-status",
      progress: countedRecord({ step: 2, label: "Connecting to DSM", unit: "", count: 0 })
    }
  });
  assert.equal(context.liveProgressDetail, "Step 2 of 7: Connecting to DSM.");
});

test("a byte counter is rendered in bytes, not as a file count", () => {
  const context = snapshotContext({
    liveProgress: {
      active: true,
      operation: "run",
      progress: countedRecord({ step: 7, total: 8, label: "Uploading files", unit: "bytes", count: 4096 })
    }
  });
  assert.match(context.liveProgressDetail, /4096 B so far/);
  assert.doesNotMatch(context.liveProgressDetail, /4,096 bytes/);
});

// A progress record that stopped updating is itself information: the job may be
// wedged. Rendering the last phase forever, with no note that it is the last
// thing we heard, is how a stuck job looks exactly like a patient one.
test("a progress record that has stopped advancing says so", () => {
  const context = snapshotContext({
    liveProgress: {
      active: true,
      operation: "sync-status",
      progress: countedRecord({ updatedAt: 1757548800 - 600 })
    }
  });
  assert.match(context.liveProgressDetail, /Step 5 of 7: Comparing file contents/);
  assert.match(context.liveProgressDetail, /has not advanced since/);
  assert.match(context.liveProgressDetail, /may be stuck/);
});

// Fail-closed is the correct behaviour and it must be visible. A silently
// dropped record is how the previous version of this stayed broken through a
// release: nothing rendered, nothing went red, and nobody could tell the
// difference between "no writer" and "a writer nobody trusts".
// Both timestamps come from the NAS. Subtracting a NAS epoch from the browser's
// clock would report every record on an unsynchronised NAS as wedged, which is
// the one false alarm this warning cannot afford: it tells an operator to go and
// restart a package that is working.
test("staleness is measured against the package clock, not the browser's", () => {
  const context = snapshotContext({
    liveProgress: {
      active: true,
      operation: "sync-status",
      // Two seconds old by the package's clock, and years old by the browser's.
      progress: countedRecord({ updatedAt: 1757548798 })
    }
  });
  assert.doesNotMatch(context.liveProgressDetail, /may be stuck/);
});

test("no package clock means no staleness claim rather than a guess", () => {
  const context = snapshotContext({
    liveProgress: { active: true, operation: "sync-status", progress: countedRecord({ updatedAt: 1 }) }
  });
  context.snapshot.generated_at_epoch = 0;
  context.controllerLiveness = component.computed.controllerLiveness.call(context);
  assert.equal(context.controllerLiveness.packageEpoch, 0);
  assert.doesNotMatch(component.computed.liveProgressDetail.call(context), /may be stuck/);
  assert.equal(component.computed.queuedWaitDetail.call(context).kind, "queued");
});

test("a record that fails validation renders as unavailable rather than as nothing", () => {
  for (const progress of [
    { unavailable: true },
    { step: 5, total: 7, label: "Comparing file contents", updated_at: 1757548790, unit: "files", count: 12480, detail: "seventh key" },
    { step: 0, total: 7, label: "Comparing file contents", updatedAt: 1757548790, unit: "files", count: 1 },
    { step: 9, total: 7, label: "Comparing file contents", updatedAt: 1757548790, unit: "files", count: 1 },
    { step: 5, total: 7, label: "", updatedAt: 1757548790, unit: "files", count: 1 }
  ]) {
    const context = snapshotContext({
      liveProgress: { active: true, operation: "sync-status", progress }
    });
    assert.match(
      context.liveProgressDetail,
      /^Progress unavailable/,
      `${JSON.stringify(progress)} must be reported, not dropped`
    );
    assert.match(context.liveProgressDetail, /still queued/);
  }
});

// The catalogue allow-list, enforced again on the side that does the rendering.
// A bridge whose phase table has drifted from this one is caught and reported
// rather than quietly putting a string this window has never reviewed on screen.
test("a label outside the shared catalogue is refused", () => {
  for (const label of [
    "Doing something the catalogue does not name",
    "comparing file contents",
    "Comparing file contents ",
    "<img src=x onerror=alert(1)>"
  ]) {
    const context = snapshotContext({
      liveProgress: { active: true, operation: "sync-status", progress: countedRecord({ label }) }
    });
    assert.match(context.liveProgressDetail, /^Progress unavailable/, `${label} must be refused`);
  }
});

test("every label in both halves of the catalogue is renderable", () => {
  const catalog = appSource.match(/const DOCTOR_SECTION_CATALOG = Object\.freeze\(\[(.*?)\n\]\);/s);
  assert.ok(catalog, "the doctor half of the catalogue must be present");
  const tables = [...appSource.matchAll(/const [A-Z_]+_PHASE_SPECS = Object\.freeze\(\[(.*?)\n\]\);/gs)];
  assert.equal(tables.length, 4, "the four phase tables mirrored from src/lib.rs must be present");
  const labels = [
    ...catalog[1].matchAll(/label: "([^"]+)"/g),
    ...tables.flatMap((table) => [...table[1].matchAll(/label: "([^"]+)"/g)])
  ].map((match) => match[1]);
  assert.ok(labels.length >= 16 + 7 + 6 + 8 + 4, "every catalogued label must be covered");
  for (const label of labels) {
    const context = snapshotContext({
      liveProgress: { active: true, operation: "sync-status", progress: countedRecord({ label, unit: "", count: 0 }) }
    });
    assert.match(
      context.liveProgressDetail,
      /^Step 5 of 7: /,
      `${label} is in the catalogue and must render`
    );
  }
});

// When a job has published no phase, the wait explanation is what is left. This
// is the pairing that means a pending job is never rendered as nothing at all.
test("an operation with no published progress falls back to why it is waiting", () => {
  const context = snapshotContext({
    service: "stopped",
    controller: { state: "stopped", pid: 0, updated_epoch: 1757548000 },
    liveProgress: { active: true, operation: "sync-status", progress: null }
  });
  assert.match(context.liveProgressDetail, /package controller is stopped/);
});

// The lead-in belongs to exactly one caller. A surface showing a spinner and
// "Doctor is running" must be told when the thing is not running at all; the
// queued toast and the reconciliation barrier have each already said it, and
// repeating it there reads as a stutter.
test("the running surfaces lead with Queued and the toast does not repeat it", () => {
  const stopped = {
    service: "stopped",
    controller: { state: "stopped", pid: 0, updated_epoch: 1757548000 }
  };
  const running = snapshotContext({
    ...stopped,
    liveProgress: { active: true, operation: "action", progress: null }
  });
  assert.match(running.liveProgressDetail, /^Queued\. The package controller is stopped/);

  const reported = reportContext({ snapshot: stopped });
  const error = Object.assign(new Error("The package accepted this change and is still working through its queue."), {
    stillPending: true, accepted: true, outcomeUnknown: false, progress: null
  });
  const message = methods.reportMutationError.call(reported, error, "Failed", "Unknown", "fallback").message;
  assert.doesNotMatch(message, /Queued\. The package controller/);
  assert.match(message, /working through its queue\. The package controller is stopped/);
});

test("nothing is rendered when no operation is running", () => {
  const context = snapshotContext({ liveProgress: { active: false, operation: "", progress: null } });
  assert.equal(context.liveProgressDetail, "");
});

function reportContext(overrides = {}) {
  const base = snapshotContext(overrides.snapshot || {});
  return Object.assign(base, {
    toasts: [],
    csrfToken: "csrf",
    bridgeIssue: { title: "", message: "" },
    connectionLabel: "",
    toast(title, message, error = false) { this.toasts.push({ title, message, error }); }
  }, overrides.context || {});
}

// The fixed string this replaces built its own sentence and never read the
// `progress` field the error had carried all along.
test("a still-queued report names the phase the package published", () => {
  const context = reportContext();
  const error = Object.assign(new Error("The package accepted this change and is still working through its queue."), {
    stillPending: true,
    accepted: true,
    outcomeUnknown: false,
    progress: countedRecord()
  });
  const report = methods.reportMutationError.call(context, error, "Failed", "Unknown", "fallback");
  assert.equal(report.stillPending, true);
  assert.match(report.message, /Step 5 of 7: Comparing file contents — 12,480 files so far/);
  assert.match(report.message, /Do not send it again/);
  assert.equal(context.toasts[0].error, false, "a queued change is not an error");
  assert.equal(context.toasts[0].title, "Change queued");
});

test("a still-queued report with no phase explains the wait from the snapshot", () => {
  const context = reportContext({
    snapshot: { service: "stopped", controller: { state: "stopped", pid: 0, updated_epoch: 1757548000 } }
  });
  const error = Object.assign(new Error("The package accepted this change and is still working through its queue."), {
    stillPending: true, accepted: true, outcomeUnknown: false, progress: null
  });
  const report = methods.reportMutationError.call(context, error, "Failed", "Unknown", "fallback");
  assert.match(report.message, /package controller is stopped/);
  assert.match(report.message, /Package Center/);
});

test("a still-queued report survives a snapshot that cannot explain the wait", () => {
  const context = reportContext({ snapshot: { service: null, controller: null } });
  delete context.queuedWaitDetail;
  const error = Object.assign(new Error("still queued"), {
    stillPending: true, accepted: true, outcomeUnknown: false, progress: null
  });
  const report = methods.reportMutationError.call(context, error, "Failed", "Unknown", "fallback");
  assert.equal(report.stillPending, true);
  assert.match(report.message, /Do not send it again/);
});

// Never "Operation could not be completed" alone, and never "unresolved" with no
// reason attached.
test("each named queued failure renders its cause and its next step", () => {
  const expectations = {
    classification_failed: [/could not determine what kind of operation/, /submit the request again/],
    secret_claim_failed: [/could not be claimed/, /never started/, /submit the request again/],
    consumer_failed: [/exited before it finished/, /out-of-memory killer/],
    consumer_wrote_no_result: [/wrote no result/, /unconfirmed/]
  };
  for (const [code, patterns] of Object.entries(expectations)) {
    const context = reportContext();
    const error = Object.assign(new Error("Package operation failed"), {
      code,
      consumerFailed: true,
      accepted: true,
      outcomeUnknown: false,
      requiresInspection: code === "consumer_wrote_no_result",
      exitCode: code === "consumer_failed" ? 137 : null
    });
    const report = methods.reportMutationError.call(context, error, "Failed", "Unknown", "fallback");
    assert.equal(report.queuedFailure, true, `${code} must be reported as a named failure`);
    assert.equal(report.code, code);
    for (const pattern of patterns) {
      assert.match(report.message, pattern, `${code} must explain itself: ${pattern}`);
    }
    assert.doesNotMatch(report.message, /^Operation could not be completed\.$/);
    assert.doesNotMatch(report.message, /unresolved/i);
  }
});

test("a killed worker reports the exit code it died with", () => {
  const context = reportContext();
  const error = Object.assign(new Error("Package operation failed"), {
    code: "consumer_failed", accepted: true, outcomeUnknown: false, exitCode: 137
  });
  const report = methods.reportMutationError.call(context, error, "Failed", "Unknown", "fallback");
  assert.match(report.message, /exited with code 137/);
  assert.equal(context.toasts[0].title, "Queued operation was terminated");
});

test("a missing exit code is omitted rather than reported as a clean exit", () => {
  const context = reportContext();
  const error = Object.assign(new Error("Package operation failed"), {
    code: "consumer_failed", accepted: true, outcomeUnknown: false, exitCode: null
  });
  const report = methods.reportMutationError.call(context, error, "Failed", "Unknown", "fallback");
  assert.doesNotMatch(report.message, /exited with code/);
});

test("an outcome-unknown verdict still outranks a named code", () => {
  // Naming a stage must never be able to convert an unknown outcome into a
  // claimed-known one; the barrier's meaning is unchanged by this work.
  const context = reportContext();
  const error = Object.assign(new Error("Package operation failed"), {
    code: "consumer_failed", accepted: true, outcomeUnknown: true
  });
  const report = methods.reportMutationError.call(context, error, "Failed", "Unknown", "fallback");
  assert.equal(report.unknown, true);
  assert.notEqual(report.queuedFailure, true);
});

test("the live progress regions are polite and separate from the assertive barrier", () => {
  const regions = [...appSource.matchAll(/class="sdsync-live-progress"[^>]*/g)].map((match) => match[0]);
  assert.ok(regions.length >= 3, "sync, resync and doctor each need a live region");
  const barrier = appSource.match(/class="sdsync-banner sdsync-mutation-barrier" role="alert" aria-live="assertive"/);
  assert.ok(barrier, "the mutation barrier must stay assertive");
  for (const region of regions) {
    const inDoctorStatus = region.indexOf("role=") < 0;
    assert.ok(
      inDoctorStatus || /role="status" aria-live="polite"/.test(region),
      `a per-tick progress note must never interrupt as an alert: ${region}`
    );
  }
});
