// A token this UI does not recognise must show its raw code and a warning or
// failure colour, never the neutral grey of "nobody has looked".
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { appContext, loadApp } from "./helpers/load-app.mjs";

const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const NOW = 1_700_000_000;

function rule(selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = css.match(new RegExp(`(?:^|\\n)${escaped}\\s*\\{([^}]*)\\}`));
  assert.ok(match, `missing rule ${selector}`);
  return match[1];
}

const outcomes = (profile) => ({
  schema: "sdsync.dsm-outcomes.v1",
  run: { outcome: "succeeded", stale: {}, scope: "all", finished_epoch: NOW - 60 },
  alerts: { status: "ok" },
  profiles: [profile],
  routines: [],
  errors: []
});

test("the AppWindow shows an unrecognised outcome as a warning with the raw token", async () => {
  const component = await loadApp();
  const ctx = appContext(component, {
    snapshot: { schema: "sdsync.dsm-api.v1", generated_at_epoch: NOW, outcomes: outcomes({ profile: "photos", outcome: "exploded", reason: "x" }) },
    connected: true, freshness: "x", nowMs: NOW * 1000, snapshotReceivedAtMs: NOW * 1000
  });
  const row = ctx.profileOutcome("photos");
  assert.equal(row.level, "warn");
  assert.match(row.text, /exploded/);
  const stale = appContext(component, { snapshot: ctx.snapshot, connected: false, freshness: "x", nowMs: NOW * 1000, snapshotReceivedAtMs: NOW * 1000 });
  assert.equal(stale.profileOutcome("photos").level, "unknown", "a retained reading stays dimmed");
});

test("the stylesheet colours unknown as a warning in the attention list and on the desktop card", () => {
  assert.match(rule(".sdsync-attention li.is-unknown"), /border-left-color:\s*var\(--sdsync-amber\)/);
  assert.match(rule(".sdsync-widget.is-unknown .sdsync-widget-dot"), /color:\s*var\(--sdsync-amber\)/);
  assert.match(rule(".sdsync-widget-profile.is-unknown:not(.is-stale) .sdsync-widget-profile-outcome"), /color:\s*var\(--sdsync-amber\)/);
  assert.match(rule(".sdsync-ui-error"), /border-left-color:\s*var\(--sdsync-red\)/);
});
