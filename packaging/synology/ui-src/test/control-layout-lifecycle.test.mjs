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

test("nested fit-container textfield shells receive stable package-owned fill markers", () => {
  const target = { classList: classList("v-textfield-input-element", "fit-container"), parentElement: null };
  const inner = { classList: classList("v-textfield-input-inner", "fit-container"), parentElement: null };
  const outer = { classList: classList("v-textfield-input", "fit-container"), parentElement: null };
  const owner = {
    classList: classList("sdsync-input-control", "v-textfield", "fit-container"),
    querySelectorAll(selector) {
      return selector.includes("input:not") ? [target] : [];
    }
  };
  target.parentElement = inner;
  inner.parentElement = outer;
  outer.parentElement = owner;
  const root = {
    querySelectorAll(selector) {
      return selector.includes(".sdsync-input-control") ? [owner] : [];
    }
  };
  const context = vm.createContext({ Set });
  vm.runInContext(`${source.replace(/^export\s+/gm, "")}\nthis.markControlShells = markControlShells;`, context);

  context.markControlShells(root);

  assert.equal(owner.classList.contains("sdsync-control-owner"), true);
  assert.equal(target.classList.contains("sdsync-semantic-control"), true);
  for (const shell of [inner, outer]) {
    assert.equal(shell.classList.contains("sdsync-control-shell"), true);
    assert.equal(shell.classList.contains("sdsync-input-shell"), true);
  }
});

test("AppWindow shell state tracks height independently from browser viewport width", () => {
  let rect = { width: 900, height: 520 };
  const shell = {
    classList: classList(),
    getBoundingClientRect: () => rect
  };
  const context = vm.createContext({ Set });
  vm.runInContext(`${source.replace(/^export\s+/gm, "")}\nthis.setShellState = setShellState; this.clearShellState = clearShellState;`, context);

  context.setShellState(shell);
  assert.equal(shell.classList.contains("sdsync-medium-shell"), true);
  assert.equal(shell.classList.contains("sdsync-compact-shell"), false);
  assert.equal(shell.classList.contains("sdsync-short-shell"), true,
    "the inclusive 520px AppWindow height must activate short-shell layout");

  rect = { width: 900, height: 521 };
  context.setShellState(shell);
  assert.equal(shell.classList.contains("sdsync-short-shell"), false,
    "a shell one pixel above the short boundary must retain its full layout");

  rect = { width: 600, height: 420 };
  context.setShellState(shell);
  assert.equal(shell.classList.contains("sdsync-compact-shell"), true);
  assert.equal(shell.classList.contains("sdsync-short-shell"), true);

  context.clearShellState(shell);
  assert.equal(shell.classList.contains("sdsync-medium-shell"), false);
  assert.equal(shell.classList.contains("sdsync-compact-shell"), false);
  assert.equal(shell.classList.contains("sdsync-short-shell"), false);
});

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
      if (selector === ".sdsync-settings-panel, .sdsync-horizontal-form, .sdsync-editor") return renderedForms;
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
  const editorAtThreshold = form(520);
  const editorAboveThreshold = form(521);
  renderedForms = [first, editorAtThreshold, editorAboveThreshold];
  const cleanup = context.installControlLayout(root);
  const resize = resizeInstances[0];
  const mutation = mutationInstances[0];
  assert.equal(resize.observed.has(first), true);
  assert.equal(resize.observed.has(editorAtThreshold), true, "profile editors receive their own width observer");
  assert.equal(resize.observed.has(editorAboveThreshold), true, "wide profile editors stay observed while resizing");
  assert.equal(first.classList.contains("sdsync-compact-form"), false,
    "a usable 640px form must retain label/control rows");
  assert.equal(editorAtThreshold.classList.contains("sdsync-compact-form"), true,
    "the inclusive 520px boundary must stack editor fields");
  assert.equal(editorAboveThreshold.classList.contains("sdsync-compact-form"), false,
    "an editor one pixel above the compact boundary must retain horizontal rows");

  const second = form(900);
  renderedForms = [second];
  mutation.callback();
  await Promise.resolve();
  assert.deepEqual(resize.unobserved, [first, editorAtThreshold, editorAboveThreshold]);
  assert.equal(resize.observed.has(first), false);
  assert.equal(first.classList.contains("sdsync-compact-form"), false);
  assert.equal(editorAtThreshold.classList.contains("sdsync-compact-form"), false,
    "removed editors release their scoped compact class");
  assert.equal(resize.observed.has(second), true);

  renderedForms = [first];
  mutation.callback();
  await Promise.resolve();
  assert.equal(resize.observed.has(second), false);
  assert.equal(resize.observed.has(first), true, "a reinserted route receives a fresh observation");

  const queuedAfterRemoval = form(380);
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
