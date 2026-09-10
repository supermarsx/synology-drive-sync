import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// Loaded and driven exactly as autosave-coordinator.test.mjs does, deliberately by copy rather
// than by extracting a shared fixture: this file is new, so keeping it self-contained means it
// cannot collide with concurrent edits to the existing suite.
const source = await readFile(new URL("../src/autosave.js", import.meta.url), "utf8");

async function loadAutosave() {
  return import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}#${Date.now()}-${Math.random()}`);
}

class FakeClock {
  constructor() {
    this.time = 0;
    this.sequence = 0;
    this.timers = new Map();
  }

  now = () => this.time;

  setTimeout = (callback, delay) => {
    const id = ++this.sequence;
    this.timers.set(id, { id, callback, dueAt: this.time + Number(delay) });
    return id;
  };

  clearTimeout = (id) => {
    this.timers.delete(id);
  };

  async settle() {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  }

  async advance(milliseconds) {
    const target = this.time + milliseconds;
    for (;;) {
      const next = [...this.timers.values()]
        .filter((timer) => timer.dueAt <= target)
        .sort((left, right) => left.dueAt - right.dueAt || left.id - right.id)[0];
      if (!next) break;
      this.time = next.dueAt;
      this.timers.delete(next.id);
      next.callback();
      await this.settle();
    }
    this.time = target;
    await this.settle();
  }
}

function coordinatorOptions(clock, dispatch, extra = {}) {
  return {
    dispatch,
    now: clock.now,
    setTimeout: clock.setTimeout,
    clearTimeout: clock.clearTimeout,
    ...extra
  };
}

// The two coordinator properties the dashboard's connection hold is built on. They are pinned
// here, at the coordinator, because the dashboard reads them and does not restate them: a change
// to either one silently changes what the hold does, and the last time that happened the symptom
// was an edit that vanished while the status line said everything was saved.
//
// The defect: "edit a profile field, press Browse or Test within the 1.3 s debounce, dismiss the
// browser" lost the edit. `holdProfileAutosaveForConnection` cancelled the scope, and a cancelled
// entry is unreachable, so the release could not bring it back.

// Property one. Blocking suspends a pending edit and keeps it; unblocking dispatches it. This is
// what makes the hold non-destructive on the path where the connection request succeeds and
// nothing was ever wrong with the draft.
test("blocking a scope suspends its pending edit and unblocking dispatches it", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const dispatched = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(
    clock,
    async (task) => { dispatched.push(task.value); }
  ));

  coordinator.hydrate("profile", { name: "docs", remote: "/home/old" });
  coordinator.update("profile", { name: "docs", remote: "/home/NEW" });
  assert.equal(coordinator.getState("profile").scheduled, true);

  // Inside the debounce window, the way a real one is.
  await clock.advance(400);
  const held = coordinator.setScopeBlocked("profile", true);
  assert.equal(held.dirty, true, "blocking must not discard the edit it is holding");
  assert.equal(held.cancelled, false, "blocking is not cancelling");

  // However long the connection request takes, the held edit neither fires nor expires.
  await clock.advance(60000);
  assert.deepEqual(dispatched, [], "a blocked edit dispatched while it was blocked");
  assert.equal(coordinator.getState("profile").dirty, true, "the blocked edit was lost");

  coordinator.setScopeBlocked("profile", false);
  await clock.advance(0);
  assert.deepEqual(
    dispatched,
    [{ name: "docs", remote: "/home/NEW" }],
    "the released edit was not dispatched"
  );
  assert.equal(coordinator.getState("profile").dirty, false, "a dispatched edit must leave the scope clean");
});

// Property two, and the one that is easy to "clean up" by mistake. `cancel` does NOT discard the
// edit: it leaves the entry dirty and unreachable -- no timer, no queue slot, and no way back
// except a further edit. That is deliberate. The dashboard cancels the scope on the path where a
// connection probe failed or returned an unknown outcome, because the draft must not be dispatched
// into an unresolved incident, and it then reports the surviving dirty entry as "Unsaved changes ·
// use Save now" so the operator can recover it by hand.
//
// Making `cancel` reset the entry to its baseline instead looks tidier and is wrong: it destroys
// the evidence that guard reads, so the status line falls through to "all changes saved" over a
// form that still visibly holds the operator's edit -- the same lie, in a new place. If this test
// fails because `cancel` now leaves a clean entry, App.vue's `currentAutosaveStatus` needs a
// different signal before that change can land.
test("cancel leaves the edit in place and unreachable rather than discarding it", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const dispatched = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(
    clock,
    async (task) => { dispatched.push(task.value); }
  ));

  coordinator.hydrate("profile", { name: "docs", remote: "/home/old" });
  coordinator.update("profile", { name: "docs", remote: "/home/NEW" });
  await clock.advance(400);

  assert.equal(coordinator.cancel("profile"), true);
  const state = coordinator.getState("profile");
  assert.equal(state.cancelled, true);
  assert.equal(
    state.dirty,
    true,
    "cancel discarded the edit, so nothing is left for the status line to report as unsaved"
  );
  assert.equal(state.scheduled, false, "a cancelled entry must hold no timer");
  assert.equal(state.queued, false, "a cancelled entry must hold no queue slot");

  await clock.advance(60000);
  assert.deepEqual(dispatched, [], "a cancelled edit must never dispatch on its own");

  // The only route back is an explicit further edit, which is what makes Save now and a resumed
  // edit both work after an incident has been reconciled.
  coordinator.update("profile", { name: "docs", remote: "/home/LATER" });
  await clock.advance(1300);
  assert.deepEqual(
    dispatched,
    [{ name: "docs", remote: "/home/LATER" }],
    "a later edit after a cancellation must autosave again"
  );
});

// A hold suspends one scope, not the coordinator. The dashboard tells the operator as much
// ("unrelated controls and autosave remain available"), so it has to be true.
test("blocking one scope leaves the others autosaving", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const order = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(
    clock,
    async (task) => { order.push(task.scope); }
  ));

  coordinator.hydrate("profile", { value: 0 });
  coordinator.hydrate("interface", { value: 0 });
  coordinator.update("profile", { value: 1 });
  coordinator.setScopeBlocked("profile", true);
  coordinator.update("interface", { value: 1 });

  await clock.advance(1300);
  assert.deepEqual(order, ["interface"], "a blocked scope blocked an unrelated one");

  coordinator.setScopeBlocked("profile", false);
  await clock.advance(0);
  assert.deepEqual(order, ["interface", "profile"]);
});
