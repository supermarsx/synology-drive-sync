import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import {
  WIDGET_ACTIVE_POLL_MS,
  WIDGET_BACKOFF_RAMP_MS,
  WIDGET_IDLE_POLL_MS
} from "../src/widgetModel.mjs";
import { APP_MODULE_STUBS } from "./helpers/app-stubs.mjs";

// Behavioral coverage for how the dashboard surfaces a realtime routine's
// backend_reason: the Overview three-way summary (realtimeDetail), the
// Routines catalog row, the idle "next run" cell, and the routine editor's
// live status line. backend_reason is additive to the snapshot and absent
// reads as "ok" (see the realtime watcher contract); every case here proves
// that default alongside the nine literal reason values.

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
    ...APP_MODULE_STUBS,
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

function routine(overrides = {}) {
  return Object.assign({
    profile: "photos", enabled: true, mode: "realtime", backend: "inotify",
    state: "scheduled", next_run_epoch: 0
  }, overrides);
}

function realtimeDetailFor(routines) {
  return component.computed.realtimeDetail.call({
    realtimeRoutines: routines.filter((item) => item.enabled === true && item.mode === "realtime")
  });
}

test("realtimeDetail: no enabled realtime routine", () => {
  assert.equal(realtimeDetailFor([]), "No enabled realtime routine");
  assert.equal(realtimeDetailFor([routine({ enabled: false })]), "No enabled realtime routine");
  assert.equal(realtimeDetailFor([routine({ mode: "interval" })]), "No enabled realtime routine");
});

test("realtimeDetail: healthy native watchers report watching with inotify", () => {
  assert.equal(realtimeDetailFor([routine(), routine({ profile: "docs" })]), "2 watching with inotify");
  // Absent backend_reason reads as ok, exactly like a pre-upgrade state file.
  assert.equal(realtimeDetailFor([Object.assign(routine(), { backend_reason: undefined })]), "1 watching with inotify");
  assert.equal(realtimeDetailFor([Object.assign(routine(), { backend_reason: "ok" })]), "1 watching with inotify");
});

test("realtimeDetail: a polling backend with an ok reason is the fallback tier", () => {
  assert.equal(realtimeDetailFor([routine({ backend: "polling" })]), "1 using polling fallback");
  assert.equal(
    realtimeDetailFor([routine({ backend: "polling" }), routine({ profile: "docs", backend: "inotify" })]),
    "1 using polling fallback"
  );
});

test("realtimeDetail: the six not-observing reasons outrank both other tiers", () => {
  for (const reason of [
    "source_missing", "source_symlink", "watch_limit", "watch_setup_failed", "poll_unreadable", "poll_unavailable"
  ]) {
    assert.equal(
      realtimeDetailFor([routine({ backend: "none", backend_reason: reason })]),
      "1 not observing changes",
      reason
    );
    // Ranked ahead of a healthy polling routine beside it.
    assert.equal(
      realtimeDetailFor([routine({ backend: "none", backend_reason: reason }), routine({ profile: "docs", backend: "polling" })]),
      "1 not observing changes",
      reason
    );
  }
});

test("realtimeDetail: watch_unavailable and watch_stopped still have a working observation path", () => {
  // Neither reason is in the not-observing set: watch_unavailable means
  // polling is covering for a missing native watcher, and watch_stopped is a
  // self-healed transient. Both must fall through to the backend-based tiers.
  assert.equal(realtimeDetailFor([routine({ backend: "polling", backend_reason: "watch_unavailable" })]), "1 using polling fallback");
  assert.equal(realtimeDetailFor([routine({ backend: "inotify", backend_reason: "watch_stopped" })]), "1 watching with inotify");
});

test("the Routines catalog row appends backend_reason only when it is not ok, and the string it replaced is gone", () => {
  assert.doesNotMatch(appSource, /Native\/fallback backend reported healthy/);
  const row = appSource.match(/<button v-for="routine in routines"[\s\S]*?<\/button>/);
  assert.ok(row, "Routines catalog row markup is missing");
  assert.match(row[0], /\{\{ routine\.backend_reason && routine\.backend_reason !== 'ok' \? ' · ' \+ routine\.backend_reason : '' \}\}/);
  assert.match(row[0], /<time>\{\{ routineNextRunLabel\(routine\) \}\}<\/time>/);
});

test("routineNextRunLabel: disabled wins outright, then realtime-idle, then the ordinary date", () => {
  const label = (overrides) => component.methods.routineNextRunLabel.call({}, routine(overrides));
  assert.equal(label({ enabled: false, next_run_epoch: 1700000000 }), "Disabled");
  // A healthy, idle realtime routine must never read "Unavailable".
  assert.equal(label({ next_run_epoch: 0 }), "Watching");
  assert.equal(label({ next_run_epoch: 0, backend_reason: "ok" }), "Watching");
  assert.equal(label({ next_run_epoch: 0, backend_reason: undefined }), "Watching");
  assert.equal(label({ next_run_epoch: 0, backend_reason: "watch_limit" }), "watch_limit");
  assert.equal(label({ next_run_epoch: 0, backend_reason: "source_missing" }), "source_missing");
  // A realtime row with a real next_run_epoch (should never happen today, but
  // the label must still make sense) falls through to the ordinary date.
  assert.equal(label({ next_run_epoch: 1700000000 }), "date:1700000000");
  // Non-realtime modes always use the ordinary date, never "Watching".
  assert.equal(label({ mode: "interval", next_run_epoch: 1700000000 }), "date:1700000000");
  assert.equal(label({ mode: "interval", next_run_epoch: 0 }), "Unavailable");
});

test("realtimeReasonGuidance names an operator action for every non-ok reason, and nothing for ok or unknown", () => {
  for (const reason of [
    "source_missing", "source_symlink", "watch_unavailable", "watch_setup_failed",
    "watch_limit", "watch_stopped", "poll_unreadable", "poll_unavailable"
  ]) {
    const guidance = component.methods.realtimeReasonGuidance.call({}, reason);
    assert.ok(guidance.length > 0, reason);
  }
  assert.equal(component.methods.realtimeReasonGuidance.call({}, "ok"), "");
  assert.equal(component.methods.realtimeReasonGuidance.call({}, "banana"), "");
  assert.equal(component.methods.realtimeReasonGuidance.call({}, undefined), "");

  // The four examples the review named must be recognizable in the guidance
  // text, not just present as opaque strings.
  assert.match(component.methods.realtimeReasonGuidance.call({}, "watch_unavailable"), /inotify-tools/);
  assert.match(component.methods.realtimeReasonGuidance.call({}, "watch_limit"), /max_user_watches/);
  assert.match(component.methods.realtimeReasonGuidance.call({}, "poll_unreadable"), /unreadable/);
  assert.match(component.methods.realtimeReasonGuidance.call({}, "source_missing"), /does not exist/);
  assert.match(component.methods.realtimeReasonGuidance.call({}, "source_symlink"), /symlink/);
});

test("the routine editor surfaces the live backend, state, reason, and last-poll age for a realtime routine", () => {
  const editor = appSource.match(/<v-form-item v-if="routineForm\.mode === 'realtime'"[^>]*label="Fallback poll[\s\S]*?<\/div>\s*<v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Retry attempts"/);
  assert.ok(editor, "the realtime status block must sit beside the realtime-only fields");
  const block = editor[0];
  assert.match(block, /v-if="routineForm\.mode === 'realtime' && selectedRoutine"/);
  assert.match(block, /\{\{ selectedRoutine\.backend \|\| 'none' \}\}/);
  assert.match(block, /\{\{ selectedRoutine\.state \|\| \(selectedRoutine\.enabled \? 'enabled' : 'disabled'\) \}\}/);
  assert.match(block, /selectedRoutine\.backend_reason && selectedRoutine\.backend_reason !== 'ok'/);
  assert.match(block, /last poll \{\{ formatDate\(selectedRoutine\.last_poll_epoch\) \}\}/);
  assert.match(block, /\{\{ realtimeReasonGuidance\(selectedRoutine\.backend_reason\) \}\}/);
  // The status block must not exist without a selectedRoutine (a brand new,
  // unsaved routine draft has nothing live to report yet).
  assert.doesNotMatch(block.split("selectedRoutine.backend_reason")[0], /v-if="routineForm\.mode === 'realtime'"[^"]*"[^>]*>\s*<p class="sdsync-field-note">Realtime status/);
});

test("the routine-poll help copy no longer calls polling a fallback", () => {
  assert.match(
    appSource,
    /"routine-poll": "How often the source is re-read to detect changes\. This runs whether or not a native watcher is available\."/
  );
  assert.doesNotMatch(appSource, /Fallback observation cadence when a native realtime hook is unavailable/);
});
