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
      removeRoutine: "remove-routine", execute: "execute", testProfileAuth: "test-profile-auth", browseRemote: "browse-remote"
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
    autosaveIncidents: Object.fromEntries(["profile", "routine", "alerts", "security", "interface"].map((name) => [name, { active: false, outcomeUnknown: false, requiresInspection: false, message: "", requestId: "", jobId: "", subject: "" }])),
    isolatedIncidents: {
      connection: { active: false, kind: "", outcomeUnknown: false, requiresInspection: false, message: "", requestId: "", jobId: "" },
      operations: { active: false, kind: "", outcomeUnknown: false, requiresInspection: false, message: "", requestId: "", jobId: "" }
    },
    profileConnectionState: "idle", profileConnectionAutosaveHeld: false,
    pathBrowser: { visible: false, kind: "", loading: false },
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
  return { clock, context, component };
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
    autosaveIncidents: Object.fromEntries(["profile", "routine", "alerts", "security", "interface"].map((name) => [name, { active: false, outcomeUnknown: false, requiresInspection: false, message: "", requestId: "", jobId: "", subject: "" }])),
    isolatedIncidents: {
      connection: { active: false, kind: "", outcomeUnknown: false, requiresInspection: false, message: "", requestId: "", jobId: "" },
      operations: { active: false, kind: "", outcomeUnknown: false, requiresInspection: false, message: "", requestId: "", jobId: "" }
    },
    profileConnectionState: "idle", profileConnectionAutosaveHeld: false,
    pathBrowser: { visible: false, kind: "", loading: false },
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

test("authentication cancels a pending profile autosave and a failed probe never releases it", async () => {
  let authenticationPosts = 0;
  let profilePosts = 0;
  const unknown = Object.assign(new Error("Authentication result could not be observed"), {
    outcomeUnknown: true,
    trustedRequestId: true,
    requestId: "9".repeat(32)
  });
  const { clock, context, component } = await coordinatorRuntime(async (_auth, _csrf, action) => {
    if (action === "test-profile-auth") {
      authenticationPosts += 1;
      throw unknown;
    }
    if (action === "configure-profile") profilePosts += 1;
    return { ok: true };
  });
  Object.assign(context, {
    profileEditorOpen: true,
    canTestProfileAuthentication: true,
    profileSaveState: "idle",
    profileForm: {
      name: "nightly", source: "/volume1/source", url: "https://nas.example.invalid",
      username: "sync-user", allow_http: false, danger_invalid_certs: false,
      ca_certificate: "", connect_timeout: 15, timeout: 120, retries: 2
    },
    selectedProfile: "nightly",
    selectedProfileModel: { name: "nightly", has_password: true, has_totp: false },
    secretModes: { password: "keep", totp: "keep", remote_log_token: "keep" },
    secretValues: { password: "", totp: "", remote_log_token: "" },
    profileConnectionRequest: 0,
    profileConnectionMessage: "",
    connectionIncidentEvidence: "",
    connectionProof: "",
    connectionProofExpires: 0,
    connectionProofTimer: 0,
    bridgeIssue: { title: "", message: "" },
    connectionLabel: "Authenticated package bridge",
    toasts: [],
    toast(title, message, error = false) { this.toasts.push({ title, message, error }); }
  });
  for (const name of [
    "strictDraftInteger", "between", "clearConnectionProofTimer", "scheduleConnectionProofExpiry",
    "connectionRequestPayload", "holdProfileAutosaveForConnection", "releaseProfileAutosaveFromConnection",
    "autosaveChanged", "reportMutationError", "ensureProfileFailureRecords",
    "syncProfileFailureState", "clearProfileConfigurationFailure"
  ]) context[name] = (...args) => component.methods[name].apply(context, args);

  context.autosaveCoordinator.hydrate("profile", { name: "nightly", source: "/volume1/source" });
  context.autosaveCoordinator.update("profile", { name: "nightly", source: "/volume1/pending" });
  assert.equal(context.autosaveCoordinator.getState("profile").scheduled, true);

  await component.methods.testProfileAuthentication.call(context, { preventDefault() {} });
  await clock.advance(5000);

  assert.equal(authenticationPosts, 1);
  assert.equal(profilePosts, 0, "the canceled pre-test draft must not fire after a failed or unknown auth probe");
  assert.equal(context.isolatedIncidents.connection.active, true);
  assert.equal(context.autosaveFailureScopes.profile, false);

  context.autosaveCoordinator.update("profile", { name: "nightly", source: "/volume1/later-edit" });
  await clock.advance(1300);
  assert.equal(profilePosts, 1, "a later profile edit remains autosavable despite connection-only evidence");
});

test("remote browsing cancels a pending profile autosave and a failed browse never releases it", async () => {
  let browsePosts = 0;
  let profilePosts = 0;
  const unknown = Object.assign(new Error("Browse result could not be observed"), { outcomeUnknown: true });
  const { clock, context, component } = await coordinatorRuntime(async (_auth, _csrf, action) => {
    if (action === "browse-remote") {
      browsePosts += 1;
      throw unknown;
    }
    if (action === "configure-profile") profilePosts += 1;
    return { ok: true };
  });
  Object.assign(context, {
    profileEditorOpen: true,
    profileSaveState: "idle",
    profileForm: {
      name: "nightly", source: "/volume1/source", url: "https://nas.example.invalid",
      username: "sync-user", allow_http: false, danger_invalid_certs: false,
      ca_certificate: "", connect_timeout: 15, timeout: 120, retries: 2
    },
    selectedProfile: "nightly",
    selectedProfileModel: { name: "nightly", has_password: true, has_totp: false },
    secretModes: { password: "keep", totp: "keep", remote_log_token: "keep" },
    secretValues: { password: "", totp: "", remote_log_token: "" },
    profileConnectionState: "success",
    connectionTestReady: true,
    connectionIncidentEvidence: "",
    connectionProof: `v1.${Math.floor(Date.now() / 1000) + 300}.${"1".repeat(64)}.${"2".repeat(64)}`,
    connectionProofExpires: Math.floor(Date.now() / 1000) + 300,
    pathBrowser: { visible: true, kind: "remote", current: "/home/Drive", parent: "/home", directories: [], truncated: false, loading: false, error: "", request: 0 },
    bridgeIssue: { title: "", message: "" },
    connectionLabel: "Authenticated package bridge",
    toasts: [],
    toast(title, message, error = false) { this.toasts.push({ title, message, error }); }
  });
  for (const name of [
    "strictDraftInteger", "between", "connectionRequestPayload", "browserParent",
    "holdProfileAutosaveForConnection", "releaseProfileAutosaveFromConnection",
    "autosaveChanged", "reportMutationError", "ensureProfileFailureRecords",
    "syncProfileFailureState", "clearProfileConfigurationFailure"
  ]) context[name] = (...args) => component.methods[name].apply(context, args);

  context.autosaveCoordinator.hydrate("profile", { name: "nightly", source: "/volume1/source" });
  context.autosaveCoordinator.update("profile", { name: "nightly", source: "/volume1/pending" });
  await component.methods.browsePath.call(context, "/home/Drive");
  await clock.advance(5000);

  assert.equal(browsePosts, 1);
  assert.equal(profilePosts, 0, "the canceled pre-browse draft must not fire after an unknown browse");
  assert.equal(context.isolatedIncidents.connection.active, true);

  context.autosaveCoordinator.update("profile", { name: "nightly", source: "/volume1/later-edit" });
  await clock.advance(1300);
  assert.equal(profilePosts, 1, "a later edit remains autosavable after connection-only browse evidence");
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

test("manual mutation outcome-unknown errors block only their own scope without Save-now guidance", async () => {
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
    for (const other of ["profile", "routine", "alerts", "security", "interface"].filter((candidate) => candidate !== scope)) {
      assert.equal(context.autosaveOutcomeUnknownScopes[other], false, `${scope} must not poison ${other}`);
    }
  }

  assert.equal(posts.length, cases.length);
  for (let index = 0; index < cases.length; index += 1) {
    await actions[index].call(cases[index].context, { preventDefault() {} });
  }
  assert.equal(posts.length, cases.length, "a second click must not redispatch any mutation family after an unknown outcome");
});

test("scope incidents preserve the first trusted correlation and profile subject", () => {
  const component = loadAppComponent();
  const methods = component.methods;
  const { context } = manualFailureContext(methods, "profile", {
    selectedProfile: "nightly",
    profileForm: { name: "nightly" }
  });
  const first = Object.assign(new Error("first uncertain result"), {
    outcomeUnknown: true,
    trustedRequestId: true,
    requestId: "a".repeat(32),
    trustedJobId: true,
    jobId: "b".repeat(48)
  });
  const later = Object.assign(new Error("later retry also uncertain"), {
    outcomeUnknown: true,
    trustedRequestId: true,
    requestId: "c".repeat(32),
    trustedJobId: true,
    jobId: "d".repeat(48)
  });

  methods.pauseAutosave.call(context, "profile", first);
  methods.pauseAutosave.call(context, "profile", later);

  assert.equal(context.autosaveIncidents.profile.requestId, first.requestId);
  assert.equal(context.autosaveIncidents.profile.jobId, first.jobId);
  assert.equal(context.autosaveIncidents.profile.subject, "nightly");
  const guidance = component.computed.profileOutcomeGuidance.call(context);
  assert.match(guidance, new RegExp(first.requestId));
  assert.match(guidance, new RegExp(first.jobId));
  assert.doesNotMatch(guidance, new RegExp(later.requestId));
});

test("an unresolved profile blocks dependent routines and operations but not unrelated alerts", async () => {
  const posts = [];
  const component = loadAppComponent(async (_auth, _csrf, action) => {
    posts.push(action);
    return { ok: true };
  });
  const methods = component.methods;
  const profileUnknown = {
    autosaveFailureScopes: { profile: true, routine: false, alerts: false, security: false, interface: false },
    autosaveOutcomeUnknownScopes: { profile: true, routine: false, alerts: false, security: false, interface: false },
    autosaveInspectionScopes: { profile: true, routine: false, alerts: false, security: false, interface: false }
  };
  const routine = manualFailureContext(methods, "routine", {
    ...profileUnknown,
    canChangeRoutines: true,
    routineForm: { profile: "nightly" },
    routineAutosavePayload: () => ({ profile: "nightly", allow_delete: false }),
    validateRoutinePayload: () => "",
    routineAutosaveNeedsReview: () => false
  }).context;
  const operation = manualFailureContext(methods, "security", {
    ...profileUnknown,
    canRunOperations: true,
    canAllowDestructive: true,
    diagnostic: { title: "", output: "" }
  }).context;
  const alerts = manualFailureContext(methods, "alerts", {
    ...profileUnknown,
    canChangeNotifications: true,
    alertPayload: () => ({ enabled: true }),
    validateAlertPayload: () => "",
    alertDirty: true
  }).context;

  await methods.saveRoutine.call(routine, { preventDefault() {} });
  await methods.executeOperation.call(operation, "run", { scope: "all", allow_delete: false });
  await methods.saveAlerts.call(alerts, { preventDefault() {} });

  assert.deepEqual(posts, ["alert-policy"]);
  assert.equal(routine.toasts.at(-1).title, "Routine save locked");
  assert.equal(operation.toasts.at(-1).title, "Operation locked");
  assert.equal(component.computed.canChangeRoutines.call({
    canMutate: true,
    operationBusy: false,
    profileOutcomeUnresolved: true,
    securityPolicy: { allow_routine_changes: true }
  }), false, "dependent routine controls must disable while profile state is unresolved");
  assert.equal(component.computed.canChangeRoutines.call({
    canMutate: true,
    operationBusy: false,
    profileOutcomeUnresolved: false,
    securityPolicy: { allow_routine_changes: true }
  }), true, "routine controls must recover when profile state is settled");
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
    assert.equal(context.isolatedIncidents.connection.active, false);
    assert.equal(context.isolatedIncidents.operations.active, false);
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

test("authoritative hydration reconciles only its named scope", () => {
  const methods = loadAppComponent().methods;
  const coordinator = {
    hydrate() {},
    getState() { return { registered: true, dirty: false, inFlight: false }; },
    setScopeBlocked() {}
  };
  const latched = manualFailureContext(methods, "security", {
    autosaveCoordinator: coordinator,
    autosaveFailureScopes: { profile: true, routine: false, alerts: false, security: true, interface: false },
    autosaveOutcomeUnknownScopes: { profile: true, routine: false, alerts: false, security: true, interface: false },
    autosaveInspectionScopes: { profile: true, routine: false, alerts: false, security: true, interface: false }
  });

  methods.hydrateAutosave.call(latched.context, "security", { require_https: true }, true);

  assert.equal(latched.context.autosaveOutcomeUnknownScopes.security, false, "the observed scope may reconcile");
  assert.equal(latched.context.autosaveOutcomeUnknownScopes.profile, true, "reconciling security must not clear profile evidence");
  assert.equal(latched.context.autosavePhase, "blocked");
  assert.equal(loadAppComponent().computed.profileOutcomeUnresolved.call(latched.context), true);
  assert.equal(loadAppComponent().computed.securityOutcomeUnresolved.call(latched.context), false);

  const fresh = manualFailureContext(methods, "security", {
    canMutate: true,
    securityDirty: true
  }).context;
  assert.equal(loadAppComponent().computed.canSubmitSecurity.call({
    canMutate: true,
    securityDirty: true,
    operationBusy: false,
    securityOutcomeUnresolved: false
  }), true, "an unrelated or reconciled security scope remains usable");
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
  assert.equal(profile.autosaveOutcomeUnknownScopes.profile, true);
  assert.equal(routine.autosaveOutcomeUnknownScopes.routine, true);
  assert.equal(notifications.autosaveOutcomeUnknownScopes.interface, true);
  assert.equal(operation.isolatedIncidents.operations.active, true);
  assert.equal(profile.autosaveOutcomeUnknownScopes.routine, false);
  assert.equal(routine.autosaveOutcomeUnknownScopes.profile, false);
});

test("a scoped mutation outcome blocks only matching autosave dispatch before transport", async () => {
  let posts = 0;
  const methods = loadAppComponent(async () => { posts += 1; return { ok: true }; }).methods;
  const context = manualFailureContext(methods, "alerts", {
    autosaveFailureScopes: { profile: false, routine: false, alerts: true, security: false, interface: false },
    autosaveOutcomeUnknownScopes: { profile: false, routine: false, alerts: true, security: false, interface: false },
    autosaveInspectionScopes: { profile: false, routine: false, alerts: true, security: false, interface: false }
  }).context;

  await assert.rejects(
    methods.dispatchAutosave.call(context, { scope: "alerts", value: { enabled: true } }),
    /Package alerts is locked.*may already have been accepted/i
  );
  assert.equal(posts, 0);
  assert.equal(context.operationBusy, false);

  await methods.dispatchAutosave.call(context, { scope: "routine", value: { profile: "nightly" } });
  assert.equal(posts, 1, "an alerts incident must not block routine autosave transport");
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
