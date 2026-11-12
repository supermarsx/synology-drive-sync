// Copy and behaviour for the NAS field report: a refused package file named, a slow read that
// does not lock edits, an applyStep failure that says where, the "Every second" cadence gone,
// and the lasting per-file reasons naming their file.
import assert from "node:assert/strict";
import test from "node:test";
import { keepsFailingSentence, profileOutcomeRow, reasonCopy } from "../src/outcomes.mjs";
import { appContext, loadApp } from "./helpers/load-app.mjs";

const { AppComponent, settingsFromStoredValue } = await loadApp({ exports: "settingsFromStoredValue" });

test("a refused package file is named with the check it failed and the repair", () => {
  const context = appContext(AppComponent, {});
  const issue = context.describeBridgeError({
    status: 503,
    code: "config_file_unsafe",
    unsafeFile: { file: "var/state/routines/office.state", check: "mode" }
  }, "status");
  assert.equal(issue.title, "Package file permissions are unsafe");
  assert.match(issue.message, /The package refused var\/state\/routines\/office\.state: its permissions are not 600\./);
  assert.match(issue.message, /Repair the package from Package Center/);
  assert.match(issue.message, /do not change the file by hand/);
  // Without a named file the copy is the one it always was.
  const bare = context.describeBridgeError({ status: 503, code: "config_file_unsafe" }, "status");
  assert.match(bare.message, /^A package file has unexpected ownership or permissions\./);
});

test("the dashboard checks a refused file again twice before it stops refreshing", () => {
  const context = appContext(AppComponent, { windowHidden: false, snapshotFailureCode: "config_file_unsafe", snapshotFailureStatus: 503 });
  for (const [streak, stopped] of [[1, false], [2, false], [3, true], [4, true]]) {
    context.snapshotFailureStreak = streak;
    assert.equal(context.snapshotRetryDelay(5000) === 0, stopped, `streak ${streak}`);
  }
});

test("a slow read keeps the mutation token and edits, and says the NAS is slow", async () => {
  const timeout = Object.assign(new Error("DSM did not return a complete read response within the client read limit."), {
    name: "ClientRequestTimeoutError", status: 0, code: "client_timeout", stage: "read_observation"
  });
  const context = appContext(AppComponent, {});
  const issue = context.describeBridgeError(timeout, "status");
  assert.equal(issue.title, "Package is slow to answer");
  assert.match(issue.message, /did not answer within 10 seconds/);
  assert.doesNotMatch(issue.message, /could not be reached|Confirm the package is running/);
  const deadline = context.describeBridgeError({ status: 503, code: "cgi_deadline_exceeded", stage: "dsm_authentication" }, "status");
  assert.equal(deadline.title, "Package request timed out");
  assert.match(deadline.message, /Failure stage: dsm_authentication/);
});

test("the Every second cadence is gone, and a stored one becomes five seconds", () => {
  const stored = (status) => JSON.stringify({ theme: "dark", status_refresh: status, log_refresh: 5000 });
  assert.equal(settingsFromStoredValue(stored(1000)).status_refresh, 5000);
  assert.equal(settingsFromStoredValue(stored(3000)).status_refresh, 3000);
  assert.equal(settingsFromStoredValue(stored(0)).status_refresh, 0);
  const context = appContext(AppComponent, {});
  assert.equal(context.statusRefreshOptions.some((option) => option.value === "1000"), false);
  assert.ok(context.statusRefreshOptions.every((option) => typeof option.value === "string"));
  assert.ok(context.logRefreshOptions.every((option) => typeof option.value === "string"));
});

test("an applyStep failure names its section and its cause, and logs under [sdsync]", async () => {
  const { applyStep } = await loadApp({ exports: "applyStep" });
  const toasts = [];
  const vm = { toast: (title, message, error) => toasts.push({ title, message, error }) };
  const logged = [];
  const previous = console.error;
  console.error = (...args) => logged.push(args);
  try {
    assert.equal(applyStep(vm, "the alert settings", () => { throw new TypeError("alerts.threshold is undefined"); }), false);
  } finally {
    console.error = previous;
  }
  assert.equal(toasts[0].title, "Dashboard error in the alert settings");
  assert.match(toasts[0].message, /\(TypeError: alerts\.threshold is undefined\)/);
  assert.match(toasts[0].message, /The rest of the page is current/);
  assert.equal(toasts[0].error, true);
  assert.match(logged[0][0], /^\[sdsync\] the alert settings: TypeError: alerts\.threshold is undefined$/);
  assert.equal(vm.applyError, "the alert settings: alerts.threshold is undefined");
});

test("every lasting per-file reason has copy, and its row names the file", () => {
  for (const code of ["file-keeps-changing", "source-stays-unreadable", "remote-digest-keeps-failing", "delete-keeps-failing"]) {
    const copy = reasonCopy(code);
    assert.equal(copy.known, true, code);
    assert.doesNotMatch(copy.text, /unknown/, code);
    assert.match(copy.text, /every run/, code);
  }
  assert.equal(
    keepsFailingSentence("keeps_failing path=docs%2Fledger%20final.xlsx runs=3 cause=source-unreadable; Permission denied"),
    "the file docs/ledger final.xlsx (3 runs in a row; last cause: source-unreadable)"
  );
  assert.equal(
    keepsFailingSentence("file_keeps_changing path=mail%2Finbox.db runs=3; last cause source-changed: size moved"),
    "the file mail/inbox.db (3 runs in a row; last cause: source-changed)"
  );
  assert.equal(keepsFailingSentence("error: something else"), "");
  const row = profileOutcomeRow({
    profile: "office", outcome: "succeeded", reason: "file-keeps-changing", warnings: [], failures: 0,
    detail: "file_keeps_changing path=mail%2Finbox.db runs=3; last cause source-changed: size moved"
  });
  assert.match(row.detail, /the file mail\/inbox\.db \(3 runs in a row/);
  assert.doesNotMatch(row.detail, /core said/);
});
