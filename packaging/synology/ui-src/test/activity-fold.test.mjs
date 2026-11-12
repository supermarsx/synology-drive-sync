// The Activity feed folds each run's rows into one row with its outcome. The rows
// stay in the data, every fold opens, the view can show every event, and a warn or
// error row is never folded away.
import assert from "node:assert/strict";
import test from "node:test";
import { appContext, loadApp } from "./helpers/load-app.mjs";

const { AppComponent, normalizedActivityEvent, foldedActivityRows } = await loadApp({
  exports: "normalizedActivityEvent, foldedActivityRows"
});

let serial = 0;
function row(epoch, code, message, overrides = {}) {
  serial += 1;
  const event = normalizedActivityEvent({
    epoch, code, profile: "checkoffice_nas", state: code.split(".")[1], category: "operations",
    level: "info", message, client_request_id: null, ...overrides
  });
  event.rowKey = `row-${serial}`;
  return event;
}

// One scheduled run as the shell runner writes it, oldest first.
function scheduledRun(start, transaction, outcome = "succeeded", seconds = 25) {
  const failed = outcome === "failed";
  return [
    row(start - 1, "audit.requested", `Module run requested [${transaction}]`, { category: "audit", state: "requested" }),
    row(start, "run.started", "Package action started (sync)", { state: "running" }),
    row(start + seconds, `run.${outcome}`, failed ? "Package action failed: error: final reconciliation found 1 pending in-scope operations" : "Package action succeeded", { level: failed ? "error" : "info" }),
    row(start + seconds, `audit.${outcome}`, `Module run ${outcome} [${transaction}]`, { category: "audit", level: failed ? "error" : "info" }),
    row(start + seconds, `module.${outcome}`, `Module run ${outcome} [${transaction}]`, { level: failed ? "error" : "info" })
  ];
}

const newestFirst = (events) => events.slice().reverse();

test("five rows of a successful run fold into one row that names its outcome and duration", () => {
  const events = newestFirst(scheduledRun(1794165822, `scheduler-${"a".repeat(64)}`));
  const rows = foldedActivityRows(events, {});
  assert.equal(rows.length, 1);
  assert.equal(rows[0].message, "Run of checkoffice_nas succeeded in 25 s · 5 events");
  assert.equal(rows[0].code, "run.succeeded");
  assert.equal(rows[0].state, "succeeded");
  assert.equal(rows[0].level, "info");
  assert.equal(rows[0].fold.count, 5);
  assert.equal(rows[0].fold.expanded, false);
  // Opened, the fold draws every row it holds, newest first, under its own row.
  const open = foldedActivityRows(events, { [rows[0].fold.key]: true });
  assert.equal(open.length, 6);
  assert.deepEqual(open.slice(1), events);
});

test("a failed run keeps its failures drawn while its fold is closed", () => {
  const events = newestFirst([
    ...scheduledRun(1794164000, `scheduler-${"b".repeat(64)}`),
    ...scheduledRun(1794164385, `scheduler-${"c".repeat(64)}`, "failed"),
    row(1794164386, "notification.unavailable", "DSM desktop notification delivery unavailable", { category: "notifications", level: "warn", state: "unavailable" })
  ]);
  const rows = foldedActivityRows(events, {});
  const failed = rows.find((candidate) => candidate.fold && candidate.state === "failed");
  assert.ok(failed, JSON.stringify(rows.map((candidate) => candidate.message)));
  assert.equal(failed.level, "error");
  assert.match(failed.message, /^Run of checkoffice_nas failed in 25 s/);
  const index = rows.indexOf(failed);
  const beneath = rows.slice(index + 1, index + 4);
  assert.deepEqual(beneath.map((candidate) => candidate.code).sort(), ["audit.failed", "module.failed", "run.failed"]);
  assert.match(beneath.find((candidate) => candidate.code === "run.failed").message, /final reconciliation found 1 pending/);
  // A row outside any run is never folded at all.
  assert.ok(rows.some((candidate) => candidate.code === "notification.unavailable" && !candidate.fold));
  // The successful run is one row.
  assert.equal(rows.filter((candidate) => candidate.fold).length, 2);
  assert.equal(rows.length, 1 + 1 + 3 + 1);
});

test("an audited change folds by its transaction, and one still in flight is not folded", () => {
  const transaction = `bridge-${"d".repeat(64)}`;
  const requestId = "e1c45b23c3ef99bb24dd72d610496118";
  const done = newestFirst([
    row(100, "audit.requested", `Module configure-profile requested [${transaction}] request_id=${requestId}`, { category: "audit", profile: "nightly", client_request_id: requestId }),
    row(101, "audit.succeeded", `Module configure-profile succeeded [${transaction}] request_id=${requestId}`, { category: "audit", profile: "nightly", client_request_id: requestId })
  ]);
  const rows = foldedActivityRows(done, {});
  assert.equal(rows.length, 1);
  assert.equal(rows[0].message, "configure-profile of nightly succeeded · 2 events");
  assert.equal(rows[0].client_request_id, requestId, "the request ID stays searchable on the folded row");
  const pending = newestFirst([
    row(200, "audit.requested", `Module remove-profile requested [${transaction.replace("d", "e")}]`, { category: "audit" }),
    row(201, "run.started", "Package action started (sync)", { state: "running" })
  ]);
  assert.deepEqual(foldedActivityRows(pending, {}), pending);
});

test("the AppWindow folds by default and can show every event", () => {
  const events = scheduledRun(1794165822, `scheduler-${"f".repeat(64)}`);
  const context = appContext(AppComponent, {
    activityEvents: [],
    activitySearchDebounced: "",
    activityCategory: "all",
    activityLevel: "all",
    activityTab: undefined,
    activityRenderLimit: 200,
    activityExpandedFolds: {},
    activityGrouped: true
  });
  Object.defineProperty(context, "reversedActivity", { get: () => newestFirst(events), configurable: true });
  assert.equal(context.displayedActivity.length, 1);
  assert.equal(context.shownActivity.length, 1);
  assert.equal(context.activityGroupingLabel(), "Show every event");
  context.toggleActivityFold(context.displayedActivity[0].fold.key);
  assert.equal(context.displayedActivity.length, 6);
  assert.equal(context.activityFoldLabel(context.displayedActivity[0].fold), "Hide its events");
  context.toggleActivityGrouping();
  assert.equal(context.displayedActivity.length, 5);
  assert.equal(context.activityGroupingLabel(), "Group by run");
  // A context built without the flag draws every event, as before.
  const legacy = appContext(AppComponent, { activityRenderLimit: 200 });
  Object.defineProperty(legacy, "reversedActivity", { get: () => newestFirst(events), configurable: true });
  assert.equal(legacy.displayedActivity.length, 5);
});
