import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

// Harness copied from autosave-api-limits.test.mjs rather than extracted into a
// shared fixture. This file is new, and a self-contained copy cannot collide
// with concurrent edits to the existing suite -- the same reason
// autosave-hold-release.test.mjs gives for doing the same thing.

const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function jsonResponse(model, status = 200, textPromise = null) {
  return {
    redirected: false,
    status,
    ok: status >= 200 && status < 300,
    headers: { get: (name) => name.toLowerCase() === "content-type" ? "application/json" : null },
    text() { return textPromise || Promise.resolve(JSON.stringify(model)); }
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

  hasTimerIn(milliseconds) {
    return [...this.timers.values()].some((timer) => timer.dueAt === this.time + milliseconds);
  }

  async settleUntil(predicate, label) {
    for (let attempt = 0; attempt < 100; attempt += 1) {
      if (predicate()) return;
      await Promise.resolve();
    }
    assert.fail(`${label} was not armed after 100 microtasks`);
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

function installBrowser(packageFetch, tokenFetch = undefined, timers = undefined) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = {
    AbortController: globalThis.AbortController,
    TextEncoder: globalThis.TextEncoder,
    crypto: globalThis.crypto || webcrypto,
    fetch: tokenFetch,
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

// A save that queues behind a running sync is the ordinary case, and the
// browser's thirty seconds of patience is not evidence about the package. When
// the last trusted read said `pending`, the deadline must report "still queued"
// rather than "outcome unknown": the first tells an operator to wait, the second
// tells them to stop and inspect a change that is going to apply.
test("an observation deadline after a trusted pending read is still-queued, not outcome-unknown", async () => {
  const clock = new FakeClock();
  const jobId = "e".repeat(48);
  let posts = 0;
  let pendingReads = 0;
  const restore = installBrowser((_url, options) => {
    if (options.method === "POST") {
      posts += 1;
      const request = JSON.parse(options.body);
      return Promise.resolve(jsonResponse({
        schema: "sdsync.dsm-queued.v1",
        ok: true,
        state: "queued",
        request_id: request.request_id,
        job_id: jobId
      }, 202));
    }
    pendingReads += 1;
    return Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-result-status.v1",
      job_id: jobId,
      state: "pending"
    }));
  }, undefined, clock);
  try {
    const api = await loadApi();
    const pending = api.apiPost(
      {},
      "csrf-token",
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      true,
      5,
      limits(api, clock, { resultObservationTimeoutMs: 40, resultRequestTimeoutMs: 20 })
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.QueuedStillPendingError, true);
      assert.equal(error.stillPending, true);
      // The two properties that make this different from outcome-unknown.
      assert.equal(error.outcomeUnknown, false);
      assert.equal(error.accepted, true);
      assert.equal(error.jobId, jobId);
      assert.equal(error.operation, api.ACTIONS.clientEvent);
      assert.match(error.message, /still working through its queue/i);
      assert.doesNotMatch(error.message, /inspect Activity and Logs/i);
      return true;
    });

    await clock.settleUntil(() => pendingReads >= 1, "a first trusted pending read");
    await clock.advance(200);
    await rejected;

    assert.equal(posts, 1, "a job that is merely still queued must never be sent a second time");
    assert.ok(pendingReads >= 1, "the verdict must rest on at least one trusted pending read");
  } finally {
    restore();
  }
});
