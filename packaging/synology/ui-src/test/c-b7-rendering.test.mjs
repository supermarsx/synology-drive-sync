// Contract C-B7 / C-S1: a pending controller reload, a stalled run, the core's own
// error line and failed path per profile, scan-limited logs, percent-encoded paths.
import assert from "node:assert/strict";
import test from "node:test";
import { appContext, appSource, loadApp } from "./helpers/load-app.mjs";
import * as outcomesModule from "../src/outcomes.mjs";
import * as textSafety from "../src/textSafety.mjs";
import { widgetView } from "../src/widgetModel.mjs";

globalThis.window ??= globalThis;
globalThis.document ??= { hidden: false };

const { attentionItems, normalizeOutcomes, profileOutcomeRow } = outcomesModule;
const NOW = 1_700_000_000;

function block(over = {}) {
  return {
    schema: "sdsync.dsm-outcomes.v1",
    run: { outcome: "succeeded", scope: "all", operation: "sync", started_epoch: NOW - 100, finished_epoch: NOW - 60, exit_code: 0, stale: { detected: false, lock: "absent" } },
    alerts: { status: "ok", failure_count: 0 },
    profiles: [{ profile: "photos", outcome: "succeeded", status_cache: "trusted", finished_epoch: NOW - 60 }],
    routines: [],
    errors: [],
    ...over
  };
}

function snapshot(outcomes, over = {}) {
  return {
    schema: "sdsync.dsm-api.v1", generated_at_epoch: NOW, service: { state: "running" },
    run: { state: "succeeded", scope: "all", operation: "sync", finished_epoch: NOW - 60 },
    profiles: [{ name: "photos", health: { state: "succeeded", checked_epoch: NOW - 60 } }],
    routines: [{ profile: "photos", state: "succeeded", last_success_epoch: NOW - 60 }],
    outcomes, ...over
  };
}

test("a pending controller reload is a persistent warning that says to restart the package service", () => {
  const pending = normalizeOutcomes(block({ controller_reload: { pending: true, epoch: NOW - 600, reason: "identity_refused", pid: 4242, operation: "configure-routine" } }));
  const item = attentionItems(pending, NOW).find((candidate) => candidate.id === "controller-reload");
  assert.equal(item.level, "warn");
  assert.match(item.title, /Saved, not yet applied; restart the package service/);
  assert.match(item.detail, /configure-routine/);
  assert.match(item.detail, /not the verified controller/);
  assert.doesNotMatch(item.detail, /unknown reason/);
  // Settled, and absent (an older bridge), say nothing.
  for (const reload of [{ pending: false }, undefined]) {
    const items = attentionItems(normalizeOutcomes(block({ controller_reload: reload })), NOW);
    assert.equal(items.some((candidate) => candidate.id === "controller-reload"), false);
  }
  // An unreadable marker is not "applied".
  const unreadable = attentionItems(normalizeOutcomes(block({ controller_reload: { pending: null } })), NOW);
  assert.match(unreadable.find((candidate) => candidate.id === "controller-reload").title, /Cannot tell whether saved changes are applied/);
  // The desktop card sees it too (amber headline, not green).
  const view = widgetView(snapshot(block({ controller_reload: { pending: true, epoch: NOW - 5, reason: "signal_failed", pid: 1, operation: "set-schedule" } })), NOW * 1000);
  assert.notEqual(view.overview.level, "ok");
  assert.match(view.overview.title, /not yet applied/);
});

test("a stalled run warns that nothing has moved for 30 minutes", () => {
  const running = block({ run: { outcome: "running", scope: "all", operation: "sync", started_epoch: NOW - 4000, running_for_seconds: 4000, stalled: true, warnings: ["stalled"], stale: { detected: false, lock: "live" } } });
  const items = attentionItems(normalizeOutcomes(running), NOW);
  const item = items.find((candidate) => candidate.id === "run-stalled");
  assert.equal(item.level, "warn");
  assert.match(item.title, /Run may be stuck: no progress for 30 minutes/);
  const timed = attentionItems(normalizeOutcomes(block({ run: { ...running.run, heartbeat_epoch: NOW - 2700 } })), NOW).find((candidate) => candidate.id === "run-stalled");
  assert.match(timed.title, /Run may be stuck: no progress for 45 min/);
  assert.equal(items.filter((candidate) => /warnings/.test(candidate.title)).length, 0, "the stalled token is not listed twice");
  assert.equal(attentionItems(normalizeOutcomes(block({ run: { ...running.run, stalled: null, warnings: [] } })), NOW).some((candidate) => candidate.id === "run-stalled"), false, "no heartbeat means no claim");
  // Busy waiting behind a stalled run is a warning on the card.
  const view = widgetView(snapshot({ ...running, routines: [{ profile: "photos", defer_reason: "busy" }] }, {
    run: { state: "running", scope: "all", operation: "sync" }, routines: [{ profile: "photos", state: "deferred", last_success_epoch: NOW - 60 }]
  }), NOW * 1000);
  assert.equal(view.rows[0].level, "warn");
  assert.match(view.rows[0].detail, /run may be stuck, no progress for over 30 minutes/);
});

test("a profile's failure shows the core's error line and the decoded path it failed on", () => {
  const profile = { profile: "photos", outcome: "failed", reason: "core_failed", status_cache: "last_run_failed", finished_epoch: NOW - 60, detail: "error: listing failed (DSM 408)", failed_path: "/team/caf%C3%A9/a%20b.txt" };
  const row = profileOutcomeRow(normalizeOutcomes(block({ profiles: [profile] })).profiles[0], NOW);
  assert.match(row.detail, /core said: error: listing failed \(DSM 408\)/);
  assert.match(row.detail, /failed on \/team\/café\/a b\.txt/);
  // Null (not yet written by the runner) adds nothing.
  const bare = profileOutcomeRow(normalizeOutcomes(block({ profiles: [{ ...profile, detail: null, failed_path: null }] })).profiles[0], NOW);
  assert.doesNotMatch(bare.detail, /core said|failed on/);
  // A hostile path stays encoded or escaped; it never reaches the screen as a control or bidi character.
  const hostile = normalizeOutcomes(block({ profiles: [{ ...profile, failed_path: "/a%E2%80%AEgpj.exe%0Aforged" }] })).profiles[0];
  assert.ok(!hostile.failedPath.includes(String.fromCharCode(0x202e)) && !hostile.failedPath.includes(String.fromCharCode(10)), hostile.failedPath);
});

test("percent-decoding is exact and refuses to decode what would break a line", () => {
  const { decodePercentPath, decodeLogPaths } = textSafety;
  assert.equal(decodePercentPath("/a/b.txt"), "/a/b.txt");
  assert.equal(decodePercentPath("caf%C3%A9%20x"), "café x");
  assert.equal(decodePercentPath("bad%FF%FE"), "bad%FF%FE", "invalid UTF-8 stays encoded");
  assert.equal(decodePercentPath("q%22uote%5Cback%0Anl"), "q%22uote%5Cback%0Anl", "quote, backslash and control bytes stay encoded");
  assert.equal(decodePercentPath("100%"), "100%");
  const line = '{"level":"warn","event":"entry.deferred","path":"/x/caf%C3%A9","reason":"source-changed"}';
  assert.equal(decodeLogPaths(line), '{"level":"warn","event":"entry.deferred","path":"/x/café","reason":"source-changed"}');
  assert.equal(decodeLogPaths("not json path"), "not json path");
});

test("scan-limited logs say that older history was not scanned", async () => {
  const component = await loadApp();
  const ctx = appContext(component, { logSource: "sync" });
  const records = ctx.logRecordsFrom({
    schema: "sdsync.dsm-logs.v1",
    logs: [{ source: "sync", scan_limited: true, lines: ['{"level":"error","path":"/t/caf%C3%A9"}'] }, { source: "api", lines: ['{"level":"info"}'] }]
  });
  assert.equal(records[0].scanLimited, true);
  assert.equal(records[1].scanLimited, undefined);
  assert.match(records[0].text, /café/, "the encoded path is shown decoded");
  assert.match(appSource, /v-if="record\.scanLimited">newest lines only; older history was not scanned/);
});

test("a service state of unknown (sdsync-dsm status can now print it) is not drawn as a calm grey pill", () => {
  assert.ok(appSource.includes("pillClass(serviceState === 'unknown' ? 'failed' : serviceState)"));
});

test("the stalled item says what the runner last saw advance", () => {
  const run = (source) => block({ run: { outcome: "running", scope: "all", operation: "sync", started_epoch: NOW - 4000, running_for_seconds: 4000, stalled: true, heartbeat_epoch: NOW - 2000, heartbeat_source: source, warnings: ["stalled"], stale: { detected: false } } });
  const detail = (source) => attentionItems(normalizeOutcomes(run(source)), NOW).find((candidate) => candidate.id === "run-stalled").detail;
  assert.match(detail("io"), /reading or writing data/);
  assert.match(detail("progress"), /progress record/);
  assert.match(detail("liveness"), /only tell that the core is alive/);
  assert.doesNotMatch(detail(null), /last sign of life|only tell/);
  assert.doesNotMatch(detail("bogus"), /last sign of life|only tell/);
});
