// A mutation the bridge refused before queueing it is reported by the refusal's
// code, in this window's words, and its Activity row says why in the same words.
import assert from "node:assert/strict";
import test from "node:test";
import { appContext, loadApp } from "./helpers/load-app.mjs";

const REQUEST_ID = "e1c45b23c3ef99bb24dd72d610496118";
const TRANSACTION = `bridge-${"a".repeat(64)}`;

function refusal(code, status) {
  return {
    status,
    code,
    message: "Request could not be completed.",
    preAcceptance: true,
    trustedRejection: true,
    requestId: REQUEST_ID,
    trustedRequestId: true
  };
}

function reporter(AppComponent) {
  return appContext(AppComponent, {
    csrfToken: "token",
    bridgeIssue: { title: "", message: "" },
    connectionLabel: "Authenticated package bridge"
  });
}

test("a refused mutation names its cause by code instead of one constant sentence", async () => {
  const { AppComponent } = await loadApp({ exports: "POST_REJECTION_COPY" });
  for (const [code, status, pattern] of [
    ["forbidden", 403, /security policy does not allow/],
    ["invalid_request", 400, /does not accept the request this page sent.*Reload/],
    ["conflict", 409, /same request ID, or a full queue/],
    ["payload_too_large", 413, /larger than the package accepts/]
  ]) {
    const context = reporter(AppComponent);
    const report = context.reportMutationError(refusal(code, status), "Save failed", "Outcome unknown", "Request could not be completed.");
    assert.equal(report.rejected, true, code);
    assert.equal(report.code, code);
    assert.equal(report.unknown, false);
    assert.match(report.message, pattern, code);
    assert.match(report.message, new RegExp(`Client request ID: ${REQUEST_ID}`), code);
    assert.match(report.message, /nothing was changed/, code);
    assert.doesNotMatch(report.message, /Request could not be completed/, code);
    assert.equal(context.toasts.length, 1);
    assert.equal(context.toasts[0].error, true);
    // Only the stale page takes the header: every other refusal is about one change.
    assert.equal(context.bridgeIssue.title, "", code);
  }
});

test("a page from an older package version is told to reload, in the header too", async () => {
  const { AppComponent } = await loadApp({ exports: "POST_REJECTION_COPY" });
  const context = reporter(AppComponent);
  const report = context.reportMutationError(refusal("ui_build_stale", 409), "Save failed", "Outcome unknown", "fallback");
  assert.equal(report.code, "ui_build_stale");
  assert.match(report.message, /older version of the package/);
  assert.match(report.message, /Reload the app/);
  assert.equal(context.bridgeIssue.title, "This page is out of date");
  assert.equal(context.connectionLabel, "This page is out of date");
});

test("a 503 refusal and an untrusted body keep their own paths", async () => {
  const { AppComponent } = await loadApp({ exports: "POST_REJECTION_COPY" });
  const busy = reporter(AppComponent).reportMutationError(refusal("control_lock_busy", 503), "Save failed", "Outcome unknown", "fallback");
  assert.notEqual(busy.rejected, true);
  assert.match(busy.message, /busy recording another change/);
  const proxied = { ...refusal("forbidden", 403), trustedRejection: false };
  assert.notEqual(reporter(AppComponent).reportMutationError(proxied, "Save failed", "Outcome unknown", "fallback").rejected, true);
});

test("a refused mutation's Activity row is read for its reason and request ID only in its exact shape", async () => {
  const { AppComponent, normalizedActivityEvent } = await loadApp({ exports: "normalizedActivityEvent" });
  const row = (message) => normalizedActivityEvent({
    epoch: 1757000000, code: "audit.failed", profile: "all", state: "failed",
    category: "audit", level: "error", message,
    client_request_id: message.endsWith(REQUEST_ID) ? REQUEST_ID : null
  });
  const tagged = row(`Module rejected-post failed [${TRANSACTION}] reason=forbidden request_id=${REQUEST_ID}`);
  assert.equal(tagged.rejection_reason, "forbidden");
  assert.equal(tagged.client_request_id, REQUEST_ID);
  assert.equal(row(`Module rejected-post failed [${TRANSACTION}] reason=ui_build_stale`).rejection_reason, "ui_build_stale");
  // The untagged rows an older writer left, and any other record, carry no reason at all.
  for (const message of [
    `Module rejected-post failed [${TRANSACTION}]`,
    `Module configure-profile failed [${TRANSACTION}] reason=forbidden`,
    `Module rejected-post failed [${TRANSACTION}] reason=Forbidden`,
    `prefix Module rejected-post failed [${TRANSACTION}] reason=forbidden`
  ]) {
    assert.equal("rejection_reason" in row(message), false, message);
  }
  const context = appContext(AppComponent, {});
  assert.equal(
    context.rejectionReasonText("forbidden"),
    "Dashboard change refused: the security policy does not allow this change."
  );
  assert.equal(
    context.rejectionReasonText("unclassified"),
    "Dashboard change refused: for a reason this page has no words for; the package API log names it."
  );
  assert.equal(context.rejectionReasonText("made_up"), "Dashboard change refused: reason made_up.");
});
