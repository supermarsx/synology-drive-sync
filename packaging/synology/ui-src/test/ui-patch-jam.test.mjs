// ADV-U-1, the mechanism. Run in its own process because the failure it documents
// is permanent: once a patch exception escapes Vue 2's scheduler flush, `waiting`
// and `flushing` are never reset and no component in the page repaints again.
import assert from "node:assert/strict";
import test from "node:test";
import { installFakeDom } from "./helpers/fake-dom.mjs";

installFakeDom();
const { default: Vue } = await import("vue");
Vue.config.productionTip = false;
Vue.config.devtools = false;
Vue.config.warnHandler = () => {};

const tick = () => new Promise((resolve) => Vue.nextTick(resolve));

test("two rows sharing a v-for key throw out of the patch and freeze every other component", async () => {
  const thrown = [];
  Vue.config.errorHandler = (error) => { thrown.push(error.message); };
  const badge = new Vue({ data: { text: "Succeeded" }, render(h) { return h("b", this.text); } }).$mount();
  const keys = ["k2", "k2", "k1", "k4", "k3", "k1", "k3", "k2", "k1", "k2", "k1", "k0"];
  const feed = new Vue({
    data: { keys },
    render(h) { return h("ol", this.keys.map((key) => h("li", { key }, key))); }
  }).$mount();
  await tick();
  feed.keys = ["k0", "k1", "k2", "k3", "k3", "k1", "k2", "k2", "fresh"];
  await tick();
  // The exception escapes the flush as an uncaught error, not through errorHandler.
  badge.text = "FAILED";
  await tick();
  await tick();
  assert.equal(badge.text, "FAILED", "the data changed");
  assert.equal(badge.$el.textContent, "Succeeded", "but the page still says Succeeded: the UI is frozen");
  void thrown;
});
