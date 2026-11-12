// The mutation token is replaced before the package would refuse it, and a replay
// never carries a token the package may already have refused. Every request names
// the page's build.
import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function response(body, status = 200, contentType = "application/json; charset=utf-8") {
  const text = typeof body === "string" ? body : JSON.stringify(body);
  return {
    redirected: false,
    status,
    ok: status >= 200 && status < 300,
    headers: { get: (name) => (name.toLowerCase() === "content-type" ? contentType : null) },
    async text() { return text; }
  };
}

// A browser whose clock the test moves. The package answers by route.
function installBrowser(packageFetch) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  const clock = { ms: 1_000_000 };
  globalThis.window = {
    AbortController: globalThis.AbortController,
    TextEncoder: globalThis.TextEncoder,
    crypto: globalThis.crypto || webcrypto,
    fetch: async () => response({ success: true, data: { synotoken: "dsm-token" } }),
    setTimeout: (callback) => globalThis.setTimeout(callback, 0),
    clearTimeout: globalThis.clearTimeout,
    now: () => clock.ms
  };
  globalThis.fetch = packageFetch;
  const restore = () => {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  };
  return { clock, restore };
}

// Tokens shaped as the bridge issues them: `v1.<issued>.<expires>.<nonce>.<mac>`.
function token(serial, lifetime = 300) {
  const issued = 1_757_000_000 + serial;
  return `v1.${issued}.${issued + lifetime}.${"0".repeat(31)}${serial}.${"f".repeat(64)}`;
}

function packageRoutes({ post }) {
  const seen = [];
  let issued = 0;
  const fetcher = async (url, options) => {
    const parsed = new URL(url, "https://nas.invalid");
    const action = parsed.searchParams.get("action");
    if (options.method === "POST") {
      const request = JSON.parse(options.body);
      seen.push({ kind: "post", csrf: options.headers["X-SDSYNC-CSRF"], build: options.headers["X-SDSYNC-Build"], request });
      return post(request, seen.filter((entry) => entry.kind === "post").length);
    }
    seen.push({ kind: action, build: options.headers["X-SDSYNC-Build"] });
    if (action === "csrf") {
      issued += 1;
      return response({ schema: "sdsync.dsm-csrf.v1", csrf_token: token(issued), expires_at_epoch: 0 });
    }
    if (action === "request-status") {
      return response({
        schema: "sdsync.dsm-request-status.v1",
        request_id: parsed.searchParams.get("request_id"),
        state: "unresolved"
      }, 202);
    }
    throw new TypeError(`unexpected read ${action}`);
  };
  fetcher.seen = seen;
  return fetcher;
}

function queued(request) {
  return response({
    schema: "sdsync.dsm-queued.v1",
    ok: true,
    request_id: request.request_id,
    job_id: "a".repeat(48),
    state: "queued",
    replayed: false,
    durability_warning: null
  }, 202);
}

function windowAuth() {
  const reissued = [];
  return { reissued, onCsrfReissued(previous, replacement) { reissued.push([previous, replacement]); } };
}

test("a token near its expiry is replaced before the POST that would have sent it", async () => {
  const fetcher = packageRoutes({ post: queued });
  const { clock, restore } = installBrowser(fetcher);
  try {
    const api = await loadApi();
    const auth = windowAuth();
    const first = (await api.apiGet(auth, "csrf")).csrf_token;
    assert.equal(api.csrfTokenStale(auth, first), false);

    // Well inside its five minutes: sent as it is, no extra read.
    clock.ms += 200_000;
    await api.apiPost(auth, first, api.ACTIONS.clientEvent, { event: "interface-settings" }, false);
    assert.deepEqual(fetcher.seen.map((entry) => entry.kind), ["csrf", "post"]);
    assert.equal(fetcher.seen[1].csrf, first);

    // Thirty seconds before the package would refuse it, it is due.
    clock.ms += 70_000;
    assert.equal(api.csrfTokenStale(auth, first), true);
    await api.apiPost(auth, first, api.ACTIONS.clientEvent, { event: "interface-settings" }, false);
    const kinds = fetcher.seen.map((entry) => entry.kind);
    assert.deepEqual(kinds.slice(2), ["csrf", "post"]);
    assert.equal(fetcher.seen[3].csrf, token(2), "the POST carries the replacement");
    assert.deepEqual(auth.reissued, [[first, token(2)]], "the AppWindow is handed the replacement");
    assert.equal(api.csrfTokenStale(auth, token(2)), false);
    assert.equal(api.csrfTokenStale(auth, first), true, "the replaced token is never fresh again");
  } finally {
    restore();
  }
});

test("a replay after an unreadable 403 carries a fresh token, never the refused one", async () => {
  const fetcher = packageRoutes({
    post: (request, count) => (count === 1
      ? response("<html><body>Forbidden</body></html>", 403, "text/html")
      : queued(request))
  });
  const { restore } = installBrowser(fetcher);
  try {
    const api = await loadApi();
    const auth = windowAuth();
    const first = (await api.apiGet(auth, "csrf")).csrf_token;
    const result = await api.apiPost(auth, first, api.ACTIONS.clientEvent, { event: "interface-settings" }, false);
    assert.equal(result.state, "queued");
    const posts = fetcher.seen.filter((entry) => entry.kind === "post");
    assert.equal(posts.length, 2);
    assert.equal(posts[0].csrf, first);
    assert.equal(posts[1].csrf, token(2));
    assert.notEqual(posts[1].csrf, posts[0].csrf);
    assert.equal(posts[1].request.request_id, posts[0].request.request_id, "the replay is the same request");
    assert.deepEqual(posts[1].request, posts[0].request, "with the byte-identical body");
    assert.deepEqual(
      fetcher.seen.map((entry) => entry.kind),
      ["csrf", "post", "request-status", "csrf", "post"]
    );
  } finally {
    restore();
  }
});

test("without a way to replace a refused token, the request is not replayed at all", async () => {
  const fetcher = packageRoutes({ post: () => response("<html>Forbidden</html>", 403, "text/html") });
  const { restore } = installBrowser(fetcher);
  try {
    const api = await loadApi();
    // A caller that never fetched its token through this module: nothing to reissue under.
    await assert.rejects(
      api.apiPost({}, token(9), api.ACTIONS.clientEvent, { event: "interface-settings" }, false, undefined, {
        csrfReissueTimeoutMs: 1000,
        postRequestTimeoutMs: 1000,
        postResponseTimeoutMs: 1000,
        readTimeoutMs: 1000,
        resultRequestTimeoutMs: 1000,
        resultObservationTimeoutMs: 1000,
        requestReconciliationTimeoutMs: 1,
        requestReconciliationPollIntervalMs: 1
      }),
      (error) => error.outcomeUnknown === true
    );
    assert.equal(fetcher.seen.filter((entry) => entry.kind === "post").length, 1);
  } finally {
    restore();
  }
});

test("a token the package explicitly refused is replaced by the next request", async () => {
  const fetcher = packageRoutes({
    post: (request, count) => (count === 1
      ? response({ schema: "sdsync.dsm-error.v1", ok: false, code: "csrf_rejected", message: "Request could not be completed." }, 403)
      : queued(request))
  });
  const { restore } = installBrowser(fetcher);
  try {
    const api = await loadApi();
    const auth = windowAuth();
    const first = (await api.apiGet(auth, "csrf")).csrf_token;
    await assert.rejects(
      api.apiPost(auth, first, api.ACTIONS.clientEvent, { event: "interface-settings" }, false),
      (error) => error.csrfRejected === true && error.preAcceptance === true
    );
    assert.equal(fetcher.seen.filter((entry) => entry.kind === "post").length, 1, "never retried automatically");
    assert.equal(api.csrfTokenStale(auth, first), true);
    await api.apiPost(auth, first, api.ACTIONS.clientEvent, { event: "interface-settings" }, false);
    const posts = fetcher.seen.filter((entry) => entry.kind === "post");
    assert.equal(posts[1].csrf, token(2));
  } finally {
    restore();
  }
});

test("every package request names the page's build", async () => {
  const fetcher = packageRoutes({ post: queued });
  const { restore } = installBrowser(fetcher);
  try {
    const api = await loadApi();
    const auth = windowAuth();
    const first = (await api.apiGet(auth, "csrf")).csrf_token;
    await api.apiPost(auth, first, api.ACTIONS.clientEvent, { event: "interface-settings" }, false);
    assert.equal(api.DASHBOARD_BUILD, "1");
    assert.ok(fetcher.seen.length >= 2);
    for (const entry of fetcher.seen) assert.equal(entry.build, api.DASHBOARD_BUILD, entry.kind);
  } finally {
    restore();
  }
});

test("a tab that becomes visible again drops a stale token before it refreshes", () => {
  const visible = appSource.slice(
    appSource.indexOf("this.visibilityHandler = () => {"),
    appSource.indexOf('document.addEventListener("visibilitychange", this.visibilityHandler);')
  );
  const drop = visible.indexOf("if (this.csrfToken && csrfTokenStale(this.auth, this.csrfToken)) this.csrfToken = \"\";");
  const refresh = visible.indexOf("this.refreshSnapshot(false);");
  assert.ok(drop > 0, "the visible branch must check the token");
  assert.ok(refresh > drop, "the snapshot refresh, which fetches a missing token first, must follow");
  assert.match(appSource, /if \(!this\.csrfToken\) await this\.refreshCsrf\(\);/);
});
