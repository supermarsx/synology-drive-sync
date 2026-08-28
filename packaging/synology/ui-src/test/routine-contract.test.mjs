import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

function jsonResponse(model) {
  return {
    redirected: false,
    status: 200,
    ok: true,
    headers: { get: (name) => name.toLowerCase() === "content-type" ? "application/json" : null },
    async text() { return JSON.stringify(model); }
  };
}

async function loadApi() {
  return import(`data:text/javascript;base64,${Buffer.from(apiSource).toString("base64")}#${Date.now()}-${Math.random()}`);
}

function common(mode) {
  return {
    action: "sync",
    allow_delete: false,
    depends_on: [],
    enabled: true,
    max_total_delete: 100,
    mode,
    profile: "nightly",
    retry_backoff_seconds: 60,
    retry_count: 5,
    retry_exponential: true
  };
}

test("routine requests contain only the timing fields used by their mode", async () => {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  const requests = [];
  globalThis.window = {
    crypto: globalThis.crypto || webcrypto,
    TextEncoder: globalThis.TextEncoder,
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    fetch: async () => jsonResponse({ success: true, data: { synotoken: "token" } })
  };
  globalThis.fetch = async (_url, options) => {
    const request = JSON.parse(options.body);
    requests.push(request.arguments);
    return jsonResponse({
      schema: "sdsync.dsm-queued.v1",
      ok: true,
      state: "queued",
      request_id: request.request_id,
      job_id: "a".repeat(48)
    });
  };
  try {
    const api = await loadApi();
    await api.apiPost({}, "csrf", api.ACTIONS.routine, {
      ...common("interval"), interval_seconds: 3600
    }, false);
    await api.apiPost({}, "csrf", api.ACTIONS.routine, {
      ...common("daily"), weekdays: [1, 2, 3, 4, 5],
      time_window_start: "01:30", time_window_end: "04:00"
    }, false);
    await api.apiPost({}, "csrf", api.ACTIONS.routine, {
      ...common("realtime"), debounce_seconds: 45, poll_seconds: 30
    }, false);

    assert.equal(requests.length, 3);
    assert.deepEqual(
      Object.keys(requests[0]).filter((key) => /interval|weekday|window|debounce|poll/.test(key)),
      ["interval_seconds"]
    );
    assert.deepEqual(
      Object.keys(requests[1]).filter((key) => /interval|weekday|window|debounce|poll/.test(key)).sort(),
      ["time_window_end", "time_window_start", "weekdays"]
    );
    assert.deepEqual(
      Object.keys(requests[2]).filter((key) => /interval|weekday|window|debounce|poll/.test(key)).sort(),
      ["debounce_seconds", "poll_seconds"]
    );
  } finally {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  }
});

test("routine request validation rejects missing and cross-mode timing fields before transport", async () => {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  let transports = 0;
  globalThis.window = {
    crypto: globalThis.crypto || webcrypto,
    TextEncoder: globalThis.TextEncoder,
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    fetch: async () => { transports += 1; return jsonResponse({}); }
  };
  globalThis.fetch = async () => { transports += 1; return jsonResponse({}); };
  try {
    const api = await loadApi();
    await assert.rejects(
      api.apiPost({}, "csrf", api.ACTIONS.routine, common("realtime"), false),
      /reviewed bridge contract/
    );
    await assert.rejects(
      api.apiPost({}, "csrf", api.ACTIONS.routine, {
        ...common("realtime"), debounce_seconds: 45, poll_seconds: 30, interval_seconds: 60
      }, false),
      /reviewed bridge contract/
    );
    assert.equal(transports, 0);
  } finally {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  }
});

test("routine deletion ceiling keeps the shared DSM portable bound", () => {
  assert.match(
    appSource,
    /between\(payload\.max_total_delete, 0, 2147483647\)/,
    "the AppWindow must reject a deletion ceiling that cannot round-trip on every DSM target"
  );
});
