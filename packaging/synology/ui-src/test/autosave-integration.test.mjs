import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const autosaveSource = await readFile(new URL("../src/autosave.js", import.meta.url), "utf8");

async function loadAutosave() {
  return import(`data:text/javascript;base64,${Buffer.from(autosaveSource).toString("base64")}#${Date.now()}-${Math.random()}`);
}

class FakeClock {
  constructor() {
    this.time = 0;
    this.sequence = 0;
    this.timers = new Map();
  }

  now = () => this.time;

  setTimeout = (callback, delay) => {
    const id = ++this.sequence;
    this.timers.set(id, { id, callback, dueAt: this.time + Number(delay) });
    return id;
  };

  clearTimeout = (id) => { this.timers.delete(id); };

  async settle() {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  }

  async advance(milliseconds) {
    const target = this.time + milliseconds;
    for (;;) {
      const next = [...this.timers.values()]
        .filter((timer) => timer.dueAt <= target)
        .sort((left, right) => left.dueAt - right.dueAt || left.id - right.id)[0];
      if (!next) break;
      this.time = next.dueAt;
      this.timers.delete(next.id);
      next.callback();
      await this.settle();
    }
    this.time = target;
    await this.settle();
  }
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function emptyProfileFailureRecordsState() {
  return {
    configuration: { active: false, outcomeUnknown: false, requiresInspection: false },
    secrets: {
      password: { active: false, outcomeUnknown: false, requiresInspection: false },
      totp: { active: false, outcomeUnknown: false, requiresInspection: false },
      "remote-log-token": { active: false, outcomeUnknown: false, requiresInspection: false }
    }
  };
}

function loadAppComponent(
  postSpy = async () => ({ ok: true }),
  getSpy = async (_auth, action, query = {}) => action === "source-path"
    ? { schema: "sdsync.dsm-source-path.v1", path: query.path, valid: true }
    : {}
) {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "App.vue script block is missing");
  let executable = script[1]
    .replace(/^import \{ ActionIcon \} from "\.\/ActionIcon";\s*/m, "")
    .replace(/^import \{ createAutosaveCoordinator \} from "\.\/autosave";\s*/m, "")
    .replace(/^import \{ installControlLayout \} from "\.\/controlLayout";\s*/m, "")
    .replace(/import \{[\s\S]*?\}\s*from "\.\/api";\s*/, "")
    .replace(/^import SecurityPanel from "\.\/SecurityPanel\.vue";\s*/m, "")
    .replace("export default {", "const AppComponent = {");
  executable += "\nreturn AppComponent;";

  const stubs = {
    ACTIONS: {
      configureProfile: "configure-profile", setSecret: "set-secret", routine: "routine", alertPolicy: "alert-policy",
      securityPolicy: "security-policy", clientEvent: "client-event", removeProfile: "remove-profile",
      removeRoutine: "remove-routine", execute: "execute"
    },
    AUTOSAVE_API_LIMITS: Object.freeze({
      csrfReissueTimeoutMs: 10000,
      postRequestTimeoutMs: 15000,
      postResponseTimeoutMs: 10000,
      readTimeoutMs: 10000,
      resultRequestTimeoutMs: 10000,
      resultObservationTimeoutMs: 30000
    }),
    MAX_RESPONSE_BYTES: 1024 * 1024,
    QueuedOutcomeUnknownError: class QueuedOutcomeUnknownError extends Error {
      constructor(jobId, message, requestId = "") {
        super(message);
        this.jobId = jobId;
        this.requestId = requestId;
        this.outcomeUnknown = true;
        this.accepted = true;
      }
    },
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    apiGet: getSpy,
    apiPost: postSpy,
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback),
    formatBytes: String,
    formatDate: String,
    formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    createAutosaveCoordinator: () => ({}),
    installControlLayout: () => () => {},
    ActionIcon: { name: "ActionIcon" },
    SecurityPanel: {}
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

function methodSource(name, nextName) {
  const start = appSource.indexOf(`    ${name}(`);
  const end = appSource.indexOf(`    ${nextName}(`, start + 1);
  assert.notEqual(start, -1, `${name} method is missing`);
  assert.notEqual(end, -1, `${nextName} method is missing after ${name}`);
  return appSource.slice(start, end);
}

async function coordinatorRuntime(postSpy, getSpy = async () => ({})) {
  const autosave = await loadAutosave();
  const clock = new FakeClock();
  const component = loadAppComponent(postSpy, getSpy);
  const methods = component.methods;
  const context = {
    disposed: false,
    csrfToken: "csrf",
    auth: {},
    operationBusy: false,
    autosavePhase: "saved",
    autosaveMessage: "Autosave ready",
    autosaveFailureScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    autosaveOutcomeUnknownScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    autosaveInspectionScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    mutationSessionBarrier: { active: false, kind: "", outcomeUnknown: false, requiresInspection: false },
    profileFailureRecords: emptyProfileFailureRecordsState(),
    autosaveCoordinator: null,
    alertDirty: false,
    securityDirty: false,
    reports: [],
    refreshSnapshot: async () => {},
    reportMutationError(error) { this.reports.push(error); }
  };
  for (const name of [
    "dispatchAutosave", "autosaveSucceeded", "autosaveFailed", "refreshAutosaveStatus",
    "cancelAutosave", "pauseAutosave", "refreshCsrf"
  ]) context[name] = (...args) => methods[name].apply(context, args);
  context.autosaveCoordinator = autosave.createAutosaveCoordinator({
    delayMs: 1300,
    now: clock.now,
    setTimeout: clock.setTimeout,
    clearTimeout: clock.clearTimeout,
    dispatch: (task) => context.dispatchAutosave(task),
    onSuccess: (task) => context.autosaveSucceeded(task),
    onError: (error, task) => context.autosaveFailed(error, task),
    onSuperseded: () => context.refreshAutosaveStatus()
  });
  context.autosaveCoordinator.hydrate("alerts", { enabled: false });
  return { clock, context };
}

function manualFailureContext(methods, scope, overrides = {}) {
  const context = Object.assign({
    disposed: false,
    operationBusy: false,
    profileEditorOpen: true,
    profileSaveState: "idle",
    profileSaveMessage: "",
    auth: {},
    csrfToken: "csrf",
    connected: false,
    autosaveCoordinator: null,
    autosavePhase: "saved",
    autosaveMessage: "All changes saved",
    autosaveFailureScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    autosaveOutcomeUnknownScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    autosaveInspectionScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    mutationSessionBarrier: { active: false, kind: "", outcomeUnknown: false, requiresInspection: false },
    profileFailureRecords: emptyProfileFailureRecordsState(),
    reports: [],
    toasts: [],
    toast(title, message, error = false) { this.toasts.push({ title, message, error }); },
    reportMutationError(error) {
      this.reports.push(error);
      return { unknown: Boolean(error && error.outcomeUnknown === true) };
    },
    refreshSnapshot: async () => {},
    scheduleSnapshot() {},
    scheduleLogs() {}
  }, overrides);
  for (const name of [
    "refreshAutosaveStatus", "cancelAutosave", "pauseAutosave", "hydrateAutosave",
    "clearAutosaveFailure", "ensureProfileFailureRecords", "syncProfileFailureState",
    "recordProfileFailure", "clearProfileConfigurationFailure", "clearProfileSecretFailures",
    "applyTrustedSecretPresence"
  ]) context[name] = (...args) => methods[name].apply(context, args);
  return { context, scope };
}

async function exerciseManualMutationFailures(failure) {
  const posts = [];
  const methods = loadAppComponent(async (_auth, _csrf, action, payload) => {
    posts.push({ action, payload });
    throw failure;
  }).methods;
  const event = { preventDefault() {} };
  const cases = [
    manualFailureContext(methods, "profile", {
      selectedProfile: "nightly",
      canChangeProfiles: true,
      profileAutosavePayload: () => ({ name: "nightly" }),
      secretOperations: () => [],
      validateProfile: () => "",
      confirmAction: async () => true,
      clearSecrets() {},
      closeProfile() {}
    }),
    manualFailureContext(methods, "routine", {
      canChangeRoutines: true,
      routineForm: { profile: "nightly" },
      routineAutosavePayload: () => ({ profile: "nightly", allow_delete: false }),
      validateRoutinePayload: () => "",
      routineAutosaveNeedsReview: () => false,
      loadRoutine() {}
    }),
    manualFailureContext(methods, "alerts", {
      canChangeNotifications: true,
      alertPayload: () => ({ enabled: true }),
      validateAlertPayload: () => "",
      alertDirty: true
    }),
    manualFailureContext(methods, "security", {
      canMutate: true,
      securityDirty: true,
      securityPayload: () => ({ require_https: true }),
      validateSecurityPayload: () => "",
      securityRelaxed: () => false,
      confirmAction: async () => true,
      hydrateSecurityPolicy() {}
    }),
    manualFailureContext(methods, "interface", {
      canChangeInterface: true,
      settings: { theme: "dark" },
      interfaceSettingsPayload: () => ({ theme: "light" }),
      validateInterfacePayload: () => "",
      captureSettingsTransaction() { return { settings: { theme: "dark" } }; },
      persistSettings: () => true,
      applySettingsState(settings) { this.settings = Object.assign({}, settings); },
      preferenceAuditWasRejected(error) {
        return Boolean(error && error.preAcceptance === true && error.trustedRejection === true);
      },
      restoreSettingsTransaction(transaction) { this.applySettingsState(transaction.settings); return true; }
    })
  ];
  const actions = [
    methods.saveProfile,
    methods.saveRoutine,
    methods.saveAlerts,
    methods.saveSecurityPolicy,
    methods.saveInterfaceSettings
  ];
  for (let index = 0; index < cases.length; index += 1) {
    await actions[index].call(cases[index].context, event);
  }
  return { actions, cases, methods, posts };
}

test("AppWindow registers the exact 1300ms autosave scopes and excludes secret and permission state", () => {
  assert.match(appSource, /const AUTOSAVE_SCOPES = Object\.freeze\(\["profile", "routine", "alerts", "security", "interface"\]\)/);
  assert.match(appSource, /createAutosaveCoordinator\(\{\s*delayMs: 1300,/);

  const watchBlock = appSource.match(/\n  watch: \{([\s\S]*?)\n  \},\n  async mounted\(\)/);
  assert.ok(watchBlock, "AppWindow watcher block is missing");
  for (const form of ["profileForm", "routineForm", "alertForm", "securityForm", "settings"]) {
    assert.match(watchBlock[1], new RegExp(`${form.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}:`));
  }
  assert.doesNotMatch(watchBlock[1], /notificationForm|secretModes|secretValues/);

  const notifications = methodSource("async saveNotificationPreferences", "async saveInterfaceSettings");
  assert.match(notifications, /Notification\.requestPermission\(\)/);
  assert.doesNotMatch(notifications, /autosaveChanged|hydrateAutosave|AUTOSAVE_SCOPES/);
  assert.match(notifications, /if \(report\.unknown \|\| report\.inspection\) this\.pauseAutosave\("interface", error\)/);
});

test("new profiles and routines plus risk-changing edits stay behind Save now", async () => {
  let postCount = 0;
  const methods = loadAppComponent(async () => {
    postCount += 1;
    return { ok: true };
  }).methods;
  const profileContext = {
    profileEditorOpen: true,
    selectedProfile: "",
    profileAutosavePayload: () => ({ name: "new-profile" }),
    validateProfile: () => "",
    profileAutosaveNeedsReview: () => false
  };
  assert.match(methods.autosaveCandidate.call(profileContext, "profile").manual, /new profile once/);

  profileContext.selectedProfile = "existing-profile";
  profileContext.selectedProfileModel = {
    allow_http: false, delete: false, allow_empty_source: false, danger_invalid_certs: false
  };
  profileContext.profileAutosavePayload = () => ({
    name: "existing-profile", allow_http: true, delete: false,
    allow_empty_source: false, danger_accept_invalid_certs: false
  });
  profileContext.profileAutosaveNeedsReview = (...args) => methods.profileAutosaveNeedsReview.call(profileContext, ...args);
  assert.match(methods.autosaveCandidate.call(profileContext, "profile").manual, /Save now/);

  const confirmations = [];
  const manualProfile = {
    profileEditorOpen: true,
    selectedProfile: "existing-profile",
    profileSaveState: "idle",
    canChangeProfiles: true,
    operationBusy: false,
    disposed: false,
    cancelAutosave() {},
    profileAutosavePayload: () => profileContext.profileAutosavePayload(),
    secretOperations: () => [],
    validateProfile: () => "",
    async confirmAction(...args) { confirmations.push(args); return false; },
    toast() {}
  };
  await methods.saveProfile.call(manualProfile, { preventDefault() {} });
  assert.equal(postCount, 0, "plain-HTTP approval must happen before any profile mutation");
  assert.equal(confirmations.length, 1);
  assert.match(confirmations[0][1], /plain-HTTP/);

  const routineContext = {
    routineEditorOpen: true,
    selectedRoutine: null,
    routineAutosavePayload: () => ({ profile: "existing-profile" }),
    validateRoutinePayload: () => "",
    routineAutosaveNeedsReview: () => false
  };
  assert.match(methods.autosaveCandidate.call(routineContext, "routine").manual, /new routine once/);

  routineContext.selectedRoutine = { profile: "existing-profile" };
  routineContext.routineAutosaveNeedsReview = () => true;
  assert.match(methods.autosaveCandidate.call(routineContext, "routine").manual, /Save now/);
});

test("manual profile and routine saves reject blank or non-canonical integer drafts", async () => {
  let postCount = 0;
  const methods = loadAppComponent(async () => {
    postCount += 1;
    return { ok: true };
  }).methods;
  const profileForm = {
    name: "nightly", source: "/volume1/source", url: "https://nas.example.invalid",
    username: "backup-user", remote: "/home/Drive/Backup", compare: "content", jobs: 2,
    allow_http: false, delete: false, max_delete: 100, make_default: false,
    excludes: "@eaDir/", allow_empty_source: false, retries: 2, timeout: 7200,
    connect_timeout: 15, max_rate: 0, ca_certificate: "", danger_invalid_certs: false,
    verbosity: 0, quiet: false, log_level: "info", log_format: "json", progress: "never",
    output: "human", remote_log_url: "", remote_log_mode: "best-effort"
  };
  const profileToasts = [];
  const profile = {
    profileEditorOpen: true,
    profileSaveState: "idle",
    canChangeProfiles: true,
    operationBusy: false,
    profileForm,
    cancelAutosave() {},
    toast(title, message, error) { profileToasts.push({ title, message, error }); }
  };
  for (const name of ["strictDraftInteger", "integer", "profilePayload", "profileAutosavePayload"]) {
    profile[name] = (...args) => methods[name].call(profile, ...args);
  }
  for (const invalid of ["", "01", "1.5"]) {
    profileForm.retries = invalid;
    assert.equal(profile.profileAutosavePayload(), null, `profile draft ${JSON.stringify(invalid)} was coerced`);
    await methods.saveProfile.call(profile, { preventDefault() {} });
    assert.equal(profileToasts.at(-1).error, true);
    assert.match(profileToasts.at(-1).message, /whole number/);
  }

  const routineForm = {
    profile: "nightly", enabled: true, action: "sync", mode: "interval",
    interval_seconds: 3600, weekdays: [1, 2, 3, 4, 5, 6, 7],
    time_window_start: "00:00", time_window_end: "23:59", debounce_seconds: 45,
    poll_seconds: 30, retry_count: 5, retry_backoff_seconds: 60,
    retry_exponential: true, allow_delete: false, max_total_delete: 100, depends_on: []
  };
  const routineToasts = [];
  const routine = {
    canChangeRoutines: true,
    operationBusy: false,
    routineForm,
    cancelAutosave() {},
    toast(title, message, error) { routineToasts.push({ title, message, error }); }
  };
  for (const name of ["strictDraftInteger", "integer", "routinePayload", "routineAutosavePayload"]) {
    routine[name] = (...args) => methods[name].call(routine, ...args);
  }
  for (const invalid of ["", "01", "1.5"]) {
    routineForm.retry_count = invalid;
    assert.equal(routine.routineAutosavePayload(), null, `routine draft ${JSON.stringify(invalid)} was coerced`);
    await methods.saveRoutine.call(routine, { preventDefault() {} });
    assert.equal(routineToasts.at(-1).error, true);
    assert.match(routineToasts.at(-1).message, /whole number/);
  }
  assert.equal(postCount, 0, "invalid numeric drafts reached the package mutation bridge");
});

test("failure pause state blocks later edits and retained manual forms never resume themselves", () => {
  const methods = loadAppComponent().methods;
  const blocked = [];
  const context = {
    disposed: false,
    autosaveFailureScopes: { routine: true },
    autosaveCoordinator: {
      getState: () => ({ registered: true, dirty: true }),
      update: () => ({ dirty: true }),
      setScopeBlocked: (_scope, value) => blocked.push(value)
    },
    autosaveCandidate: () => ({ payload: { profile: "nightly" }, invalid: "", manual: "" }),
    alertDirty: false,
    autosavePhase: "saved",
    autosaveMessage: "All changes saved"
  };
  methods.autosaveChanged.call(context, "routine");
  assert.deepEqual(blocked, [true]);
  assert.equal(context.autosavePhase, "blocked");
  assert.match(context.autosaveMessage, /use Save now/);

  assert.match(methodSource("autosaveFailed", "hydrateAutosave"), /pauseAutosave\(task\.scope, error\)/);
  const pause = methodSource("pauseAutosave", "clearAutosaveFailure");
  assert.match(pause, /autosaveFailureScopes\[scope\] = true/);
  assert.match(pause, /setScopeBlocked\(scope, true\)/);

  for (const [name, next, scope, errorName] of [
    ["async saveRoutine", "async removeRoutine", "routine", "error"],
    ["async saveAlerts", "async executeOperation", "alerts", "error"],
    ["async saveSecurityPolicy", "clearProfileFilters", "security", "caught"],
    ["async saveInterfaceSettings", "persistSettings", "interface", "error"]
  ]) {
    assert.match(methodSource(name, next), new RegExp(`pauseAutosave\\("${scope}", ${errorName}\\)`));
  }
  assert.match(
    methodSource("async saveProfile", "async saveProfileSecrets"),
    /pauseAutosave\("profile", reportedError, activeSecretKind\)/
  );
  assert.match(
    methodSource("async saveProfileSecrets", "async removeProfile"),
    /pauseAutosave\("profile", reportedError, activeSecretKind\)/
  );
  assert.match(
    methodSource("async saveSecurityPolicy", "clearProfileFilters"),
    /pauseAutosave\("security", appliedPolicy\)/
  );
});

test("cancel and failure-clear paths recompute status while manual review never masks another failed scope", () => {
  const methods = loadAppComponent().methods;
  const state = {
    profile: { registered: true, dirty: true, cancelled: false },
    routine: { registered: false, dirty: false, cancelled: false },
    alerts: { registered: false, dirty: false, cancelled: false },
    security: { registered: false, dirty: false, cancelled: false },
    interface: { registered: false, dirty: false, cancelled: false }
  };
  const coordinator = {
    cancel(scope) { state[scope] = { ...state[scope], cancelled: true }; },
    getState(scope) { return { blocked: false, inFlight: false, scheduled: false, queued: false, ...state[scope] }; },
    update() { return { dirty: true }; },
    setScopeBlocked() {}
  };
  const context = {
    autosaveCoordinator: coordinator,
    autosaveFailureScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    autosaveOutcomeUnknownScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    autosaveInspectionScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    profileFailureRecords: emptyProfileFailureRecordsState(),
    autosavePhase: "pending",
    autosaveMessage: "Autosave pending · 1.3 seconds"
  };
  context.refreshAutosaveStatus = (...args) => methods.refreshAutosaveStatus.call(context, ...args);
  context.ensureProfileFailureRecords = (...args) => methods.ensureProfileFailureRecords.call(context, ...args);
  context.syncProfileFailureState = (...args) => methods.syncProfileFailureState.call(context, ...args);
  methods.cancelAutosave.call(context, "profile");
  assert.equal(context.autosavePhase, "saved");
  assert.equal(context.autosaveMessage, "All changes saved");

  context.autosaveFailureScopes.profile = true;
  context.profileFailureRecords.configuration.active = true;
  context.autosavePhase = "blocked";
  context.autosaveMessage = "Profile autosave paused · use Save now";
  methods.clearAutosaveFailure.call(context, "profile");
  assert.equal(context.autosavePhase, "saved");
  assert.equal(context.autosaveMessage, "All changes saved");

  state.profile = { registered: true, dirty: true, cancelled: false };
  context.disposed = false;
  context.autosaveFailureScopes.alerts = true;
  context.autosaveCandidate = () => ({
    payload: { name: "nightly" }, invalid: "", manual: "Use Save now to approve profile risk."
  });
  methods.autosaveChanged.call(context, "profile");
  assert.equal(context.autosavePhase, "blocked");
  assert.equal(context.autosaveMessage, "Alerts autosave paused · use Save now");

  assert.match(methodSource("closeProfile", "clearSecrets"), /cancelAutosave\("profile"\)/);
  assert.match(methodSource("closeRoutine", "routinePayload"), /cancelAutosave\("routine"\)/);
  assert.match(methodSource("async removeProfile", "loadRoutine"), /clearAutosaveFailure\("profile"\)/);
  assert.match(methodSource("async removeRoutine", "async saveAlerts"), /clearAutosaveFailure\("routine"\)/);
});

test("profile autosave serializes configuration only and manual saves reconcile their baselines", async () => {
  const posts = [];
  const contracts = [];
  const methods = loadAppComponent(async (_auth, _csrf, action, payload, awaitTerminal, pollInterval, limits) => {
    posts.push({ action, payload });
    contracts.push({ awaitTerminal, pollInterval, limits });
    return { ok: true };
  }).methods;
  const context = {
    disposed: false,
    csrfToken: "csrf",
    auth: {},
    operationBusy: false,
    autosavePhase: "pending",
    autosaveMessage: "Autosave pending",
    refreshSnapshot: async () => {},
    reportMutationError() { assert.fail("configuration autosave unexpectedly failed"); }
  };
  await methods.dispatchAutosave.call(context, {
    scope: "profile",
    value: { name: "nightly", source: "/volume1/source", username: "backup-user" }
  });
  assert.deepEqual(posts, [{
    action: "configure-profile",
    payload: { name: "nightly", source: "/volume1/source", username: "backup-user" }
  }]);
  assert.equal(contracts[0].awaitTerminal, true);
  assert.equal(contracts[0].pollInterval, undefined);
  assert.equal(contracts[0].limits.resultObservationTimeoutMs, 30000);

  const profilePayload = methodSource("profilePayload", "secretOperations");
  const profileAutosavePayload = methodSource("profileAutosavePayload", "routineAutosavePayload");
  const autosaveDispatch = methodSource("async dispatchAutosave", "autosaveSucceeded");
  for (const block of [profilePayload, profileAutosavePayload, autosaveDispatch]) {
    assert.doesNotMatch(block, /secretValues|secretModes|setSecret|remote_log_token|\bpassword\b|\btotp\b/);
  }

  const profileSave = methodSource("async saveProfile", "async saveProfileSecrets");
  const routineSave = methodSource("async saveRoutine", "async removeRoutine");
  const alertSave = methodSource("async saveAlerts", "async executeOperation");
  const securitySave = methodSource("async saveSecurityPolicy", "clearProfileFilters");
  const interfaceSave = methodSource("async saveInterfaceSettings", "persistSettings");
  assert.match(profileSave, /cancelAutosave\("profile"\)/);
  assert.match(profileSave, /closeProfile\(\{ refresh: false \}\)/);
  assert.match(routineSave, /cancelAutosave\("routine"\)/);
  assert.match(routineSave, /loadRoutine\(payload\.profile\)/);
  assert.match(alertSave, /cancelAutosave\("alerts"\)/);
  assert.match(alertSave, /alertDirty = false;\s*this\.hydrateAutosave\("alerts", payload\)/);
  assert.match(securitySave, /cancelAutosave\("security"\)/);
  assert.match(securitySave, /hydrateSecurityPolicy\(true\)/);
  assert.match(interfaceSave, /cancelAutosave\("interface"\)/);
  assert.match(interfaceSave, /hydrateAutosave\("interface", candidate\)/);
});

test("accepted autosave releases Saving before a slow observational snapshot settles", async () => {
  let releaseSnapshot;
  const snapshot = new Promise((resolve) => { releaseSnapshot = resolve; });
  const methods = loadAppComponent(async () => ({ ok: true })).methods;
  const context = {
    disposed: false,
    csrfToken: "csrf",
    auth: {},
    operationBusy: false,
    autosavePhase: "pending",
    autosaveMessage: "Autosave pending",
    snapshotCalls: 0,
    refreshSnapshot() { this.snapshotCalls += 1; return snapshot; },
    reportMutationError() { assert.fail("accepted autosave unexpectedly failed"); }
  };

  let completed = false;
  const dispatch = methods.dispatchAutosave.call(context, {
    scope: "profile",
    value: { name: "nightly" }
  }).then(() => { completed = true; });
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(context.snapshotCalls, 1);
  assert.equal(completed, true, "snapshot observation must not extend the accepted mutation lifetime");
  assert.equal(context.operationBusy, false);
  releaseSnapshot();
  await dispatch;

  assert.match(appSource, /onSuperseded: \(\) => this\.refreshAutosaveStatus\(\)/);
});

test("outcome-unknown POST exits Saving, reports once, and never starts success observation", async () => {
  const failure = Object.assign(new Error("mutation acknowledgement was lost"), { outcomeUnknown: true });
  const methods = loadAppComponent(async () => { throw failure; }).methods;
  const reports = [];
  const context = {
    disposed: false,
    csrfToken: "csrf",
    auth: {},
    operationBusy: false,
    autosavePhase: "pending",
    autosaveMessage: "Autosave pending",
    snapshotCalls: 0,
    refreshSnapshot() { this.snapshotCalls += 1; return Promise.resolve(); },
    reportMutationError(error) { reports.push(error); }
  };

  await assert.rejects(
    methods.dispatchAutosave.call(context, { scope: "alerts", value: { enabled: true } }),
    (error) => error === failure
  );
  assert.equal(context.operationBusy, false);
  assert.deepEqual(reports, [failure]);
  assert.equal(context.snapshotCalls, 0);
});

test("real coordinator transitions deferred terminal success from Saving to saved", async () => {
  const terminal = deferred();
  let attempts = 0;
  const { clock, context } = await coordinatorRuntime(() => {
    attempts += 1;
    return terminal.promise;
  });

  context.autosaveCoordinator.update("alerts", { enabled: true });
  await clock.advance(1300);
  assert.equal(attempts, 1);
  assert.equal(context.operationBusy, true);
  assert.equal(context.autosavePhase, "saving");
  assert.equal(context.autosaveMessage, "Saving changes…");

  terminal.resolve({ ok: true });
  await clock.settle();
  assert.equal(context.operationBusy, false);
  assert.equal(context.autosavePhase, "saved");
  assert.equal(context.autosaveMessage, "Changes autosaved");
  assert.equal(context.autosaveCoordinator.getState("alerts").dirty, false);
});

test("real coordinator blocks outcome-unknown autosave without retry or Save-now guidance", async () => {
  const terminal = deferred();
  let attempts = 0;
  const { clock, context } = await coordinatorRuntime(() => {
    attempts += 1;
    return terminal.promise;
  });
  const unknown = Object.assign(new Error("accepted job result observation timed out"), {
    outcomeUnknown: true,
    accepted: true
  });

  context.autosaveCoordinator.update("alerts", { enabled: true });
  await clock.advance(1300);
  terminal.reject(unknown);
  await clock.settle();

  assert.equal(context.operationBusy, false);
  assert.equal(context.autosavePhase, "blocked");
  assert.match(context.autosaveMessage, /outcome unknown/i);
  assert.match(context.autosaveMessage, /Activity \/ Logs/);
  assert.doesNotMatch(context.autosaveMessage, /Save now/i);
  assert.deepEqual(context.reports, [unknown]);
  assert.equal(context.autosaveFailureScopes.alerts, true);
  assert.equal(context.autosaveOutcomeUnknownScopes.alerts, true);

  context.autosaveCoordinator.update("alerts", { enabled: false });
  await clock.advance(100000);
  assert.equal(attempts, 1, "an outcome-unknown mutation must never retry automatically");
});

test("security autosave never invites a duplicate after terminal success and CSRF refresh failure", async () => {
  let posts = 0;
  const refreshFailure = new Error("DSM CSRF refresh returned an invalid document");
  const { clock, context } = await coordinatorRuntime(
    async () => {
      posts += 1;
      return { ok: true, status: "succeeded" };
    },
    async () => { throw refreshFailure; }
  );
  context.autosaveCoordinator.hydrate("security", { require_https: false });

  context.autosaveCoordinator.update("security", { require_https: true });
  await clock.advance(1300);
  await clock.settle();

  assert.equal(posts, 1, "the policy POST must complete exactly once");
  assert.equal(context.operationBusy, false);
  assert.equal(context.autosavePhase, "blocked");
  assert.match(context.autosaveMessage, /outcome unknown/i);
  assert.match(context.autosaveMessage, /Activity \/ Logs/);
  assert.doesNotMatch(context.autosaveMessage, /Save now|save the policy again/i);
  assert.equal(context.autosaveFailureScopes.security, true);
  assert.equal(context.autosaveOutcomeUnknownScopes.security, true);
  assert.equal(context.reports.length, 1);
  assert.equal(context.reports[0].outcomeUnknown, true);
  assert.equal(context.reports[0].accepted, true);
  assert.match(context.reports[0].message, /applied the security autosave/i);
  assert.match(context.reports[0].message, /Do not save the policy again/);
  assert.ok(context.reports[0].message.length < 256, "accepted-state guidance must remain bounded");

  context.autosaveCoordinator.update("security", { require_https: false });
  await clock.advance(100000);
  assert.equal(posts, 1, "the failed CSRF observation must never redispatch the applied policy");
});

test("manual mutation outcome-unknown errors block every autosave scope without Save-now guidance", async () => {
  const unknown = Object.assign(new Error("DSM may have accepted this mutation"), {
    outcomeUnknown: true,
    acceptanceUnknown: true,
    trustedRequestId: true,
    requestId: "a".repeat(32)
  });
  const { actions, cases, posts } = await exerciseManualMutationFailures(unknown);

  for (const { context, scope } of cases) {
    assert.equal(context.operationBusy, false, `${scope} must leave the manual operation boundary`);
    assert.equal(context.autosaveFailureScopes[scope], true);
    assert.equal(context.autosaveOutcomeUnknownScopes[scope], true);
    assert.equal(context.autosavePhase, "blocked");
    assert.match(context.autosaveMessage, /outcome unknown/i);
    assert.match(context.autosaveMessage, /Activity \/ Logs/);
    assert.doesNotMatch(context.autosaveMessage, /Save now/i);
    assert.deepEqual(context.reports, [unknown]);
    assert.equal(context.mutationSessionBarrier.active, true);
  }

  assert.equal(posts.length, cases.length);
  for (let index = 0; index < cases.length; index += 1) {
    await actions[index].call(cases[index].context, { preventDefault() {} });
  }
  assert.equal(posts.length, cases.length, "a second click must not redispatch any mutation family after an unknown outcome");
});

test("trusted pre-acceptance manual rejections retain Save-now recovery guidance", async () => {
  const rejected = Object.assign(new Error("The package rejected the mutation before acceptance"), {
    preAcceptance: true,
    trustedRejection: true,
    trustedRequestId: true,
    requestId: "b".repeat(32)
  });
  const { actions, cases, posts } = await exerciseManualMutationFailures(rejected);

  for (const { context, scope } of cases) {
    assert.equal(context.operationBusy, false, `${scope} must leave the manual operation boundary`);
    assert.equal(context.autosaveFailureScopes[scope], true);
    assert.equal(context.autosaveOutcomeUnknownScopes[scope], false);
    assert.equal(context.autosavePhase, "blocked");
    assert.match(context.autosaveMessage, /use Save now/i);
    assert.doesNotMatch(context.autosaveMessage, /outcome unknown|Activity \/ Logs/i);
    assert.deepEqual(context.reports, [rejected]);
    assert.equal(context.mutationSessionBarrier.active, false);
  }

  for (let index = 0; index < cases.length; index += 1) {
    await actions[index].call(cases[index].context, { preventDefault() {} });
  }
  assert.equal(posts.length, cases.length * 2, "a trusted pre-acceptance rejection remains manually retryable");
});

test("full profile save marks a later trusted rejection as durable partial application", async () => {
  const requestId = "c".repeat(32);
  const jobId = "d".repeat(48);
  const rejected = Object.assign(new Error("The later credential mutation was rejected"), {
    preAcceptance: true,
    trustedRejection: true,
    csrfRejected: true,
    trustedRequestId: true,
    requestId,
    trustedJobId: true,
    jobId,
    status: 403,
    transportStatus: 200,
    code: "csrf_rejected",
    stage: "mutation_validation"
  });
  const posts = [];
  const methods = loadAppComponent(async (_auth, _csrf, action, payload) => {
    posts.push({ action, payload });
    if (posts.length === 1) return { ok: true };
    throw rejected;
  }).methods;
  let closed = 0;
  let refreshed = 0;
  const { context } = manualFailureContext(methods, "profile", {
    canChangeProfiles: true,
    profileAutosavePayload: () => ({ name: "nightly" }),
    secretOperations: () => [{ profile: "nightly", kind: "password", mode: "replace", value: "redacted" }],
    validateProfile: () => "",
    clearSecrets() {},
    secretModes: { password: "replace", totp: "keep", remote_log_token: "keep" },
    secretValues: { password: "redacted", totp: "", remote_log_token: "" },
    closeProfile() { closed += 1; },
    refreshSnapshot: async () => { refreshed += 1; }
  });

  await methods.saveProfile.call(context, { preventDefault() {} });

  assert.deepEqual(posts.map(({ action }) => action), ["configure-profile", "set-secret"]);
  assert.equal(context.operationBusy, false);
  assert.equal(context.autosaveFailureScopes.profile, true);
  assert.equal(context.autosaveOutcomeUnknownScopes.profile, false);
  assert.equal(context.autosaveInspectionScopes.profile, true);
  assert.deepEqual(context.profileFailureRecords.configuration, {
    active: false, outcomeUnknown: false, requiresInspection: false
  });
  assert.deepEqual(context.profileFailureRecords.secrets.password, {
    active: true, outcomeUnknown: false, requiresInspection: true
  });
  assert.equal(context.autosavePhase, "blocked");
  assert.match(context.autosaveMessage, /need inspection/i);
  assert.match(context.autosaveMessage, /Activity \/ Logs/);
  assert.doesNotMatch(context.autosaveMessage, /Save now/i);
  assert.equal(closed, 0, "partial application must preserve the editor for inspection");
  assert.equal(refreshed, 0, "profile-owned state blocks observational refresh until the editor closes");
  assert.equal(context.profileSaveState, "error");
  assert.equal(context.reports.length, 1);
  const failure = context.reports[0];
  assert.equal(failure.name, "PartialMutationInspectionRequiredError");
  assert.equal(failure.outcomeUnknown, false);
  assert.equal(failure.requiresInspection, true);
  assert.equal(failure.partialApplication, true);
  assert.equal(failure.requestId, requestId);
  assert.equal(failure.trustedRequestId, true);
  assert.equal(failure.jobId, jobId);
  assert.equal(failure.trustedJobId, true);
  assert.equal(failure.preAcceptance, true);
  assert.equal(failure.trustedRejection, true);
  assert.equal(failure.csrfRejected, true);
  assert.equal(failure.status, 403);
  assert.equal(failure.transportStatus, 200);
  assert.equal(failure.code, "csrf_rejected");
  assert.equal(failure.stage, "mutation_validation");
  assert.match(failure.message, /Earlier profile stages were applied/);
  assert.match(failure.message, /Do not retry/);

  await methods.saveProfile.call(context, { preventDefault() {} });
  assert.equal(posts.length, 2, "partial profile application must freeze a second manual submission");
  assert.equal(context.secretModes.password, "replace", "the unapplied secret stage remains in the draft");
  assert.equal(context.secretValues.password, "redacted");
  assert.equal(context.profileEditorOpen, true, "the draft remains open for reconciliation");
});

test("secrets-only save blocks profile autosave after any earlier secret was applied", async () => {
  const requestId = "e".repeat(32);
  const rejected = Object.assign(new Error("The second credential mutation was rejected"), {
    preAcceptance: true,
    trustedRejection: true,
    trustedRequestId: true,
    requestId,
    status: 422,
    code: "secret_rejected",
    stage: "secret_write"
  });
  const posts = [];
  const methods = loadAppComponent(async (_auth, _csrf, action, payload) => {
    posts.push({ action, payload });
    if (posts.length === 1) return { ok: true };
    throw rejected;
  }).methods;
  let refreshed = 0;
  const { context } = manualFailureContext(methods, "profile", {
    selectedProfile: "nightly",
    canManageSecrets: true,
    secretOperations: () => [
      { profile: "nightly", kind: "password", mode: "replace", value: "redacted-one" },
      { profile: "nightly", kind: "totp", mode: "replace", value: "redacted-two" }
    ],
    validateSecretOperations: () => "",
    clearSecrets() {},
    secretModes: { password: "replace", totp: "replace", remote_log_token: "keep" },
    refreshSnapshot: async () => { refreshed += 1; }
  });

  await methods.saveProfileSecrets.call(context, { preventDefault() {} });

  assert.deepEqual(posts.map(({ action }) => action), ["set-secret", "set-secret"]);
  assert.equal(context.operationBusy, false);
  assert.equal(context.autosaveFailureScopes.profile, true);
  assert.equal(context.autosaveOutcomeUnknownScopes.profile, false);
  assert.equal(context.autosaveInspectionScopes.profile, true);
  assert.deepEqual(context.profileFailureRecords.secrets.password, {
    active: false, outcomeUnknown: false, requiresInspection: false
  });
  assert.deepEqual(context.profileFailureRecords.secrets.totp, {
    active: true, outcomeUnknown: false, requiresInspection: true
  });
  assert.equal(context.autosavePhase, "blocked");
  assert.match(context.autosaveMessage, /need inspection/i);
  assert.match(context.autosaveMessage, /Activity \/ Logs/);
  assert.doesNotMatch(context.autosaveMessage, /Save now/i);
  assert.equal(refreshed, 0);
  assert.deepEqual(context.secretModes, { password: "keep", totp: "replace", remote_log_token: "keep" });
  assert.equal(context.reports.length, 1);
  const failure = context.reports[0];
  assert.equal(failure.outcomeUnknown, false);
  assert.equal(failure.requiresInspection, true);
  assert.equal(failure.partialApplication, true);
  assert.equal(failure.requestId, requestId);
  assert.equal(failure.trustedRequestId, true);
  assert.equal(failure.preAcceptance, true);
  assert.equal(failure.trustedRejection, true);
  assert.equal(failure.status, 422);
  assert.equal(failure.code, "secret_rejected");
  assert.equal(failure.stage, "secret_write");
  assert.match(failure.message, /Earlier secret stages were applied/);
  assert.match(failure.message, /Do not retry/);

  await methods.saveProfileSecrets.call(context, { preventDefault() {} });
  assert.equal(posts.length, 2, "partial secret application must freeze a second manual submission");
  assert.equal(context.secretModes.totp, "replace", "the unapplied secret draft remains available for inspection");
});

test("zero-applied profile and secret rejections retain ordinary Save-now recovery", async () => {
  const rejected = Object.assign(new Error("The package rejected the mutation before acceptance"), {
    preAcceptance: true,
    trustedRejection: true,
    trustedRequestId: true,
    requestId: "f".repeat(32),
    status: 422,
    code: "validation_failed",
    stage: "mutation_validation"
  });
  const methods = loadAppComponent(async () => { throw rejected; }).methods;
  const profileCase = manualFailureContext(methods, "profile", {
    canChangeProfiles: true,
    profileAutosavePayload: () => ({ name: "nightly" }),
    secretOperations: () => [{ profile: "nightly", kind: "password", mode: "replace", value: "redacted" }],
    validateProfile: () => "",
    clearSecrets() {},
    closeProfile() { assert.fail("a zero-applied profile rejection must retain the editor"); }
  });
  const secretsCase = manualFailureContext(methods, "profile", {
    selectedProfile: "nightly",
    canManageSecrets: true,
    secretOperations: () => [{ profile: "nightly", kind: "password", mode: "replace", value: "redacted" }],
    validateSecretOperations: () => "",
    clearSecrets() {},
    secretModes: { password: "replace", totp: "keep", remote_log_token: "keep" }
  });

  await methods.saveProfile.call(profileCase.context, { preventDefault() {} });
  await methods.saveProfileSecrets.call(secretsCase.context, { preventDefault() {} });

  assert.equal(profileCase.context.operationBusy, false);
  assert.equal(profileCase.context.autosaveFailureScopes.profile, true);
  assert.equal(profileCase.context.autosaveOutcomeUnknownScopes.profile, false);
  assert.equal(profileCase.context.autosaveInspectionScopes.profile, false);
  assert.deepEqual(profileCase.context.profileFailureRecords.configuration, {
    active: true, outcomeUnknown: false, requiresInspection: false
  });
  assert.equal(profileCase.context.autosavePhase, "blocked");
  assert.match(profileCase.context.autosaveMessage, /use Save now/i);
  assert.doesNotMatch(profileCase.context.autosaveMessage, /outcome unknown|Activity \/ Logs/i);
  assert.deepEqual(profileCase.context.reports, [rejected]);

  assert.equal(secretsCase.context.operationBusy, false);
  assert.equal(secretsCase.context.autosaveFailureScopes.profile, false);
  assert.equal(secretsCase.context.autosaveOutcomeUnknownScopes.profile, false);
  assert.equal(secretsCase.context.autosaveInspectionScopes.profile, false);
  assert.deepEqual(secretsCase.context.profileFailureRecords, emptyProfileFailureRecordsState());
  assert.equal(secretsCase.context.autosavePhase, "saved");
  assert.doesNotMatch(secretsCase.context.autosaveMessage, /outcome unknown|Activity \/ Logs|Save now/i);
  assert.deepEqual(secretsCase.context.reports, [rejected]);
});

test("authoritative hydration cannot clear the session latch; a fresh AppWindow can", () => {
  const methods = loadAppComponent().methods;
  const coordinator = {
    hydrate() {},
    getState() { return { registered: true, dirty: false, inFlight: false }; },
    setScopeBlocked() {}
  };
  const latched = manualFailureContext(methods, "security", {
    autosaveCoordinator: coordinator,
    autosaveFailureScopes: { profile: false, routine: false, alerts: false, security: true, interface: false },
    autosaveOutcomeUnknownScopes: { profile: false, routine: false, alerts: false, security: true, interface: false },
    autosaveInspectionScopes: { profile: false, routine: false, alerts: false, security: true, interface: false },
    mutationSessionBarrier: { active: true, kind: "Security mutation", outcomeUnknown: true, requiresInspection: true }
  });

  methods.hydrateAutosave.call(latched.context, "security", { require_https: true }, true);

  assert.equal(latched.context.autosaveOutcomeUnknownScopes.security, false, "the observed scope may reconcile");
  assert.equal(latched.context.mutationSessionBarrier.active, true, "authoritative hydration must not silently unlock the AppWindow");
  assert.equal(latched.context.autosavePhase, "blocked");
  assert.equal(loadAppComponent().computed.mutationOutcomeUnresolved.call(latched.context), true);

  const fresh = manualFailureContext(methods, "security", {
    canMutate: true,
    securityDirty: true
  }).context;
  assert.equal(loadAppComponent().computed.mutationOutcomeUnresolved.call(fresh), false);
  assert.equal(loadAppComponent().computed.canSubmitSecurity.call({
    canMutate: true,
    securityDirty: true,
    operationBusy: false,
    mutationOutcomeUnresolved: false
  }), true, "a new AppWindow instance starts from authoritative state without the prior session latch");
});

test("unknown removals, operations, and notification audits latch before a second dispatch", async () => {
  const unknown = Object.assign(new Error("DSM may have accepted the request"), {
    outcomeUnknown: true,
    trustedRequestId: true,
    requestId: "2".repeat(32)
  });
  const posts = [];
  const methods = loadAppComponent(async (_auth, _csrf, action, payload) => {
    posts.push({ action, payload });
    throw unknown;
  }).methods;
  let confirmations = 0;
  const profile = manualFailureContext(methods, "profile", {
    selectedProfile: "nightly",
    canChangeProfiles: true,
    confirmAction: async () => { confirmations += 1; return true; }
  }).context;
  const routine = manualFailureContext(methods, "routine", {
    canChangeRoutines: true,
    routineForm: { profile: "nightly" },
    selectedRoutine: { profile: "nightly" },
    confirmAction: async () => { confirmations += 1; return true; }
  }).context;
  const operation = manualFailureContext(methods, "security", {
    canRunOperations: true,
    canAllowDestructive: true,
    canRunDoctorWrite: true,
    hasCapability: () => true,
    diagnostic: { title: "", output: "" }
  }).context;
  const notifications = manualFailureContext(methods, "interface", {
    canChangeNotifications: true,
    notificationForm: { desktop_notifications: false, audible: false },
    captureSettingsTransaction: () => ({ settings: { desktop_notifications: true, audible: true } }),
    persistSettings: () => true,
    applySettingsState() {},
    preferenceAuditWasRejected: () => false
  }).context;

  await methods.removeProfile.call(profile);
  await methods.removeRoutine.call(routine);
  await methods.executeOperation.call(operation, "run", { scope: "all", allow_delete: false });
  await methods.saveNotificationPreferences.call(notifications, { preventDefault() {} });
  assert.deepEqual(posts.map(({ action }) => action), ["remove-profile", "remove-routine", "execute", "client-event"]);
  assert.equal(confirmations, 2);

  await methods.removeProfile.call(profile);
  await methods.removeRoutine.call(routine);
  await methods.executeOperation.call(operation, "run", { scope: "all", allow_delete: false });
  await methods.saveNotificationPreferences.call(notifications, { preventDefault() {} });
  assert.equal(posts.length, 4, "each unknown family must reject a second direct invocation before transport");
  assert.equal(confirmations, 2, "a locked destructive retry must not reach confirmation");
  for (const context of [profile, routine, operation, notifications]) {
    assert.equal(context.mutationSessionBarrier.active, true);
  }
});

test("a latched mutation outcome blocks autosave dispatch before transport", async () => {
  let posts = 0;
  const methods = loadAppComponent(async () => { posts += 1; return { ok: true }; }).methods;
  const context = manualFailureContext(methods, "alerts", {
    mutationSessionBarrier: { active: true, kind: "Alert mutation", outcomeUnknown: true, requiresInspection: true }
  }).context;

  await assert.rejects(
    methods.dispatchAutosave.call(context, { scope: "alerts", payload: { enabled: true } }),
    /mutation outcome unknown.*may already have been accepted/i
  );
  assert.equal(posts, 0);
  assert.equal(context.operationBusy, false);
});

test("manual security success followed by CSRF refresh failure remains applied and non-repeatable", async () => {
  let posts = 0;
  const component = loadAppComponent(
    async () => {
      posts += 1;
      return { ok: true, status: "succeeded" };
    },
    async () => { throw new Error("replacement CSRF unavailable"); }
  );
  const methods = component.methods;
  const { context } = manualFailureContext(methods, "security", {
    canMutate: true,
    securityDirty: true,
    securityPayload: () => ({ require_https: true }),
    validateSecurityPayload: () => "",
    securityRelaxed: () => false,
    confirmAction: async () => true,
    hydrateSecurityPolicy() {},
    bridgeIssue: { title: "", message: "" },
    connectionLabel: "Authenticated package bridge",
    freshness: "Current",
    snapshot: {}
  });
  context.refreshCsrf = (...args) => methods.refreshCsrf.apply(context, args);

  await methods.saveSecurityPolicy.call(context, { preventDefault() {} });

  assert.equal(posts, 1);
  assert.equal(context.operationBusy, false);
  assert.equal(context.securityDirty, false);
  assert.equal(context.autosaveFailureScopes.security, true);
  assert.equal(context.autosaveOutcomeUnknownScopes.security, true);
  assert.equal(context.autosavePhase, "blocked");
  assert.match(context.autosaveMessage, /outcome unknown/i);
  assert.match(context.autosaveMessage, /Activity \/ Logs/);
  assert.doesNotMatch(context.autosaveMessage, /Save now/i);
  assert.match(context.bridgeIssue.message, /do not repeat the save/i);
  assert.equal(context.toasts.at(-1).title, "Security policy saved · refresh required");
});
