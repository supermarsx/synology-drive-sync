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

function installBrowser(packageFetch, tokenFetch = undefined, timers = undefined) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = {
    AbortController: globalThis.AbortController,
    TextEncoder: globalThis.TextEncoder,
    crypto: globalThis.crypto || webcrypto,
    fetch: tokenFetch,
    setTimeout: timers ? timers.setTimeout : globalThis.setTimeout,
    clearTimeout: timers ? timers.clearTimeout : globalThis.clearTimeout
  };
  globalThis.fetch = packageFetch;
  return () => {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  };
}

test("default GET aborts a hung fetch with an ordinary typed read timeout", async () => {
  const clock = new FakeClock();
  let requestSignal;
  const restore = installBrowser((_url, options) => {
    requestSignal = options.signal;
    return new Promise(() => {});
  }, undefined, clock);
  try {
    const api = await loadApi();
    const pending = api.apiGet({}, "snapshot");
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.ClientRequestTimeoutError, true);
      assert.equal(error.clientTimeout, true);
      assert.equal(error.preAcceptance, true);
      assert.equal(error.stage, "read_observation");
      assert.equal(error.outcomeUnknown, undefined);
      assert.match(error.message, /client read limit/i);
      return true;
    });

    await clock.settleUntil(() => clock.hasTimerIn(10000), "default GET fetch timeout");
    await clock.advance(10000);
    await rejected;
    assert.equal(requestSignal instanceof AbortSignal && requestSignal.aborted, true);
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("default GET aborts a hung response body with an ordinary typed read timeout", async () => {
  const clock = new FakeClock();
  let requestSignal;
  const restore = installBrowser((_url, options) => {
    requestSignal = options.signal;
    return Promise.resolve(jsonResponse({}, 200, new Promise(() => {})));
  }, undefined, clock);
  try {
    const api = await loadApi();
    const pending = api.apiGet({}, "activity", { lines: 100 });
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.ClientRequestTimeoutError, true);
      assert.equal(error.clientTimeout, true);
      assert.equal(error.stage, "read_observation");
      assert.equal(error.outcomeUnknown, undefined);
      return true;
    });

    await clock.settleUntil(() => clock.hasTimerIn(10000), "default GET body timeout");
    await clock.advance(10000);
    await rejected;
    assert.equal(requestSignal instanceof AbortSignal && requestSignal.aborted, true);
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("unbounded-terminal POST dispatch still times out, aborts, and exact-replays", async () => {
  const clock = new FakeClock();
  const bodies = [];
  const signals = [];
  const restore = installBrowser((_url, options) => {
    bodies.push(options.body);
    signals.push(options.signal);
    return new Promise(() => {});
  }, undefined, clock);
  try {
    const api = await loadApi();
    const pending = api.apiPost(
      {},
      "csrf-token",
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      false,
      0
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
      assert.equal(error.outcomeUnknown, true);
      assert.match(error.requestId, /^[0-9a-f]{32}$/);
      return true;
    });

    for (const [timeout, backoff] of [[15000, 250], [15000, 1000], [15000, null]]) {
      await clock.settleUntil(() => clock.hasTimerIn(timeout), "default POST attempt timeout");
      await clock.advance(timeout);
      if (backoff !== null) {
        await clock.settleUntil(() => clock.hasTimerIn(backoff), "default exact-replay backoff");
        await clock.advance(backoff);
      }
    }

    await rejected;
    assert.equal(bodies.length, 3);
    assert.deepEqual(bodies, [bodies[0], bodies[0], bodies[0]]);
    assert.equal(signals.every((signal) => signal instanceof AbortSignal && signal.aborted), true);
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("unbounded terminal observation aborts five hung result reads without an overall deadline", async () => {
  const clock = new FakeClock();
  const jobId = "d".repeat(48);
  const resultSignals = [];
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
    resultSignals.push(options.signal);
    return new Promise(() => {});
  }, undefined, clock);
  try {
    const api = await loadApi();
    const pending = api.apiPost(
      {},
      "csrf-token",
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      true,
      0
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.QueuedOutcomeUnknownError, true);
      assert.equal(error.outcomeUnknown, true);
      assert.equal(error.jobId, jobId);
      assert.match(error.message, /accepted.*cannot currently be observed/i);
      return true;
    });

    for (let attempt = 0; attempt < 5; attempt += 1) {
      await clock.settleUntil(
        () => resultSignals.length === attempt + 1 && clock.hasTimerIn(10000),
        `default result request timeout ${attempt + 1}`
      );
      assert.equal(clock.hasTimerIn(30000), false, "unbounded terminal polling must not arm an overall deadline");
      await clock.advance(10000);
    }

    await rejected;
    assert.equal(resultSignals.length, 5);
    assert.equal(resultSignals.every((signal) => signal instanceof AbortSignal && signal.aborted), true);
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

function limits(api, clock, overrides = {}) {
  return {
    ...api.AUTOSAVE_API_LIMITS,
    ...overrides,
    setTimeout: clock.setTimeout,
    clearTimeout: clock.clearTimeout
  };
}

test("autosave bounds exact replay of a hung dispatched POST as acceptance-unknown", async () => {
  const bodies = [];
  const signals = [];
  const restore = installBrowser((_url, options) => {
    bodies.push(options.body);
    signals.push(options.signal);
    return new Promise(() => {});
  });
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
    await clock.settleUntil(() => clock.hasTimerIn(250), "POST replay backoff");
    await clock.advance(250);
    await clock.settleUntil(() => clock.hasTimerIn(5), "replayed POST request timeout");
    await clock.advance(5);
    await clock.settleUntil(() => clock.hasTimerIn(1000), "final POST replay backoff");
    await clock.advance(1000);
    await clock.settleUntil(() => clock.hasTimerIn(5), "final replayed POST request timeout");
    await clock.advance(5);
    await rejected;
    assert.equal(bodies.length, 3);
    assert.deepEqual(bodies, [bodies[0], bodies[0], bodies[0]], "every ambiguous dispatch replay must use the exact serialized body");
    assert.match(JSON.parse(bodies[0]).request_id, /^[0-9a-f]{32}$/);
    assert.equal(signals.every((signal) => signal instanceof AbortSignal && signal.aborted), true,
      "every timed-out dispatch must abort its own fetch before replay");
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("autosave bounds exact replay of a hung POST response body as acceptance-unknown", async () => {
  const bodies = [];
  const signals = [];
  const restore = installBrowser((_url, options) => {
    bodies.push(options.body);
    signals.push(options.signal);
    return Promise.resolve(jsonResponse({}, 202, new Promise(() => {})));
  });
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
    await clock.settleUntil(() => clock.hasTimerIn(250), "POST response replay backoff");
    await clock.advance(250);
    await clock.settleUntil(() => clock.hasTimerIn(5), "replayed POST response timeout");
    await clock.advance(5);
    await clock.settleUntil(() => clock.hasTimerIn(1000), "final POST response replay backoff");
    await clock.advance(1000);
    await clock.settleUntil(() => clock.hasTimerIn(5), "final replayed POST response timeout");
    await clock.advance(5);
    await rejected;
    assert.equal(bodies.length, 3);
    assert.deepEqual(bodies, [bodies[0], bodies[0], bodies[0]], "every response-observation replay must use the exact serialized body");
    assert.equal(signals.every((signal) => signal instanceof AbortSignal && signal.aborted), true,
      "every timed-out response body must abort its fetch before replay");
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("lost 202 acknowledgement replays the exact request and observes the original job", async () => {
  const jobId = "9".repeat(48);
  const posts = [];
  const attemptSignals = [];
  let resultReads = 0;
  const restore = installBrowser(
    (_url, options) => {
      attemptSignals.push(options.signal);
      if (options.method === "POST") {
        posts.push({ body: options.body, headers: { ...options.headers } });
        const request = JSON.parse(options.body);
        if (posts.length === 1) {
          return Promise.resolve({
            redirected: false,
            status: 202,
            ok: true,
            headers: { get: () => "application/json" },
            async text() { throw new TypeError("socket closed after queue admission"); }
          });
        }
        return Promise.resolve(jsonResponse({
          schema: "sdsync.dsm-queued.v1",
          ok: true,
          state: "queued",
          replayed: true,
          request_id: request.request_id,
          job_id: jobId
        }, 202));
      }
      resultReads += 1;
      return Promise.resolve(jsonResponse({
        schema: "sdsync.dsm-result-status.v1",
        ok: true,
        state: "complete",
        job_id: jobId,
        result: { schema: "sdsync.dsm-result.v1", ok: true, event: "interface-settings" }
      }));
    },
    () => Promise.resolve(jsonResponse({ success: true, data: { synotoken: "pinned token" } }))
  );
  try {
    const api = await loadApi();
    const clock = new FakeClock();
    const controller = new AbortController();
    const pending = api.apiPost(
      { signal: controller.signal },
      "csrf-token",
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      true,
      5,
      limits(api, clock)
    );
    await clock.settleUntil(() => clock.hasTimerIn(250), "lost-ack replay backoff");
    await clock.advance(250);
    const result = await pending;
    assert.equal(result.ok, true);
    assert.equal(posts.length, 2);
    assert.equal(posts[1].body, posts[0].body);
    assert.deepEqual(posts[1].headers, posts[0].headers);
    assert.equal(posts[0].headers["X-SDSYNC-CSRF"], "csrf-token");
    assert.equal(posts[0].headers["X-SYNO-TOKEN"], "pinned%20token");
    assert.match(JSON.parse(posts[0].body).request_id, /^[0-9a-f]{32}$/);
    assert.equal(resultReads, 1);
    assert.equal(attemptSignals.length, 3);
    assert.equal(attemptSignals.every((signal) => !signal.aborted), true);
    controller.abort();
    await clock.settle();
    assert.equal(attemptSignals.every((signal) => !signal.aborted), true,
      "settled attempts must release their parent abort listeners");
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("cancellation during ambiguous dispatch backoff prevents replay and clears its timer", async () => {
  let posts = 0;
  const restore = installBrowser(() => {
    posts += 1;
    return Promise.reject(new TypeError("temporary dispatch failure"));
  });
  try {
    const api = await loadApi();
    const clock = new FakeClock();
    const controller = new AbortController();
    const pending = api.apiPost(
      { signal: controller.signal },
      "csrf-token",
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      false,
      0,
      limits(api, clock)
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
      assert.equal(error.acceptanceUnknown, true);
      return true;
    });
    await clock.settleUntil(() => clock.hasTimerIn(250), "ambiguous dispatch replay backoff");
    controller.abort();
    await clock.settle();
    await rejected;
    assert.equal(posts, 1);
    assert.equal(clock.timers.size, 0);
    await clock.advance(2000);
    assert.equal(posts, 1, "cancellation must prevent every later replay attempt");
  } finally {
    restore();
  }
});

test("AppWindow cancellation aborts the active dispatch attempt and prevents replay", async () => {
  let posts = 0;
  let requestSignal;
  const restore = installBrowser((_url, options) => {
    posts += 1;
    requestSignal = options.signal;
    return new Promise((_resolve, reject) => {
      options.signal.addEventListener("abort", () => reject(new Error("fetch aborted")), { once: true });
    });
  });
  try {
    const api = await loadApi();
    const clock = new FakeClock();
    const controller = new AbortController();
    const pending = api.apiPost(
      { signal: controller.signal },
      "csrf-token",
      api.ACTIONS.clientEvent,
      { event: "interface-settings" },
      false,
      0,
      limits(api, clock, { postRequestTimeoutMs: 100 })
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
      assert.equal(error.acceptanceUnknown, true);
      return true;
    });
    await clock.settleUntil(
      () => requestSignal instanceof AbortSignal && clock.hasTimerIn(100),
      "active linked POST attempt"
    );
    controller.abort();
    await clock.settle();
    await rejected;
    assert.equal(requestSignal.aborted, true);
    assert.equal(posts, 1);
    assert.equal(clock.timers.size, 0);
    await clock.advance(2000);
    assert.equal(posts, 1, "AppWindow cancellation must not leave a replay timer behind");
  } finally {
    restore();
  }
});

test("trusted rejection after an ambiguous dispatch remains acceptance-unknown", async () => {
  const bodies = [];
  const restore = installBrowser((_url, options) => {
    bodies.push(options.body);
    if (bodies.length === 1) {
      return Promise.resolve({
        redirected: false,
        status: 202,
        ok: true,
        headers: { get: () => "application/json" },
        async text() { throw new TypeError("lost queue acknowledgement"); }
      });
    }
    return Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-error.v1",
      ok: false,
      code: "csrf_rejected",
      message: "CSRF mutation token expired."
    }, 403));
  });
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
      limits(api, clock)
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
      assert.equal(error.acceptanceUnknown, true);
      assert.equal(error.preAcceptance, undefined);
      assert.equal(error.csrfRejected, undefined);
      assert.equal(error.requestId, JSON.parse(bodies[0]).request_id);
      return true;
    });
    await clock.settleUntil(() => clock.hasTimerIn(250), "rejection replay backoff");
    await clock.advance(250);
    await rejected;
    assert.equal(bodies.length, 2);
    assert.equal(bodies[1], bodies[0]);
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
  let reissueSignal;
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
        if (csrfReads === 2) {
          reissueSignal = options.signal;
          return reissue.promise;
        }
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
    assert.equal(reissueSignal instanceof AbortSignal && reissueSignal.aborted, true,
      "a bounded CSRF reissue timeout must abort the underlying read");
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

test("late default direct-CSRF settlement cannot suppress the next required reissue", async () => {
  const previousNow = Date.now;
  const clock = new FakeClock();
  let now = 2000000;
  let tokenReads = 0;
  let csrfReads = 0;
  let posts = 0;
  let postedCsrf = "";
  const replacements = [];
  const lateDirectRead = deferred();
  let lateDirectReadSignal;
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
        if (csrfReads === 2) {
          lateDirectReadSignal = options.signal;
          return lateDirectRead.promise;
        }
        return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-csrf.v1", csrf_token: "fresh-csrf" }));
      }
      return Promise.resolve(jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true }));
    },
    () => {
      tokenReads += 1;
      if (tokenReads === 1) return Promise.reject(new TypeError("temporary token bootstrap failure"));
      return Promise.resolve(jsonResponse({ success: true, data: { synotoken: "recovered-token" } }));
    },
    clock
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

    const pending = api.apiGet(auth, "csrf");
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.ClientRequestTimeoutError, true);
      assert.equal(error.clientTimeout, true);
      assert.equal(error.stage, "read_observation");
      return true;
    });
    await clock.settleUntil(() => clock.hasTimerIn(10000), "default direct CSRF read timeout");
    await clock.advance(10000);
    await rejected;
    assert.equal(lateDirectReadSignal instanceof AbortSignal && lateDirectReadSignal.aborted, true,
      "a default bounded direct read timeout must abort its fetch");

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

test("bounded result polling survives more than five transient observation failures", async () => {
  const jobId = "8".repeat(48);
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
    if (resultReads <= 6) return Promise.reject(new TypeError("temporary result transport failure"));
    return Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-result-status.v1",
      ok: true,
      state: "complete",
      job_id: jobId,
      result: { schema: "sdsync.dsm-result.v1", ok: true, recovered: true }
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
      limits(api, clock, { resultRequestTimeoutMs: 8, resultObservationTimeoutMs: 100 })
    );
    await clock.settleUntil(
      () => resultReads === 1 && clock.hasTimerIn(5) && clock.hasTimerIn(100),
      "transient result retry and overall observation deadline"
    );
    await clock.advance(30);
    const result = await pending;
    assert.equal(result.recovered, true);
    assert.equal(resultReads, 7);
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("autosave retries hung accepted result GETs until the overall observation budget", async () => {
  const jobId = "a".repeat(48);
  let resultReads = 0;
  const resultSignals = [];
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
    resultSignals.push(options.signal);
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
      assert.match(error.message, /terminal result observation exceeded the autosave limit/i);
      return true;
    });
    await clock.settleUntil(
      () => clock.hasTimerIn(5) && clock.hasTimerIn(30),
      "result request and observation timeouts"
    );
    await clock.advance(30);
    await rejected;
    assert.equal(resultReads, 3, "per-request timeouts must remain retryable until the overall budget");
    assert.equal(resultSignals.every((signal) => signal instanceof AbortSignal && signal.aborted), true,
      "every timed-out result observation must abort before retry or terminal return");
    await clock.advance(100);
    assert.equal(resultReads, 3, "the overall timeout must stop the result polling loop");
    assert.equal(clock.timers.size, 0);
  } finally {
    restore();
  }
});

test("overall result observation expiry aborts the active result fetch", async () => {
  const jobId = "e".repeat(48);
  const resultSignals = [];
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
    resultSignals.push(options.signal);
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
      limits(api, clock, { resultRequestTimeoutMs: 100, resultObservationTimeoutMs: 20 })
    );
    const rejected = assert.rejects(pending, (error) => {
      assert.equal(error instanceof api.QueuedOutcomeUnknownError, true);
      assert.match(error.message, /terminal result observation exceeded the autosave limit/i);
      return true;
    });
    await clock.settleUntil(
      () => resultReads === 1 && clock.hasTimerIn(100) && clock.hasTimerIn(20),
      "active result request and overall observation deadline"
    );
    await clock.advance(20);
    await rejected;
    assert.equal(resultReads, 1);
    assert.equal(resultSignals.length, 1);
    assert.equal(resultSignals[0] instanceof AbortSignal && resultSignals[0].aborted, true,
      "the outer observation deadline must abort the current result fetch");
    assert.equal(clock.timers.size, 0);
    await clock.advance(200);
    assert.equal(resultReads, 1, "outer expiry must leave no result retry behind");
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
