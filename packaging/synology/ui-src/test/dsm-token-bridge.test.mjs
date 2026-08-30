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
