// What the daemon engine adds to `outcomes`: the per-profile checksum (pair) index,
// the lasting holds (`lasting`) and the active service engine (`engine`). Nothing the
// backend names may be dropped: an unrecognised code is shown as unknown, in amber.
import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { attentionItems, indexLine, normalizeOutcomes, outcomesProblem, profileOutcomeRow, reasonCopy } from "../src/outcomes.mjs";
import { appContext, appSource, loadApp } from "./helpers/load-app.mjs";

const NOW = 1_700_000_000;

function block(over = {}, profile = {}) {
  return {
    schema: "sdsync.dsm-outcomes.v1",
    run: { outcome: "succeeded", reason: null, scope: "all", started_epoch: NOW - 100, finished_epoch: NOW - 60, stale: {} },
    alerts: { status: "ok", failure_count: 0 },
    profiles: [{ profile: "photos", outcome: "succeeded", reason: null, finished_epoch: NOW - 120, status_cache: "trusted", ...profile }],
    routines: [],
    errors: [],
    ...over
  };
}

const INDEX = {
  state: "warm", entries: 1000, hits: 975, misses: 25, canary_checked: 25, canary_mismatches: 0,
  refreshed: false, issue: null, issue_detail: null
};
const withIndex = (over) => block({}, { index: { ...INDEX, ...over } });
const items = (raw) => attentionItems(normalizeOutcomes(raw), NOW);
const indexItems = (raw) => items(raw).filter((item) => item.id.startsWith("pair-index-"));

test("a profile's checksum index reads as one compact line beside its last run", () => {
  const outcomes = normalizeOutcomes(withIndex({}));
  const row = profileOutcomeRow(outcomes.profiles[0], NOW);
  assert.equal(row.indexNote, "Checksum index: warm, 1,000 pairs; 975 reused, 25 checked live (25 canary)");
  assert.equal(indexLine(outcomes.profiles[0].index), row.indexNote);
});

test("the checksum index line covers every state and stays honest about missing numbers", () => {
  const line = (over) => indexLine(normalizeOutcomes(withIndex(over)).profiles[0].index);
  assert.equal(line({ state: "cold", entries: 1, hits: 0, misses: 1, canary_checked: 0 }), "Checksum index: cold, 1 pair; 0 reused, 1 checked live");
  assert.equal(line({ state: "off", entries: null, hits: 0, misses: 0, canary_checked: 0 }), "Checksum index: off; every file was checked live");
  assert.equal(line({ state: "unusable", entries: null, hits: 0, misses: 40, canary_checked: null }), "Checksum index: unusable; 0 reused, 40 checked live");
  assert.match(line({ state: "refreshed", entries: 12 }), /^Checksum index: refreshed, 12 pairs/);
  assert.match(line({ state: "local-only", entries: 12 }), /^Checksum index: local-only, 12 pairs/);
  assert.match(line({ canary_mismatches: 2 }), /2 canary mismatches/);
  assert.equal(line({ state: "warm", entries: null, hits: null, misses: null, canary_checked: null, canary_mismatches: null }), "Checksum index: warm");
  assert.match(line({ state: "from-the-future" }), /unknown \(from-the-future\)/);
});

test("a profile with no index reports no line, and an older backend's profile reads as before", () => {
  const none = profileOutcomeRow(normalizeOutcomes(block({}, { index: null })).profiles[0], NOW);
  assert.equal(none.indexNote, "");
  const missing = profileOutcomeRow(normalizeOutcomes(block()).profiles[0], NOW);
  assert.equal(missing.indexNote, "");
  assert.deepEqual(indexItems(block()), []);
});

test("an index that is not a document is shown as unreadable, not dropped", () => {
  const outcomes = normalizeOutcomes(block({}, { index: "warm" }));
  assert.match(profileOutcomeRow(outcomes.profiles[0], NOW).indexNote, /unreadable/);
  const found = indexItems(block({}, { index: "warm" }));
  assert.equal(found.length, 1);
  assert.equal(found[0].level, "warn");
  assert.match(found[0].title, /photos/);
});

test("AppWindow adds the index line to the profile's last-run text", async () => {
  const component = await loadApp();
  const ctx = appContext(component, {
    snapshot: { schema: "sdsync.dsm-api.v1", generated_at_epoch: NOW, outcomes: withIndex({}) },
    connected: true, freshness: "x", nowMs: NOW * 1000, snapshotReceivedAtMs: NOW * 1000
  });
  assert.match(ctx.profileOutcome("photos").text, /Checksum index: warm, 1,000 pairs; 975 reused, 25 checked live \(25 canary\)/);
});

const ISSUES = {
  "pair-index-corrupt": /damaged; it was set aside and rebuilt from this run, so every file was checked live/,
  "pair-index-locked": /another process|in use|locked/i,
  "pair-index-readonly": /read-only/i,
  "pair-index-store-failed": /could not be saved/i,
  // S3-21: the code is also named for a panic record or a file that could not be examined at all,
  // so the copy must not claim only an ownership or mode check.
  "pair-index-unsafe": /could not be examined or failed its safety check/i,
  "pair-index-directory-unusable": /folder/i,
  "pair-index-unbound": /different/i,
  "pair-index-format-unsupported": /version|format/i,
  "pair-index-canary-mismatch": /disagreed/i,
  "pair-index-writeback-unknown": /writeback setting could not be read/i,
  "pair-index-writeback-unbounded": /writeback is switched off/i,
  "pair-index-nas-clock-skewed": /clock .*more than 20 seconds/i,
  // S3-21: also named for a crash record, a listing and an unreadable age, not only a copy that stays.
  "pair-index-cleanup-failed": /could not be cleaned up/i,
  "pair-index-remote-ctime-static": /change time/i,
  "pair-index-unsettled-unusable": /keep changing could not be used/i,
  "pair-index-unkeepable": /cannot keep it; it is no longer rebuilt and every file is checked live/
};

for (const [issue, words] of Object.entries(ISSUES)) {
  test(`index issue ${issue} is an amber item that names the profile, the cause in plain words and the detail`, () => {
    const found = indexItems(withIndex({ issue, issue_detail: "/var/state/cache/pairs/photos.redb: it broke" }));
    assert.equal(found.length, 1, JSON.stringify(found));
    assert.equal(found[0].level, "warn");
    assert.match(found[0].title, /photos/);
    assert.match(found[0].detail, words);
    // The raw detail is evidence behind a disclosure, not the sentence itself.
    assert.doesNotMatch(found[0].detail, /it broke/);
    assert.equal(found[0].raw, "/var/state/cache/pairs/photos.redb: it broke");
    assert.doesNotMatch(found[0].detail, /unknown/);
  });
}

test("info-level index codes never produce an item", () => {
  for (const issue of ["pair-index-retired", "pair-index-not-applicable", "pair-index-unsupported-platform", "pair-index-plan-clock-unbounded", "pair-index-compaction-failed", "pair-index-link-guard-unchecked", "pair-index-failed-run", "pair-index-compacted", "pair-index-remote-ctime-unavailable"]) {
    assert.deepEqual(indexItems(withIndex({ issue })), [], issue);
  }
});

test("a retired index explains itself in the status line, with the detail, and raises no item", () => {
  const raw = withIndex({ state: "cold", entries: 60, hits: 0, misses: 60, canary_checked: 0, issue: "pair-index-retired", issue_detail: "bound elsewhere" });
  const row = profileOutcomeRow(normalizeOutcomes(raw).profiles[0], NOW);
  assert.equal(row.indexNote, "Checksum index: cold, 60 pairs; 0 reused, 60 checked live; rebuilt, it belonged to another source or remote (bound elsewhere)");
  assert.deepEqual(indexItems(raw), []);
  // Without a detail the sentence still stands; the other info codes explain themselves too.
  assert.match(indexLine(normalizeOutcomes(withIndex({ state: "cold", issue: "pair-index-retired" })).profiles[0].index), /rebuilt, it belonged to another source or remote$/);
  assert.match(indexLine(normalizeOutcomes(withIndex({ state: "off", issue: "pair-index-not-applicable" })).profiles[0].index), /not used for this profile/);
  assert.match(indexLine(normalizeOutcomes(withIndex({ state: "off", issue: "pair-index-unsupported-platform" })).profiles[0].index), /not supported on this platform/);
  assert.match(indexLine(normalizeOutcomes(withIndex({ state: "warm", issue: "pair-index-plan-clock-unbounded" })).profiles[0].index), /checked live again because the destination's clock is more than 20 seconds/);
  // A warning-level issue is an item, not a line note.
  assert.doesNotMatch(indexLine(normalizeOutcomes(withIndex({ issue: "pair-index-corrupt" })).profiles[0].index), /corrupt|damaged/);
});

test("the two info notes a run can name beyond the first four explain themselves in the status line, never as an unknown code", () => {
  // S3-21: both were missing, so a published one read as an amber "unknown index issue".
  const compaction = indexLine(normalizeOutcomes(withIndex({ state: "warm", issue: "pair-index-compaction-failed", issue_detail: "/p/photos.redb: disk full" })).profiles[0].index);
  assert.match(compaction, /could not be compacted/);
  assert.match(compaction, /photos\.redb: disk full/);
  const guard = indexLine(normalizeOutcomes(withIndex({ state: "warm", issue: "pair-index-link-guard-unchecked" })).profiles[0].index);
  assert.match(guard, /without a real path/);
  for (const line of [compaction, guard]) assert.doesNotMatch(line, /unknown/);
});

test("an index issue code this UI has never heard of is amber and names the code", () => {
  const found = indexItems(withIndex({ issue: "pair-index-brand-new", issue_detail: "x" }));
  assert.equal(found.length, 1);
  assert.equal(found[0].level, "warn");
  assert.match(found[0].detail, /unknown index issue \(pair-index-brand-new\)/);
});

test("two profiles with index issues produce two items with distinct ids", () => {
  const raw = block({}, { index: { ...INDEX, issue: "pair-index-corrupt" } });
  raw.profiles.push({ ...raw.profiles[0], profile: "docs" });
  const found = indexItems(raw);
  assert.equal(found.length, 2);
  assert.equal(new Set(found.map((item) => item.id)).size, 2);
});

// ---- lasting holds -------------------------------------------------------------------------

const oom = { profile: "photos", reason: "core_oom_killed", detail: "killed at 2026-11-04", since_epoch: NOW - 600, clears_on: "run_now_or_settings_change" };
const lastingItems = (raw) => items(raw).filter((item) => item.id.startsWith("lasting-"));

test("a lasting OOM hold is an amber item with the cause, the detail and how it clears", () => {
  const found = lastingItems(block({ lasting: [oom] }));
  assert.equal(found.length, 1);
  assert.equal(found[0].level, "warn");
  assert.equal(found[0].title, "photos: not synced until you fix this");
  assert.ok(found[0].detail.startsWith(reasonCopy("core_oom_killed").text + " killed at 2026-11-04 "), found[0].detail);
  assert.ok(found[0].detail.endsWith("It runs again when you click Run now, or when you change this profile's settings or the performance level."), found[0].detail);
});

test("an unreadable lasting record is held, says why, and tells the operator how to clear it", () => {
  const found = lastingItems(block({ lasting: [{ profile: "photos", reason: "lasting_state_unreadable", error: "the record is not a private package file", clears_on: "run_now" }] }));
  assert.equal(found.length, 1);
  assert.equal(found[0].level, "warn");
  assert.equal(found[0].title, "photos: not synced until you fix this");
  assert.equal(found[0].detail, "The record that holds this profile's automatic runs could not be read (the record is not a private package file); automatic runs stay held. Click Run now to clear it.");
  assert.ok(reasonCopy("lasting_state_unreadable").known);
});

test("a lasting reason this UI does not know is amber and named, never dropped", () => {
  const found = lastingItems(block({ lasting: [{ profile: "photos", reason: "disk_on_fire", detail: "d" }] }));
  assert.equal(found.length, 1);
  assert.equal(found[0].level, "warn");
  assert.match(found[0].detail, /disk_on_fire/);
});

test("lasting entries survive without a profile name, and one profile may be held and indexed at once", () => {
  assert.equal(lastingItems(block({ lasting: [{ reason: "core_oom_killed", detail: "d" }, "junk"] })).length, 2);
  const raw = block({ lasting: [oom] }, { index: { ...INDEX, issue: "pair-index-corrupt" } });
  assert.equal(lastingItems(raw).length, 1);
  assert.equal(indexItems(raw).length, 1);
});

test("a missing lasting list is empty; a present non-list is a malformed outcomes document", () => {
  assert.deepEqual(lastingItems(block()), []);
  assert.deepEqual(lastingItems(block({ lasting: [] })), []);
  assert.ok(normalizeOutcomes(block()), "an older backend without the member still reads");
  for (const bad of ["held", {}, null, 3]) {
    assert.match(outcomesProblem(block({ lasting: bad })), /lasting/);
    assert.equal(normalizeOutcomes(block({ lasting: bad })), null);
  }
});

// ---- service engine -------------------------------------------------------------------------

const engine = (over) => ({ active: "daemon", file: "absent", detail: null, ...over });

test("About names the active service engine; the shell is called out as the fallback", async () => {
  const component = await loadApp();
  const label = (outcomes) => appContext(component, { snapshot: { schema: "sdsync.dsm-api.v1", outcomes } }).serviceEngineLabel;
  assert.equal(label(block({ engine: engine() })), "daemon");
  assert.equal(label(block({ engine: engine({ active: "shell", file: "shell" }) })), "shell (fallback)");
  assert.equal(label(block({ engine: engine({ active: "daemon", file: "invalid", detail: "x" }) })), "daemon");
  assert.match(label(block({ engine: engine({ active: "wasm" }) })), /unknown \(wasm\)/);
  assert.equal(label(block()), "", "an older backend reports no engine: nothing is shown");
  assert.equal(label(undefined), "");
});

test("the About page template carries the engine line only when the backend reports one", async () => {
  const { appSource } = await import("./helpers/load-app.mjs");
  assert.match(appSource, /v-if="serviceEngineLabel"[^\n]*Service engine[^\n]*\{\{ serviceEngineLabel \}\}/);
});

test("an engine setting that is not understood is an amber item with the detail", () => {
  const found = items(block({ engine: engine({ file: "invalid", detail: "state/engine says 'turbo'" }) })).filter((item) => item.id === "engine-invalid");
  assert.equal(found.length, 1);
  assert.equal(found[0].level, "warn");
  assert.equal(found[0].title, "The service engine setting is not understood; the daemon runs");
  assert.match(found[0].detail, /state\/engine says 'turbo'/);
});

test("a valid or absent engine setting and an older backend add no item", () => {
  for (const raw of [block(), block({ engine: engine() }), block({ engine: engine({ active: "shell", file: "shell" }) }), block({ engine: engine({ file: "daemon" }) })]) {
    assert.deepEqual(items(raw).filter((item) => /engine/.test(item.id)), []);
  }
});

test("an engine file value this UI does not know is amber and named, and a non-document engine is malformed", () => {
  const found = items(block({ engine: engine({ file: "quantum" }) })).filter((item) => item.id === "engine-unknown");
  assert.equal(found.length, 1);
  assert.equal(found[0].level, "warn");
  assert.match(found[0].detail, /quantum/);
  for (const bad of ["daemon", [], null]) assert.match(outcomesProblem(block({ engine: bad })), /engine/);
});

// ---- reason vocabulary the daemon writes ---------------------------------------------------

test("the daemon's reasons read as copy with a cause and a next step", () => {
  const expect = {
    run_stalled: /no progress for 30 minutes and was stopped.*Logs.*reachable/s,
    launch_failed: /could not be started.*Logs.*repair the package/s,
    state_unwritable: /not started because its record could not be written.*free space on the package volume/s,
    consumer_timeout: /dashboard request did not finish within its deadline.*try again.*Logs/is,
    lasting_state_unreadable: /automatic runs.*held|could not be read/s
  };
  for (const [token, words] of Object.entries(expect)) {
    const copy = reasonCopy(token);
    assert.ok(copy.known, token);
    assert.match(copy.text, words, token);
  }
  for (const token of ["core_killed", "core_oom_killed"]) assert.ok(reasonCopy(token).known, token);
});

// ---- saves that commit beside a running sync -----------------------------------------------
//
// sdsync-dsm answers a save made while the daemon is syncing with the warning code
// `applies_next_run` (api_mutation_result). Only operations in MUTATION_WARNING_OPERATIONS pass
// their warnings to the window, so a save that can carry it and is not listed would read as
// "applied". The carriers below are the queued operations that go through the manager's
// api_capture_or_error in sdsync-dsm (QUEUED_OPERATION_IDS in src/dsm_api.rs minus the reads,
// probes, actions and log/preference writes, which never print the note).
const CARRIERS = ["configure-profile", "remove-profile", "set-default", "set-secret", "schedule", "routine", "remove-routine", "alert-policy", "security-policy"];
const NEVER_CARRIERS = ["test-profile-auth", "browse-remote", "client-event", "action", "sync-status", "resync", "clear-logs"];
const NEXT_RUN = "this change takes effect on the next run; the run in progress keeps the settings it started with";

globalThis.window ??= globalThis;
globalThis.document ??= { hidden: false };
const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
const api = await import(`data:text/javascript;base64,${Buffer.from(apiSource).toString("base64")}#next-run`);

const jsonResponse = (model, status = 200) => ({
  redirected: false, status, ok: status >= 200 && status < 300,
  headers: { get: (name) => (name.toLowerCase() === "content-type" ? "application/json" : null) },
  text: () => Promise.resolve(JSON.stringify(model))
});

function payloadFor(operation) {
  if (operation === "routine") {
    return Object.fromEntries(["action", "allow_delete", "depends_on", "enabled", "max_total_delete", "mode", "profile", "retry_backoff_seconds", "retry_count", "retry_exponential", "interval_seconds"].map((key) => [key, key === "mode" ? "interval" : ""]));
  }
  return Object.fromEntries(api.ARGUMENT_KEYS[operation].map((key) => [key, ""]));
}

async function save(operation, result, seen) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  const jobId = "d".repeat(48);
  let requestId = "";
  globalThis.window = { AbortController, TextEncoder, crypto: globalThis.crypto || webcrypto, setTimeout, clearTimeout };
  globalThis.fetch = (url, options) => {
    if (options.method === "POST") {
      requestId = JSON.parse(options.body).request_id;
      return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-queued.v1", ok: true, state: "queued", request_id: requestId, job_id: jobId }, 202));
    }
    return Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-result-status.v1", ok: true, state: "complete", job_id: jobId, client_request_id: requestId,
      result: { schema: "sdsync.dsm-result.v1", ok: true, message: "Saved", ...result }
    }));
  };
  try {
    return await api.apiPost({ onMutationWarning: (warnings) => seen.push(warnings) }, "csrf-token", operation, payloadFor(operation));
  } finally {
    globalThis.window = previous.window;
    globalThis.fetch = previous.fetch;
  }
}

test("the operations whose warnings reach the window are exactly the saves that can carry applies_next_run", () => {
  const listed = apiSource.match(/const MUTATION_WARNING_OPERATIONS = Object\.freeze\(\[([^\]]*)\]/)[1].match(/"[a-z-]+"/g).map((name) => name.slice(1, -1));
  assert.deepEqual([...listed].sort(), [...CARRIERS].sort(), "the list holds the carriers and no dead name");
  for (const operation of listed) assert.ok(api.ARGUMENT_KEYS[operation], `${operation} is not an operation this window can send`);
  for (const operation of CARRIERS) assert.ok(listed.includes(operation), `${operation} can commit during a sync but its warning would be dropped`);
  for (const operation of NEVER_CARRIERS) assert.ok(!listed.includes(operation), `${operation} never carries the note, and listing it would clear a standing banner`);
});

for (const operation of CARRIERS) {
  test(`a ${operation} save made during a sync reaches the window as an applies_next_run notice`, async () => {
    const seen = [];
    const result = await save(operation, { warnings: [{ code: "applies_next_run", message: NEXT_RUN }] }, seen);
    assert.equal(result.ok, true, "the save itself still succeeds");
    assert.deepEqual(seen, [[{ code: "applies_next_run", text: NEXT_RUN }]]);
  });

  test(`a ${operation} save with no warning stays a plain Saved and clears an older notice`, async () => {
    const seen = [];
    await save(operation, {}, seen);
    assert.deepEqual(seen, [null]);
  });
}

test("the notice is headed by what it means, and the generic heading is kept for real problems", async () => {
  const component = await loadApp();
  const next = [{ code: "applies_next_run", text: NEXT_RUN }];
  const ctx = appContext(component, { disposed: false, mutationWarning: null });
  ctx.noteMutationWarning(next);
  assert.equal(ctx.mutationWarningTitle, "Saved, takes effect on the next run");
  assert.deepEqual(ctx.toasts.map((toast) => [toast.title, toast.message]), [["Saved, takes effect on the next run", NEXT_RUN]]);

  const reload = [{ code: "controller_reload_failed", text: "saved but not reloaded" }];
  const mixed = appContext(component, { disposed: false, mutationWarning: null });
  mixed.noteMutationWarning([...next, ...reload]);
  assert.equal(mixed.mutationWarningTitle, "Saved, but it is not fully in effect");
  assert.match(mixed.toasts[0].title, /not fully in effect/);
  const other = appContext(component, { disposed: false, mutationWarning: null });
  other.noteMutationWarning(reload);
  assert.equal(other.mutationWarningTitle, "Saved, but it is not fully in effect");

  const clean = appContext(component, { disposed: false, mutationWarning: next });
  clean.noteMutationWarning(null);
  assert.equal(clean.mutationWarning, null);
  assert.deepEqual(clean.toasts, [], "a clean save raises no warning toast");
  assert.match(appSource, /<strong>\{\{ mutationWarningTitle \}\}<\/strong>/);
});

test("the damaged-index item says what it means for the sync and what to do, with the redb text kept back", () => {
  const found = indexItems(withIndex({ issue: "pair-index-corrupt", issue_detail: "...cache/pairs/office.redb: DB corrupted: Database page size 0 does not match expected 4096; set aside as /var" }));
  assert.equal(found.length, 1);
  assert.match(found[0].detail, /still compared in full/);
  assert.match(found[0].detail, /Storage Manager/);
  assert.doesNotMatch(found[0].detail, /page size/);
  assert.match(found[0].raw, /Database page size 0 does not match expected 4096/);
});

test("a failed run that kept its index evidence is a note on the index line, not an item", () => {
  const line = indexLine(normalizeOutcomes(withIndex({ state: "warm", issue: "pair-index-failed-run" })).profiles[0].index);
  assert.match(line, /the last run failed; the checksums it proved were kept/);
});
