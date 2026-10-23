// ADV-U-1 (continued in ui-patch-jam.test.mjs and ui-patch-guard.test.mjs, which each
// need a process of their own: once Vue's scheduler jams it never repaints again).
// Rows get keys that cannot collide, and a render error that escapes is shown as a banner.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { installFakeDom } from "./helpers/fake-dom.mjs";
import { appContext, appSource, loadApp } from "./helpers/load-app.mjs";

const { document: fakeDocument } = installFakeDom();
const { default: Vue } = await import("vue");
Vue.config.productionTip = false;
Vue.config.devtools = false;
Vue.config.warnHandler = () => {};

const tick = () => new Promise((resolve) => Vue.nextTick(resolve));

function seeded(seed) {
  let state = seed;
  return () => {
    state = (state * 1103515245 + 12345) & 0x7fffffff;
    return state / 0x7fffffff;
  };
}

async function patchSurvives(keysOver, rounds, random) {
  const errors = [];
  const previous = Vue.config.errorHandler;
  Vue.config.errorHandler = (error) => { errors.push(error.message); };
  try {
    for (let round = 0; round < rounds && !errors.length; round += 1) {
      const rows = keysOver();
      const feed = new Vue({
        data: { rows },
        render(h) { return h("ol", this.rows.map((row) => h("li", { key: row.key }, row.text))); }
      }).$mount();
      await tick();
      const next = rows.filter(() => random() > 0.4).reverse().concat([{ key: `fresh-${round}`, text: "fresh" }]);
      feed.rows = next;
      await tick();
      if (!errors.length) assert.equal(feed.$el.children.length, next.length, "the patched list must hold every row");
    }
  } finally {
    Vue.config.errorHandler = previous;
  }
  return errors;
}

function duplicateHeavyEvents(count, random) {
  return Array.from({ length: count }, (_, index) => ({
    epoch: 1_700_000_000 + Math.floor(index / 8),
    code: ["authentication.failed", "bridge.failed", "routine.deferred"][Math.floor(random() * 3)],
    profile: ["photos", "none"][Math.floor(random() * 2)],
    category: "operations",
    level: "error",
    state: "failed",
    message: "same text"
  }));
}

test("the Activity feed key expression is unique for identical events and survives filtering and sliding", async () => {
  const keyExpression = appSource.match(/v-for="event in reversedActivity"\s+:key="([^"]+)"/);
  assert.ok(keyExpression, "the Activity feed v-for must declare a :key");
  const keyOf = new Function("event", `return ${keyExpression[1]};`);
  const component = await loadApp();
  const random = seeded(11);
  const events = duplicateHeavyEvents(60, random);
  const context = appContext(component, {
    activityEvents: events, activitySearchDebounced: "", activityTab: "events", activityCategory: "all", activityLevel: "all"
  });
  const rows = context.reversedActivity;
  assert.equal(rows.length, 60);
  const keys = rows.map(keyOf);
  assert.equal(new Set(keys).size, keys.length, "every row key must be unique even when the events are identical");

  const errors = await patchSurvives(() => {
    const fresh = appContext(component, {
      activityEvents: duplicateHeavyEvents(40, random), activitySearchDebounced: "", activityTab: "events", activityCategory: "all", activityLevel: "all"
    });
    return fresh.reversedActivity.map((event) => ({ key: keyOf(event), text: event.message }));
  }, 150, random);
  assert.deepEqual(errors, []);
});

test("keys stay stable when the 200-line window slides by one event", async () => {
  const keyExpression = appSource.match(/v-for="event in reversedActivity"\s+:key="([^"]+)"/);
  const keyOf = new Function("event", `return ${keyExpression[1]};`);
  const component = await loadApp();
  const base = duplicateHeavyEvents(30, seeded(3));
  const first = appContext(component, { activityEvents: base, activitySearchDebounced: "", activityTab: "events", activityCategory: "all", activityLevel: "all" });
  const slid = appContext(component, { activityEvents: base.slice(1).concat([{ ...base[29], epoch: base[29].epoch + 1, message: "newest" }]), activitySearchDebounced: "", activityTab: "events", activityCategory: "all", activityLevel: "all" });
  const before = new Set(first.reversedActivity.map(keyOf));
  const survivors = slid.reversedActivity.map(keyOf).filter((key) => before.has(key));
  assert.ok(survivors.length >= 20, `most rows should keep their key across a slide, kept ${survivors.length}`);
});

test("a render error that escapes shows a visible banner and is reported", async () => {
  const { reportUiError } = await import("../src/uiErrors.mjs");
  const reported = [];
  const originalError = console.error;
  console.error = (...args) => { reported.push(args.map(String).join(" ")); };
  try {
    const root = new Vue({ render(h) { return h("div", { class: "app" }, "dashboard"); } }).$mount();
    reportUiError(new Error("Cannot read properties of undefined (reading 'key')"), "render", root);
    const banner = root.$el.firstChild;
    assert.equal(banner.getAttribute("role"), "alert");
    assert.match(banner.textContent, /internal error/i);
    assert.match(banner.textContent, /reading 'key'/);
    assert.match(banner.textContent, /Reload/);
    assert.ok(reported.some((line) => line.includes("reading 'key'")), "the error must also reach the console");
    // A second error updates the one banner; it does not stack banners.
    reportUiError(new Error("again"), "watcher", root);
    reportUiError(new Error("again"), "watcher", root);
    const banners = root.$el.children.filter((child) => child.getAttribute("role") === "alert");
    assert.equal(banners.length, 1);
    assert.match(banners[0].textContent, /3 errors|\(3\)/);
    // Hostile text is bounded and set as text, never markup.
    reportUiError(new Error(`<img src=x onerror=alert(1)>${"x".repeat(5000)}`), "render", root);
    assert.ok(root.$el.children[0].textContent.length < 800);
    assert.equal(root.$el.children[0].children.length > 0, true);
  } finally {
    console.error = originalError;
  }
});

test("the AppWindow installs the boundary on its own wrapper, not on DSM's shared Vue.config", async () => {
  const main = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
  assert.match(main, /errorCaptured/, "the Instance wrapper must capture descendant render errors");
  assert.match(main, /reportUiError/);
  for (const file of ["App.vue", "SecurityPanel.vue", "WidgetPanel.vue"]) {
    const source = await readFile(new URL(`../src/${file}`, import.meta.url), "utf8");
    assert.match(source, /mixins:\s*\[patchGuard\]/, `${file} must guard its own patch`);
  }
  // Vue is DSM's global (webpack external), so a global errorHandler would also
  // swallow every other DSM app's errors.
  assert.doesNotMatch(main, /Vue\.config\.errorHandler\s*=/);
  assert.ok(fakeDocument, "fake DOM installed");
});

test("the banner goes into the dashboard's workspace column when DSM draws the window elsewhere", async () => {
  const { reportUiError } = await import("../src/uiErrors.mjs");
  const workspace = fakeDocument.createElement("main");
  workspace.setAttribute("class", "sdsync-workspace");
  const previous = fakeDocument.querySelector;
  fakeDocument.querySelector = (selector) => (selector === ".sdsync-workspace" ? workspace : null);
  const originalError = console.error;
  console.error = () => {};
  try {
    const wrapper = new Vue({ render(h) { return h("div", "wrapper element, not where the page is"); } }).$mount();
    reportUiError(new Error("boom"), "render", wrapper);
    assert.equal(workspace.firstChild.getAttribute("role"), "alert");
    assert.equal(wrapper.$el.children.length, 0, "the wrapper element is left alone");
  } finally {
    fakeDocument.querySelector = previous;
    console.error = originalError;
  }
});
