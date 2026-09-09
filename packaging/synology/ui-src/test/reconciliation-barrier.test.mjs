import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

function loadAppComponent({ probe = async () => ({}), refreshSnapshot = async () => true } = {}) {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "App.vue script block is missing");
  const executable = script[1]
    .replace(/^import \{ ActionIcon \} from "\.\/ActionIcon";\s*/m, "")
    .replace(/^import \{ createAutosaveCoordinator \} from "\.\/autosave";\s*/m, "")
    .replace(/^import \{ installControlLayout \} from "\.\/controlLayout";\s*/m, "")
    .replace(/import \{[\s\S]*?\}\s*from "\.\/api";\s*/, "")
    .replace(/^import SecurityPanel from "\.\/SecurityPanel\.vue";\s*/m, "")
    .replace("export default {", "const AppComponent = {")
    + "\nreturn AppComponent;";
  const stubs = {
    ACTIONS: {
      configureProfile: "configure-profile", setSecret: "set-secret",
      testProfileAuth: "test-profile-auth", browseRemote: "browse-remote",
      routine: "routine", alertPolicy: "alert-policy", securityPolicy: "security-policy",
      clientEvent: "client-event", removeProfile: "remove-profile", execute: "action"
    },
    AUTOSAVE_API_LIMITS: Object.freeze({}),
    MAX_RESPONSE_BYTES: 1024 * 1024,
    QueuedOutcomeUnknownError: class extends Error {},
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    SYNC_STATUS_MAX_LIMIT: 200,
    apiGet: async () => ({}),
    apiPost: async () => ({ ok: true }),
    probeRequestOutcome: probe,
    purgeReconciliationAuth: () => undefined,
    reconcileMutationRequest: async () => ({}),
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback).slice(0, 65536),
    formatBytes: String,
    formatDate: (value) => `@${value}`,
    formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    createAutosaveCoordinator: () => ({
      cancel() {}, dispose() {}, getState: () => ({ registered: false, dirty: false }),
      hydrate() {}, setGlobalBusy() {}, setScopeBlocked() {}, update: () => ({ dirty: false })
    }),
    installControlLayout: () => () => {},
    ActionIcon: { name: "ActionIcon" },
    SecurityPanel: {}
  };
  const component = Function(...Object.keys(stubs), executable)(...Object.values(stubs));
  component.refreshSnapshotStub = refreshSnapshot;
  return component;
}

const REQUEST_ID = "e1c45b23c3ef99bb24dd72d610496118";
const JOB_ID = "d".repeat(48);

function scopeIncident(overrides = {}) {
  return {
    active: true, outcomeUnknown: true, requiresInspection: true,
    message: "outcome unknown", requestId: REQUEST_ID, jobId: "", subject: "",
    operation: "security-policy", stage: "", transportStage: "", secretKind: "",
    expectedConfiguration: null, creatingProfile: false, ...overrides
  };
}

function isolatedIncident(overrides = {}) {
  return {
    active: true, kind: "operations", operation: "action", outcomeUnknown: true,
    requiresInspection: true, settled: false, message: "outcome unknown",
    requestId: REQUEST_ID, jobId: "", subject: "", retryable: false, ...overrides
  };
}

function emptyScopes(value = false) {
  return { profile: value, routine: value, alerts: value, security: value, interface: value };
}

function context(component, overrides = {}) {
  const toasts = [];
  const base = {
    disposed: false,
    auth: {},
    incidentProbe: { active: false, scope: "", verdict: "", attempts: 0, checkedAt: 0, jobId: "", progress: null, message: "" },
    incidentProbeStep: 0,
    incidentProbeTimer: 0,
    autosaveIncidents: {},
    isolatedIncidents: {},
    autosaveFailureScopes: emptyScopes(),
    autosaveOutcomeUnknownScopes: emptyScopes(),
    autosaveInspectionScopes: emptyScopes(),
    toasts,
    toast(title, message) { toasts.push({ title, message }); },
    refreshAutosaveStatus() {},
    scheduleIncidentProbe() {},
    refreshSnapshot: component.refreshSnapshotStub,
    ...overrides
  };
  base.releaseIncidentScope = (scope) => component.methods.releaseIncidentScope.call(base, scope);
  base.checkIncidentOutcomes = (manual) => component.methods.checkIncidentOutcomes.call(base, manual);
  return base;
}

// Finding 2: the manual Reconcile controls only ever covered four operations, so
// Run/Doctor and the routine, alerts, security and interface autosave scopes
// rendered a barrier with no control at all. Establishing what happened applies
// nothing, so it is offered for every locked scope that names a request.
test("every locked scope that names a request is probeable, not only the four with an apply path", () => {
  const component = loadAppComponent();
  const targets = component.computed.incidentProbeTargets.call({
    autosaveIncidents: { security: scopeIncident() },
    isolatedIncidents: { operations: isolatedIncident() },
    autosaveOutcomeUnknownScopes: { ...emptyScopes(), security: true },
    autosaveInspectionScopes: emptyScopes()
  });
  assert.deepEqual(targets.map((target) => target.scope), ["security", "operations"]);

  // An incident with no trusted request ID has nothing exact to ask about.
  assert.deepEqual(component.computed.incidentProbeTargets.call({
    autosaveIncidents: { security: scopeIncident({ requestId: "" }) },
    isolatedIncidents: {},
    autosaveOutcomeUnknownScopes: { ...emptyScopes(), security: true },
    autosaveInspectionScopes: emptyScopes()
  }), []);
});

test("the barrier offers Check again now and keeps the running account out of the assertive alert", () => {
  assert.match(appSource, /v-if="incidentProbeTargets\.length"[\s\S]*?@click="checkIncidentOutcomes\(true\)"/);
  assert.match(appSource, /class="sdsync-barrier-progress" role="status" aria-live="polite"/);
  // The barrier itself stays assertive; a per-tick counter inside it would make
  // a screen reader interrupt itself every few seconds.
  const barrier = appSource.match(/<div v-if="incidentOutcomeUnresolved"[^>]*>/);
  assert.ok(barrier);
  assert.match(barrier[0], /role="alert" aria-live="assertive"/);
});

test("the running account reports attempts, when it last looked, and the published step", () => {
  const component = loadAppComponent();
  const render = (probe) => component.computed.incidentProbeAccount.call({
    incidentProbeTargets: [{ scope: "operations", incident: isolatedIncident() }],
    incidentProbe: probe
  });

  assert.equal(render({ attempts: 0, active: false }), "Preparing to check the preserved request with DSM…");
  assert.equal(render({ attempts: 0, active: true }), "Checking with DSM now…");

  const account = render({
    attempts: 12,
    active: false,
    checkedAt: 1757000000,
    message: "DSM accepted this request and its job is still running.",
    jobId: JOB_ID,
    progress: { step: 7, total: 16, label: "DSM session authentication" }
  });
  assert.match(account, /Still reconciling · checked 12 times · last at @1757000000\./);
  assert.match(account, /Step 7 of 16: DSM session authentication\./);
  assert.match(account, new RegExp(`Queued job ID: ${JOB_ID}\\.`));
  assert.match(render({ attempts: 1, active: false, checkedAt: 1, message: "x" }), /checked 1 time ·/);

  // Nothing left to reconcile means no account to give.
  assert.equal(component.computed.incidentProbeAccount.call({
    incidentProbeTargets: [],
    incidentProbe: { attempts: 4 }
  }), "");
});

test("blocked and available are derived from the same predicates the controls consult", () => {
  const component = loadAppComponent();
  const availability = component.computed.incidentScopeAvailability.call({
    autosaveIncidents: { alerts: scopeIncident({ operation: "alert-policy" }) },
    isolatedIncidents: {},
    autosaveOutcomeUnknownScopes: { ...emptyScopes(), alerts: true },
    autosaveInspectionScopes: emptyScopes()
  });
  assert.equal(availability.blocked, "Package alerts");
  assert.match(availability.available, /Profile configuration and secrets/);
  assert.match(availability.available, /Run and Doctor operations/);
  assert.match(availability.available, /Security policy/);
  assert.doesNotMatch(availability.available, /Package alerts/);

  // A locked profile also gates routines and Run / Doctor, and the list says so
  // rather than asserting that unrelated controls remain available.
  const profileLocked = component.computed.incidentScopeAvailability.call({
    autosaveIncidents: { profile: scopeIncident({ operation: "configure-profile" }) },
    isolatedIncidents: {},
    autosaveOutcomeUnknownScopes: { ...emptyScopes(), profile: true },
    autosaveInspectionScopes: emptyScopes()
  });
  assert.match(profileLocked.blocked, /Routines/);
  assert.match(profileLocked.blocked, /Run and Doctor operations/);
  assert.match(profileLocked.available, /Package alerts/);
});

test("a settled verdict releases a scope that has nothing to apply, and refreshes it", async () => {
  let refreshed = 0;
  const component = loadAppComponent({
    probe: async () => ({ verdict: "settled", job_id: JOB_ID, checked_at: 1757000001, progress: null }),
    refreshSnapshot: async () => { refreshed += 1; return true; }
  });
  const scope = context(component, {
    autosaveIncidents: { security: scopeIncident() },
    autosaveFailureScopes: { ...emptyScopes(), security: true },
    autosaveOutcomeUnknownScopes: { ...emptyScopes(), security: true },
    autosaveInspectionScopes: { ...emptyScopes(), security: true }
  });

  await scope.checkIncidentOutcomes(true);

  assert.equal(scope.incidentProbe.verdict, "settled");
  assert.equal(scope.incidentProbe.attempts, 1);
  assert.equal(scope.autosaveIncidents.security.active, false);
  assert.equal(scope.autosaveOutcomeUnknownScopes.security, false);
  assert.equal(scope.autosaveInspectionScopes.security, false);
  assert.equal(scope.autosaveFailureScopes.security, false);
  assert.equal(refreshed, 1);
  assert.equal(scope.toasts.length, 1);
  assert.match(scope.toasts[0].message, /No new request was submitted/);
});

// The guard's premise is "the outcome is unknown". These verdicts do not
// disprove it, so the lock stays exactly where it was.
test("accepted, absent, unknown and unavailable all keep the lock", async () => {
  for (const verdict of ["accepted", "absent", "unknown", "unavailable"]) {
    const component = loadAppComponent({ probe: async () => ({ verdict, checked_at: 1 }) });
    const scope = context(component, {
      isolatedIncidents: { operations: isolatedIncident() }
    });
    await scope.checkIncidentOutcomes();
    assert.equal(scope.incidentProbe.verdict, verdict, `verdict ${verdict}`);
    assert.equal(scope.isolatedIncidents.operations.active, true, `${verdict} must not unlock`);
    assert.equal(scope.toasts.length, 0, `${verdict} must not claim resolution`);
  }
});

// A user reading "no record found" would otherwise conclude it never happened
// and retry, which is the exact thing the guard exists to prevent.
test("absent says what it does not rule out", () => {
  const copy = appSource.match(/absent: "([^"]+)"/);
  assert.ok(copy, "the absent verdict must carry its own copy");
  assert.match(copy[1], /does not confirm it never ran/);
  assert.match(copy[1], /reaped/);
});

test("a settled verdict never unlocks a scope that has an apply path", async () => {
  const component = loadAppComponent({
    probe: async () => ({ verdict: "settled", job_id: JOB_ID, checked_at: 1 })
  });
  const scope = context(component, {
    autosaveIncidents: { profile: scopeIncident({ operation: "configure-profile" }) },
    isolatedIncidents: { connection: isolatedIncident({ operation: "test-profile-auth" }) },
    autosaveOutcomeUnknownScopes: { ...emptyScopes(), profile: true },
    autosaveInspectionScopes: emptyScopes()
  });

  await scope.checkIncidentOutcomes();

  assert.equal(scope.autosaveIncidents.profile.active, true);
  assert.equal(scope.isolatedIncidents.connection.active, true);
  assert.equal(scope.autosaveOutcomeUnknownScopes.profile, true);
  assert.equal(scope.toasts.length, 0);
  // Those two keep the reviewed manual Reconcile path, which verifies a fresh
  // snapshot against the preserved draft before it unlocks anything.
  assert.match(appSource, /const SELF_RELEASING_INCIDENT_SCOPES = Object\.freeze\(\[([^\]]*)\]\)/);
  const releasing = appSource.match(/const SELF_RELEASING_INCIDENT_SCOPES = Object\.freeze\(\[([^\]]*)\]\)/)[1];
  assert.doesNotMatch(releasing, /"profile"/);
  assert.doesNotMatch(releasing, /"connection"/);
});

test("the probe walks a decaying ramp and stands down when the tab is hidden", () => {
  const ramp = appSource.match(/const INCIDENT_PROBE_RAMP_MS = Object\.freeze\(\[([^\]]*)\]\)/);
  assert.ok(ramp, "the probe must ramp rather than poll at a fixed rate");
  const steps = ramp[1].split(",").map((value) => Number(value.trim()));
  assert.equal(steps[0] >= 1000, true, "the first retry must not hammer a constrained NAS");
  assert.deepEqual(steps, [...steps].sort((left, right) => left - right), "the ramp must decay");

  const schedule = appSource.slice(appSource.indexOf("scheduleIncidentProbe() {"));
  assert.match(schedule.slice(0, 500), /if \(this\.disposed \|\| document\.hidden \|\| !probeableIncidents\(this\)\.length\) return;/);
  assert.match(appSource, /stopTimers\(\) \{[^}]*clearTimeout\(this\.incidentProbeTimer\)/);
});
