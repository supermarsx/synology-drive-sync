import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

// Harness copied from queued-still-pending.test.mjs rather than extracted into a
// shared fixture, for the reason that file gives for copying it in turn: a new
// self-contained copy cannot collide with concurrent edits to the existing
// suite.

const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function jsonResponse(model, status = 200) {
  return {
    redirected: false,
    status,
    ok: status >= 200 && status < 300,
    headers: { get: (name) => name.toLowerCase() === "content-type" ? "application/json" : null },
    text() { return Promise.resolve(JSON.stringify(model)); }
  };
}

class FakeClock {
  constructor() {
    this.time = 0;
    this.sequence = 0;
    this.timers = new Map();
  }

  setTimeout = (callback, delay) => {
    const id = ++this.sequence;
    this.timers.set(id, { id, callback, dueAt: this.time + Number(delay) });
    return id;
  };

  clearTimeout = (id) => { this.timers.delete(id); };

  async settle() {
    await new Promise((resolve) => setImmediate(resolve));
  }

  async settleUntil(predicate, label) {
    for (let attempt = 0; attempt < 100; attempt += 1) {
      if (predicate()) return;
      await Promise.resolve();
    }
    assert.fail(`${label} was not reached after 100 microtasks`);
  }

  async advance(milliseconds) {
    const target = this.time + milliseconds;
    for (;;) {
      const next = [...this.timers.values()]
        .filter((timer) => timer.dueAt <= target)
        .sort((left, right) => left.dueAt - right.dueAt || left.id - right.id)[0];
      if (!next) break;
      this.time = next.dueAt;
      this.timers.delete(next.id);
      next.callback();
      await this.settle();
    }
    this.time = target;
    await this.settle();
  }
}

function installBrowser(packageFetch, timers = undefined) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = {
    AbortController: globalThis.AbortController,
    TextEncoder: globalThis.TextEncoder,
    crypto: globalThis.crypto || webcrypto,
    fetch: undefined,
    setTimeout: timers ? timers.setTimeout : globalThis.setTimeout,
    clearTimeout: timers ? timers.clearTimeout : globalThis.clearTimeout,
    now: timers ? () => timers.time : undefined
  };
  globalThis.fetch = packageFetch;
  return () => {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  };
}

function limits(api, clock, overrides = {}) {
  return {
    ...api.AUTOSAVE_API_LIMITS,
    ...overrides,
    setTimeout: clock.setTimeout,
    clearTimeout: clock.clearTimeout,
    now: () => clock.time
  };
}

const JOB_ID = "e".repeat(48);

function countedRecord(overrides = {}) {
  return {
    step: 5,
    total: 7,
    label: "Comparing file contents",
    updated_at: 1757548800,
    unit: "files",
    count: 12480,
    ...overrides
  };
}

// The two accepted shapes, and nothing between or beyond them. The exact-key
// enumeration is the property that lets a label be rendered at all: every string
// that reaches an operator's screen is resolved from the catalogue against a
// document whose field set was decided here, not by whatever wrote the record.
test("the progress validator accepts exactly the four-key and six-key shapes", async () => {
  const api = await loadApi();

  // The record as it originally shipped, normalised to the counted shape so that
  // one rendering path serves both wire forms.
  assert.deepEqual(
    api.trustedRequestProgress({
      step: 2, total: 7, label: "Connecting to DSM", updated_at: 1757548800
    }),
    { step: 2, total: 7, label: "Connecting to DSM", updatedAt: 1757548800, unit: "", count: 0 }
  );

  assert.deepEqual(api.trustedRequestProgress(countedRecord()), {
    step: 5,
    total: 7,
    label: "Comparing file contents",
    updatedAt: 1757548800,
    unit: "files",
    count: 12480
  });

  // A determinate phase: no counter, and no fabricated denominator standing in
  // for one.
  assert.deepEqual(
    api.trustedRequestProgress(countedRecord({ step: 2, label: "Connecting to DSM", unit: "", count: 0 })),
    { step: 2, total: 7, label: "Connecting to DSM", updatedAt: 1757548800, unit: "", count: 0 }
  );
});

test("a seventh key fails the whole record closed rather than being ignored", async () => {
  const api = await loadApi();

  // The failure this pins is the one that cannot be seen: a tolerated-extras
  // check would render the six reviewed fields and silently carry an unreviewed
  // seventh into whatever reads the record next.
  assert.equal(api.trustedRequestProgress(countedRecord({ detail: "arbitrary job text" })), null);
  assert.equal(api.trustedRequestProgress(countedRecord({ __proto__: null, extra: 1 })), null);

  // Five keys is neither shape. Accepting it would mean the enumeration had
  // quietly become "at least the four required fields".
  const fiveKey = countedRecord();
  delete fiveKey.count;
  assert.equal(api.trustedRequestProgress(fiveKey), null);

  const alsoFive = countedRecord();
  delete alsoFive.unit;
  assert.equal(api.trustedRequestProgress(alsoFive), null);
});

test("unit is a catalogue enumeration, never free text from the job", async () => {
  const api = await loadApi();

  for (const unit of ["", "bytes", "entries", "files"]) {
    assert.ok(
      api.trustedRequestProgress(countedRecord({ unit, count: unit ? 1 : 0 })),
      `${unit || "(empty)"} is a catalogue unit and must be accepted`
    );
  }

  // Each of these is a plausible thing a writer might emit, and each would be a
  // job-controlled string arriving on screen. `unit` carries the same allow-list
  // property `label` does, and for the same reason.
  for (const unit of [
    "FILES",
    "file",
    "objects",
    " files",
    "files ",
    "<script>alert(1)</script>",
    "bytes; rm -rf /",
    "‮files"
  ]) {
    assert.equal(
      api.trustedRequestProgress(countedRecord({ unit })),
      null,
      `${JSON.stringify(unit)} is not a catalogue unit and must be refused`
    );
  }

  for (const unit of [null, 0, 1, true, ["files"], { unit: "files" }]) {
    assert.equal(api.trustedRequestProgress(countedRecord({ unit })), null);
  }
});

test("count is a bounded non-negative integer", async () => {
  const api = await loadApi();

  assert.ok(api.trustedRequestProgress(countedRecord({ count: 0 })));
  assert.ok(api.trustedRequestProgress(countedRecord({ count: 1000000000 })));

  for (const count of [-1, 1.5, 1000000001, Number.NaN, Infinity, "12480", null, true, []]) {
    assert.equal(
      api.trustedRequestProgress(countedRecord({ count })),
      null,
      `${JSON.stringify(count)} must be refused as a running count`
    );
  }
});

// The invariants the four-key form already enforced. The six-key extension is
// additive, so none of them may have loosened on the way.
test("the phase counter invariants survive the six-key extension", async () => {
  const api = await loadApi();

  assert.equal(api.trustedRequestProgress(countedRecord({ step: 0 })), null);
  assert.equal(api.trustedRequestProgress(countedRecord({ total: 0 })), null);
  assert.equal(api.trustedRequestProgress(countedRecord({ step: 8, total: 7 })), null);
  assert.equal(api.trustedRequestProgress(countedRecord({ step: 1.5 })), null);
  assert.equal(api.trustedRequestProgress(countedRecord({ updated_at: 0 })), null);
  assert.equal(api.trustedRequestProgress(countedRecord({ label: "" })), null);
  assert.equal(api.trustedRequestProgress(countedRecord({ label: "x".repeat(129) })), null);
  assert.ok(api.trustedRequestProgress(countedRecord({ label: "x".repeat(128) })));
  assert.equal(api.trustedRequestProgress(null), null);
  assert.equal(api.trustedRequestProgress([countedRecord()]), null);
  assert.equal(api.trustedRequestProgress("Step 5 of 7"), null);
});

function terminalResultFetch(result) {
  let dispatched = "";
  return (_url, options) => {
    if (options && options.method === "POST") {
      const request = JSON.parse(options.body);
      dispatched = request.request_id;
      return Promise.resolve(jsonResponse({
        schema: "sdsync.dsm-queued.v1",
        ok: true,
        state: "queued",
        request_id: request.request_id,
        job_id: JOB_ID
      }, 202));
    }
    return Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-result-status.v1",
      job_id: JOB_ID,
      client_request_id: dispatched,
      state: "complete",
      result: { schema: "sdsync.dsm-result.v1", ok: false, ...result }
    }));
  };
}

function pendingResultFetch(progress, onPost = () => {}) {
  return (_url, options) => {
    if (options && options.method === "POST") {
      onPost();
      const request = JSON.parse(options.body);
      return Promise.resolve(jsonResponse({
        schema: "sdsync.dsm-queued.v1",
        ok: true,
        state: "queued",
        request_id: request.request_id,
        job_id: JOB_ID
      }, 202));
    }
    const document = { schema: "sdsync.dsm-result-status.v1", job_id: JOB_ID, state: "pending" };
    if (progress !== undefined) document.progress = progress;
    return Promise.resolve(jsonResponse(document));
  };
}

async function queuedPendingError(api, clock, progress, options = {}) {
  const pending = api.apiPost(
    {},
    "csrf-token",
    api.ACTIONS.clientEvent,
    { event: "interface-settings" },
    true,
    5,
    limits(api, clock, { resultObservationTimeoutMs: 40, resultRequestTimeoutMs: 20 }),
    options.onProgress || null
  );
  let captured = null;
  const rejected = assert.rejects(pending, (error) => {
    captured = error;
    return error instanceof api.QueuedStillPendingError;
  });
  await clock.settleUntil(() => options.reads() >= 1, "a first trusted pending read");
  await clock.advance(200);
  await rejected;
  return captured;
}

// The finding this exists to close. `result` is the endpoint the live poll
// actually uses, and its progress field was read raw: the document is checked
// for its schema and job ID but never for an exact key set, so nothing at all
// enumerated what could arrive in that field. It was inert only because no
// writer existed. The moment the bridge publishes progress on `result` it
// becomes a live path for job-controlled text into the error a person reads.
test("progress read from the result endpoint goes through the same validator", async () => {
  const clock = new FakeClock();
  let reads = 0;
  const hostile = {
    step: 5,
    total: 7,
    label: "Comparing file contents",
    updated_at: 1757548800,
    unit: "files; and then something the operator should never read",
    count: 12480,
    detail: "an unreviewed seventh key"
  };
  const restore = installBrowser(pendingResultFetch(hostile), clock);
  try {
    const api = await loadApi();
    const error = await queuedPendingError(api, clock, hostile, {
      reads: () => reads,
      onProgress: () => { reads += 1; }
    });
    // Not the raw document, and not null either: null would mean the package
    // published nothing, and this package published something that did not
    // validate. The difference is what the operator is told.
    assert.equal(error.progress, api.PROGRESS_UNAVAILABLE);
    assert.equal(error.progress.unavailable, true);
    assert.equal(error.progress.unit, undefined);
    assert.equal(error.progress.detail, undefined);
    assert.doesNotMatch(JSON.stringify(error.progress), /operator should never read/);
  } finally {
    restore();
  }
});

test("a valid result-endpoint record reaches the still-pending error validated", async () => {
  const clock = new FakeClock();
  let reads = 0;
  const restore = installBrowser(pendingResultFetch(countedRecord()), clock);
  try {
    const api = await loadApi();
    const error = await queuedPendingError(api, clock, countedRecord(), {
      reads: () => reads,
      onProgress: () => { reads += 1; }
    });
    assert.deepEqual(error.progress, {
      step: 5,
      total: 7,
      label: "Comparing file contents",
      updatedAt: 1757548800,
      unit: "files",
      count: 12480
    });
  } finally {
    restore();
  }
});

test("a result document with no progress field reports no progress, not a failed one", async () => {
  const clock = new FakeClock();
  let reads = 0;
  const restore = installBrowser(pendingResultFetch(undefined), clock);
  try {
    const api = await loadApi();
    const error = await queuedPendingError(api, clock, undefined, {
      reads: () => reads,
      onProgress: () => { reads += 1; }
    });
    // The ordinary state of a fast job, and of every job until a writer exists.
    // Reporting it as "progress unavailable" would invent a fault.
    assert.equal(error.progress, null);
  } finally {
    restore();
  }
});

// Unbounded observation -- Doctor and the status walk -- has no observation
// object and no deadline, which is precisely why progress could never reach it.
// The sink is what makes a slow operation able to say what it is doing while it
// runs rather than only once the browser gives up.
test("every pending read publishes to the progress observer", async () => {
  const clock = new FakeClock();
  const published = [];
  let reads = 0;
  const restore = installBrowser(pendingResultFetch(countedRecord()), clock);
  try {
    const api = await loadApi();
    await queuedPendingError(api, clock, countedRecord(), {
      reads: () => reads,
      onProgress: (progress) => { reads += 1; published.push(progress); }
    });
    assert.ok(published.length >= 1, "a pending read must publish to the observer");
    assert.deepEqual(published[0], {
      step: 5,
      total: 7,
      label: "Comparing file contents",
      updatedAt: 1757548800,
      unit: "files",
      count: 12480
    });
  } finally {
    restore();
  }
});

test("a progress observer that is not a function is refused before anything is dispatched", async () => {
  const restore = installBrowser(() => assert.fail("no request may be dispatched"));
  try {
    const api = await loadApi();
    await assert.rejects(
      api.apiPost({}, "csrf", api.ACTIONS.clientEvent, { event: "interface-settings" },
        true, 5, undefined, "not a function"),
      TypeError
    );
  } finally {
    restore();
  }
});

// Each of these used to be an `unresolved` job: the controller deleted the
// request, wrote the reason to a log with no link back to it, and the dashboard
// said the outcome could not be established. A named code is the difference
// between a cause an operator can act on and a verdict they cannot.
test("a named terminal failure keeps its code, exit status and released barrier", async () => {
  const clock = new FakeClock();
  const restore = installBrowser(terminalResultFetch({
    code: "consumer_failed",
    exit_code: 137,
    message: "The worker running this operation exited before it finished."
  }), clock);
  try {
    const api = await loadApi();
    await assert.rejects(
      api.apiPost({}, "csrf-token", api.ACTIONS.clientEvent, { event: "interface-settings" },
        true, 5, limits(api, clock, { resultObservationTimeoutMs: 5000 })),
      (error) => {
        assert.equal(error instanceof api.QueuedConsumerFailedError, true);
        assert.equal(error.code, "consumer_failed");
        assert.equal(error.exitCode, 137);
        // The outcome is known: it failed, and we know which stage dropped it.
        assert.equal(error.outcomeUnknown, false);
        assert.equal(error.accepted, true);
        // Terminal and known, so it releases rather than holding a barrier over
        // a question that has already been answered.
        assert.equal(error.requiresInspection, false);
        return true;
      }
    );
  } finally {
    restore();
  }
});

test("a worker that exited zero without writing a result still needs inspection", async () => {
  const clock = new FakeClock();
  const restore = installBrowser(terminalResultFetch({
    code: "consumer_wrote_no_result",
    message: "The worker exited cleanly but wrote no result."
  }), clock);
  try {
    const api = await loadApi();
    await assert.rejects(
      api.apiPost({}, "csrf-token", api.ACTIONS.clientEvent, { event: "interface-settings" },
        true, 5, limits(api, clock, { resultObservationTimeoutMs: 5000 })),
      (error) => {
        assert.equal(error.code, "consumer_wrote_no_result");
        assert.equal(error.exitCode, null);
        // Naming the stage that dropped the job is not the same as knowing what
        // it left behind. This one ran; how far it got is exactly what nobody
        // can say, so the barrier stays and the authentication is retained.
        assert.equal(error.requiresInspection, true);
        assert.equal(error.outcomeUnknown, false);
        return true;
      }
    );
  } finally {
    restore();
  }
});

test("an unnamed failure code stays an ordinary package failure", async () => {
  const clock = new FakeClock();
  const restore = installBrowser(terminalResultFetch({
    code: "invalid_request",
    message: "The package rejected the request."
  }), clock);
  try {
    const api = await loadApi();
    await assert.rejects(
      api.apiPost({}, "csrf-token", api.ACTIONS.clientEvent, { event: "interface-settings" },
        true, 5, limits(api, clock, { resultObservationTimeoutMs: 5000 })),
      (error) => {
        assert.equal(error instanceof api.QueuedConsumerFailedError, false);
        assert.equal(error instanceof api.DsmApiError, true);
        assert.equal(error.code, "invalid_request");
        return true;
      }
    );
  } finally {
    restore();
  }
});
