import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

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

function installBrowser(packageFetch) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = {
    AbortController: globalThis.AbortController,
    TextEncoder: globalThis.TextEncoder,
    crypto: globalThis.crypto || webcrypto,
    fetch: () => Promise.resolve(jsonResponse({ success: true, data: { synotoken: "probe-token" } })),
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

const REQUEST_ID = "e1c45b23c3ef99bb24dd72d610496118";
const JOB_ID = "b".repeat(48);
const OPERATION = "configure-profile";

function statusDocument(overrides = {}) {
  return {
    schema: "sdsync.dsm-request-status.v1",
    request_id: REQUEST_ID,
    job_id: JOB_ID,
    operation: OPERATION,
    state: "pending",
    ...overrides
  };
}

function progressDocument(overrides = {}) {
  return { step: 7, total: 16, label: "DSM session authentication", updated_at: 1757000000, ...overrides };
}

function activityEvent(overrides = {}) {
  return {
    epoch: 1757000000,
    code: "audit.requested",
    profile: "archive",
    state: "requested",
    category: "audit",
    level: "info",
    message: `Module ${OPERATION} requested [${JOB_ID}] request_id=${REQUEST_ID}`,
    client_request_id: REQUEST_ID,
    ...overrides
  };
}

// Routes every package read by its `action` query parameter.
function routed(handlers) {
  const seen = [];
  const fetcher = (url) => {
    const action = new URL(url, "https://nas.invalid").searchParams.get("action");
    seen.push(action);
    const handler = handlers[action];
    if (!handler) return Promise.reject(new TypeError(`unexpected read action ${action}`));
    return handler();
  };
  fetcher.seen = seen;
  return fetcher;
}

test("a pending job's published progress is trusted rather than treated as a corrupt document", async () => {
  const fetcher = routed({
    "request-status": () => Promise.resolve(jsonResponse(
      statusDocument({ progress: progressDocument() }),
      202
    ))
  });
  const restore = installBrowser(fetcher);
  try {
    const api = await loadApi();
    const observed = await api.probeRequestOutcome({}, REQUEST_ID, OPERATION);
    assert.equal(observed.schema, api.REQUEST_PROBE_SCHEMA);
    assert.equal(observed.verdict, "accepted");
    assert.equal(observed.job_id, JOB_ID);
    assert.deepEqual(observed.progress, {
      step: 7,
      total: 16,
      label: "DSM session authentication",
      updatedAt: 1757000000
    });
    // The queue answered, so Activity is never consulted.
    assert.deepEqual(fetcher.seen, ["request-status"]);
  } finally {
    restore();
  }
});

// The regression this exists for: an exact five-key match rejected the six-key
// pending-with-progress document, and pollRequestStatus treats an untrusted
// document as fatal. Reconciliation of a long Doctor run therefore failed on the
// very first read without consuming any of its recovery window.
test("reconciliation survives a progress-bearing pending document and settles on the result", async () => {
  let statusReads = 0;
  const restore = installBrowser(routed({
    "request-status": () => {
      statusReads += 1;
      // The queue has not published the record yet, so the loop must go round
      // at least once and then trust the progress-bearing document it meets.
      if (statusReads === 1) {
        return Promise.resolve(jsonResponse({
          schema: "sdsync.dsm-request-status.v1",
          request_id: REQUEST_ID,
          state: "unresolved"
        }, 202));
      }
      return Promise.resolve(jsonResponse(statusDocument({ progress: progressDocument() }), 202));
    },
    result: () => Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-result-status.v1",
      state: "complete",
      job_id: JOB_ID,
      client_request_id: REQUEST_ID,
      result: { schema: "sdsync.dsm-result.v1", ok: true }
    }))
  }));
  try {
    const api = await loadApi();
    const reconciled = await api.reconcileMutationRequest({}, REQUEST_ID, OPERATION, 1, {
      ...api.AUTOSAVE_API_LIMITS,
      requestReconciliationPollIntervalMs: 1
    });
    assert.equal(reconciled.schema, "sdsync.dsm-reconciled-result.v1");
    assert.equal(reconciled.job_id, JOB_ID);
    assert.equal(reconciled.result.ok, true);
    assert.equal(statusReads, 2, "the progress-bearing document must have been trusted, not fatal");
  } finally {
    restore();
  }
});

test("an unreviewed extra key, or progress on a completed job, still fails closed", async () => {
  for (const document of [
    statusDocument({ progress: progressDocument(), surprise: "unreviewed" }),
    statusDocument({ state: "complete", progress: progressDocument() }),
    statusDocument({ progress: progressDocument({ step: 99 }) }),
    statusDocument({ progress: progressDocument({ label: "" }) }),
    statusDocument({ progress: { step: 1, total: 2 } })
  ]) {
    const restore = installBrowser(routed({
      "request-status": () => Promise.resolve(jsonResponse(document, 202)),
      activity: () => Promise.resolve(jsonResponse({ events: [] }))
    }));
    try {
      const api = await loadApi();
      const observed = await api.probeRequestOutcome({}, REQUEST_ID, OPERATION);
      assert.equal(observed.verdict, "unavailable", `trusted a document it must reject: ${JSON.stringify(document)}`);
    } finally {
      restore();
    }
  }
});

test("a completed queue record settles the outcome without reading Activity", async () => {
  const fetcher = routed({
    "request-status": () => Promise.resolve(jsonResponse(statusDocument({ state: "complete" })))
  });
  const restore = installBrowser(fetcher);
  try {
    const api = await loadApi();
    const observed = await api.probeRequestOutcome({}, REQUEST_ID, OPERATION);
    assert.equal(observed.verdict, "settled");
    assert.equal(observed.job_id, JOB_ID);
    assert.deepEqual(fetcher.seen, ["request-status"]);
  } finally {
    restore();
  }
});

test("a reaped queue record is answered from the exact request ID in Activity", async () => {
  const fetcher = routed({
    "request-status": () => Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-request-status.v1",
      request_id: REQUEST_ID,
      state: "unresolved"
    }, 202)),
    activity: () => Promise.resolve(jsonResponse({
      events: [
        activityEvent(),
        activityEvent({ code: "audit.succeeded", state: "succeeded", epoch: 1757000009 }),
        // A different request's terminal record must not be borrowed.
        activityEvent({ code: "audit.failed", client_request_id: "f".repeat(32) })
      ]
    }))
  });
  const restore = installBrowser(fetcher);
  try {
    const api = await loadApi();
    const observed = await api.probeRequestOutcome({}, REQUEST_ID, OPERATION);
    assert.equal(observed.verdict, "settled");
    assert.equal(observed.code, "audit.succeeded");
    assert.equal(observed.epoch, 1757000009);
    assert.deepEqual(fetcher.seen, ["request-status", "activity"]);
  } finally {
    restore();
  }
});

test("a terminal Activity record outranks the bare acceptance recorded beside it", async () => {
  const restore = installBrowser(routed({
    "request-status": () => Promise.resolve(jsonResponse({
      schema: "sdsync.dsm-request-status.v1",
      request_id: REQUEST_ID,
      state: "unresolved"
    }, 202)),
    activity: () => Promise.resolve(jsonResponse({
      events: [
        activityEvent({ code: "audit.failed", state: "failed", epoch: 1757000002 }),
        activityEvent({ epoch: 1757000050 })
      ]
    }))
  }));
  try {
    const api = await loadApi();
    const observed = await api.probeRequestOutcome({}, REQUEST_ID, OPERATION);
    assert.equal(observed.verdict, "settled");
    assert.equal(observed.code, "audit.failed");
  } finally {
    restore();
  }
});

test("no trace in either source is reported as absent, and an unreadable source is not", async () => {
  const unresolved = () => Promise.resolve(jsonResponse({
    schema: "sdsync.dsm-request-status.v1",
    request_id: REQUEST_ID,
    state: "unresolved"
  }, 202));

  const absent = installBrowser(routed({
    "request-status": unresolved,
    activity: () => Promise.resolve(jsonResponse({ events: [activityEvent({ client_request_id: "c".repeat(32) })] }))
  }));
  try {
    const api = await loadApi();
    assert.equal((await api.probeRequestOutcome({}, REQUEST_ID, OPERATION)).verdict, "absent");
  } finally {
    absent();
  }

  // The queue was readable and empty, but Activity was not reachable. That is
  // not the same finding and must not be reported as one.
  const unreadable = installBrowser(routed({
    "request-status": unresolved,
    activity: () => Promise.reject(new TypeError("activity read failed"))
  }));
  try {
    const api = await loadApi();
    assert.equal((await api.probeRequestOutcome({}, REQUEST_ID, OPERATION)).verdict, "unavailable");
  } finally {
    unreadable();
  }
});

test("the probe rejects an untrusted request identity before opening any connection", async () => {
  let reads = 0;
  const restore = installBrowser(() => {
    reads += 1;
    return Promise.reject(new TypeError("must not be reached"));
  });
  try {
    const api = await loadApi();
    await assert.rejects(
      () => api.probeRequestOutcome({}, "not-a-request-id", OPERATION),
      (error) => error instanceof TypeError
    );
    await assert.rejects(
      () => api.probeRequestOutcome({}, REQUEST_ID, "not-an-operation"),
      (error) => error instanceof TypeError
    );
    assert.equal(reads, 0);
  } finally {
    restore();
  }
});

test("the probe is read-only and never submits or replays a mutation", () => {
  const start = apiSource.indexOf("export async function probeRequestOutcome(");
  const end = apiSource.indexOf("\nexport async function apiPost(", start);
  assert.ok(start >= 0 && end > start, "probeRequestOutcome must precede the POST bridge");
  const probeSource = apiSource.slice(start, end);
  assert.equal(probeSource.includes("apiPost("), false);
  assert.equal(probeSource.includes("fetch(API_URL"), false);
  assert.equal(probeSource.includes("method: \"POST\""), false);
  // It must not drop the authentication the manual Reconcile controls still need.
  assert.equal(probeSource.includes("forgetReconciliationAuth("), false);
});
