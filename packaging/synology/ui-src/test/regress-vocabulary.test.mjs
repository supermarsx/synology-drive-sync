// Round-3 regression-verifier vocabulary (verify3-regress.md section 4): every token a fixed writer will
// emit has a label here before the writer lands, so none reads as "unknown reason" on an operator's Overview.
import assert from "node:assert/strict";
import test from "node:test";
import { appContext, loadApp } from "./helpers/load-app.mjs";
import { attentionItems, normalizeOutcomes, reasonCopy, warningsSentence } from "../src/outcomes.mjs";

const NOW = 1_700_000_000;

function block(run = {}, over = {}) {
  return {
    schema: "sdsync.dsm-outcomes.v1",
    run: { outcome: "succeeded", reason: null, scope: "all", operation: "sync", started_epoch: NOW - 100, finished_epoch: NOW - 60, exit_code: 0, stale: { detected: false, lock: "absent" }, ...run },
    alerts: { status: "ok", failure_count: 0 },
    profiles: [], routines: [], errors: [],
    ...over
  };
}

test("the share-wide refusal and the unusable-real-path reasons have a cause and a next step", () => {
  for (const code of ["remote_refused_share_wide", "remote_real_path_unusable"]) {
    const copy = reasonCopy(code);
    assert.equal(copy.known, true, code);
    assert.doesNotMatch(copy.text, /unknown reason/, code);
  }
  assert.match(reasonCopy("remote_refused_share_wide").text, /most|share/i);
  assert.match(reasonCopy("remote_refused_share_wide").text, /permission|File Station|Doctor/i, "names a next step");
  assert.match(reasonCopy("remote_real_path_unusable").text, /link/i);
  // The kebab spelling the old label used still resolves.
  assert.equal(reasonCopy("remote-real-path-unusable").known, true);
  const failed = attentionItems(normalizeOutcomes(block({ outcome: "failed", reason: "remote_refused_share_wide", exit_code: 1 })), NOW);
  assert.equal(failed[0].level, "fail");
  assert.doesNotMatch(failed[0].detail, /unknown reason/);
});

test("an overlapping-destinations refusal reads as a configuration problem with the pair's detail", () => {
  const copy = reasonCopy("config_invalid");
  assert.match(copy.text, /overlapping destinations/);
  assert.match(copy.text, /separate destinations/);
  const profile = { profile: "b", outcome: "failed", reason: "config_invalid", status_cache: "last_run_failed", finished_epoch: NOW - 5, detail: 'configuration did not validate: error: profiles "a" and "b" have overlapping NAS destinations and "b" deletes' };
  const items = attentionItems(normalizeOutcomes(block({ outcome: "failed", reason: "config_invalid", exit_code: 2 }, { profiles: [profile] })), NOW);
  assert.ok(items.some((item) => item.id === "profile-b" && /overlapping NAS destinations/.test(item.detail)), JSON.stringify(items));
});

test("link_guard_unchecked is an amber count, never a failure and never silent", () => {
  const run = normalizeOutcomes(block({ outcome: "succeeded", link_guard_unchecked: 7, warnings: ["link_guard_unchecked"] })).run;
  assert.equal(run.linkGuardUnchecked, 7);
  const text = warningsSentence(run);
  assert.match(text, /link check unavailable for 7 folders/i);
  const items = attentionItems(normalizeOutcomes(block({ outcome: "succeeded", link_guard_unchecked: 7, warnings: ["link_guard_unchecked"] })), NOW);
  const item = items.find((candidate) => candidate.id === "run-warnings");
  assert.ok(item, JSON.stringify(items));
  assert.equal(item.level, "warn");
  assert.match(item.detail, /link check unavailable for 7 folders/i);
  assert.match(warningsSentence(normalizeOutcomes(block({ link_guard_unchecked: 1, warnings: ["link_guard_unchecked"] })).run), /1 folder\b/);
  // An absent or zero count is no warning at all.
  assert.equal(normalizeOutcomes(block()).run.linkGuardUnchecked, null);
  assert.doesNotMatch(warningsSentence(normalizeOutcomes(block({ link_guard_unchecked: 0 })).run), /link/i);
  // Without a count, the token alone still reads as copy, not as an unknown warning.
  assert.doesNotMatch(warningsSentence({ warnings: ["link_guard_unchecked"] }), /unknown warning/);
});

test("stall_unobservable says hang detection is unavailable and is never read as healthy or as stalled", async () => {
  const running = block({ outcome: "running", stalled: null, heartbeat_source: "liveness", warnings: ["stall_unobservable"], running_for_seconds: 600, stale: { detected: false, lock: "live" } });
  const outcomes = normalizeOutcomes(running);
  assert.equal(outcomes.run.stalled, false, "null is not 'stalled'");
  assert.equal(outcomes.run.stallUnobservable, true);
  assert.match(warningsSentence(outcomes.run), /Cannot tell whether this run is stuck.*cannot be observed/i);
  const items = attentionItems(outcomes, NOW);
  assert.ok(!items.some((item) => item.id === "run-stalled"), "no stuck claim");
  assert.ok(!items.some((item) => item.id === "run-warnings-extra"), "and no 'also reported warnings' alarm for a quiet note");
  const component = await loadApp();
  const ctx = appContext(component, { snapshot: { schema: "sdsync.dsm-api.v1", generated_at_epoch: NOW, service: { state: "running" }, run: { state: "running" }, outcomes: running }, connected: true, freshness: "x", nowMs: NOW * 1000, snapshotReceivedAtMs: NOW * 1000 });
  assert.match(ctx.runningForSuffix, /hang detection unavailable|progress cannot be observed/i);
  // A running run whose heartbeat the bridge can read is unchanged.
  const watched = normalizeOutcomes(block({ outcome: "running", stalled: false, heartbeat_source: "io", running_for_seconds: 600, stale: { detected: false, lock: "live" } }));
  assert.equal(watched.run.stallUnobservable, false);
});

test("clock_skew in the record errors is explained, not just named", () => {
  const items = attentionItems(normalizeOutcomes(block({}, { errors: [{ source: "run", code: "clock_skew" }] })), NOW);
  const item = items.find((candidate) => candidate.id === "record-errors");
  assert.ok(item);
  assert.match(item.detail, /clock_skew/);
  assert.match(item.detail, /clock/i);
  assert.match(item.detail, /NAS clock|time/i);
});

test("run.state clock_skew leaves a running run with no age and no 'stuck' claim, and says why", () => {
  const skewed = block({ outcome: "running", stalled: null, running_for_seconds: null, heartbeat_source: "io", stale: { detected: false, lock: "live" } }, { errors: [{ source: "run.state", code: "clock_skew" }] });
  const outcomes = normalizeOutcomes(skewed);
  assert.equal(outcomes.run.runningFor, null);
  assert.equal(outcomes.run.stallUnobservable, true);
  const items = attentionItems(outcomes, NOW);
  assert.ok(!items.some((item) => item.id === "run-stalled" || item.id === "run-long"));
  assert.match(items.find((item) => item.id === "record-errors").detail, /run\.state clock_skew \(.*NAS time/);
});
