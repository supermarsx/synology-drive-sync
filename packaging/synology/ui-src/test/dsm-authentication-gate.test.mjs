// After DSM refuses the session this page presents, the page presents it again
// only for an explicit action, and then once. Over a QuickConnect relay every
// presentation is a session check DSM fails ("IP is not the same"); a poll or a
// background card repeating it can only harm the session it is refused for.
import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { installBrowser as installAppBrowser, loadAppComponent, mountContext } from "./helpers/app-component.mjs";
import { widgetBridgeIssue } from "../src/widgetModel.mjs";

const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function jsonResponse(model, status = 200) {
  const body = JSON.stringify(model);
  return {
    redirected: false,
    status,
    ok: status >= 200 && status < 300,
    headers: {
      get(name) {
        if (name.toLowerCase() === "content-type") return "application/json; charset=utf-8";
        if (name.toLowerCase() === "content-length") return String(new TextEncoder().encode(body).byteLength);
        return null;
      }
    },
    async text() { return body; }
  };
}

function refusal(code = "dsm_authentication_quickconnect_unsupported", status = 401) {
  return jsonResponse({
    schema: "sdsync.dsm-error.v1",
    ok: false,
    status,
    code,
    stage: "dsm_authentication",
    message: "DSM authentication failed"
  }, status);
}

const SNAPSHOT = { schema: "sdsync.dsm-api.v1", ok: true };

function installBrowser(packageFetch) {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  const state = { tokenReads: 0 };
  globalThis.window = {
    AbortController: globalThis.AbortController,
    TextEncoder: globalThis.TextEncoder,
    crypto: globalThis.crypto || webcrypto,
    fetch: async () => {
      state.tokenReads += 1;
      return jsonResponse({ success: true, data: { synotoken: `token-${state.tokenReads}` } });
    },
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout
  };
  globalThis.fetch = packageFetch;
  state.restore = () => {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  };
  return state;
}

for (const code of ["dsm_authentication_quickconnect_unsupported", "dsm_authentication_rejected", "dsm_authentication_webapi_rejected"]) {
  test(`after ${code} no package request leaves the page until an explicit action`, async () => {
    const sent = [];
    const browser = installBrowser(async (url, options) => {
      sent.push({ url, method: options.method });
      return refusal(code);
    });
    try {
      const api = await loadApi();
      await assert.rejects(api.apiGet({}, "snapshot"), (error) => error.status === 401 && error.code === code);
      assert.equal(sent.length, 1);
      assert.equal(api.dsmAuthenticationRefused(), true);

      // Polls, the card, a token read and a mutation: none reaches the network.
      for (let index = 0; index < 5; index += 1) {
        await assert.rejects(api.apiGet({}, "snapshot"), (error) => {
          assert.equal(error.status, 401);
          assert.equal(error.code, code);
          assert.equal(error.notSent, true);
          assert.equal(error.trustedRejection, true);
          return true;
        });
      }
      await assert.rejects(api.apiGet({}, "csrf"), (error) => error.notSent === true);
      await assert.rejects(
        api.apiPost({}, "package-csrf", api.ACTIONS.setDefault, { name: "profile" }, false),
        (error) => error.notSent === true && error.preAcceptance === true && error.code === code
      );
      assert.equal(sent.length, 1, "a refused session was presented again without an explicit action");
    } finally {
      browser.restore();
    }
  });
}

test("an explicit action asks DSM once: concurrent requests wait for that one verdict", async () => {
  const sent = [];
  const browser = installBrowser(async (url) => {
    sent.push(url);
    return refusal();
  });
  try {
    const api = await loadApi();
    await assert.rejects(api.apiGet({}, "snapshot"));
    assert.equal(sent.length, 1);
    const tokenReadsBefore = browser.tokenReads;

    assert.equal(api.rearmDsmAuthentication(), true);
    assert.equal(api.rearmDsmAuthentication(), false, "re-arming an asking gate changes nothing");
    // The AppWindow's start-up reads the token and the snapshot together.
    const results = await Promise.allSettled([
      api.apiGet({}, "csrf"),
      api.apiGet({}, "snapshot"),
      api.apiGet({}, "snapshot")
    ]);
    assert.ok(results.every((result) => result.status === "rejected"));
    assert.equal(sent.length, 2, "one explicit action presented the session more than once");
    assert.equal(browser.tokenReads, tokenReadsBefore + 1, "an explicit action fetches the DSM token again");
    assert.equal(api.dsmAuthenticationRefused(), true);
  } finally {
    browser.restore();
  }
});

test("an accepted session reopens the gate, releases the waiters and tells the card", async () => {
  let refuse = true;
  const sent = [];
  const browser = installBrowser(async (url) => {
    sent.push(url);
    return refuse ? refusal("dsm_authentication_rejected") : jsonResponse(SNAPSHOT);
  });
  try {
    const api = await loadApi();
    let accepted = 0;
    const stop = api.onDsmAuthenticationAccepted(() => { accepted += 1; });
    await assert.rejects(api.apiGet({}, "snapshot"));
    refuse = false;
    api.rearmDsmAuthentication();
    const [first, second] = await Promise.all([api.apiGet({}, "snapshot"), api.apiGet({}, "snapshot")]);
    assert.equal(first.schema, SNAPSHOT.schema);
    assert.equal(second.schema, SNAPSHOT.schema);
    assert.equal(sent.length, 3);
    assert.equal(accepted, 1);
    assert.equal(api.dsmAuthenticationRefused(), false);
    stop();
  } finally {
    browser.restore();
  }
});

test("only a dsm_authentication_* 401 holds the page", async () => {
  for (const [code, status] of [["unauthorized", 401], ["manager_busy", 503], ["csrf_rejected", 403], ["dsm_authentication_forbidden", 403], ["dsm_authentication_webapi_unavailable", 503]]) {
    const sent = [];
    const browser = installBrowser(async (url) => {
      sent.push(url);
      return refusal(code, status);
    });
    try {
      const api = await loadApi();
      await assert.rejects(api.apiGet({}, "snapshot"));
      await assert.rejects(api.apiGet({}, "snapshot"));
      assert.equal(sent.length, 2, `${code} must not hold the page`);
      assert.equal(api.dsmAuthenticationRefused(), false, code);
    } finally {
      browser.restore();
    }
  }
  // A DSM authentication 401 this release has no name for holds it too.
  const sent = [];
  const browser = installBrowser(async (url) => {
    sent.push(url);
    return refusal("dsm_authentication_from_a_newer_package", 401);
  });
  try {
    const api = await loadApi();
    await assert.rejects(api.apiGet({}, "snapshot"));
    await assert.rejects(api.apiGet({}, "snapshot"), (error) => error.notSent === true);
    assert.equal(sent.length, 1);
  } finally {
    browser.restore();
  }
});

test("the AppWindow stops its own poll on a DSM authentication refusal and Retry re-arms", () => {
  const component = loadAppComponent();
  const retryDelay = component.methods.snapshotRetryDelay;
  for (const code of ["dsm_authentication_quickconnect_unsupported", "dsm_authentication_rejected", "dsm_authentication_webapi_rejected"]) {
    const vm = { windowHidden: false, snapshotFailureCode: code, snapshotFailureStatus: 401, snapshotFailureStreak: 1 };
    assert.equal(retryDelay.call(vm, 5000), 0, code);
  }
  // A session that merely expired without a name keeps the ladder it always had.
  const vm = { windowHidden: false, snapshotFailureCode: "unauthorized", snapshotFailureStatus: 401, snapshotFailureStreak: 1 };
  assert.equal(retryDelay.call(vm, 5000), 5000);
});

// The whole AppWindow against the real transport: after one refused open, cadence ticks,
// timers, visibility changes and background reads put nothing on the network; Retry puts
// exactly one request on it.
test("after one DSM authentication refusal the AppWindow makes no request until Retry, and Retry makes one", async () => {
  const sent = [];
  const fetchPackage = async (url, options) => {
    sent.push({ url, method: options.method });
    return refusal("dsm_authentication_quickconnect_unsupported");
  };
  const restore = installAppBrowser({
    window: {
      TextEncoder: globalThis.TextEncoder,
      crypto: globalThis.crypto || webcrypto,
      fetch: async () => jsonResponse({ success: true, data: { synotoken: "token" } })
    }
  });
  const previousFetch = globalThis.fetch;
  globalThis.fetch = fetchPackage;
  try {
    const api = await loadApi();
    const component = loadAppComponent({
      apiGet: api.apiGet,
      apiPost: api.apiPost,
      csrfTokenStale: api.csrfTokenStale,
      isDsmAuthenticationRefusal: api.isDsmAuthenticationRefusal,
      rearmDsmAuthentication: api.rearmDsmAuthentication
    });
    const context = mountContext(component, {
      scheduleSnapshot() { component.methods.scheduleSnapshot.call(this); },
      scheduleIncidentProbe() {},
      clearSecrets() {},
      stopTimers() {}
    });
    await component.mounted.call(context);
    // Opening the app is the one attempt: the token and the snapshot go out together.
    const afterOpen = sent.length;
    assert.ok(afterOpen >= 1 && afterOpen <= 2, `opening sent ${afterOpen} requests`);
    assert.equal(context.snapshotFailureCode, "dsm_authentication_quickconnect_unsupported");
    assert.equal(context.bridgeIssue.title, "QuickConnect relay unsupported");
    assert.match(context.bridgeIssue.message, /does not retry by itself/);
    assert.equal(context.snapshotTimer, 0, "a poll was scheduled after the refusal");

    const runTimers = async () => {
      for (const timer of restore.timers.splice(0)) await timer.callback();
    };
    for (let tick = 0; tick < 10; tick += 1) {
      context.scheduleSnapshot();
      await runTimers();
      // What a timer, a feed read or a background refresh would do.
      await context.refreshSnapshot(false);
      // The tab hidden and shown again.
      globalThis.document.hidden = true;
      context.visibilityHandler();
      globalThis.document.hidden = false;
      context.visibilityHandler();
      await new Promise((resolve) => setImmediate(resolve));
      await runTimers();
    }
    assert.equal(sent.length, afterOpen, "a refused session was presented again without an explicit action");
    assert.equal(context.snapshotTimer, 0);

    // Retry: the explicit action. One request reaches the NAS; the token read beside it waits for
    // that verdict and is answered in the page.
    await context.refreshSnapshot(true);
    assert.equal(sent.length, afterOpen + 1, "Retry presented the session more than once");
    for (let tick = 0; tick < 5; tick += 1) {
      context.scheduleSnapshot();
      await runTimers();
      context.visibilityHandler();
      await new Promise((resolve) => setImmediate(resolve));
    }
    assert.equal(sent.length, afterOpen + 1);
  } finally {
    if (previousFetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previousFetch;
    restore();
  }
});

test("the desktop card names a QuickConnect refusal instead of an expired session", () => {
  const issue = widgetBridgeIssue({ status: 401, code: "dsm_authentication_quickconnect_unsupported" });
  assert.equal(issue.title, "QuickConnect relay unsupported");
  assert.match(issue.detail, /direct QuickConnect/);
  assert.equal(widgetBridgeIssue({ status: 401, code: "dsm_authentication_rejected" }).title, "DSM session expired");
});
