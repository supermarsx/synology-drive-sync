// The desktop card must never go blank or freeze on its last paint when the
// derivation behind it throws: the failure is the headline, with its message.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { APP_MODULE_STUBS } from "./helpers/app-stubs.mjs";

const panel = await readFile(new URL("../src/WidgetPanel.vue", import.meta.url), "utf8");

function loadPanel(overrides) {
  const script = panel.match(/<script>\s*([\s\S]*?)\s*<\/script>/)[1]
    .replace(/^import[^;]*;$/gm, "")
    .replace("export default {", "const WidgetPanelComponent = {");
  const stubs = {
    ...APP_MODULE_STUBS,
    ActionIcon: {}, SNAPSHOT_SCHEMA: "x", apiGet: async () => ({}),
    APPWINDOW_SETTINGS_KEY: "k", appWindowPreferences: () => ({}), widgetBridgeIssue: () => ({}),
    widgetPollDelay: () => 1, widgetRun: () => ({}), widgetView: () => ({ overview: {}, rows: [] }),
    ...overrides
  };
  return Function(...Object.keys(stubs), `${script}\nreturn WidgetPanelComponent;`)(...Object.values(stubs));
}

test("a derivation that throws becomes a visible failure headline with the message", () => {
  const component = loadPanel({ widgetView: () => { throw new Error("row derivation exploded"); } });
  const view = component.computed.view.call({ snapshot: {}, packageNowMs: 1, stale: false });
  assert.equal(view.overview.level, "fail");
  assert.match(view.overview.title, /could not read this status/);
  assert.match(view.overview.detail, /row derivation exploded/);
  assert.deepEqual(view.rows, []);
});

test("the card measures ages on the package's clock", () => {
  const component = loadPanel();
  const at = (snapshot, updatedAtMs, nowMs) => component.computed.packageNowMs.call({ snapshot, updatedAtMs, nowMs });
  // Browser two hours fast: the package said 1_700_000_000 when the card read it.
  assert.equal(at({ generated_at_epoch: 1_700_000_000 }, 5_000_000_000_000, 5_000_000_030_000), 1_700_000_030_000);
  // No generation time, or nothing read yet: fall back to the browser clock.
  assert.equal(at({}, 1, 99), 99);
  assert.equal(at({ generated_at_epoch: 1_700_000_000 }, 0, 99), 99);
});
