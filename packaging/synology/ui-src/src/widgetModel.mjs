/**
 * Pure snapshot-to-view-model derivation and poll cadence policy for the DSM
 * desktop widget.
 *
 * This module deliberately imports nothing. DSM instantiates the widget on the
 * desktop document, outside the AppWindow and outside any Vue tree, so the
 * decisions that matter most here -- how often a background card on a NAS
 * desktop is allowed to talk to the package, and what a 318x84 pixel card is
 * allowed to claim about sync health -- must be reviewable and testable as
 * plain data, with no DOM, no transport, and no bundler.
 */

// DSM's own desktop widgets poll on a 60-second registered interval:
// SYNO.SDS.SystemInfoApp.SystemHealthWidget calls pollReg({interval: 60}) and
// SYNO.SDS.ResourceMonitor.PollingInterval is 5 only because it rides a shared
// push socket rather than a request per widget. A third-party widget that sits
// on the desktop for the entire life of a DSM session has no business being
// more eager than the first-party ones, so 60 s is the resting cadence.
export const WIDGET_IDLE_POLL_MS = 60000;
// While a run is genuinely in flight the card is worth refreshing sooner --
// otherwise "a run is active" can be a minute stale in both directions. This
// stays well above the AppWindow's 5000 ms default so the widget is never the
// eager surface, and widgetPollDelay() still clamps it up to the AppWindow's
// own configured cadence when the operator has chosen a slower one.
export const WIDGET_ACTIVE_POLL_MS = 20000;
// Failure backoff. A NAS whose package service is stopped, whose bridge is
// misconfigured, or whose session has expired will fail every single request,
// forever, on every desktop that has the widget pinned. Decaying to a five
// minute floor keeps a broken install from being a standing load generator
// while still recovering on its own once the cause is fixed.
export const WIDGET_BACKOFF_RAMP_MS = Object.freeze([60000, 120000, 240000, 300000]);
export const WIDGET_MAX_POLL_MS = WIDGET_BACKOFF_RAMP_MS[WIDGET_BACKOFF_RAMP_MS.length - 1];

export const WIDGET_LEVELS = Object.freeze({
  ok: "ok",
  warn: "warn",
  fail: "fail",
  idle: "idle",
  unknown: "unknown"
});

function objectOf(value) {
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

function arrayOf(value) {
  return Array.isArray(value) ? value : [];
}

function textOf(value) {
  return typeof value === "string" ? value : "";
}

function numberOf(value) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : 0;
}

function plural(count, singular, pluralForm) {
  return `${count} ${count === 1 ? singular : pluralForm}`;
}

/**
 * Choose the delay before the next snapshot read.
 *
 * `appWindowIntervalMs` is the AppWindow's own configured status cadence. The
 * widget is a glance surface, not the dashboard, so it must never be the more
 * frequent of the two: clamping the computed delay up to that value keeps the
 * promise even when an operator raises the AppWindow to its slowest setting.
 * A value of 0 means the AppWindow is on manual refresh and imposes no floor,
 * which leaves the widget on its own resting cadence rather than going silent.
 */
export function widgetPollDelay(options = {}) {
  const settings = objectOf(options);
  const rawFailures = Number(settings.failures);
  const failures = Number.isFinite(rawFailures) && rawFailures > 0 ? Math.floor(rawFailures) : 0;
  const base = failures > 0
    ? WIDGET_BACKOFF_RAMP_MS[Math.min(failures - 1, WIDGET_BACKOFF_RAMP_MS.length - 1)]
    : (settings.active === true ? WIDGET_ACTIVE_POLL_MS : WIDGET_IDLE_POLL_MS);
  const configured = Number(settings.appWindowIntervalMs);
  const floor = Number.isFinite(configured) && configured > 0 ? configured : 0;
  return Math.min(WIDGET_MAX_POLL_MS, Math.max(base, floor));
}

/**
 * Render an epoch as a glanceable age.
 *
 * The widget has one short line per profile, so an absolute timestamp costs
 * more width than it returns. The seconds-or-milliseconds discrimination
 * matches formatDate() in the API module so both surfaces read the same field
 * the same way.
 */
export function relativeEpochLabel(epoch, nowMs = Date.now()) {
  const seconds = numberOf(epoch);
  if (seconds <= 0) return "never";
  const milliseconds = seconds < 100000000000 ? seconds * 1000 : seconds;
  const elapsed = Math.round((numberOf(nowMs) - milliseconds) / 1000);
  if (elapsed < 60) return "just now";
  if (elapsed < 3600) return `${Math.floor(elapsed / 60)} min ago`;
  if (elapsed < 86400) return `${Math.floor(elapsed / 3600)} h ago`;
  if (elapsed < 2592000) return `${Math.floor(elapsed / 86400)} d ago`;
  return `${Math.floor(elapsed / 2592000)} mo ago`;
}

/**
 * Describe the single global run record.
 *
 * The snapshot carries exactly one run object for the whole package, not one
 * per profile, and its `scope` names either "all" or a single profile. That is
 * the only attribution available, so the widget reports it as-is instead of
 * implying a per-profile run history the package does not keep.
 */
export function widgetRun(snapshot) {
  const model = objectOf(snapshot);
  const current = objectOf(model.run);
  // Same two-name read the AppWindow uses, so both surfaces survive a snapshot
  // that carries the older `last_run` spelling.
  const run = current.state === undefined ? objectOf(model.last_run) : current;
  const state = textOf(run.state);
  const scope = textOf(run.scope);
  const operation = textOf(run.operation);
  const named = scope && scope !== "none" && scope !== "all" ? scope : "";
  const noun = operation === "plan" ? "Plan" : "Sync";
  return {
    active: state === "running",
    state,
    scope,
    operation,
    profile: named,
    label: state === "running"
      ? `${noun} running${named ? ` · ${named}` : (scope === "all" ? " · all profiles" : "")}`
      : "",
    startedEpoch: numberOf(run.started_epoch),
    finishedEpoch: numberOf(run.finished_epoch)
  };
}

/**
 * Fold one profile into a single widget row.
 *
 * The per-profile evidence in the snapshot is split across two records and
 * neither is a complete "last run": a routine carries its own last execution
 * state plus `last_success_epoch`, and the profile carries the cached Doctor
 * result plus `checked_epoch`. The routine is the better answer where one
 * exists because it describes actual sync activity; the Doctor result is the
 * honest fallback and is labelled as such rather than being passed off as a
 * run. This mirrors how the AppWindow's healthRows computed property joins the
 * same two records.
 */
function widgetProfileRow(profile, routines, run, nowMs) {
  const model = objectOf(profile);
  const name = textOf(model.name);
  const health = objectOf(model.health);
  const routine = arrayOf(routines).find((item) => textOf(objectOf(item).profile) === name);
  const routineModel = objectOf(routine);
  const routineState = textOf(routineModel.state);
  const runningRun = run.active && (run.scope === "all" || run.profile === name);
  const running = runningRun || routineState === "running";

  let level = WIDGET_LEVELS.unknown;
  let outcome = "Unknown";
  let source = "none";
  let epoch = 0;
  let epochPrefix = "";

  if (routine) {
    source = "routine";
    epoch = numberOf(routineModel.last_success_epoch);
    epochPrefix = "last success";
    if (routineState === "failed") {
      level = WIDGET_LEVELS.fail;
      outcome = "Failed";
    } else if (routineState === "succeeded") {
      level = WIDGET_LEVELS.ok;
      outcome = "Succeeded";
    } else if (routineState === "deferred") {
      level = WIDGET_LEVELS.warn;
      outcome = "Deferred";
    } else if (routineState === "scheduled") {
      level = WIDGET_LEVELS.idle;
      outcome = "Scheduled";
    } else if (routineState === "never") {
      level = WIDGET_LEVELS.idle;
      outcome = "Not yet run";
    }
  } else {
    source = "health";
    epoch = numberOf(health.checked_epoch);
    epochPrefix = "checked";
    const healthState = textOf(health.state);
    if (healthState === "failed") {
      level = WIDGET_LEVELS.fail;
      outcome = "Doctor failed";
    } else if (healthState === "succeeded") {
      level = WIDGET_LEVELS.ok;
      outcome = "Doctor passed";
    } else {
      level = WIDGET_LEVELS.idle;
      outcome = "No routine";
    }
  }

  if (running) {
    level = WIDGET_LEVELS.warn;
    outcome = "Running";
  }

  return {
    name,
    level,
    outcome,
    running,
    source,
    epoch,
    age: relativeEpochLabel(epoch, nowMs),
    detail: epoch > 0 ? `${epochPrefix} ${relativeEpochLabel(epoch, nowMs)}` : (
      source === "routine" ? "no successful run yet" : "never checked"
    )
  };
}

export function widgetProfileRows(snapshot, nowMs = Date.now()) {
  const model = objectOf(snapshot);
  const routines = arrayOf(model.routines);
  const run = widgetRun(model);
  return arrayOf(model.profiles)
    .map((profile) => widgetProfileRow(profile, routines, run, nowMs))
    .filter((row) => row.name !== "");
}

/**
 * Roll the whole estate up into the one headline the card can afford.
 *
 * The ordering is a severity ladder, not a summary: anything that makes the
 * reported state untrustworthy (no snapshot, an unverified service identity)
 * outranks a real failure, a real failure outranks a stopped service, and a
 * stopped service outranks "some profiles have never run". A widget that
 * reports "Healthy" while one profile is failing is worse than no widget.
 */
export function widgetOverview(snapshot, nowMs = Date.now()) {
  if (!snapshot || typeof snapshot !== "object" || Array.isArray(snapshot)) {
    return {
      level: WIDGET_LEVELS.unknown,
      title: "Status unavailable",
      detail: "The package status bridge has not answered yet."
    };
  }
  const service = objectOf(snapshot.service);
  const serviceState = textOf(service.state);
  if (serviceState === "untrusted") {
    return {
      level: WIDGET_LEVELS.fail,
      title: "Service identity untrusted",
      detail: "Open the dashboard; the controller process failed its identity check."
    };
  }
  const rows = widgetProfileRows(snapshot, nowMs);
  const failing = rows.filter((row) => row.level === WIDGET_LEVELS.fail);
  if (failing.length) {
    return {
      level: WIDGET_LEVELS.fail,
      title: `${plural(failing.length, "profile", "profiles")} failing`,
      detail: failing.map((row) => row.name).join(", ")
    };
  }
  if (!rows.length) {
    return {
      level: WIDGET_LEVELS.idle,
      title: "No profiles configured",
      detail: "Create a profile in the dashboard to start syncing."
    };
  }
  if (serviceState !== "running") {
    return {
      level: WIDGET_LEVELS.warn,
      title: "Package service stopped",
      detail: "Routines and schedules do not run while the service is stopped."
    };
  }
  const pending = rows.filter((row) => row.level === WIDGET_LEVELS.idle);
  if (pending.length === rows.length) {
    return {
      level: WIDGET_LEVELS.idle,
      title: "Waiting for the first run",
      detail: `${plural(rows.length, "profile", "profiles")} configured.`
    };
  }
  if (pending.length) {
    return {
      level: WIDGET_LEVELS.warn,
      title: `${plural(pending.length, "profile", "profiles")} not yet run`,
      detail: pending.map((row) => row.name).join(", ")
    };
  }
  return {
    level: WIDGET_LEVELS.ok,
    title: "All profiles healthy",
    detail: `${plural(rows.length, "profile", "profiles")} reporting success.`
  };
}

/**
 * The named in-service failures the package bridge can now report.
 *
 * Every one of these arrives as a semantic 503 and used to be indistinguishable
 * from every other 503, so a card whose package was mid-upgrade and a card whose
 * package files had been left group-writable said the same sentence. The titles
 * are the AppWindow's, character for character -- `BRIDGE_FAILURE_COPY` in
 * App.vue is the other half of this table and a test compares the two surfaces
 * code by code. Only `detail` is shortened here, to fit a 318-pixel line.
 *
 * `level` follows the service's own split. A transient unavailability is `warn`
 * and says the card will look again; a runtime the package refuses to vouch for
 * is `fail` and names the repair, because waiting will not fix it.
 */
const WIDGET_BRIDGE_FAILURES = Object.freeze({
  runtime_upgrading: Object.freeze({
    level: "warn", title: "Package is upgrading", detail: "Retrying while the upgrade finishes."
  }),
  runtime_uninstalling: Object.freeze({
    level: "warn", title: "Package is being removed", detail: "Synology Drive Sync is being uninstalled."
  }),
  runtime_closed: Object.freeze({
    level: "warn", title: "Package is stopping or upgrading", detail: "Retrying while it settles."
  }),
  runtime_marker_unsafe: Object.freeze({
    level: "fail", title: "Package state unconfirmed", detail: "Inspect the package API log."
  }),
  policy_unreadable: Object.freeze({
    level: "fail", title: "Security policy unreadable", detail: "Repair the package."
  }),
  manager_busy: Object.freeze({
    level: "warn", title: "Package is busy", detail: "Retrying shortly."
  }),
  manager_lane_poisoned: Object.freeze({
    level: "fail", title: "Package service needs a restart", detail: "Restart it in Package Center."
  }),
  manager_unsafe: Object.freeze({
    level: "fail", title: "Package files are not in a safe state", detail: "Repair or reinstall the package."
  }),
  manager_spawn_failed: Object.freeze({
    level: "warn", title: "Package helper could not start", detail: "Retrying."
  }),
  manager_timeout: Object.freeze({
    level: "warn", title: "Package took too long to answer", detail: "Retrying."
  }),
  manager_output_too_large: Object.freeze({
    level: "warn", title: "Package answer was too large", detail: "Inspect the package API log."
  }),
  manager_exit_status: Object.freeze({
    level: "warn", title: "Package could not assemble this view", detail: "Inspect the package API log."
  }),
  manager_output_invalid: Object.freeze({
    level: "warn", title: "Package answer could not be read", detail: "Inspect the package API log."
  }),
  manager_output_schema: Object.freeze({
    level: "fail", title: "UI and package versions differ", detail: "Repair or reinstall one complete release."
  }),
  config_file_unsafe: Object.freeze({
    level: "fail", title: "Package file permissions are unsafe", detail: "Repair the package."
  }),
  package_state_corrupt: Object.freeze({
    level: "warn", title: "Package record is corrupt", detail: "Inspect Logs; restarting does not repair it."
  }),
  clock_unavailable: Object.freeze({
    level: "fail", title: "NAS clock is not set", detail: "Set the NAS system time."
  })
});

/**
 * Classify why the package bridge did not answer.
 *
 * For a card that lives on the DSM desktop permanently, "not authenticated" and
 * "package stopped" are not edge cases -- they are its resting states on a
 * stopped or newly installed NAS, and "Status unavailable" is a useless thing
 * to stare at for a week. The titles are the exact ones App.vue's
 * describeBridgeError produces for the same conditions so the two surfaces
 * never disagree about what is wrong; only the guidance is shortened to fit a
 * 318-pixel line.
 *
 * The error is read by duck-typed `status`/`code`, the same way the AppWindow
 * reads it, so the widget stays decoupled from the transport's error class.
 */
export function widgetBridgeIssue(error) {
  const status = Number(error && error.status) || 0;
  const code = textOf(error && error.code).toLowerCase();
  const message = textOf(error && error.message).toLowerCase();
  const issue = (level, title, detail) => ({ level, title, detail });

  if (status === 401) {
    return issue(WIDGET_LEVELS.warn, "DSM session expired", "Sign in to DSM again.");
  }
  if (status === 403) {
    return issue(WIDGET_LEVELS.warn, "DSM access denied", "Requires a DSM administrator account.");
  }
  // Ahead of the bare 503 below, which would otherwise claim all of these and
  // tell the operator to start a package that is already running and merely
  // busy, or mid-upgrade, or refusing to vouch for its own files.
  const named = WIDGET_BRIDGE_FAILURES[code];
  if (named && (status === 503 || status === 0)) {
    return issue(WIDGET_LEVELS[named.level], named.title, named.detail);
  }
  // Ordered ahead of the substring tests below, exactly as App.vue orders it:
  // the specific codes for a helper or web API that could not start are spelled
  // `dsm_authentication_*_unavailable`, so a generic "authentication" match
  // would otherwise claim them and tell the operator to sign in again while the
  // real answer is that the package is not running.
  if (status === 503 || code.includes("unavailable")) {
    return issue(WIDGET_LEVELS.warn, "Package service unavailable", "Start the package in Package Center.");
  }
  if (status === 404 || code === "non_json_response" || code === "malformed_json") {
    return issue(WIDGET_LEVELS.fail, "Package UI route unavailable", "Repair or reinstall the package release.");
  }
  if (message.includes("unsupported dsm api schema") || code === "invalid_document") {
    return issue(WIDGET_LEVELS.fail, "UI and package versions differ", "Repair or reinstall one complete release.");
  }
  if (code.includes("forbidden")) {
    return issue(WIDGET_LEVELS.warn, "DSM access denied", "Requires a DSM administrator account.");
  }
  if (code.includes("unauthorized") || code.includes("authentication") || message.includes("redirect")) {
    return issue(WIDGET_LEVELS.warn, "DSM session expired", "Sign in to DSM again.");
  }
  // Anything left is a transport that did not complete: an unreachable NAS, a
  // dropped connection, a proxy in the way. The card says it does not know,
  // rather than blaming a component it has no evidence against.
  return issue(WIDGET_LEVELS.unknown, "Package bridge unavailable", "Confirm the package is running.");
}

/**
 * The AppWindow persists its refresh cadence under this key. Reading it lets
 * the widget honour an operator who has already told this package to slow
 * down, instead of making them state the same preference twice.
 */
export const APPWINDOW_SETTINGS_KEY = "sdsync.ui.settings.v1";
export const APPWINDOW_STATUS_INTERVALS_MS = Object.freeze([0, 1000, 3000, 5000, 10000, 30000]);
export const APPWINDOW_THEMES = Object.freeze(["dark", "light", "system"]);

/**
 * Read the AppWindow's stored theme and status cadence.
 *
 * Everything here fails to the dark default: the value is operator preference,
 * never authentication or package state, and a widget that throws while the
 * DSM desktop is booting is worse than a widget showing the default theme.
 */
export function appWindowPreferences(storedValue) {
  const fallback = { theme: "dark", statusIntervalMs: 0 };
  try {
    const parsed = JSON.parse(typeof storedValue === "string" && storedValue ? storedValue : "null");
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return fallback;
    const interval = Number(parsed.status_refresh);
    return {
      theme: APPWINDOW_THEMES.includes(parsed.theme) ? parsed.theme : fallback.theme,
      statusIntervalMs: APPWINDOW_STATUS_INTERVALS_MS.includes(interval) ? interval : fallback.statusIntervalMs
    };
  } catch (_error) {
    return fallback;
  }
}
