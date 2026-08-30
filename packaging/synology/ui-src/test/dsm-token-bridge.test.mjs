import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function jsonResponse(model, status = 200, overrides = {}) {
  const body = typeof model === "string" ? model : JSON.stringify(model);
  const contentType = overrides.contentType === undefined
    ? "application/json; charset=utf-8"
    : overrides.contentType;
  const contentLength = overrides.contentLength === undefined
    ? String(new TextEncoder().encode(body).byteLength)
    : overrides.contentLength;
  return {
    redirected: Boolean(overrides.redirected),
    status,
    ok: status >= 200 && status < 300,
    headers: {
      get(name) {
        if (name.toLowerCase() === "content-type") return contentType;
        if (name.toLowerCase() === "content-length") return contentLength;
        return null;
      }
    },
    async text() {
      if (typeof overrides.beforeText === "function") await overrides.beforeText();
      return body;
    }
  };
}

function installBrowser(tokenFetch, packageFetch) {
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

function packageResponse(url, options) {
  if (options.method === "POST") {
    const request = JSON.parse(options.body);
    return jsonResponse({
      schema: "sdsync.dsm-queued.v1",
      ok: true,
      state: "queued",
      request_id: request.request_id,
      job_id: "a".repeat(48)
    });
  }
  return jsonResponse({ ok: true, url });
}

test("DSM token bootstrap is same-origin, cached in memory, and header-only", async () => {
  const rawToken = "native-token+/= current?";
  const token = encodeURIComponent(rawToken);
  const tokenRequests = [];
  const packageRequests = [];
  const restore = installBrowser(
    async (url, options) => {
      tokenRequests.push({ url, options });
      return jsonResponse({ success: true, data: { synotoken: rawToken } });
    },
    async (url, options) => {
      packageRequests.push({ url, options });
      return packageResponse(url, options);
    }
  );
  try {
    const api = await loadApi();
    await api.apiGet({}, "snapshot");
    await api.apiPost({}, "package-csrf", api.ACTIONS.setDefault, { name: "profile" }, false);

    assert.equal(tokenRequests.length, 1, "one bootstrap must serve every request in this AppWindow");
    assert.equal(tokenRequests[0].url, api.DSM_TOKEN_URL);
    assert.deepEqual(
      Object.assign({}, tokenRequests[0].options, { signal: undefined }),
      {
        method: "GET",
        credentials: "same-origin",
        cache: "no-store",
        redirect: "error",
        signal: undefined,
        headers: { Accept: "application/json" }
      }
    );
    assert.equal("body" in tokenRequests[0].options, false);

    assert.equal(packageRequests.length, 2);
    for (const request of packageRequests) {
      assert.equal(request.options.credentials, "same-origin");
      assert.equal(request.options.headers["X-SDSYNC-Request"], "1");
      assert.equal(request.options.headers["X-SYNO-TOKEN"], token);
      assert.equal(request.url.includes(rawToken), false);
      assert.equal(request.url.includes(token), false);
      assert.equal(String(request.options.body || "").includes(rawToken), false);
      assert.equal(String(request.options.body || "").includes(token), false);
    }
  } finally {
    restore();
  }
});

test("concurrent package requests share one DSM token bootstrap", async () => {
  let tokenReads = 0;
  let releaseToken;
  const tokenPending = new Promise((resolve) => { releaseToken = resolve; });
  const packageRequests = [];
  const restore = installBrowser(
    async () => {
      tokenReads += 1;
      await tokenPending;
      return jsonResponse({ success: true, data: { synotoken: "shared-token+/=" } });
    },
    async (url, options) => {
      packageRequests.push({ url, options });
      return packageResponse(url, options);
    }
  );
  try {
    const api = await loadApi();
    const first = api.apiGet({}, "snapshot");
    const second = api.apiGet({}, "activity", { lines: 10 });
    await Promise.resolve();
    assert.equal(tokenReads, 1);
    releaseToken();
    await Promise.all([first, second]);
    assert.equal(packageRequests.length, 2);
    assert.ok(packageRequests.every(
      (request) => request.options.headers["X-SYNO-TOKEN"] === encodeURIComponent("shared-token+/=")
    ));
  } finally {
    restore();
  }
});

test("unavailable or invalid DSM token bootstrap preserves bounded cookie-only requests", async (t) => {
  const oversized = JSON.stringify({
    success: true,
    data: { synotoken: "x".repeat(1025) }
  });
  const cases = [
    ["network failure", async () => { throw new TypeError("unavailable"); }],
    ["redirect", async () => jsonResponse({ success: true }, 200, { redirected: true })],
    ["non-JSON", async () => jsonResponse("not json", 200, { contentType: "text/html" })],
    ["rejected envelope", async () => jsonResponse({ success: false, error: { code: 119 } })],
    ["empty token", async () => jsonResponse({ success: true, data: { synotoken: "" } })],
    ["oversized token", async () => jsonResponse(oversized)],
    ["encoded header exceeds 1024 bytes", async () => jsonResponse({
      success: true,
      data: { synotoken: " ".repeat(342) }
    })],
    ["oversized declaration", async () => jsonResponse(
      { success: true, data: { synotoken: "token" } },
      200,
      { contentLength: String(16 * 1024 + 1) }
    )]
  ];

  for (const [name, tokenFetch] of cases) {
    await t.test(name, async () => {
      let tokenReads = 0;
      const packageRequests = [];
      const restore = installBrowser(
        async (...args) => {
          tokenReads += 1;
          return tokenFetch(...args);
        },
        async (url, options) => {
          packageRequests.push({ url, options });
          return packageResponse(url, options);
        }
      );
      try {
        const api = await loadApi();
        await api.apiGet({}, "snapshot");
        await api.apiGet({}, "snapshot");
        assert.equal(tokenReads, 1, "a failed bootstrap must use its bounded retry cooldown");
        assert.equal(packageRequests.length, 2);
        for (const request of packageRequests) {
          assert.equal(request.options.headers["X-SDSYNC-Request"], "1");
          assert.equal(Object.hasOwn(request.options.headers, "X-SYNO-TOKEN"), false);
        }
      } finally {
        restore();
      }
    });
  }
});

test("cooldown token recovery reissues CSRF before dispatching the first POST", async () => {
  const previousNow = Date.now;
  let now = 1_000_000;
  let tokenReads = 0;
  let appCsrf = "";
  const replacements = [];
  const packageRequests = [];
  Date.now = () => now;
  const restore = installBrowser(
    async () => {
      tokenReads += 1;
      if (tokenReads === 1) throw new TypeError("temporary token bootstrap failure");
      return jsonResponse({ success: true, data: { synotoken: "recovered-token+/=" } });
    },
    async (url, options) => {
      packageRequests.push({ url, options });
      const action = options.method === "GET"
        ? new URL(url, "https://nas.example.invalid").searchParams.get("action")
        : "post";
      if (action === "csrf") {
        const tokenBound = Object.hasOwn(options.headers, "X-SYNO-TOKEN");
        return jsonResponse({
          schema: "sdsync.dsm-csrf.v1",
          csrf_token: tokenBound ? "csrf-token-bound" : "csrf-cookie-only"
        });
      }
      if (action === "snapshot") {
        return jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true });
      }
      return packageResponse(url, options);
    }
  );
  try {
    const api = await loadApi();
    const auth = {
      onCsrfReissued(previousToken, replacementToken) {
        replacements.push([previousToken, replacementToken]);
        if (appCsrf === previousToken) appCsrf = replacementToken;
      }
    };

    const initialCsrf = await api.apiGet(auth, "csrf");
    appCsrf = initialCsrf.csrf_token;
    assert.equal(appCsrf, "csrf-cookie-only");
    assert.equal(Object.hasOwn(packageRequests[0].options.headers, "X-SYNO-TOKEN"), false);

    now += 30000;
    await api.apiGet(auth, "snapshot");
    const queued = await api.apiPost(
      auth,
      appCsrf,
      api.ACTIONS.setDefault,
      { name: "profile" },
      false
    );

    const postRequests = packageRequests.filter((request) => request.options.method === "POST");
    assert.equal(tokenReads, 2);
    assert.equal(postRequests.length, 1, "CSRF recovery must not retry a dispatched POST");
    assert.deepEqual(replacements, [["csrf-cookie-only", "csrf-token-bound"]]);
    assert.equal(appCsrf, "csrf-token-bound");
    assert.equal(postRequests[0].options.headers["X-SDSYNC-CSRF"], "csrf-token-bound");
    assert.equal(
      postRequests[0].options.headers["X-SYNO-TOKEN"],
      encodeURIComponent("recovered-token+/=")
    );
    assert.equal(queued.schema, "sdsync.dsm-queued.v1");
    assert.equal(queued.state, "queued");
    assert.equal(packageRequests.length, 4, "recovery must add one CSRF GET before one POST");
  } finally {
    Date.now = previousNow;
    restore();
  }
});

test("accepted cookie-only mutations pin authentication through terminal result polling", async () => {
  const previousNow = Date.now;
  let now = 2_000_000;
  let tokenReads = 0;
  let api;
  let recoveryTriggered = false;
  const auth = {};
  const packageRequests = [];
  const jobId = "b".repeat(48);
  let requestId = "";
  Date.now = () => now;
  const restore = installBrowser(
    async () => {
      tokenReads += 1;
      if (tokenReads === 1) throw new TypeError("temporary token bootstrap failure");
      return jsonResponse({ success: true, data: { synotoken: "recovered-token+/=" } });
    },
    async (url, options) => {
      const action = options.method === "GET"
        ? new URL(url, "https://nas.example.invalid").searchParams.get("action")
        : "post";
      packageRequests.push({ action, url, options });
      if (action === "csrf") {
        return jsonResponse({
          schema: "sdsync.dsm-csrf.v1",
          csrf_token: "csrf-cookie-only"
        });
      }
      if (action === "snapshot") {
        return jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true });
      }
      if (action === "post") {
        const request = JSON.parse(options.body);
        requestId = request.request_id;
        return jsonResponse({
          schema: "sdsync.dsm-queued.v1",
          ok: true,
          state: "queued",
          request_id: request.request_id,
          job_id: jobId
        }, 202, {
          beforeText: async () => {
            if (recoveryTriggered) return;
            recoveryTriggered = true;
            now += 30000;
            await api.apiGet(auth, "snapshot");
          }
        });
      }
      if (action === "result") {
        if (Object.hasOwn(options.headers, "X-SYNO-TOKEN")) {
          return jsonResponse({
            schema: "sdsync.dsm-result-status.v1",
            job_id: jobId,
            state: "expired_or_missing",
            result: {
              schema: "sdsync.dsm-result.v1",
              ok: false,
              code: "expired_or_missing",
              message: "Authentication binding changed."
            }
          }, 410);
        }
        return jsonResponse({
          schema: "sdsync.dsm-result-status.v1",
          job_id: jobId,
          state: "complete",
          client_request_id: requestId,
          result: {
            schema: "sdsync.dsm-result.v1",
            ok: true,
            message: "Authentication binding remained pinned."
          }
        });
      }
      throw new Error(`unexpected package action ${action}`);
    }
  );
  try {
    api = await loadApi();
    const csrf = await api.apiGet(auth, "csrf");
    const result = await api.apiPost(
      auth,
      csrf.csrf_token,
      api.ACTIONS.setDefault,
      { name: "profile" },
      true,
      0
    );

    assert.equal(result.ok, true);
    assert.equal(result.message, "Authentication binding remained pinned.");
    assert.equal(tokenReads, 2, "a concurrent normal GET must still recover the DSM token");
    assert.deepEqual(
      packageRequests.map((request) => request.action),
      ["csrf", "post", "snapshot", "result"]
    );
    const post = packageRequests.find((request) => request.action === "post");
    const recovery = packageRequests.find((request) => request.action === "snapshot");
    const resultRead = packageRequests.find((request) => request.action === "result");
    assert.equal(Object.hasOwn(post.options.headers, "X-SYNO-TOKEN"), false);
    assert.equal(
      recovery.options.headers["X-SYNO-TOKEN"],
      encodeURIComponent("recovered-token+/=")
    );
    assert.equal(
      Object.hasOwn(resultRead.options.headers, "X-SYNO-TOKEN"),
      false,
      "terminal observation must retain the exact cookie-only POST binding"
    );
    assert.equal(
      packageRequests.filter((request) => request.options.method === "POST").length,
      1,
      "auth recovery must never redispatch an accepted mutation"
    );
  } finally {
    Date.now = previousNow;
    restore();
  }
});

test("manual reconciliation reuses an outcome-unknown request binding and clears it after terminal settlement", async () => {
  const previousNow = Date.now;
  let now = 3_000_000;
  let api;
  let tokenReads = 0;
  let postReads = 0;
  let requestId = "";
  let phase = "initial";
  const recoveredToken = "later recovered token+/=";
  const jobId = "c".repeat(48);
  const packageRequests = [];
  const auth = {};
  Date.now = () => now;
  const restore = installBrowser(
    async () => {
      tokenReads += 1;
      if (tokenReads === 1) throw new TypeError("temporary token bootstrap failure");
      return jsonResponse({ success: true, data: { synotoken: recoveredToken } });
    },
    async (url, options) => {
      const action = options.method === "POST"
        ? "post"
        : new URL(url, "https://nas.example.invalid").searchParams.get("action");
      packageRequests.push({ phase, action, url, options });

      if (action === "post") {
        postReads += 1;
        const request = JSON.parse(options.body);
        requestId = request.request_id;
        assert.equal(Object.hasOwn(options.headers, "X-SYNO-TOKEN"), false);
        if (postReads === 1) {
          return {
            redirected: false,
            status: 202,
            ok: true,
            headers: { get: () => "application/json" },
            async text() { throw new TypeError("queue acknowledgement was lost"); }
          };
        }
        return jsonResponse({
          schema: "sdsync.dsm-error.v1",
          ok: false,
          code: "csrf_rejected",
          message: "The replay could not prove whether the original request was accepted."
        }, 403);
      }

      const hasRecoveredToken = options.headers["X-SYNO-TOKEN"] === encodeURIComponent(recoveredToken);
      if (action === "request-status" && phase === "initial") {
        assert.equal(hasRecoveredToken, false);
        return jsonResponse({
          schema: "sdsync.dsm-request-status.v1",
          request_id: requestId,
          state: "unresolved"
        });
      }
      if (action === "snapshot") {
        assert.equal(hasRecoveredToken, true, "the normal AppWindow auth snapshot should recover independently");
        return jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true });
      }
      if (action === "request-status") {
        if (hasRecoveredToken) {
          return jsonResponse({
            schema: "sdsync.dsm-request-status.v1",
            request_id: requestId,
            state: "complete",
            job_id: jobId,
            operation: "client-event",
            wrong_binding: true
          });
        }
        return jsonResponse({
          schema: "sdsync.dsm-request-status.v1",
          request_id: requestId,
          state: "complete",
          job_id: jobId,
          operation: "client-event"
        });
      }
      if (action === "result") {
        assert.equal(hasRecoveredToken, false, "terminal lookup must retain the original cookie-only binding");
        return jsonResponse({
          schema: "sdsync.dsm-result-status.v1",
          job_id: jobId,
          client_request_id: requestId,
          state: "complete",
          result: {
            schema: "sdsync.dsm-result.v1",
            ok: true,
            message: "The original cookie-only request completed."
          }
        });
      }
      throw new Error(`unexpected package action ${action}`);
    }
  );
  try {
    api = await loadApi();
    let incident;
    try {
      await api.apiPost(
        auth,
        "csrf-token",
        api.ACTIONS.clientEvent,
        { event: "interface-settings" },
        false,
        0
      );
      assert.fail("expected the lost acknowledgement to remain outcome-unknown");
    } catch (error) {
      incident = error;
    }
    assert.equal(incident instanceof api.MutationOutcomeUnknownError, true);
    assert.equal(incident.requestId, requestId);
    assert.equal(incident.operation, api.ACTIONS.clientEvent);
    assert.equal(incident.outcomeUnknown, true);
    const serializedIncident = JSON.stringify({
      name: incident.name,
      message: incident.message,
      stack: incident.stack,
      ...incident
    });
    assert.equal(serializedIncident.includes(recoveredToken), false);
    assert.equal(serializedIncident.includes(encodeURIComponent(recoveredToken)), false);

    now += 30000;
    phase = "global-recovery";
    await api.apiGet({}, "snapshot");
    assert.equal(tokenReads, 2);
    phase = "manual";
    const reconciled = await api.reconcileMutationRequest(
      auth,
      requestId,
      api.ACTIONS.clientEvent,
      0,
      {
        ...api.AUTOSAVE_API_LIMITS,
        requestReconciliationTimeoutMs: 25,
        requestReconciliationPollIntervalMs: 1
      }
    );
    assert.equal(reconciled.request_id, requestId);
    assert.equal(reconciled.job_id, jobId);
    assert.equal(reconciled.operation, api.ACTIONS.clientEvent);
    assert.equal(reconciled.result.message, "The original cookie-only request completed.");
    assert.equal(tokenReads, 2, "manual retry must not replace the retained request binding");

    const firstManualRequests = packageRequests.filter((request) => request.phase === "manual");
    assert.deepEqual(firstManualRequests.map((request) => request.action), ["request-status", "result"]);
    assert.equal(
      firstManualRequests.some((request) => Object.hasOwn(request.options.headers, "X-SYNO-TOKEN")),
      false
    );

    phase = "after-terminal";
    await assert.rejects(
      api.reconcileMutationRequest(
        auth,
        requestId,
        api.ACTIONS.clientEvent,
        0,
        {
          ...api.AUTOSAVE_API_LIMITS,
          requestReconciliationTimeoutMs: 25,
          requestReconciliationPollIntervalMs: 1
        }
      ),
      (error) => {
        assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
        const serialized = JSON.stringify({ message: error.message, stack: error.stack, ...error });
        assert.equal(serialized.includes(recoveredToken), false);
        assert.equal(serialized.includes(encodeURIComponent(recoveredToken)), false);
        return true;
      }
    );
    const afterTerminal = packageRequests.filter((request) => request.phase === "after-terminal");
    assert.equal(afterTerminal.length, 1);
    assert.equal(afterTerminal[0].action, "request-status");
    assert.equal(
      afterTerminal[0].options.headers["X-SYNO-TOKEN"],
      encodeURIComponent(recoveredToken),
      "terminal settlement must clear the retained cookie-only binding"
    );
  } finally {
    Date.now = previousNow;
    restore();
  }
});

test("cleanup-inspection failures retain exact request authentication until reconciliation settles", async () => {
  const previousNow = Date.now;
  let now = 3_500_000;
  let tokenReads = 0;
  let requestId = "";
  let phase = "initial";
  const recoveredToken = "later cleanup token+/=";
  const draftPassword = "draft-password-must-not-leak";
  const jobId = "f".repeat(48);
  const packageRequests = [];
  const auth = {};
  Date.now = () => now;
  const restore = installBrowser(
    async () => {
      tokenReads += 1;
      if (tokenReads === 1) throw new TypeError("temporary token bootstrap failure");
      return jsonResponse({ success: true, data: { synotoken: recoveredToken } });
    },
    async (url, options) => {
      const action = options.method === "POST"
        ? "post"
        : new URL(url, "https://nas.example.invalid").searchParams.get("action");
      packageRequests.push({ phase, action, url, options });

      if (action === "post") {
        const request = JSON.parse(options.body);
        requestId = request.request_id;
        assert.equal(Object.hasOwn(options.headers, "X-SYNO-TOKEN"), false);
        return jsonResponse({
          schema: "sdsync.dsm-queued.v1",
          ok: true,
          state: "queued",
          request_id: requestId,
          job_id: jobId
        }, 202);
      }
      if (action === "snapshot") {
        assert.equal(options.headers["X-SYNO-TOKEN"], encodeURIComponent(recoveredToken));
        return jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true });
      }
      if (action === "request-status") {
        if (phase === "manual") {
          assert.equal(
            Object.hasOwn(options.headers, "X-SYNO-TOKEN"),
            false,
            "cleanup reconciliation must retain the original cookie-only binding"
          );
          return jsonResponse({
            schema: "sdsync.dsm-request-status.v1",
            request_id: requestId,
            state: "complete",
            job_id: jobId,
            operation: "test-profile-auth"
          });
        }
        assert.equal(options.headers["X-SYNO-TOKEN"], encodeURIComponent(recoveredToken));
        return jsonResponse({
          schema: "sdsync.dsm-request-status.v1",
          request_id: requestId,
          state: "unresolved"
        });
      }
      if (action === "result") {
        assert.equal(
          Object.hasOwn(options.headers, "X-SYNO-TOKEN"),
          false,
          "terminal cleanup evidence must use the original request binding"
        );
        return jsonResponse({
          schema: "sdsync.dsm-result-status.v1",
          job_id: jobId,
          client_request_id: requestId,
          state: "complete",
          result: {
            schema: "sdsync.dsm-result.v1",
            ok: false,
            code: "file_station_logout_failed",
            message: "Temporary File Station session cleanup needs inspection."
          }
        });
      }
      throw new Error(`unexpected package action ${action}`);
    }
  );
  try {
    const api = await loadApi();
    const payload = {
      allow_http: false,
      ca_certificate: null,
      connect_timeout_seconds: 15,
      danger_accept_invalid_certs: false,
      password: draftPassword,
      password_source: "transient",
      profile: "profile",
      retries: 2,
      timeout_seconds: 120,
      totp: null,
      totp_source: "none",
      url: "https://files.example.invalid",
      username: "admin"
    };
    let inspection;
    try {
      await api.apiPost(auth, "csrf-token", api.ACTIONS.testProfileAuth, payload, true, 0);
      assert.fail("expected cleanup inspection failure");
    } catch (error) {
      inspection = error;
    }
    assert.equal(inspection instanceof api.DsmApiError, true);
    assert.equal(inspection.requiresInspection, true);
    assert.equal(inspection.outcomeUnknown, undefined);
    assert.equal(inspection.requestId, requestId);
    assert.equal(inspection.jobId, jobId);
    const serializedInspection = JSON.stringify({ message: inspection.message, stack: inspection.stack, ...inspection });
    assert.equal(serializedInspection.includes(draftPassword), false);
    assert.equal(serializedInspection.includes(recoveredToken), false);

    now += 30000;
    phase = "global-recovery";
    await api.apiGet({}, "snapshot");
    assert.equal(tokenReads, 2);

    phase = "manual";
    await assert.rejects(
      api.reconcileMutationRequest(
        auth,
        requestId,
        api.ACTIONS.testProfileAuth,
        0,
        {
          ...api.AUTOSAVE_API_LIMITS,
          requestReconciliationTimeoutMs: 25,
          requestReconciliationPollIntervalMs: 1
        }
      ),
      (error) => error instanceof api.DsmApiError
        && error.requiresInspection === true
        && error.accepted === true
        && error.requestId === requestId
        && error.jobId === jobId
    );

    phase = "after-terminal";
    await assert.rejects(
      api.reconcileMutationRequest(
        auth,
        requestId,
        api.ACTIONS.testProfileAuth,
        0,
        {
          ...api.AUTOSAVE_API_LIMITS,
          requestReconciliationTimeoutMs: 10,
          requestReconciliationPollIntervalMs: 1
        }
      ),
      (error) => error instanceof api.MutationOutcomeUnknownError
    );
    const afterTerminal = packageRequests.filter((request) => request.phase === "after-terminal");
    assert.ok(afterTerminal.length >= 1);
    assert.equal(afterTerminal.every((request) => request.action === "request-status"), true);
    assert.equal(
      afterTerminal.every(
        (request) => request.options.headers["X-SYNO-TOKEN"] === encodeURIComponent(recoveredToken)
      ),
      true,
      "settled cleanup evidence must release the retained request binding"
    );
  } finally {
    Date.now = previousNow;
    restore();
  }
});

test("reconciliation auth snapshots are AppWindow-scoped, purgeable, expiring, and never disclosed", async () => {
  const previousNow = Date.now;
  let now = 5_000_000;
  let tokenReads = 0;
  let phase = "initial";
  const recoveredToken = "private replacement token+/=";
  const jobId = "e".repeat(48);
  const requestIds = new Map();
  const postReads = new Map();
  const owners = {
    scoped: {},
    purged: {},
    expired: {}
  };
  Date.now = () => now;
  const restore = installBrowser(
    async () => {
      tokenReads += 1;
      if (tokenReads === 1) throw new TypeError("temporary token bootstrap failure");
      return jsonResponse({ success: true, data: { synotoken: recoveredToken } });
    },
    async (url, options) => {
      const action = options.method === "POST"
        ? "post"
        : new URL(url, "https://nas.example.invalid").searchParams.get("action");
      const hasRecoveredToken = options.headers["X-SYNO-TOKEN"] === encodeURIComponent(recoveredToken);

      if (action === "post") {
        const request = JSON.parse(options.body);
        const event = request.arguments.event;
        requestIds.set(event, request.request_id);
        const reads = (postReads.get(request.request_id) || 0) + 1;
        postReads.set(request.request_id, reads);
        assert.equal(Object.hasOwn(options.headers, "X-SYNO-TOKEN"), false);
        if (reads === 1) {
          return {
            redirected: false,
            status: 202,
            ok: true,
            headers: { get: () => "application/json" },
            async text() { throw new TypeError("queue acknowledgement was lost"); }
          };
        }
        return jsonResponse({
          schema: "sdsync.dsm-error.v1",
          ok: false,
          code: "csrf_rejected",
          message: "The replay could not prove whether the original request was accepted."
        }, 403);
      }

      if (action === "snapshot") {
        assert.equal(hasRecoveredToken, true);
        return jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true });
      }

      const requestId = new URL(url, "https://nas.example.invalid").searchParams.get("request_id");
      if (action === "request-status" && phase === "initial") {
        assert.equal(hasRecoveredToken, false);
        return jsonResponse({
          schema: "sdsync.dsm-request-status.v1",
          request_id: requestId,
          state: "unresolved"
        });
      }
      if (action === "request-status" && phase === "scoped-owner") {
        assert.equal(requestId, requestIds.get("scoped"));
        assert.equal(hasRecoveredToken, false, "the originating AppWindow must retain its cookie-only binding");
        return jsonResponse({
          schema: "sdsync.dsm-request-status.v1",
          request_id: requestId,
          state: "complete",
          job_id: jobId,
          operation: "client-event"
        });
      }
      if (action === "request-status") {
        assert.equal(
          hasRecoveredToken,
          true,
          "another AppWindow, a purged owner, and an expired owner must use current DSM auth"
        );
        return jsonResponse({
          schema: "sdsync.dsm-request-status.v1",
          request_id: requestId,
          state: "complete",
          job_id: jobId,
          operation: "client-event",
          wrong_binding: true
        });
      }
      if (action === "result") {
        assert.equal(phase, "scoped-owner");
        assert.equal(hasRecoveredToken, false);
        return jsonResponse({
          schema: "sdsync.dsm-result-status.v1",
          job_id: jobId,
          client_request_id: requestIds.get("scoped"),
          state: "complete",
          result: {
            schema: "sdsync.dsm-result.v1",
            ok: true,
            message: "Original AppWindow binding reconciled."
          }
        });
      }
      throw new Error(`unexpected package action ${action}`);
    }
  );
  globalThis.window.performance = { now: () => now };
  let api;
  try {
    api = await loadApi();
    const createIncident = async (event, auth) => {
      await assert.rejects(
        api.apiPost(
          auth,
          "csrf-token",
          api.ACTIONS.clientEvent,
          { event },
          false,
          0
        ),
        (error) => error instanceof api.MutationOutcomeUnknownError
          && error.requestId === requestIds.get(event)
      );
    };
    await createIncident("scoped", owners.scoped);
    await createIncident("purged", owners.purged);
    await createIncident("expired", owners.expired);

    const purgeResult = api.purgeReconciliationAuth(owners.purged);
    assert.equal(purgeResult, undefined, "purging must never return a retained token snapshot");
    api.purgeReconciliationAuth(owners.purged);

    now += 30000;
    phase = "global-recovery";
    await api.apiGet({}, "snapshot");
    assert.equal(tokenReads, 2);

    const limits = {
      ...api.AUTOSAVE_API_LIMITS,
      requestReconciliationTimeoutMs: 25,
      requestReconciliationPollIntervalMs: 1
    };
    const assertCurrentAuthFailure = async (auth, event, expectedPhase) => {
      phase = expectedPhase;
      let incident;
      await assert.rejects(
        api.reconcileMutationRequest(
          auth,
          requestIds.get(event),
          api.ACTIONS.clientEvent,
          0,
          limits
        ),
        (error) => {
          incident = error;
          return error instanceof api.MutationOutcomeUnknownError;
        }
      );
      const serialized = JSON.stringify({
        name: incident.name,
        message: incident.message,
        stack: incident.stack,
        ...incident
      });
      assert.equal(serialized.includes(recoveredToken), false);
      assert.equal(serialized.includes(encodeURIComponent(recoveredToken)), false);
    };

    await assertCurrentAuthFailure({}, "scoped", "scoped-foreign-owner");
    phase = "scoped-owner";
    const reconciled = await api.reconcileMutationRequest(
      owners.scoped,
      requestIds.get("scoped"),
      api.ACTIONS.clientEvent,
      0,
      limits
    );
    assert.equal(reconciled.result.message, "Original AppWindow binding reconciled.");

    await assertCurrentAuthFailure(owners.purged, "purged", "purged-owner");

    // The package permits result retention down to 300 seconds. Crossing that
    // floor must invalidate the remaining exact-request token snapshot.
    now += 270001;
    await assertCurrentAuthFailure(owners.expired, "expired", "expired-owner");
  } finally {
    if (api) {
      api.purgeReconciliationAuth(owners.scoped);
      api.purgeReconciliationAuth(owners.purged);
      api.purgeReconciliationAuth(owners.expired);
    }
    Date.now = previousNow;
    restore();
  }
});

test("pre-acceptance rejection does not retain a reconciliation auth binding", async () => {
  const previousNow = Date.now;
  let now = 4_000_000;
  let tokenReads = 0;
  let requestId = "";
  let phase = "initial";
  const recoveredToken = "replacement token+/=";
  const jobId = "d".repeat(48);
  const packageRequests = [];
  Date.now = () => now;
  const restore = installBrowser(
    async () => {
      tokenReads += 1;
      if (tokenReads === 1) throw new TypeError("temporary token bootstrap failure");
      return jsonResponse({ success: true, data: { synotoken: recoveredToken } });
    },
    async (url, options) => {
      const action = options.method === "POST"
        ? "post"
        : new URL(url, "https://nas.example.invalid").searchParams.get("action");
      packageRequests.push({ phase, action, url, options });
      if (action === "post") {
        requestId = JSON.parse(options.body).request_id;
        assert.equal(Object.hasOwn(options.headers, "X-SYNO-TOKEN"), false);
        return jsonResponse({
          schema: "sdsync.dsm-error.v1",
          ok: false,
          code: "csrf_rejected",
          message: "The mutation token was rejected before queue admission."
        }, 403);
      }
      const hasRecoveredToken = options.headers["X-SYNO-TOKEN"] === encodeURIComponent(recoveredToken);
      if (action === "snapshot") {
        assert.equal(hasRecoveredToken, true);
        return jsonResponse({ schema: "sdsync.dsm-api.v1", ok: true });
      }
      if (action === "request-status") {
        if (hasRecoveredToken) {
          return jsonResponse({
            schema: "sdsync.dsm-request-status.v1",
            request_id: requestId,
            state: "unresolved"
          });
        }
        return jsonResponse({
          schema: "sdsync.dsm-request-status.v1",
          request_id: requestId,
          state: "complete",
          job_id: jobId,
          operation: "client-event"
        });
      }
      if (action === "result") {
        return jsonResponse({
          schema: "sdsync.dsm-result-status.v1",
          job_id: jobId,
          client_request_id: requestId,
          state: "complete",
          result: { schema: "sdsync.dsm-result.v1", ok: true }
        });
      }
      throw new Error(`unexpected package action ${action}`);
    }
  );
  try {
    const api = await loadApi();
    await assert.rejects(
      api.apiPost(
        {},
        "csrf-token",
        api.ACTIONS.clientEvent,
        { event: "interface-settings" },
        false,
        0
      ),
      (error) => {
        assert.equal(error instanceof api.DsmApiError, true);
        assert.equal(error.preAcceptance, true);
        assert.equal(error.outcomeUnknown, undefined);
        assert.equal(error.requestId, requestId);
        return true;
      }
    );

    now += 30000;
    phase = "global-recovery";
    await api.apiGet({}, "snapshot");
    assert.equal(tokenReads, 2);
    phase = "manual";
    await assert.rejects(
      api.reconcileMutationRequest(
        {},
        requestId,
        api.ACTIONS.clientEvent,
        0,
        {
          ...api.AUTOSAVE_API_LIMITS,
          requestReconciliationTimeoutMs: 15,
          requestReconciliationPollIntervalMs: 1
        }
      ),
      (error) => error instanceof api.MutationOutcomeUnknownError
        && error.stage === "request_reconciliation"
    );
    const manualRequests = packageRequests.filter((request) => request.phase === "manual");
    assert.ok(manualRequests.length >= 1);
    assert.equal(manualRequests.every((request) => request.action === "request-status"), true);
    assert.equal(
      manualRequests.every(
        (request) => request.options.headers["X-SYNO-TOKEN"] === encodeURIComponent(recoveredToken)
      ),
      true,
      "pre-acceptance rejection must not retain the original auth binding"
    );
  } finally {
    Date.now = previousNow;
    restore();
  }
});

test("token bridge has no persistent dependency and bounded attempts link and release abort signals", () => {
  assert.match(apiSource, /\/webapi\/entry\.cgi\?api=SYNO\.API\.Auth&version=6&method=token/);
  assert.match(apiSource, /authenticated\["X-SYNO-TOKEN"\] = dsmAuth\.token/);
  assert.match(apiSource, /function dsmAuthSnapshot\(\)/);
  assert.match(apiSource, /function linkedAbortAttempt\(parentSignal\)/);
  assert.match(apiSource, /const attempt = linkedAbortAttempt\(auth && auth\.signal\);[\s\S]*?apiGetWithDsmAuth\([\s\S]*?attempt\.signal[\s\S]*?\)[\s\S]*?finally \{[\s\S]*?attempt\.release\(\)/);
  assert.match(apiSource, /withinLimit\([\s\S]*?limits\.resultRequestTimeoutMs,[\s\S]*?attempt\.abort,[\s\S]*?observation/);
  assert.match(apiSource, /parentSignal\.addEventListener\("abort", abort, \{ once: true \}\)/);
  assert.match(apiSource, /parentSignal\.removeEventListener\("abort", abort\)/);
  assert.doesNotMatch(
    apiSource,
    /consumeLaunchToken|launch token|window\.location|window\.history|history\.replaceState|localStorage|sessionStorage|indexedDB|document\.cookie|console\./i
  );
});
