// What observes a routine: idle and watching, partly observed, or blind since a
// time, from the six fields both engines write to its state.
import assert from "node:assert/strict";
import test from "node:test";
import { appContext, loadApp } from "./helpers/load-app.mjs";

const { AppComponent, routineObservationText } = await loadApp({ exports: "routineObservationText" });

function realtime(overrides = {}) {
  return {
    profile: "nightly", enabled: true, mode: "realtime", backend: "inotify", backend_reason: "ok", state: "succeeded",
    next_run_epoch: 0, last_poll_epoch: 0, ...overrides
  };
}

test("an observed routine says what watches it and when it last saw a change and ran", () => {
  const idle = routineObservationText(realtime({
    watching_via: "native", observe_state: "ok", observe_cause: "none",
    last_trigger_epoch: 1794165000, last_run_epoch: 1794165100
  }));
  assert.match(idle, /^idle: watching via native events, last change seen .+, last run .+$/);
  assert.doesNotMatch(idle, /none yet/);
  // A package from before the fields: ok, watching via nothing, nothing seen yet.
  assert.equal(
    routineObservationText(realtime()),
    "idle: watching via nothing, last change seen none yet, last run none yet"
  );
});

test("a blind or partly observed routine says since when and why", () => {
  const blind = routineObservationText(realtime({ observe_state: "blind", observe_cause: "poll_walk_timeout", observe_since_epoch: 1794165000, watching_via: "poll" }));
  assert.match(blind, /^blind since .+: the poll walk of the source did not finish within its budget and was killed; changes are not noticed until it clears$/);
  const degraded = routineObservationText(realtime({ observe_state: "degraded", observe_cause: "poll_floor_failed", observe_since_epoch: 1794165000, watching_via: "native" }));
  assert.match(degraded, /^partly observed since .+: the backstop poll under the native watch cannot finish \(watching via native events\)$/);
  // A backend_reason token keeps its existing guidance; an unknown one is still a cause.
  assert.match(routineObservationText(realtime({ observe_state: "blind", observe_cause: "watch_limit", observe_since_epoch: 1 })), /inotify watch budget/);
  assert.match(routineObservationText(realtime({ observe_state: "blind", observe_cause: "brand_new_cause", observe_since_epoch: 1 })), /its source is not being watched/);
  assert.equal(routineObservationText(realtime({ observe_state: "disabled", observe_cause: "disabled" })), "disabled");
});

test("blind routines count as not observing and every blind or partly observed routine is an attention item", () => {
  const routines = [
    realtime({ profile: "blind", observe_state: "blind", observe_cause: "lasting_block", observe_since_epoch: 1794165000 }),
    realtime({ profile: "partial", observe_state: "degraded", observe_cause: "poll_floor_failed", observe_since_epoch: 1794165000 }),
    { profile: "daily", enabled: true, mode: "daily", backend: "daily", observe_state: "blind", observe_cause: "controller_blocked", observe_since_epoch: 1794165000 },
    // Already named by its demoted-watcher item: not listed twice.
    realtime({ profile: "limited", backend: "polling", backend_reason: "watch_limit", observe_state: "degraded", observe_cause: "watch_limit", observe_since_epoch: 1 }),
    realtime({ profile: "fine", observe_state: "ok", watching_via: "native" }),
    realtime({ profile: "off", enabled: false, observe_state: "blind", observe_cause: "disabled" })
  ];
  const context = appContext(AppComponent, { snapshot: { routines } });
  Object.defineProperty(context, "routines", { get: () => routines, configurable: true });
  // "blind" by its observation, "limited" by its watch_limit reason as before.
  assert.equal(context.realtimeDetail, "2 not observing changes");
  const items = context.realtimeAttentionItems;
  assert.deepEqual(items.map((item) => item.id).sort(), ["observe-blind", "observe-daily", "observe-partial", "realtime-limited"]);
  const blind = items.find((item) => item.id === "observe-blind");
  assert.equal(blind.level, "warn");
  assert.equal(blind.title, "Routine blind is not observing changes (lasting_block)");
  assert.match(blind.detail, /automatic runs are held after a failed run/);
  assert.equal(items.find((item) => item.id === "observe-partial").title, "Routine partial is only partly observed (poll_floor_failed)");
  assert.match(items.find((item) => item.id === "observe-daily").detail, /scheduling is blocked/);
});
