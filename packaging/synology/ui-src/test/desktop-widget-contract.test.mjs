import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  APPWINDOW_SETTINGS_KEY,
  APPWINDOW_STATUS_INTERVALS_MS,
  WIDGET_ACTIVE_POLL_MS,
  WIDGET_BACKOFF_RAMP_MS,
  WIDGET_IDLE_POLL_MS,
  WIDGET_MAX_POLL_MS,
  appWindowPreferences,
  relativeEpochLabel,
  widgetBridgeIssue,
  widgetOverview,
  widgetPollDelay,
  widgetProfileRows,
  widgetRun
} from "../src/widgetModel.mjs";

const adapter = await readFile(new URL("../src/widget.js", import.meta.url), "utf8");
const panel = await readFile(new URL("../src/WidgetPanel.vue", import.meta.url), "utf8");
const main = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const appConfig = JSON.parse(await readFile(new URL("../app.config", import.meta.url), "utf8"));

const WIDGET_CLASS = "SYNO.SDS.App.SynologyDriveSync.Widget";
const APP_CLASS = "SYNO.SDS.App.SynologyDriveSync.Instance";

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function declarations(selector) {
  const match = css.match(new RegExp(`${escapeRegex(selector)}\\s*\\{([\\s\\S]*?)\\n\\}`, "m"));
  assert.ok(match, `missing ${selector} declaration block`);
  return match[1];
}

function hexVariables(selector) {
  const variables = new Map();
  for (const match of declarations(selector).matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})\s*;/gi)) {
    variables.set(match[1], match[2]);
  }
  return variables;
}

function luminance(hex) {
  const channels = [1, 3, 5].map((offset) => Number.parseInt(hex.slice(offset, offset + 2), 16) / 255);
  const linear = channels.map((value) => (value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4));
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function contrast(first, second) {
  const values = [luminance(first), luminance(second)].sort((left, right) => right - left);
  return (values[0] + 0.05) / (values[1] + 0.05);
}

function snapshot(overrides = {}) {
  return Object.assign({
    schema: "sdsync.dsm-api.v1",
    service: { state: "running", pid: 42 },
    run: { state: "succeeded", started_epoch: 0, finished_epoch: 0, exit_code: 0, scope: "none", operation: "none" },
    profiles: [],
    routines: []
  }, overrides);
}

function profile(name, healthState = "unknown", checkedEpoch = 0) {
  return { name, health: { state: healthState, checked_epoch: checkedEpoch, exit_code: null, write_test: false, level: "standard" } };
}

function routine(name, state, lastSuccessEpoch = 0) {
  return { profile: name, enabled: true, state, backend: "interval", next_run_epoch: 0, last_success_epoch: lastSuccessEpoch };
}

/**
 * Load the widget panel's options object with the transport and the model
 * stubbed the way the AppWindow suite loads App.vue, plus fake window/document
 * globals so the timer discipline can be driven directly.
 */
function loadWidgetPanel(environment) {
  const script = panel.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "WidgetPanel.vue script block is missing");
  const executable = `${script[1]
    .replace(/import\s*\{\s*ActionIcon\s*\}\s*from\s*"\.\/ActionIcon";\s*/, "")
    .replace(/import\s*\{[\s\S]*?\}\s*from\s*"\.\/api";\s*/, "")
    .replace(/import\s*\{[\s\S]*?\}\s*from\s*"\.\/widgetModel\.mjs";\s*/, "")
    .replace("export default {", "const WidgetPanelComponent = {")}\nreturn WidgetPanelComponent;`;

  const stubs = {
    window: environment.window,
    document: environment.document,
    ActionIcon: { name: "ActionIcon" },
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    apiGet: environment.apiGet,
    APPWINDOW_SETTINGS_KEY,
    appWindowPreferences,
    widgetBridgeIssue,
    widgetOverview,
    widgetPollDelay,
    widgetProfileRows,
    widgetRun
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

function environment(options = {}) {
  const timers = [];
  const listeners = [];
  const state = {
    hidden: options.hidden === true,
    stored: options.stored === undefined ? null : options.stored,
    reads: 0,
    responses: options.responses || [],
    timers,
    listeners
  };
  state.window = {
    AbortController: class { constructor() { this.signal = { aborted: false }; } abort() { this.signal.aborted = true; } },
    matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
    localStorage: { getItem: () => state.stored },
    setTimeout(callback, delay) {
      timers.push({ callback, delay, cleared: false });
      return timers.length;
    },
    clearTimeout(handle) {
      if (timers[handle - 1]) timers[handle - 1].cleared = true;
    },
    addEventListener(type) { listeners.push(`window:${type}`); },
    removeEventListener(type) { listeners.push(`window:-${type}`); }
  };
  state.document = {
    get hidden() { return state.hidden; },
    addEventListener(type) { listeners.push(`document:${type}`); },
    removeEventListener(type) { listeners.push(`document:-${type}`); }
  };
  state.apiGet = async () => {
    state.reads += 1;
    if (options.alwaysFail) throw options.alwaysFail;
    const response = state.responses.shift();
    if (response instanceof Error) throw response;
    return response === undefined ? snapshot() : response;
  };
  return state;
}

/** Shape a transport rejection the way api.js's DsmApiError arrives. */
function bridgeError(status, code, message = "API request failed") {
  return Object.assign(new Error(message), { name: "DsmApiError", status, code });
}

function instantiate(state) {
  const component = loadWidgetPanel(state);
  const context = Object.assign({}, component.data());
  for (const [name, method] of Object.entries(component.methods)) {
    context[name] = method.bind(context);
  }
  for (const [name, getter] of Object.entries(component.computed)) {
    Object.defineProperty(context, name, { get: getter.bind(context), configurable: true });
  }
  component.created.call(context);
  context.$component = component;
  return context;
}

function pending(state) {
  return state.timers.filter((timer) => !timer.cleared);
}

test("widget rests at DSM's own first-party widget cadence and never outpaces the AppWindow", () => {
  assert.ok(WIDGET_IDLE_POLL_MS >= 60000, "resting cadence must not undercut DSM's 60-second widget poll");
  assert.ok(WIDGET_ACTIVE_POLL_MS >= 20000);
  // The AppWindow's own default status cadence is 5000 ms. The background card
  // must always be the quieter of the two surfaces.
  assert.ok(WIDGET_ACTIVE_POLL_MS > 5000);
  assert.ok(WIDGET_ACTIVE_POLL_MS <= WIDGET_IDLE_POLL_MS);
  assert.equal(widgetPollDelay({}), WIDGET_IDLE_POLL_MS);
  assert.equal(widgetPollDelay({ active: true }), WIDGET_ACTIVE_POLL_MS);

  // An operator who slowed the AppWindow down has already stated a preference;
  // the widget floors itself at that value rather than making them say it twice.
  for (const interval of APPWINDOW_STATUS_INTERVALS_MS) {
    const delay = widgetPollDelay({ active: true, appWindowIntervalMs: interval });
    assert.ok(delay >= interval, `active cadence ${delay} outpaced AppWindow ${interval}`);
    assert.ok(delay >= WIDGET_ACTIVE_POLL_MS);
  }
  assert.equal(widgetPollDelay({ active: true, appWindowIntervalMs: 30000 }), 30000);
  assert.equal(widgetPollDelay({ appWindowIntervalMs: 30000 }), WIDGET_IDLE_POLL_MS);

  let previous = 0;
  for (let failures = 1; failures <= WIDGET_BACKOFF_RAMP_MS.length + 3; failures += 1) {
    const delay = widgetPollDelay({ active: true, failures });
    assert.ok(delay >= previous, "failure backoff must never speed up");
    assert.ok(delay >= WIDGET_IDLE_POLL_MS, "a failing bridge must not be polled at the active cadence");
    assert.ok(delay <= WIDGET_MAX_POLL_MS);
    previous = delay;
  }
  assert.equal(widgetPollDelay({ failures: 99 }), WIDGET_MAX_POLL_MS);
  assert.ok(WIDGET_MAX_POLL_MS <= 300000);
});

test("overall health reports the worst honest answer rather than the average one", () => {
  const now = 1_700_000_000_000;
  assert.equal(widgetOverview(null, now).level, "unknown");
  assert.equal(widgetOverview(undefined, now).title, "Status unavailable");

  assert.equal(widgetOverview(snapshot(), now).title, "No profiles configured");
  assert.equal(widgetOverview(snapshot(), now).level, "idle");

  const healthy = snapshot({
    profiles: [profile("photos"), profile("documents")],
    routines: [routine("photos", "succeeded", 1_699_999_000), routine("documents", "succeeded", 1_699_999_500)]
  });
  assert.equal(widgetOverview(healthy, now).level, "ok");
  assert.equal(widgetOverview(healthy, now).title, "All profiles healthy");

  const oneFailing = snapshot({
    profiles: [profile("photos"), profile("documents")],
    routines: [routine("photos", "failed", 1_699_000_000), routine("documents", "succeeded", 1_699_999_500)]
  });
  assert.equal(widgetOverview(oneFailing, now).level, "fail");
  assert.equal(widgetOverview(oneFailing, now).title, "1 profile failing");
  assert.equal(widgetOverview(oneFailing, now).detail, "photos");

  // An untrusted controller identity outranks every other reading: nothing the
  // card would otherwise report about that service can be trusted.
  const untrusted = snapshot({
    service: { state: "untrusted", pid: 7 },
    profiles: [profile("photos")],
    routines: [routine("photos", "succeeded", 1_699_999_000)]
  });
  assert.equal(widgetOverview(untrusted, now).level, "fail");
  assert.match(widgetOverview(untrusted, now).title, /untrusted/);

  const stopped = snapshot({
    service: { state: "stopped", pid: 0 },
    profiles: [profile("photos")],
    routines: [routine("photos", "succeeded", 1_699_999_000)]
  });
  assert.equal(widgetOverview(stopped, now).level, "warn");
  assert.equal(widgetOverview(stopped, now).title, "Package service stopped");

  const never = snapshot({ profiles: [profile("photos")], routines: [routine("photos", "never")] });
  assert.equal(widgetOverview(never, now).level, "idle");
  assert.equal(widgetOverview(never, now).title, "Waiting for the first run");
});

test("per-profile rows prefer routine evidence and label the Doctor fallback honestly", () => {
  const now = 1_700_000_000_000;
  const model = snapshot({
    profiles: [profile("photos"), profile("documents", "failed", 1_699_996_400), profile("media", "succeeded", 1_699_996_400)],
    routines: [routine("photos", "succeeded", 1_699_996_400)]
  });
  const rows = widgetProfileRows(model, now);
  assert.deepEqual(rows.map((row) => row.name), ["photos", "documents", "media"]);

  assert.equal(rows[0].source, "routine");
  assert.equal(rows[0].outcome, "Succeeded");
  assert.equal(rows[0].level, "ok");
  assert.equal(rows[0].age, "1 h ago");
  assert.equal(rows[0].detail, "last success 1 h ago");

  // No routine exists for these two, so the only per-profile evidence the
  // snapshot carries is the cached Doctor result. The row says so.
  assert.equal(rows[1].source, "health");
  assert.equal(rows[1].outcome, "Doctor failed");
  assert.equal(rows[1].level, "fail");
  assert.equal(rows[2].outcome, "Doctor passed");
  assert.equal(rows[2].detail, "checked 1 h ago");

  const running = widgetProfileRows(snapshot({
    profiles: [profile("photos"), profile("documents")],
    routines: [routine("photos", "succeeded", 1_699_996_400), routine("documents", "succeeded", 1_699_996_400)],
    run: { state: "running", started_epoch: 1_699_999_900, finished_epoch: 0, exit_code: null, scope: "photos", operation: "sync" }
  }), now);
  assert.equal(running[0].running, true);
  assert.equal(running[0].outcome, "Running");
  assert.equal(running[1].running, false);

  assert.deepEqual(widgetProfileRows(null, now), []);
  assert.equal(relativeEpochLabel(0, now), "never");
  assert.equal(relativeEpochLabel(1_699_999_990, now), "just now");
  assert.equal(relativeEpochLabel(1_699_900_000, now), "1 d ago");
});

test("the active-run banner reports only the attribution the snapshot actually carries", () => {
  const idle = widgetRun(snapshot());
  assert.equal(idle.active, false);
  assert.equal(idle.label, "");

  const scoped = widgetRun(snapshot({
    run: { state: "running", started_epoch: 1, finished_epoch: 0, exit_code: null, scope: "photos", operation: "sync" }
  }));
  assert.equal(scoped.active, true);
  assert.equal(scoped.label, "Sync running · photos");

  const all = widgetRun(snapshot({
    run: { state: "running", started_epoch: 1, finished_epoch: 0, exit_code: null, scope: "all", operation: "plan" }
  }));
  assert.equal(all.label, "Plan running · all profiles");
});

test("AppWindow preferences are read defensively and default to the dark package theme", () => {
  assert.deepEqual(appWindowPreferences(null), { theme: "dark", statusIntervalMs: 0 });
  assert.deepEqual(appWindowPreferences("not json"), { theme: "dark", statusIntervalMs: 0 });
  assert.deepEqual(appWindowPreferences("[]"), { theme: "dark", statusIntervalMs: 0 });
  assert.deepEqual(
    appWindowPreferences(JSON.stringify({ theme: "light", status_refresh: 30000 })),
    { theme: "light", statusIntervalMs: 30000 }
  );
  assert.deepEqual(
    appWindowPreferences(JSON.stringify({ theme: "neon", status_refresh: 250 })),
    { theme: "dark", statusIntervalMs: 0 }
  );
  assert.equal(APPWINDOW_SETTINGS_KEY, "sdsync.ui.settings.v1");
});

test("the widget holds no timer until DSM activates it and drops it the moment it is hidden", async () => {
  const state = environment();
  const panelInstance = instantiate(state);

  // created() must not start anything: DSM constructs the card before the
  // widget tray is necessarily on screen.
  assert.equal(state.reads, 0);
  assert.equal(pending(state).length, 0);
  assert.ok(state.listeners.includes("document:visibilitychange"));

  await panelInstance.activate();
  assert.equal(state.reads, 1);
  assert.equal(pending(state).length, 1);
  assert.equal(pending(state)[0].delay, WIDGET_IDLE_POLL_MS);
  assert.equal(panelInstance.failures, 0);
  assert.equal(panelInstance.overview.level, "idle");

  panelInstance.deactivate();
  assert.equal(pending(state).length, 0, "deactivate must release the scheduled read");

  // A read that was already in flight when the card went away must not
  // reschedule itself behind DSM's back.
  await panelInstance.refresh();
  assert.equal(state.reads, 1);
  assert.equal(pending(state).length, 0);
});

test("a hidden DSM tab and a failing bridge both suppress the widget's next read", async () => {
  const state = environment({ responses: [new Error("bridge down"), new Error("bridge down"), snapshot()] });
  const panelInstance = instantiate(state);

  await panelInstance.activate();
  assert.equal(panelInstance.failures, 1);
  assert.equal(panelInstance.degraded, false);
  assert.equal(pending(state)[0].delay, WIDGET_BACKOFF_RAMP_MS[0]);

  await panelInstance.refresh();
  assert.equal(panelInstance.failures, 2);
  assert.equal(panelInstance.degraded, true);
  assert.equal(pending(state)[0].delay, WIDGET_BACKOFF_RAMP_MS[1]);

  await panelInstance.refresh();
  assert.equal(panelInstance.failures, 0, "one good read must clear the backoff");
  assert.equal(pending(state)[0].delay, WIDGET_IDLE_POLL_MS);

  state.hidden = true;
  panelInstance.visibilityHandler();
  assert.equal(pending(state).length, 0, "a hidden DSM tab must leave no armed timer");
  const before = state.reads;
  await panelInstance.refresh();
  assert.equal(state.reads, before, "a hidden DSM tab must not be read through");

  panelInstance.$component.beforeDestroy.call(panelInstance);
  assert.equal(panelInstance.disposed, true);
  assert.equal(panelInstance.abortController.signal.aborted, true);
  assert.ok(state.listeners.includes("document:-visibilitychange"));
});

test("an unreachable or unauthenticated bridge is named, not reported as 'unavailable'", () => {
  // The titles must match the ones App.vue's describeBridgeError produces for
  // the same conditions, or the card and the dashboard disagree about what is
  // wrong with the same NAS.
  assert.deepEqual(widgetBridgeIssue(bridgeError(401, "http_401")), {
    level: "warn", title: "DSM session expired", detail: "Sign in to DSM again."
  });
  assert.equal(widgetBridgeIssue(bridgeError(0, "dsm_authentication_webapi_rejected")).title, "DSM session expired");
  assert.equal(widgetBridgeIssue(bridgeError(403, "http_403")).title, "DSM access denied");
  assert.equal(widgetBridgeIssue(bridgeError(0, "forbidden")).title, "DSM access denied");
  // The resting state of a stopped package, which is the whole reason this
  // classification exists.
  assert.equal(widgetBridgeIssue(bridgeError(503, "http_503")).title, "Package service unavailable");
  assert.equal(widgetBridgeIssue(bridgeError(0, "dsm_authentication_helper_unavailable")).title, "Package service unavailable");
  assert.equal(widgetBridgeIssue(bridgeError(0, "dsm_authentication_webapi_unavailable")).title, "Package service unavailable");
  assert.equal(widgetBridgeIssue(bridgeError(404, "http_404")).title, "Package UI route unavailable");
  assert.equal(widgetBridgeIssue(bridgeError(200, "non_json_response")).title, "Package UI route unavailable");
  assert.equal(widgetBridgeIssue(new Error("Unsupported DSM API schema")).title, "UI and package versions differ");
  // A dead network blames nothing it has no evidence against.
  const unreachable = widgetBridgeIssue(new Error("Failed to fetch"));
  assert.equal(unreachable.title, "Package bridge unavailable");
  assert.equal(unreachable.level, "unknown");
  assert.deepEqual(widgetBridgeIssue(null).title, "Package bridge unavailable");
  for (const status of [401, 403, 503, 404, 0]) {
    const issue = widgetBridgeIssue(bridgeError(status, `http_${status}`));
    assert.ok(issue.detail.length <= 45, `"${issue.detail}" is too long for a 318px card`);
  }
});

test("a stopped package says so on the card instead of spinning", async () => {
  const state = environment({ alwaysFail: bridgeError(503, "http_503", "package service unavailable") });
  const panelInstance = instantiate(state);
  await panelInstance.activate();

  // Nothing has ever been read, so the reason takes the headline immediately
  // rather than waiting for the degraded threshold.
  assert.equal(panelInstance.headline.title, "Package service unavailable");
  assert.equal(panelInstance.headline.detail, "Start the package in Package Center.");
  assert.equal(panelInstance.rootClasses[1], "is-warn");
  assert.equal(panelInstance.freshness, "Not read yet");

  // A permanently failing bridge must decay to the bounded floor and issue
  // exactly one read per scheduled poll -- no retry loop inside a cycle.
  for (let poll = 0; poll < 8; poll += 1) {
    const before = state.reads;
    await panelInstance.refresh();
    assert.equal(state.reads - before, 1, "one poll must cost exactly one request");
    assert.equal(pending(state).length, 1);
  }
  assert.equal(pending(state)[0].delay, WIDGET_MAX_POLL_MS);
  assert.equal(panelInstance.degraded, true);

  // ...and it must still stop dead when DSM takes the card away.
  panelInstance.deactivate();
  assert.equal(pending(state).length, 0);
  const settled = state.reads;
  await panelInstance.refresh();
  assert.equal(state.reads, settled, "a stopped package must not be polled off-screen");
});

test("a bridge failure after a good read keeps the last status until it is clearly not a blip", async () => {
  const healthy = snapshot({
    profiles: [profile("photos")],
    routines: [routine("photos", "succeeded", 1_699_996_400)]
  });
  const state = environment({
    responses: [healthy, bridgeError(401, "http_401"), bridgeError(401, "http_401")]
  });
  const panelInstance = instantiate(state);

  await panelInstance.activate();
  assert.equal(panelInstance.headline.title, "All profiles healthy");
  const stamp = panelInstance.freshness;
  assert.match(stamp, /^Updated /);

  // One failed read does not flap the card.
  await panelInstance.refresh();
  assert.equal(panelInstance.headline.title, "All profiles healthy");
  assert.equal(panelInstance.degraded, false);

  // A second one does, and now the card names the reason.
  await panelInstance.refresh();
  assert.equal(panelInstance.headline.title, "DSM session expired");
  assert.equal(panelInstance.degraded, true);
  // The last thing it actually knew stays on screen, with its age attached.
  assert.deepEqual(panelInstance.rows.map((row) => row.name), ["photos"]);
  assert.equal(panelInstance.freshness, stamp);

  // Recovery clears the issue without needing a remount.
  state.responses.push(healthy);
  await panelInstance.refresh();
  assert.equal(panelInstance.bridgeIssue, null);
  assert.equal(panelInstance.headline.title, "All profiles healthy");
  assert.equal(pending(state)[0].delay, WIDGET_IDLE_POLL_MS);
});

test("the card never shows a spinner it cannot stop", () => {
  // sdsync-is-spinning animates forever by design. A permanently failing card
  // on a NAS desktop must not be able to sit there spinning at anybody.
  assert.ok(!panel.includes("sdsync-is-spinning"));
  assert.ok(!adapter.includes("sdsync-is-spinning"));
  assert.ok(!/\bsdsync-widget[a-z-]*\.sdsync-is-spinning/.test(css));
});

test("a stored AppWindow cadence and theme reach the widget", async () => {
  const state = environment({ stored: JSON.stringify({ theme: "light", status_refresh: 30000 }) });
  const panelInstance = instantiate(state);
  await panelInstance.activate();
  assert.equal(panelInstance.themeClass, "is-light");
  assert.equal(pending(state)[0].delay, WIDGET_IDLE_POLL_MS);
  assert.equal(panelInstance.preferences.statusIntervalMs, 30000);
});

/**
 * Load the Ext adapter with DSM's desktop toolkit stubbed.
 *
 * The adapter is the one piece of this feature that cannot be exercised
 * anywhere but a real DSM desktop, so proving the callback wiring against a
 * stand-in Ext is the strongest evidence available short of a NAS.
 */
function loadAdapter(ext, syno, calls) {
  const source = `${adapter
    .replace(/import Vue from "vue";\s*/, "")
    .replace(/import WidgetPanel from "\.\/WidgetPanel\.vue";\s*/, "")
    .replace(/^export /gm, "")}\nreturn { installWidget, WIDGET_CLASS };`;
  const Vue = {
    extend: () => function MountedPanel() {
      this.$mount = (host) => calls.push(`mount:${host.tag}`);
      this.$destroy = () => calls.push("panel:destroy");
      this.activate = () => calls.push("panel:activate");
      this.deactivate = () => calls.push("panel:deactivate");
      this.setCompact = (compact) => calls.push(`panel:compact:${compact}`);
    }
  };
  const stubs = {
    Vue,
    WidgetPanel: { name: "WidgetPanel" },
    Ext: ext,
    SYNO: syno,
    document: { createElement: (tag) => ({ tag }) }
  };
  return Function(...Object.keys(stubs), source)(...Object.values(stubs));
}

function extStub(calls) {
  function ExtPanel() {}
  ExtPanel.prototype.afterRender = function afterRender() { calls.push("base:afterRender"); };
  ExtPanel.prototype.destroy = function destroy() { calls.push("base:destroy"); };
  return {
    Panel: ExtPanel,
    extend(parent, overrides) {
      function Subclass(config) { Object.assign(this, config); }
      Subclass.prototype = Object.create(parent.prototype);
      Object.assign(Subclass.prototype, overrides);
      Subclass.prototype.constructor = Subclass;
      return Subclass;
    }
  };
}

test("the adapter registers where Ext.getClassByName looks and drives the panel through DSM's callbacks", () => {
  const calls = [];
  const syno = { SDS: { App: { SynologyDriveSync: {} } } };
  const { installWidget, WIDGET_CLASS: registeredName } = loadAdapter(extStub(calls), syno, calls);

  const Widget = installWidget();
  assert.equal(registeredName, WIDGET_CLASS);
  assert.ok(Object.keys(appConfig).includes(registeredName), "app.config must register this class");
  // Reproduce Ext.getClassByName exactly -- it walks the dotted name across the
  // global object and returns whatever it lands on -- so the declared name is
  // proven to resolve to the class the adapter actually registered.
  const resolved = registeredName.split(".").reduce((scope, part) => (scope ? scope[part] : null), { SYNO: syno });
  assert.equal(resolved, Widget);
  assert.equal(installWidget(), Widget, "registration must be idempotent across bundle loads");

  const card = new Widget({ renderTo: {}, jsConfig: {}, width: 318, height: 84 });
  card.body = { dom: { appendChild: () => calls.push("append") } };
  assert.equal(card.minimizable, false);

  card.afterRender();
  assert.deepEqual(calls.slice(-3), ["base:afterRender", "append", "mount:div"]);

  card.onActivate();
  card.doCollapse();
  card.doExpand();
  card.onDeactivate();
  card.destroy();
  // destroy() stops polling unconditionally before tearing the panel down --
  // the same belt-and-braces order DSM's own SystemHealthWidget uses -- so the
  // second deactivate here is deliberate and idempotent, not a leak.
  assert.deepEqual(calls.slice(-7), [
    "panel:activate",
    "panel:compact:true",
    "panel:compact:false",
    "panel:deactivate",
    "panel:deactivate",
    "panel:destroy",
    "base:destroy"
  ]);
  // Anything DSM calls after destruction must not reach a torn-down panel.
  const settled = calls.length;
  card.onDeactivate();
  card.doExpand();
  assert.equal(calls.length, settled);
});

test("a card DSM activates before it renders still starts polling once its panel exists", () => {
  const calls = [];
  const syno = { SDS: { App: { SynologyDriveSync: {} } } };
  const { installWidget } = loadAdapter(extStub(calls), syno, calls);
  const card = new (installWidget())({ renderTo: {} });
  card.body = { dom: { appendChild: () => {} } };
  card.onActivate();
  assert.ok(!calls.includes("panel:activate"), "there is no panel to activate yet");
  card.afterRender();
  assert.ok(calls.includes("panel:activate"), "the mounted panel must inherit the active state");
});

test("a bundle load without the DSM desktop toolkit registers nothing and throws nothing", () => {
  const calls = [];
  assert.equal(loadAdapter(undefined, { SDS: { App: { SynologyDriveSync: {} } } }, calls).installWidget(), null);
  assert.equal(loadAdapter(extStub(calls), undefined, calls).installWidget(), null);
  assert.equal(loadAdapter({}, { SDS: { App: { SynologyDriveSync: {} } } }, calls).installWidget(), null);
  assert.equal(loadAdapter(extStub(calls), { SDS: {} }, calls).installWidget(), null);
});

test("the Ext adapter forwards every DSM widget lifecycle callback and registers at load", () => {
  assert.match(adapter, new RegExp(`export const WIDGET_CLASS = "${escapeRegex(WIDGET_CLASS)}";`));
  // DSM resolves the card class with Ext.getClassByName -- a dotted lookup on
  // the global -- and constructs it with renderTo, so it has to be an Ext.Panel.
  assert.match(adapter, /namespace\.Widget = ext\.extend\(ext\.Panel, \{/);
  for (const callback of ["onActivate", "onDeactivate", "doExpand", "doCollapse", "destroy", "afterRender"]) {
    assert.ok(adapter.includes(`${callback}(`), `adapter is missing DSM callback ${callback}`);
  }
  assert.ok(adapter.includes("this.widgetPanel.activate();"));
  assert.ok(adapter.includes("this.widgetPanel.deactivate();"));
  assert.ok(adapter.indexOf("this.onDeactivate();") < adapter.indexOf("base.destroy.apply(this, arguments);"));
  assert.ok(adapter.includes("minimizable: false"), "the widget must not add a third notification surface");

  // The same bundle opens the AppWindow, so a desktop without the Ext toolkit
  // must degrade to a no-op rather than throwing during module evaluation.
  assert.match(adapter, /if \(!ext \|\| !syno \|\| typeof ext\.extend !== "function" \|\| !ext\.Panel\) return null;/);
  assert.match(main, /import \{ installWidget \} from "\.\/widget";/);
  assert.match(main, /^installWidget\(\);$/m);
  assert.equal((main.match(/Vue\.extend\(/g) || []).length, 1);
});

test("the widget stays a read-only status surface on the documented snapshot endpoint", () => {
  assert.ok(panel.includes('const snapshot = await apiGet(this.auth, "snapshot");'));
  assert.equal((panel.match(/apiGet\(/g) || []).length, 1);
  for (const forbidden of ["apiPost", "ACTIONS", "X-SYNO-TOKEN", "SynoToken", "csrf", "setInterval(", "v-html", "innerHTML"]) {
    assert.ok(!panel.includes(forbidden), `widget panel must not contain ${forbidden}`);
    assert.ok(!adapter.includes(forbidden), `widget adapter must not contain ${forbidden}`);
  }
  assert.ok(panel.includes('if (snapshot.schema !== SNAPSHOT_SCHEMA) throw new Error("Unsupported DSM API schema");'));
  // DSM launches config.appInstance when the card's header icon is clicked, so
  // the widget's only navigation affordance leads to the reviewed AppWindow.
  assert.equal(appConfig[WIDGET_CLASS].appInstance, APP_CLASS);
  assert.equal(appConfig[WIDGET_CLASS].type, "widget");
});

test("widget palette is byte-identical to the AppWindow palette it sits beside", () => {
  for (const [appSelector, widgetSelector] of [
    [".sdsync-app", ".sdsync-widget"],
    [".sdsync-app.is-light", ".sdsync-widget.is-light"]
  ]) {
    const appPalette = hexVariables(appSelector);
    const widgetPalette = hexVariables(widgetSelector);
    assert.ok(widgetPalette.size >= 7, `${widgetSelector} must define its own palette`);
    for (const [name, value] of widgetPalette) {
      assert.equal(value, appPalette.get(name), `${widgetSelector} --${name} drifted from ${appSelector}`);
    }
  }
  // Every token the widget declares has to be one it actually spends, or the
  // parity check above is guarding dead values.
  const widgetCss = css.slice(css.indexOf("/* DSM desktop widget card."));
  for (const name of hexVariables(".sdsync-widget").keys()) {
    assert.ok(widgetCss.includes(`var(--${name})`), `unused widget palette token --${name}`);
  }
  // Every colour the card paints is read at 10-11px against the widget's own
  // background, which is the darkest surface in the palette. The status colours
  // carry meaning here, so they are held to body-text contrast, not to the
  // 3:1 that would be acceptable for a decorative accent.
  for (const selector of [".sdsync-widget", ".sdsync-widget.is-light"]) {
    const palette = hexVariables(selector);
    const background = palette.get("sdsync-bg");
    for (const token of ["sdsync-text", "sdsync-muted", "sdsync-fire", "sdsync-red", "sdsync-amber"]) {
      const ratio = contrast(palette.get(token), background);
      assert.ok(ratio >= 4.5, `${selector} --${token} contrast ${ratio.toFixed(2)} is below 4.5`);
    }
  }
  // The card is 318x84 medium and 318x168 expanded; it has to be able to clip.
  assert.match(declarations(".sdsync-widget"), /overflow:\s*hidden/);
  assert.match(declarations(".sdsync-widget"), /height:\s*168px/);
  assert.match(declarations(".sdsync-widget.is-compact"), /height:\s*84px/);
  assert.match(declarations(".sdsync-widget-profiles"), /overflow-y:\s*auto/);
});
