// ADV-U-1, the defence: a component that mixes in patchGuard turns the otherwise
// silent patch exception into a banner. Own process: the jam it provokes is permanent.
import assert from "node:assert/strict";
import test from "node:test";
import { installFakeDom } from "./helpers/fake-dom.mjs";

installFakeDom();
const { default: Vue } = await import("vue");
const { patchGuard } = await import("../src/uiErrors.mjs");
Vue.config.productionTip = false;
Vue.config.devtools = false;
Vue.config.warnHandler = () => {};

const tick = () => new Promise((resolve) => Vue.nextTick(resolve));

test("a patch exception inside a guarded component raises the banner instead of freezing silently", async () => {
  const thrown = [];
  Vue.config.errorHandler = (error) => { thrown.push(error.message); };
  const logged = [];
  const originalError = console.error;
  console.error = (...args) => { logged.push(args.map(String).join(" ")); };
  try {
    const keys = ["k2", "k2", "k1", "k4", "k3", "k1", "k3", "k2", "k1", "k2", "k1", "k0"];
    const feed = new Vue({
      mixins: [patchGuard],
      data: { keys },
      render(h) { return h("ol", this.keys.map((key) => h("li", { key }, key))); }
    }).$mount();
    await tick();
    feed.keys = ["k0", "k1", "k2", "k3", "k3", "k1", "k2", "k2", "fresh"];
    await tick();
    const banner = feed.$el.children.find((child) => child.getAttribute("role") === "alert");
    assert.ok(banner, "the jam must be visible");
    assert.match(banner.textContent, /internal error/);
    assert.match(banner.textContent, /patch/);
    assert.ok(logged.some((line) => line.includes("[sdsync] patch")), "and logged");
  } finally {
    console.error = originalError;
  }
  void thrown;
});
