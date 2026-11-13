// Start-up and recovery read the mutation token and the first snapshot together.
//
// The snapshot GET needs no token, so the old order (token, then snapshot, and again after every
// failure that cleared the token) made first data wait for two CGI round trips in a row on a NAS
// where each one is a process, a DSM user-service call and a relay. These tests fix the order in
// which the requests are *issued*, which is what the critical path is made of, and that a failure
// of either read is still reported by its own name and never hides the other's answer.
import assert from "node:assert/strict";
import test from "node:test";
import { deferred, installBrowser, loadAppComponent, mountContext, snapshotContext, validSnapshot } from "./helpers/app-component.mjs";

// What the bridge is asked, in the order it is asked; each answer is released by the test.
function bridge() {
  const asked = [];
  const answers = { csrf: deferred(), snapshot: deferred() };
  return {
    asked,
    answers,
    apiGet: (_auth, action) => {
      asked.push(action);
      return answers[action].promise;
    }
  };
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

test("start-up asks for the token and the snapshot in the same tick, before either answers", async () => {
  const restore = installBrowser();
  try {
    const { asked, answers, apiGet } = bridge();
    const component = loadAppComponent({ apiGet });
    const context = mountContext(component);

    const mounting = component.mounted.call(context);
    await flush();
    assert.deepEqual(asked.slice().sort(), ["csrf", "snapshot"], "both reads must be in flight before either answers");

    answers.snapshot.resolve(validSnapshot());
    answers.csrf.resolve({ csrf_token: "token-1" });
    await mounting;
    assert.equal(context.csrfToken, "token-1");
    assert.ok(context.snapshot, "the snapshot is applied");
    assert.equal(context.connected, true);
    assert.equal(context.scheduled, 1, "one poll is armed, by the snapshot cycle");
    assert.equal(context.csrfAttempt, null);
    assert.equal(asked.length, 2, "nothing is asked twice");
  } finally {
    restore();
  }
});

test("a token the package refuses at start-up is reported once, and the snapshot it did not hold back is shown", async () => {
  const restore = installBrowser();
  try {
    const refusal = { status: 400, code: "invalid_request", message: "Request could not be completed." };
    const asked = [];
    const component = loadAppComponent({
      apiGet: async (_auth, action) => {
        asked.push(action);
        if (action === "csrf") throw refusal;
        return validSnapshot();
      }
    });
    const context = mountContext(component);

    await component.mounted.call(context);
    assert.deepEqual(asked.slice().sort(), ["csrf", "snapshot"]);
    assert.ok(context.snapshot, "a snapshot that arrived is shown, whatever became of the token");
    assert.equal(context.csrfToken, "");
    assert.equal(context.connected, false, "no token, no edits");
    assert.equal(context.bridgeIssue.title, "DSM request metadata rejected", "the issue keeps the start-up wording");
    assert.equal(context.toasts.length, 1, "reported once, not again by the snapshot cycle");
    assert.equal(context.snapshotFailureCode, "invalid_request", "the poll ladder sees it, so the token is read again on the next poll");
    assert.equal(context.scheduled, 1);
  } finally {
    restore();
  }
});

test("a snapshot that fails at start-up is its own failure, beside a token that arrived", async () => {
  const restore = installBrowser();
  try {
    const component = loadAppComponent({
      apiGet: async (_auth, action) => {
        if (action === "csrf") return { csrf_token: "token-2" };
        throw Object.assign(new Error("busy"), { status: 503, code: "client_timeout" });
      }
    });
    const context = mountContext(component);

    await component.mounted.call(context);
    assert.equal(context.snapshot, null);
    assert.equal(context.snapshotFailureCode, "client_timeout");
    assert.equal(context.csrfToken, "token-2", "a slow read keeps the token the window already holds");
    assert.equal(context.toasts.length, 0, "the start-up token read did not fail, so nothing is reported for it");
    assert.equal(context.scheduled, 1);
  } finally {
    restore();
  }
});

test("recovery after a cleared token reads the token and the snapshot together", async () => {
  const restore = installBrowser();
  try {
    const { asked, answers, apiGet } = bridge();
    const component = loadAppComponent({ apiGet });
    const context = snapshotContext(component, { csrfToken: "", snapshot: validSnapshot() });

    const cycle = context.refreshSnapshot(false);
    await flush();
    assert.deepEqual(asked.slice().sort(), ["csrf", "snapshot"], "one round trip for both, not two in a row");
    answers.csrf.resolve({ csrf_token: "token-3" });
    answers.snapshot.resolve(validSnapshot({ generated_at_epoch: 1757000005 }));
    assert.equal(await cycle, true);
    assert.equal(context.csrfToken, "token-3");
  } finally {
    restore();
  }
});

test("a token read that fails during recovery is reported through the status failure path, with the snapshot kept", async () => {
  const restore = installBrowser();
  try {
    const component = loadAppComponent({
      apiGet: async (_auth, action) => {
        if (action === "csrf") throw { status: 400, code: "invalid_request" };
        return validSnapshot();
      }
    });
    const context = snapshotContext(component, { csrfToken: "" });

    assert.equal(await context.refreshSnapshot(true), false, "the cycle did not fully succeed");
    assert.ok(context.snapshot, "the snapshot that arrived is shown");
    assert.equal(context.snapshotFailureCode, "invalid_request");
    assert.equal(context.connected, false);
    assert.equal(context.toasts.length, 1, "a manual retry says what failed");
    assert.equal(context.toasts[0].error, true);
    assert.notEqual(context.bridgeIssue.title, "", "the failure is named on the page");
  } finally {
    restore();
  }
});

test("a window that already holds a token does not read another", async () => {
  const restore = installBrowser();
  try {
    const asked = [];
    const component = loadAppComponent({
      apiGet: async (_auth, action) => { asked.push(action); return validSnapshot(); }
    });
    const context = snapshotContext(component, { csrfToken: "held" });
    assert.equal(await context.refreshSnapshot(false), true);
    assert.deepEqual(asked, ["snapshot"]);
    assert.equal(context.csrfToken, "held");
  } finally {
    restore();
  }
});
