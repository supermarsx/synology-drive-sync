// D3-UI-9: the error banner used to stay up for the life of the window, even after a
// later render succeeded and the page was plainly working. A failed render leaves the
// previous page untouched, so a clean flush afterwards is proof of recovery; a failed
// patch leaves the DOM half updated, and nothing here pretends otherwise.
import assert from "node:assert/strict";
import test from "node:test";
import { installFakeDom } from "./helpers/fake-dom.mjs";
import { appContext, appSource, loadApp } from "./helpers/load-app.mjs";

const { document: fakeDocument } = installFakeDom();
const { clearRenderErrors, reportUiError, uiErrorSerial } = await import("../src/uiErrors.mjs");

function root() {
  return { $el: fakeDocument.createElement("div") };
}

async function quietly(action) {
  const original = console.error;
  console.error = () => {};
  try { return await action(); } finally { console.error = original; }
}

test("D3-UI-9 a banner raised by failed renders comes down when the page renders cleanly again", async () => {
  await quietly(() => {
    const window = root();
    reportUiError(new Error("Cannot read properties of null (reading 'has_password')"), "render", window);
    reportUiError(new Error("again"), "render", window);
    assert.equal(window.$el.children.length, 1);
    assert.equal(clearRenderErrors(window), true);
    assert.equal(window.$el.children.length, 0, "the banner is gone");
    assert.equal(clearRenderErrors(window), false, "nothing left to clear");
    // A later failure raises a fresh banner.
    reportUiError(new Error("new"), "render", window);
    assert.equal(window.$el.children.length, 1);
  });
});

test("D3-UI-9 a banner that includes a failed patch or a handler error is never cleared by a later render", async () => {
  await quietly(() => {
    for (const info of ["patch", "v-on handler", "watcher"]) {
      const window = root();
      reportUiError(new Error("first"), "render", window);
      reportUiError(new Error("second"), info, window);
      assert.equal(clearRenderErrors(window), false, info);
      assert.equal(window.$el.children.length, 1, `${info}: the banner stays until the window is reloaded`);
    }
  });
});

test("D3-UI-9 every report moves the serial the AppWindow compares between flushes", async () => {
  await quietly(() => {
    const before = uiErrorSerial();
    reportUiError(new Error("x"), "render", root());
    assert.equal(uiErrorSerial(), before + 1);
  });
});

test("D3-UI-9 the AppWindow clears the banner on the first flush with no new report, not on the one that raised it", async () => {
  const component = await loadApp({ overrides: { clearRenderErrors, uiErrorSerial } });
  assert.equal(typeof component.updated, "function", "App.vue needs an updated() hook");
  await quietly(() => {
    const host = root();
    const ctx = appContext(component, { $root: host, uiErrorSerialSeen: uiErrorSerial() });
    const updated = () => component.updated.call(ctx);
    reportUiError(new Error("boom"), "render", host); // the flush that fails
    updated();
    assert.equal(host.$el.children.length, 1, "the flush that raised the error must not clear it");
    updated(); // the next flush rendered cleanly
    assert.equal(host.$el.children.length, 0);
  });
  assert.match(appSource, /updated\(\) \{[\s\S]*?clearRenderErrors\(this\.\$root\)/);
});
