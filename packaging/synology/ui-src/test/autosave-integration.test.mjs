import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

function loadAppComponent(postSpy = async () => ({ ok: true })) {
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
      configureProfile: "configure-profile", routine: "routine", alertPolicy: "alert-policy",
      securityPolicy: "security-policy", clientEvent: "client-event"
    },
    MAX_RESPONSE_BYTES: 1024 * 1024,
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    apiGet: async () => ({}),
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
  assert.doesNotMatch(notifications, /autosave|AUTOSAVE_SCOPES/i);
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

  assert.match(methodSource("autosaveFailed", "hydrateAutosave"), /pauseAutosave\(task\.scope\)/);
  const pause = methodSource("pauseAutosave", "clearAutosaveFailure");
  assert.match(pause, /autosaveFailureScopes\[scope\] = true/);
  assert.match(pause, /setScopeBlocked\(scope, true\)/);

  for (const [name, next, scope] of [
    ["async saveProfile", "async saveProfileSecrets", "profile"],
    ["async saveRoutine", "async removeRoutine", "routine"],
    ["async saveAlerts", "async executeOperation", "alerts"],
    ["async saveSecurityPolicy", "clearProfileFilters", "security"],
    ["async saveInterfaceSettings", "persistSettings", "interface"]
  ]) {
    assert.match(methodSource(name, next), new RegExp(`pauseAutosave\\("${scope}"\\)`));
  }
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
    autosavePhase: "pending",
    autosaveMessage: "Autosave pending · 1.3 seconds"
  };
  context.refreshAutosaveStatus = (...args) => methods.refreshAutosaveStatus.call(context, ...args);
  methods.cancelAutosave.call(context, "profile");
  assert.equal(context.autosavePhase, "saved");
  assert.equal(context.autosaveMessage, "All changes saved");

  context.autosaveFailureScopes.profile = true;
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
  const methods = loadAppComponent(async (_auth, _csrf, action, payload) => {
    posts.push({ action, payload });
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
  assert.match(profileSave, /closeProfile\(\)/);
  assert.match(routineSave, /cancelAutosave\("routine"\)/);
  assert.match(routineSave, /loadRoutine\(payload\.profile\)/);
  assert.match(alertSave, /cancelAutosave\("alerts"\)/);
  assert.match(alertSave, /alertDirty = false;\s*this\.hydrateAutosave\("alerts", payload\)/);
  assert.match(securitySave, /cancelAutosave\("security"\)/);
  assert.match(securitySave, /hydrateSecurityPolicy\(true\)/);
  assert.match(interfaceSave, /cancelAutosave\("interface"\)/);
  assert.match(interfaceSave, /hydrateAutosave\("interface", candidate\)/);
});
