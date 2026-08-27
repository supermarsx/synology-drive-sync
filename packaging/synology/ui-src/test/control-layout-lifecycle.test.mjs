import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const source = await readFile(new URL("../src/controlLayout.js", import.meta.url), "utf8");

function classList(...initial) {
  const values = new Set(initial);
  return {
    add: (...names) => names.forEach((name) => values.add(name)),
    contains: (name) => values.has(name),
    remove: (...names) => names.forEach((name) => values.delete(name)),
    toggle(name, force) {
      const enabled = force === undefined ? !values.has(name) : Boolean(force);
      if (enabled) values.add(name);
      else values.delete(name);
      return enabled;
    }
  };
}

function form(width) {
  return {
    classList: classList(),
    getBoundingClientRect: () => ({ width })
  };
}

test("responsive form observers release removed routes and cannot restart after cleanup", async () => {
  const resizeInstances = [];
  const mutationInstances = [];

  class FakeResizeObserver {
    constructor(callback) {
      this.callback = callback;
      this.observed = new Set();
      this.unobserved = [];
      this.disconnects = 0;
      resizeInstances.push(this);
    }

    observe(target) { this.observed.add(target); }
    unobserve(target) {
      this.unobserved.push(target);
      this.observed.delete(target);
    }
    disconnect() {
      this.disconnects += 1;
      this.observed.clear();
    }
  }

  class FakeMutationObserver {
    constructor(callback) {
      this.callback = callback;
      this.disconnects = 0;
      mutationInstances.push(this);
    }

    observe() {}
    disconnect() { this.disconnects += 1; }
  }

  let renderedForms = [];
  const root = {
    querySelectorAll(selector) {
      if (selector === ".sdsync-settings-panel, .sdsync-horizontal-form") return renderedForms;
      return [];
    }
  };
  const context = vm.createContext({
    MutationObserver: FakeMutationObserver,
    Promise,
    ResizeObserver: FakeResizeObserver,
    Set
  });
  vm.runInContext(`${source.replace(/^export\s+/gm, "")}\nthis.installControlLayout = installControlLayout;`, context);

  const first = form(640);
  renderedForms = [first];
  const cleanup = context.installControlLayout(root);
  const resize = resizeInstances[0];
  const mutation = mutationInstances[0];
  assert.equal(resize.observed.has(first), true);
  assert.equal(first.classList.contains("sdsync-compact-form"), true);

  const second = form(900);
  renderedForms = [second];
  mutation.callback();
  await Promise.resolve();
  assert.deepEqual(resize.unobserved, [first]);
  assert.equal(resize.observed.has(first), false);
  assert.equal(first.classList.contains("sdsync-compact-form"), false);
  assert.equal(resize.observed.has(second), true);

  renderedForms = [first];
  mutation.callback();
  await Promise.resolve();
  assert.equal(resize.observed.has(second), false);
  assert.equal(resize.observed.has(first), true, "a reinserted route receives a fresh observation");

  const queuedAfterRemoval = form(500);
  renderedForms = [queuedAfterRemoval];
  mutation.callback();
  cleanup();
  cleanup();
  await Promise.resolve();

  assert.equal(resize.disconnects, 1, "cleanup is idempotent");
  assert.equal(mutation.disconnects, 1, "cleanup is idempotent");
  assert.equal(resize.observed.size, 0);
  assert.equal(resize.observed.has(queuedAfterRemoval), false, "queued refresh cannot observe after cleanup");
  assert.equal(first.classList.contains("sdsync-compact-form"), false);

  resize.callback([{ target: queuedAfterRemoval }]);
  assert.equal(queuedAfterRemoval.classList.contains("sdsync-compact-form"), false,
    "a queued ResizeObserver delivery is inert after cleanup");
});
