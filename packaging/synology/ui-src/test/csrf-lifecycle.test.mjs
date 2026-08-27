import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");

function jsonResponse(model, status = 200) {
  return {
    redirected: false,
    status,
    ok: status >= 200 && status < 300,
    headers: { get: (name) => name.toLowerCase() === "content-type" ? "application/json" : null },
    async text() { return JSON.stringify(model); }
  };
}

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function installBrowserGlobals() {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = {
    crypto: globalThis.crypto || webcrypto,
    TextEncoder: globalThis.TextEncoder,
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout
  };
  return () => {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  };
}

async function loadAppComponent(postSpy, getSpy) {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "App.vue script block is missing");
  let executable = script[1]
    .replace(/import\s*\{[\s\S]*?\}\s*from\s*"\.\/api";\s*/, "")
    .replace(/import\s+SecurityPanel\s+from\s+"\.\/SecurityPanel\.vue";\s*/, "")
    .replace("export default {", "const AppComponent = {");
  executable += "\nreturn AppComponent;";

  const stubs = {
    ACTIONS: {
      securityPolicy: "security-policy",
      clientEvent: "client-event",
      execute: "action"
    },
    MAX_RESPONSE_BYTES: 1024 * 1024,
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    apiGet: getSpy,
    apiPost: postSpy,
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback).slice(0, 65536),
    formatBytes: String,
    formatDate: String,
    formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    ActionIcon: { name: "ActionIcon" },
    SecurityPanel: {}
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

function securityPolicy(csrfLifetime = 300) {
  return {
    policy_version: 1,
    require_https: false,
    allow_interface_changes: true,
    allow_profile_changes: true,
    allow_secret_changes: true,
    allow_routine_changes: true,
    allow_notification_changes: true,
    allow_operational_actions: true,
    allow_http_targets: true,
    allow_empty_source: true,
    allow_invalid_tls: true,
    allow_destructive_sync: true,
    allow_doctor_write_test: true,
    allow_remote_logging: true,
    csrf_lifetime_seconds: csrfLifetime,
    result_retention_seconds: 3600,
    max_outstanding_jobs: 64,
    log_levels: Object.fromEntries([
      "audit", "bridge", "authentication", "security", "configuration", "secrets",
      "routines", "operations", "notifications", "sync", "controller", "scheduler"
    ].map((category) => [category, "info"]))
  };
}

function securityContext(component, overrides = {}) {
  const methods = component.methods;
  const context = Object.assign({
    canMutate: true,
    canChangeInterface: true,
    securityDirty: true,
    operationBusy: false,
    disposed: false,
    auth: {},
    csrfToken: "csrf-before-policy-save",
    securityForm: securityPolicy(420),
    securityPolicy: securityPolicy(300),
    snapshot: { security_policy: securityPolicy(300) },
    connected: true,
    freshness: "Current",
    bridgeIssue: { title: "", message: "" },
    connectionLabel: "Authenticated package bridge",
    settings: {
      theme: "dark",
      status_refresh: 5000,
      log_refresh: 5000,
      desktop_notifications: false,
      audible: false
    },
    toasts: [],
    toast(title, message, error = false) { this.toasts.push({ title, message, error }); },
    confirmAction: async () => true,
    refreshSnapshot: async () => {},
    hydrateSecurityPolicy: () => {},
    persistSettings: () => true,
    captureSettingsTransaction() { return { raw: null, settings: Object.assign({}, this.settings) }; },
    applySettingsState(settings) { this.settings = Object.assign({}, settings); },
    restoreSettingsTransaction(transaction) { this.applySettingsState(transaction.settings); return true; },
    preferenceAuditWasRejected(error) { return Boolean(error && error.preAcceptance === true && error.trustedRejection === true); },
    scheduleSnapshot: () => {},
    scheduleLogs: () => {}
  }, overrides);
  for (const name of [
    "between", "securityPayload", "validateSecurityPayload", "securityRelaxed",
    "saveSecurityPolicy", "saveInterfaceSettings", "reportMutationError", "refreshCsrf"
  ]) context[name] = (...args) => methods[name].apply(context, args);
  return context;
}

test("failed initial CSRF bootstrap is reported once before the scheduled retry", async () => {
  const previousWindow = globalThis.window;
  const previousDocument = globalThis.document;
  let csrfReads = 0;
  let snapshotReads = 0;
  let scheduledRetries = 0;
  try {
    globalThis.window = {
      AbortController: globalThis.AbortController,
      matchMedia: () => ({
        matches: false,
        addEventListener() {},
        removeEventListener() {}
      })
    };
    globalThis.document = {
      hidden: false,
      addEventListener() {},
      removeEventListener() {}
    };
    const component = await loadAppComponent(
      async () => ({}),
      async () => {
        csrfReads += 1;
        throw { status: 400, code: "invalid_request", message: "Request could not be completed." };
      }
    );
    const context = {
      disposed: false,
      abortController: null,
      auth: { signal: undefined },
      csrfToken: "",
      connected: true,
      bridgeIssue: { title: "", message: "" },
      connectionLabel: "Connected",
      toasts: [],
      refreshCsrf(...args) { return component.methods.refreshCsrf.apply(this, args); },
      describeBridgeError(...args) { return component.methods.describeBridgeError.apply(this, args); },
      toast(title, message, error = false) { this.toasts.push({ title, message, error }); },
      async refreshSnapshot() { snapshotReads += 1; },
      scheduleSnapshot() { scheduledRetries += 1; },
      stopTimers() {}
    };

    await component.mounted.call(context);

    assert.equal(csrfReads, 1, "mount must not immediately repeat a rejected CSRF request");
    assert.equal(snapshotReads, 0, "snapshot bootstrap must wait for a valid CSRF token");
    assert.equal(scheduledRetries, 1, "the normal bounded refresh cadence owns the retry");
    assert.equal(context.bridgeIssue.title, "DSM request metadata rejected");
    assert.equal(context.toasts.length, 1);
    assert.equal(
      context.describeBridgeError({ status: 400, code: "non_json_response" }).title,
      "DSM request metadata rejected",
      "an upstream DSM HTML 400 must retain its authoritative HTTP classification"
    );
  } finally {
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
    if (previousDocument === undefined) delete globalThis.document;
    else globalThis.document = previousDocument;
  }
});

test("HTTP 503 remains a package-service failure when Webman returns non-JSON", async () => {
  const component = await loadAppComponent(async () => ({}), async () => ({}));

  assert.equal(
    component.methods.describeBridgeError({ status: 503, code: "non_json_response" }).title,
    "Package service unavailable",
    "an HTML 503 must not be mislabeled as a missing package UI route"
  );
  assert.equal(
    component.methods.describeBridgeError({ status: 404, code: "non_json_response" }).title,
    "Package UI route unavailable"
  );
  assert.equal(
    component.methods.describeBridgeError({ status: 200, code: "non_json_response" }).title,
    "Package UI route unavailable",
    "a successful-but-malformed response remains a route/bridge document failure"
  );
  assert.equal(
    component.methods.describeBridgeError({ status: 500, code: "non_json_response" }).title,
    "Package bridge unavailable",
    "an unclassified server failure must not be rewritten from its parser shape"
  );
});

test("GET semantic failures preserve application status and stage over HTTP 200", async () => {
  const restore = installBrowserGlobals();
  try {
    globalThis.fetch = async () => jsonResponse({
      schema: "sdsync.dsm-error.v1",
      ok: false,
      status: 503,
      code: "service_unavailable",
      stage: "bridge_connect",
      message: "The package service is unavailable."
    });
    const api = await loadApi();

    await assert.rejects(
      api.apiGet({}, "csrf"),
      (error) => {
        assert.equal(error instanceof api.DsmApiError, true);
        assert.equal(error.status, 503);
        assert.equal(error.transportStatus, 200);
        assert.equal(error.stage, "bridge_connect");
        assert.equal(error.code, "service_unavailable");
        assert.equal(error.trustedRejection, true);
        return true;
      }
    );

    const component = await loadAppComponent(async () => ({}), async () => ({}));
    const issue = component.methods.describeBridgeError({
      status: 503,
      code: "service_unavailable",
      stage: "bridge_connect"
    });
    assert.equal(issue.title, "Package service unavailable");
    assert.match(issue.message, /Failure stage: bridge_connect\./);
  } finally {
    restore();
  }
});

test("HTTP 200 semantic error envelopes are never returned as successful GET models", async () => {
  const restore = installBrowserGlobals();
  try {
    globalThis.fetch = async () => jsonResponse({
      schema: "sdsync.dsm-error.v1",
      ok: false,
      status: 400,
      code: "invalid_request",
      stage: "request",
      message: "Request metadata was rejected."
    });
    const api = await loadApi();
    await assert.rejects(
      api.apiGet({}, "snapshot"),
      (error) => error instanceof api.DsmApiError
        && error.status === 400
        && error.stage === "request"
    );
  } finally {
    restore();
  }
});

test("GET semantic status must be numeric before it becomes a trusted rejection", async () => {
  const restore = installBrowserGlobals();
  try {
    globalThis.fetch = async () => jsonResponse({
      schema: "sdsync.dsm-error.v1",
      ok: false,
      status: "503",
      code: "service_unavailable",
      stage: "bridge_connect",
      message: "The package service is unavailable."
    });
    const api = await loadApi();
    await assert.rejects(
      api.apiGet({}, "csrf"),
      (error) => {
        assert.equal(error instanceof api.DsmApiError, true);
        assert.equal(error.status, 200);
        assert.equal(error.transportStatus, 200);
        assert.equal(error.trustedRejection, false);
        return true;
      }
    );
  } finally {
    restore();
  }
});

test("GET result never accepts an ok-false semantic 410 as an expired result model", async () => {
  const restore = installBrowserGlobals();
  try {
    globalThis.fetch = async () => jsonResponse({
      schema: "sdsync.dsm-error.v1",
      ok: false,
      status: 410,
      code: "result_expired",
      stage: "service_request",
      message: "The result is no longer available."
    });
    const api = await loadApi();
    await assert.rejects(
      api.apiGet({}, "result", { job_id: "a".repeat(48) }),
      (error) => error instanceof api.DsmApiError
        && error.status === 410
        && error.transportStatus === 200
        && error.trustedRejection === true
    );
  } finally {
    restore();
  }
});

test("POST keeps transport-status semantics for an untrusted HTTP 200 error document", async () => {
  const restore = installBrowserGlobals();
  try {
    globalThis.fetch = async () => jsonResponse({
      schema: "sdsync.dsm-error.v1",
      ok: false,
      status: 403,
      code: "csrf_rejected",
      stage: "mutation-authentication",
      message: "Mutation token rejected."
    });
    const api = await loadApi();
    await assert.rejects(
      api.apiPost({}, "csrf-token", api.ACTIONS.clientEvent, { event: "interface-settings" }, false),
      (error) => {
        assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
        assert.equal(error.outcomeUnknown, true);
        assert.equal(error.acceptanceUnknown, true);
        return true;
      }
    );
  } finally {
    restore();
  }
});

test("security lifetime change replaces the token before the next serialized mutation", async () => {
  const trace = [];
  const posts = [];
  const component = await loadAppComponent(
    async (_auth, csrf, action, payload) => {
      trace.push(`post:${action}:${csrf}`);
      posts.push({ csrf, action, payload });
      return { ok: true };
    },
    async (_auth, action) => {
      assert.equal(action, "csrf");
      trace.push("get:csrf");
      return { csrf_token: "csrf-after-policy-save" };
    }
  );
  const context = securityContext(component, {
    async refreshSnapshot() {
      assert.equal(this.operationBusy, true, "snapshot refresh must remain inside mutation serialization");
      assert.equal(this.csrfToken, "csrf-after-policy-save");
      trace.push("snapshot");
    }
  });

  await context.saveSecurityPolicy({ preventDefault() {} });
  assert.equal(context.securityDirty, false);
  assert.equal(context.operationBusy, false);
  assert.equal(context.csrfToken, "csrf-after-policy-save");

  await context.saveInterfaceSettings({ preventDefault() {} });
  assert.equal(context.operationBusy, false);
  assert.deepEqual(trace, [
    "post:security-policy:csrf-before-policy-save",
    "get:csrf",
    "snapshot",
    "post:client-event:csrf-after-policy-save"
  ]);
  assert.equal(posts.length, 2);
  assert.equal(posts[0].payload.csrf_lifetime_seconds, 420);
  assert.equal(posts[1].payload.event, "interface-settings");
});

test("an explicit pre-acceptance CSRF 403 clears the token and never retries POST", async () => {
  const restore = installBrowserGlobals();
  try {
    const api = await loadApi();
    let fetchCount = 0;
    globalThis.fetch = async () => {
      fetchCount += 1;
      return jsonResponse({
        schema: "sdsync.dsm-error.v1",
        ok: false,
        code: "csrf_rejected",
        message: "CSRF mutation token expired."
      }, 403);
    };

    let rejection;
    try {
      await api.apiPost(
        {},
        "stale-csrf",
        api.ACTIONS.clientEvent,
        { event: "interface-settings" }
      );
      assert.fail("expected explicit CSRF rejection");
    } catch (error) {
      rejection = error;
    }
    assert.equal(fetchCount, 1);
    assert.equal(rejection.preAcceptance, true);
    assert.equal(rejection.csrfRejected, true);
    assert.equal(rejection.outcomeUnknown, undefined);
    assert.match(rejection.requestId, /^[0-9a-f]{32}$/);
    assert.equal(rejection.trustedRequestId, true);

    let postCount = 0;
    let csrfRefreshCount = 0;
    const component = await loadAppComponent(
      async () => {
        postCount += 1;
        throw rejection;
      },
      async () => {
        csrfRefreshCount += 1;
        return { csrf_token: "must-not-be-fetched-automatically" };
      }
    );
    const context = securityContext(component);
    await context.saveSecurityPolicy({ preventDefault() {} });

    assert.equal(postCount, 1, "a rejected POST must never be retried automatically");
    assert.equal(csrfRefreshCount, 0, "the user-visible Retry action owns CSRF reacquisition");
    assert.equal(context.csrfToken, "");
    assert.equal(context.operationBusy, false);
    assert.equal(context.bridgeIssue.title, "Mutation token rejected");
    assert.match(context.bridgeIssue.message, /Select Retry/);
    assert.match(context.bridgeIssue.message, /was not accepted and was not retried/);
  } finally {
    restore();
  }
});

test("a lost 202 acknowledgement is outcome-unknown with the original client request ID", async () => {
  const restore = installBrowserGlobals();
  try {
    const api = await loadApi();
    let dispatchedRequestId = "";
    let fetchCount = 0;
    globalThis.fetch = async (_url, options) => {
      fetchCount += 1;
      dispatchedRequestId = JSON.parse(options.body).request_id;
      return {
        redirected: false,
        status: 202,
        ok: true,
        headers: { get: () => "application/json" },
        async text() { throw new TypeError("socket closed after the server queued the request"); }
      };
    };

    await assert.rejects(
      api.apiPost(
        {},
        "csrf-token",
        api.ACTIONS.clientEvent,
        { event: "interface-settings" }
      ),
      (error) => {
        assert.equal(error instanceof api.MutationOutcomeUnknownError, true);
        assert.equal(error.outcomeUnknown, true);
        assert.equal(error.acceptanceUnknown, true);
        assert.equal(error.requestId, dispatchedRequestId);
        assert.equal(error.trustedRequestId, true);
        assert.match(error.requestId, /^[0-9a-f]{32}$/);
        assert.match(error.message, new RegExp(dispatchedRequestId));
        assert.match(error.message, /Do not retry it automatically/);
        return true;
      }
    );
    assert.equal(fetchCount, 1, "a lost acknowledgement must never trigger an automatic POST retry");
  } finally {
    restore();
  }
});

test("post-202 observation, invalid-result, and expired-result errors retain exact correlation IDs", async (t) => {
  const restore = installBrowserGlobals();
  try {
    for (const scenario of ["observation", "invalid", "expired"]) {
      await t.test(scenario, async () => {
        const api = await loadApi();
        const jobId = scenario === "observation" ? "a".repeat(48)
          : (scenario === "invalid" ? "b".repeat(48) : "c".repeat(48));
        let dispatchedRequestId = "";
        let postCount = 0;
        let resultCount = 0;
        globalThis.fetch = async (_url, options = {}) => {
          if (options.method === "POST") {
            postCount += 1;
            dispatchedRequestId = JSON.parse(options.body).request_id;
            return jsonResponse({
              schema: api.QUEUED_SCHEMA,
              state: "queued",
              request_id: dispatchedRequestId,
              job_id: jobId
            }, 202);
          }
          resultCount += 1;
          if (scenario === "observation") throw new TypeError("result socket unavailable");
          if (scenario === "invalid") {
            return jsonResponse({ schema: "invalid.result-status", state: "pending", job_id: jobId });
          }
          return jsonResponse({
            schema: api.RESULT_STATUS_SCHEMA,
            state: "expired_or_missing",
            job_id: jobId,
            result: { message: "Result retention elapsed." }
          });
        };

        await assert.rejects(
          api.apiPost(
            {},
            "csrf-token",
            api.ACTIONS.clientEvent,
            { event: "interface-settings" },
            true,
            0
          ),
          (error) => {
            assert.equal(error instanceof api.QueuedOutcomeUnknownError, true);
            assert.equal(error.outcomeUnknown, true);
            assert.equal(error.accepted, true);
            assert.equal(error.requestId, dispatchedRequestId);
            assert.equal(error.jobId, jobId);
            assert.equal(error.trustedRequestId, true);
            assert.equal(error.trustedJobId, true);
            return true;
          }
        );
        assert.equal(postCount, 1, "queued observation must never repeat POST");
        assert.equal(resultCount, scenario === "observation" ? 5 : 1);
      });
    }
  } finally {
    restore();
  }
});

test("mutation errors visibly report only validated, provenance-marked correlation IDs", async () => {
  const component = await loadAppComponent(async () => ({}), async () => ({}));
  const requestId = "d".repeat(32);
  const jobId = "e".repeat(48);
  const context = {
    csrfToken: "csrf",
    bridgeIssue: { title: "", message: "" },
    connectionLabel: "Connected",
    toasts: [],
    toast(title, message, error) { this.toasts.push({ title, message, error }); }
  };

  const trusted = component.methods.reportMutationError.call(
    context,
    {
      outcomeUnknown: true,
      accepted: true,
      message: "Queued result observation failed.",
      requestId,
      trustedRequestId: true,
      jobId,
      trustedJobId: true
    },
    "Mutation failed",
    "Mutation outcome unknown",
    "Mutation failed."
  );
  assert.equal(trusted.requestId, requestId);
  assert.equal(trusted.jobId, jobId);
  assert.match(context.toasts[0].message, new RegExp(`Client request ID: ${requestId}\\.`));
  assert.match(context.toasts[0].message, new RegExp(`Queued job ID: ${jobId}\\.`));

  const injectedRequest = "<img src=x onerror=alert(1)>";
  const formattedButUntrustedJob = "f".repeat(48);
  const rejected = component.methods.reportMutationError.call(
    context,
    {
      outcomeUnknown: true,
      message: "No correlation metadata was trusted.",
      requestId: injectedRequest,
      trustedRequestId: true,
      jobId: formattedButUntrustedJob,
      trustedJobId: false
    },
    "Mutation failed",
    "Mutation outcome unknown",
    "Mutation failed."
  );
  assert.equal(rejected.requestId, "");
  assert.equal(rejected.jobId, "");
  assert.doesNotMatch(context.toasts[1].message, /<img|onerror|f{48}/);
  assert.doesNotMatch(appSource, /v-html/);
});
