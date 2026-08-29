import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

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

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
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

function installBrowser(packageFetch, tokenFetch = undefined) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = {
    AbortController: globalThis.AbortController,
    TextEncoder: globalThis.TextEncoder,
    crypto: globalThis.crypto || webcrypto,
    fetch: tokenFetch,
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout
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
    clearTimeout: clock.clearTimeout
  };
}

test("autosave bounds a hung dispatched POST as acceptance-unknown", async () => {
  const hung = deferred();
  const restore = installBrowser(() => hung.promise);
  try {
    const api = await loadApi();
    const clock = new FakeClock();
    const pending = api.apiPost(
      {},
      "csrf-token",
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      false,
      0,
      limits(api, clock, { postRequestTimeoutMs: 5 })
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
      assert.equal(error.outcomeUnknown, true);
      assert.equal(error.acceptanceUnknown, true);
      assert.match(error.requestId, /^[0-9a-f]{32}$/);
      return true;
    });
    await clock.settleUntil(() => clock.hasTimerIn(5), "POST request timeout");
    await clock.advance(5);
    await rejected;
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("autosave bounds a hung POST response body as acceptance-unknown", async () => {
  const body = deferred();
  const restore = installBrowser(() => Promise.resolve(jsonResponse({}, 202, body.promise)));
  try {
    const api = await loadApi();
    const clock = new FakeClock();
    const pending = api.apiPost(
      {},
      "csrf-token",
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      false,
      0,
      limits(api, clock, { postResponseTimeoutMs: 5 })
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
      assert.equal(error.outcomeUnknown, true);
      assert.equal(error.acceptanceUnknown, true);
      return true;
    });
    await clock.settleUntil(() => clock.hasTimerIn(5), "POST response timeout");
    await clock.advance(5);
    await rejected;
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("autosave CSRF reissue timeout is pre-dispatch and never outcome-unknown", async () => {
  const previousNow = Date.now;
  let now = 1000000;
  let tokenReads = 0;
  let csrfReads = 0;
  let posts = 0;
  let postedCsrf = "";
  const replacements = [];
  const reissue = deferred();
  Date.now = () => now;
  const restore = installBrowser(
    (url, options) => {
      if (options.method === "POST") {
        posts += 1;
        postedCsrf = options.headers["X-SDSYNC-CSRF"];
        const request = JSON.parse(options.body);
        return Promise.resolve(jsonResponse({
          schema: "sdsync.dsm-queued.v1",
          ok: true,
          state: "queued",
          request_id: request.request_id,
          job_id: "c".repeat(48)
        }, 202));
      }
      const action = new URL(url, "https://nas.example.invalid").searchParams.get("action");
      if (action === "csrf") {
        csrfReads += 1;
        if (csrfReads === 1) {
          return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-csrf.v1", csrf_token: "cookie-csrf" }));
        }
        if (csrfReads === 2) return reissue.promise;
        return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-csrf.v1", csrf_token: "current-csrf" }));
      }
      return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true }));
    },
    () => {
      tokenReads += 1;
      if (tokenReads === 1) return Promise.reject(new TypeError("temporary token bootstrap failure"));
      return Promise.resolve(jsonResponse({ success: true, data: { synotoken: "recovered-token" } }));
    }
  );
  try {
    const api = await loadApi();
    const auth = {
      onCsrfReissued(previousToken, replacementToken) {
        replacements.push([previousToken, replacementToken]);
      }
    };
    const initial = await api.apiGet(auth, "csrf");
    now += 30000;
    await api.apiGet(auth, "snapshot");

    const clock = new FakeClock();
    const pending = api.apiPost(
      auth,
      initial.csrf_token,
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      false,
      0,
      limits(api, clock, { csrfReissueTimeoutMs: 5 })
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.ClientRequestTimeoutError, true);
      assert.equal(error.clientTimeout, true);
      assert.equal(error.preAcceptance, true);
      assert.equal(error.stage, "csrf_reissue");
      assert.equal(error.outcomeUnknown, undefined);
      return true;
    });
    await clock.settleUntil(() => clock.hasTimerIn(5), "CSRF reissue timeout");
    await clock.advance(5);
    await rejected;
    assert.equal(csrfReads, 2);
    assert.equal(posts, 0, "a timed-out CSRF reissue must not dispatch a mutation");

    reissue.resolve(jsonResponse({ schema: "sdsync.dsm-csrf.v1", csrf_token: "late-csrf" }));
    await new Promise((resolve) => setImmediate(resolve));
    assert.deepEqual(replacements, [], "a detached late response must not publish a replacement token");

    const queued = await api.apiPost(
      auth,
      initial.csrf_token,
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      false
    );
    assert.equal(queued.state, "queued");
    assert.equal(csrfReads, 3, "the next attempt must reissue CSRF instead of trusting the detached response");
    assert.deepEqual(replacements, [["cookie-csrf", "current-csrf"]]);
    assert.equal(postedCsrf, "current-csrf");
    assert.equal(posts, 1);
  } finally {
    Date.now = previousNow;
    restore();
  }
});

test("late bounded direct-CSRF settlement cannot suppress the next required reissue", async () => {
  const previousNow = Date.now;
  let now = 2000000;
  let tokenReads = 0;
  let csrfReads = 0;
  let posts = 0;
  let postedCsrf = "";
  const replacements = [];
  const lateDirectRead = deferred();
  Date.now = () => now;
  const restore = installBrowser(
    (url, options) => {
      if (options.method === "POST") {
        posts += 1;
        postedCsrf = options.headers["X-SDSYNC-CSRF"];
        const request = JSON.parse(options.body);
        return Promise.resolve(jsonResponse({
          schema: "sdsync.dsm-queued.v1",
          ok: true,
          state: "queued",
          request_id: request.request_id,
          job_id: "d".repeat(48)
        }, 202));
      }
      const action = new URL(url, "https://nas.example.invalid").searchParams.get("action");
      if (action === "csrf") {
        csrfReads += 1;
        if (csrfReads === 1) {
          return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-csrf.v1", csrf_token: "cookie-csrf" }));
        }
        if (csrfReads === 2) return lateDirectRead.promise;
        return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-csrf.v1", csrf_token: "fresh-csrf" }));
      }
      return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true }));
    },
    () => {
      tokenReads += 1;
      if (tokenReads === 1) return Promise.reject(new TypeError("temporary token bootstrap failure"));
      return Promise.resolve(jsonResponse({ success: true, data: { synotoken: "recovered-token" } }));
    }
  );
  try {
    const api = await loadApi();
    const auth = {
      onCsrfReissued(previousToken, replacementToken) {
        replacements.push([previousToken, replacementToken]);
      }
    };
    const initial = await api.apiGet(auth, "csrf");
    now += 30000;
    await api.apiGet(auth, "snapshot");

    const clock = new FakeClock();
    const pending = api.apiGet(
      auth,
      "csrf",
      {},
      limits(api, clock, { readTimeoutMs: 5 })
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.ClientRequestTimeoutError, true);
      assert.equal(error.clientTimeout, true);
      assert.equal(error.stage, "read_observation");
      return true;
    });
    await clock.settleUntil(() => clock.hasTimerIn(5), "direct CSRF read timeout");
    await clock.advance(5);
    await rejected;

    lateDirectRead.resolve(jsonResponse({ schema: "sdsync.dsm-csrf.v1", csrf_token: "late-direct-csrf" }));
    await new Promise((resolve) => setImmediate(resolve));
    assert.deepEqual(replacements, [], "a detached direct read must not publish a replacement token");

    const queued = await api.apiPost(
      auth,
      initial.csrf_token,
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      false
    );
    assert.equal(queued.state, "queued");
    assert.equal(csrfReads, 3, "the stale direct response must not suppress the next CSRF reissue");
    assert.deepEqual(replacements, [["cookie-csrf", "fresh-csrf"]]);
    assert.equal(postedCsrf, "fresh-csrf");
    assert.equal(posts, 1);
  } finally {
    Date.now = previousNow;
    restore();
  }
});

test("autosave bounds a hung accepted result GET and stops observation", async () => {
  const jobId = "a".repeat(48);
  let resultReads = 0;
  const restore = installBrowser((_url, options) => {
    if (options.method === "POST") {
      const request = JSON.parse(options.body);
      return Promise.resolve(jsonResponse({
        schema: "sdsync.dsm-queued.v1",
        ok: true,
        state: "queued",
        request_id: request.request_id,
        job_id: jobId
      }, 202));
    }
    resultReads += 1;
    return new Promise(() => {});
  });
  try {
    const api = await loadApi();
    const clock = new FakeClock();
    const pending = api.apiPost(
      {},
      "csrf-token",
      api.ACTIONS.alertPolicy,
      { cooldown_seconds: 3600, enabled: true, failure_threshold: 1, on_failure: true, on_success: false },
      true,
      5,
      limits(api, clock, { resultRequestTimeoutMs: 5, resultObservationTimeoutMs: 30 })
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.QueuedOutcomeUnknownError, true);
      assert.equal(error.accepted, true);
      assert.equal(error.jobId, jobId);
      assert.match(error.message, /result request exceeded the autosave limit/i);
      return true;
    });
    await clock.settleUntil(
      () => clock.hasTimerIn(5) && clock.hasTimerIn(30),
      "result request and observation timeouts"
    );
    await clock.advance(5);
    await rejected;
    assert.equal(resultReads, 1);
    await clock.advance(100);
    assert.equal(resultReads, 1, "a timed-out result request must not leave a polling loop behind");
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("autosave bounds forever-pending accepted jobs and leaves no polling timer", async () => {
  const jobId = "b".repeat(48);
  let resultReads = 0;
  const restore = installBrowser((_url, options) => {
    if (options.method === "POST") {
      const request = JSON.parse(options.body);
      return Promise.resolve(jsonResponse({
        schema: "sdsync.dsm-queued.v1",
        ok: true,
        state: "queued",
        request_id: request.request_id,
        job_id: jobId
      }, 202));
    }
    resultReads += 1;
    return Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-result-status.v1",
      ok: true,
      state: "pending",
      job_id: jobId
    }));
  });
  try {
    const api = await loadApi();
    const clock = new FakeClock();
    const pending = api.apiPost(
      {},
      "csrf-token",
      api.ACTIONS.alertPolicy,
      { cooldown_seconds: 3600, enabled: true, failure_threshold: 1, on_failure: true, on_success: false },
      true,
      5,
      limits(api, clock, { resultRequestTimeoutMs: 8, resultObservationTimeoutMs: 20 })
    );
    let settled = false;
    pending.then(() => { settled = true; }, () => { settled = true; });
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.QueuedOutcomeUnknownError, true);
      assert.equal(error.accepted, true);
      assert.equal(error.jobId, jobId);
      assert.match(error.message, /observation exceeded the autosave limit/i);
      return true;
    });
    await clock.settleUntil(() => clock.hasTimerIn(20), "terminal observation timeout");
    await clock.settle();
    await clock.advance(19);
    assert.equal(settled, false);
    await clock.advance(1);
    await rejected;
    const readsAtTimeout = resultReads;
    await clock.advance(100);
    assert.equal(resultReads, readsAtTimeout, "overall timeout must stop the underlying result poll");
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});
