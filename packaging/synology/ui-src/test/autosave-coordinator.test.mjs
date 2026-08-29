import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

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

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
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

test("canonical signatures ignore object insertion order and reject non-JSON state", async () => {
  const autosave = await loadAutosave();
  const left = autosave.canonicalAutosaveSignature({ z: 2, a: { y: true, x: [3, 1] } });
  const right = autosave.canonicalAutosaveSignature({ a: { x: [3, 1], y: true }, z: 2 });
  assert.equal(left, right);
  assert.equal(left, '{"a":{"x":[3,1],"y":true},"z":2}');
  assert.equal(autosave.canonicalAutosaveSignature({ value: -0 }), '{"value":0}');
  assert.throws(() => autosave.canonicalAutosaveSignature({ value: Number.NaN }), /finite/);
  assert.throws(() => autosave.canonicalAutosaveSignature({ value: undefined }), /JSON-compatible/);
  assert.throws(() => autosave.canonicalAutosaveSignature(new Date()), /plain objects/);
  const cyclic = {};
  cyclic.self = cyclic;
  assert.throws(() => autosave.canonicalAutosaveSignature(cyclic), /cycles/);
});

test("default 1300ms debounce dispatches only the latest canonical candidate", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const dispatched = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(
    clock,
    async (task) => { dispatched.push(task); }
  ));
  assert.equal(coordinator.delayMs, 1300);

  coordinator.hydrate("profile:nightly", { jobs: 2, name: "nightly" });
  coordinator.update("profile:nightly", { name: "nightly", jobs: 3 });
  await clock.advance(1299);
  assert.equal(dispatched.length, 0);
  coordinator.update("profile:nightly", { jobs: 4, name: "nightly" });
  await clock.advance(1299);
  assert.equal(dispatched.length, 0);
  await clock.advance(1);
  assert.equal(dispatched.length, 1);
  assert.deepEqual(dispatched[0].value, { jobs: 4, name: "nightly" });
  assert.equal(dispatched[0].revision, 3);
  assert.equal(coordinator.getState("profile:nightly").dirty, false);

  coordinator.update("profile:nightly", { name: "nightly", jobs: 4 });
  await clock.advance(1300);
  assert.equal(dispatched.length, 1, "equivalent key order must not create another save");
});

test("hydration replaces the candidate and baseline without dispatching watcher-like changes", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const dispatched = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(
    clock,
    async (task) => { dispatched.push(task); }
  ));

  coordinator.hydrate("alerts", { enabled: false, threshold: 1 });
  coordinator.update("alerts", { enabled: true, threshold: 1 });
  assert.equal(coordinator.getState("alerts").dirty, true);
  coordinator.hydrate("alerts", { threshold: 3, enabled: true });
  await clock.advance(5000);
  assert.equal(dispatched.length, 0);
  assert.deepEqual(coordinator.getState("alerts"), {
    scope: "alerts", registered: true, dirty: false, scheduled: false, queued: false,
    busy: false, blocked: false, cancelled: false, inFlight: false, revision: 3, dueAt: 0
  });

  coordinator.update("alerts", { enabled: true, threshold: 4 });
  coordinator.replaceBaseline("alerts", { enabled: true, threshold: 4 });
  await clock.advance(1300);
  assert.equal(dispatched.length, 0, "an authoritative matching baseline must cancel pending work");
});

test("serialized FIFO preserves edits made during an in-flight dispatch", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const first = deferred();
  const order = [];
  let profileDispatches = 0;
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(clock, (task) => {
    order.push(`${task.scope}:${task.value.value}`);
    if (task.scope === "profile") {
      profileDispatches += 1;
      if (profileDispatches === 1) return first.promise;
    }
    return Promise.resolve();
  }));

  coordinator.hydrate("profile", { value: "a" });
  coordinator.hydrate("alerts", { value: "a" });
  coordinator.update("profile", { value: "b" });
  coordinator.update("alerts", { value: "b" });
  await clock.advance(1300);
  assert.deepEqual(order, ["profile:b"], "only one mutation may be active");
  assert.equal(coordinator.getState("profile").inFlight, true);
  assert.equal(coordinator.getState("alerts").queued, true);

  coordinator.update("profile", { value: "c" });
  await clock.advance(1300);
  assert.deepEqual(order, ["profile:b"], "new edits wait behind the active mutation");
  first.resolve();
  await clock.settle();
  assert.deepEqual(order, ["profile:b", "alerts:b", "profile:c"]);
  await clock.settle();
  assert.equal(coordinator.getState("profile").dirty, false);
  assert.equal(coordinator.getState("alerts").dirty, false);
});

test("reverting during an in-flight save queues the persisted baseline as a compensating update", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const first = deferred();
  const values = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(clock, (task) => {
    values.push(task.value.value);
    return values.length === 1 ? first.promise : Promise.resolve();
  }));

  coordinator.hydrate("routine", { value: "original" });
  coordinator.update("routine", { value: "submitted" });
  await clock.advance(1300);
  coordinator.update("routine", { value: "original" });
  await clock.advance(1300);
  assert.deepEqual(values, ["submitted"]);
  first.resolve();
  await clock.settle();
  assert.deepEqual(values, ["submitted", "original"]);
  await clock.settle();
  assert.equal(coordinator.getState("routine").dirty, false);
});

test("authoritative hydration during an in-flight save supersedes its stale completion", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const first = deferred();
  const values = [];
  const superseded = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(clock, (task) => {
    values.push(task.value.value);
    return first.promise;
  }, { onSuperseded: (task, error) => superseded.push({ task, error }) }));

  coordinator.hydrate("alerts", { value: "snapshot-1" });
  coordinator.update("alerts", { value: "submitted" });
  await clock.advance(1300);
  assert.equal(coordinator.getState("alerts").inFlight, true);
  coordinator.hydrate("alerts", { value: "snapshot-2" });
  assert.equal(coordinator.getState("alerts").dirty, false);
  first.resolve();
  await clock.settle();
  assert.deepEqual(values, ["submitted"]);
  assert.equal(superseded.length, 1);
  assert.equal(superseded[0].task.scope, "alerts");
  assert.equal(superseded[0].error, null);
  assert.deepEqual(coordinator.getState("alerts"), {
    scope: "alerts", registered: true, dirty: false, scheduled: false, queued: false,
    busy: false, blocked: false, cancelled: false, inFlight: false, revision: 3, dueAt: 0
  });
  await clock.advance(5000);
  assert.deepEqual(values, ["submitted"], "stale completion must not resurrect superseded work");
});

test("authoritative hydration also supersedes a stale rejection without pausing later edits", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const first = deferred();
  const values = [];
  const failures = [];
  const superseded = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(clock, (task) => {
    values.push(task.value.value);
    return values.length === 1 ? first.promise : Promise.resolve();
  }, {
    onError: (error, task) => failures.push({ error, task }),
    onSuperseded: (task, error) => superseded.push({ task, error })
  }));

  coordinator.hydrate("security", { value: "snapshot-1" });
  coordinator.update("security", { value: "submitted" });
  await clock.advance(1300);
  coordinator.hydrate("security", { value: "reconciled" });
  first.reject(new Error("stale transport rejection"));
  await clock.settle();

  assert.equal(coordinator.getState("security").blocked, false);
  assert.equal(coordinator.getState("security").dirty, false);
  assert.deepEqual(failures, [], "a superseded failure must not replace reconciled UI status");
  assert.equal(superseded.length, 1, "superseded settlement must still release a Saving status");
  assert.equal(superseded[0].task.scope, "security");
  assert.match(superseded[0].error.message, /stale transport rejection/);

  coordinator.update("security", { value: "later edit" });
  await clock.advance(1300);
  assert.deepEqual(values, ["submitted", "later edit"]);
  assert.equal(coordinator.getState("security").dirty, false);
});

test("scope and global busy flags plus blocked scopes retain due FIFO work", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const order = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(
    clock,
    async (task) => { order.push(task.scope); }
  ));

  for (const scope of ["busy", "blocked", "free"]) coordinator.hydrate(scope, { value: 0 });
  coordinator.setScopeBusy("busy", true);
  coordinator.setScopeBlocked("blocked", true);
  coordinator.setGlobalBusy(true);
  coordinator.update("busy", { value: 1 });
  coordinator.update("blocked", { value: 1 });
  coordinator.update("free", { value: 1 });
  await clock.advance(1300);
  assert.deepEqual(order, []);

  coordinator.setGlobalBusy(false);
  await clock.settle();
  assert.deepEqual(order, ["free"]);
  coordinator.setScopeBusy("busy", false);
  await clock.settle();
  assert.deepEqual(order, ["free", "busy"]);
  coordinator.setScopeBlocked("blocked", false);
  await clock.settle();
  assert.deepEqual(order, ["free", "busy", "blocked"]);
});

test("failed dispatch blocks its scope and never retries until explicitly unblocked", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const failure = new Error("outcome unknown");
  const errors = [];
  let attempts = 0;
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(
    clock,
    () => {
      attempts += 1;
      if (attempts === 1) return Promise.reject(failure);
      return Promise.resolve();
    },
    { onError: (error, task) => errors.push({ error, task }) }
  ));

  coordinator.hydrate("security", { require_https: false });
  coordinator.update("security", { require_https: true });
  await clock.advance(1300);
  assert.equal(attempts, 1);
  assert.equal(coordinator.getState("security").blocked, true);
  assert.equal(coordinator.getState("security").dirty, true);
  await clock.advance(100000);
  assert.equal(attempts, 1, "elapsed time must never retry a failed mutation");
  assert.equal(errors.length, 1);
  assert.equal(errors[0].error, failure);
  assert.equal(errors[0].task.scope, "security");

  coordinator.setScopeBlocked("security", false);
  await clock.settle();
  assert.equal(attempts, 2);
  await clock.settle();
  assert.equal(coordinator.getState("security").dirty, false);
});

test("even a rejection without an Error value remains blocked and dirty", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  let attempts = 0;
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(clock, () => {
    attempts += 1;
    return Promise.reject(undefined);
  }));
  coordinator.hydrate("profile", { value: 0 });
  coordinator.update("profile", { value: 1 });
  await clock.advance(1300);
  assert.equal(attempts, 1);
  assert.equal(coordinator.getState("profile").blocked, true);
  assert.equal(coordinator.getState("profile").dirty, true);
  await clock.advance(10000);
  assert.equal(attempts, 1);
});

test("cancel scope, cancel all, and dispose prevent stale delayed dispatches", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const order = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(
    clock,
    async (task) => { order.push(task.scope); }
  ));

  coordinator.hydrate("profile:a", { value: 0 });
  coordinator.hydrate("profile:b", { value: 0 });
  coordinator.update("profile:a", { value: 1 });
  coordinator.update("profile:b", { value: 1 });
  assert.equal(coordinator.cancel("profile:a"), true);
  await clock.advance(1300);
  assert.deepEqual(order, ["profile:b"]);
  assert.equal(coordinator.getState("profile:a").cancelled, true);

  coordinator.update("profile:a", { value: 1 });
  await clock.advance(1300);
  assert.deepEqual(order, ["profile:b", "profile:a"], "a new update re-arms a cancelled scope");
  coordinator.update("profile:a", { value: 2 });
  coordinator.update("profile:b", { value: 2 });
  assert.equal(coordinator.cancelAll(), 2);
  await clock.advance(1300);
  assert.deepEqual(order, ["profile:b", "profile:a"]);

  coordinator.update("profile:a", { value: 3 });
  coordinator.dispose();
  coordinator.dispose();
  await clock.advance(5000);
  assert.deepEqual(order, ["profile:b", "profile:a"]);
  assert.throws(() => coordinator.update("profile:a", { value: 4 }), /disposed/);
  assert.equal(coordinator.cancel("profile:a"), false);
  assert.equal(coordinator.cancelAll(), 0);
});

test("candidate snapshots cannot be mutated after update or by a dispatch consumer", async () => {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const observed = [];
  const coordinator = autosave.createAutosaveCoordinator(coordinatorOptions(clock, async (task) => {
    observed.push(task.value.nested.value);
    task.value.nested.value = "dispatch mutation";
  }));
  const candidate = { nested: { value: "captured" } };
  coordinator.hydrate("settings", { nested: { value: "initial" } });
  coordinator.update("settings", candidate);
  candidate.nested.value = "caller mutation";
  await clock.advance(1300);
  assert.deepEqual(observed, ["captured"]);
  assert.equal(coordinator.getState("settings").dirty, false);
});
