import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");

async function loadApi() {
  return import(`data:text/javascript;base64,${Buffer.from(apiSource).toString("base64")}#${Date.now()}-${Math.random()}`);
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

function loadAppComponent({ post = async () => ({ ok: true }), get = async () => ({}) } = {}) {
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
      configureProfile: "configure-profile", setSecret: "set-secret",
      testProfileAuth: "test-profile-auth", browseRemote: "browse-remote",
      routine: "routine", alertPolicy: "alert-policy", securityPolicy: "security-policy",
      clientEvent: "client-event", removeProfile: "remove-profile"
    },
    AUTOSAVE_API_LIMITS: Object.freeze({}),
    MAX_RESPONSE_BYTES: 1024 * 1024,
    QueuedOutcomeUnknownError: class QueuedOutcomeUnknownError extends Error {},
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    apiGet: get,
    apiPost: post,
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback).slice(0, 65536),
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

function bind(context, methods, names) {
  for (const name of names) context[name] = (...args) => methods[name].call(context, ...args);
  return context;
}

function connectionForm() {
  return {
    url: "https://nas.example.invalid", username: "browser-user",
    allow_http: false, danger_invalid_certs: false, ca_certificate: "",
    connect_timeout: 15, timeout: 120, retries: 2,
    source: "/volume1/source", remote: "/home/Drive/Backup"
  };
}

function connectionContext(methods, overrides = {}) {
  const toasts = [];
  return bind({
    disposed: false, profileEditorOpen: true, canManageSecrets: true, operationBusy: false,
    canTestProfileAuthentication: true, selectedProfile: "", selectedProfileModel: null,
    profileForm: connectionForm(),
    secretModes: { password: "replace", totp: "keep", remote_log_token: "keep" },
    secretValues: { password: "fixture-password", totp: "", remote_log_token: "" },
    profileConnectionRequest: 0, profileConnectionState: "idle", profileConnectionMessage: "", profileConnectionAutosaveHeld: false,
    connectionProof: "", connectionProofExpires: 0, connectionProofTimer: 0, auth: {}, csrfToken: "csrf",
    isolatedIncidents: {
      connection: { active: false, kind: "", outcomeUnknown: false, requiresInspection: false, message: "", requestId: "", jobId: "" },
      operations: { active: false, kind: "", outcomeUnknown: false, requiresInspection: false, message: "", requestId: "", jobId: "" }
    },
    autosaveCoordinator: null, autosavePhase: "saved", autosaveMessage: "All changes saved",
    autosaveFailureScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    autosaveOutcomeUnknownScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    autosaveInspectionScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
    bridgeIssue: { title: "", message: "" }, connectionLabel: "Authenticated package bridge",
    pathBrowser: { visible: false, kind: "", current: "/", parent: null, directories: [], truncated: false, loading: false, error: "", request: 0 },
    toasts,
    toast(title, message, error = false) { toasts.push({ title, message, error }); },
    ...overrides
  }, methods, [
    "strictDraftInteger", "between", "clearConnectionProofTimer", "scheduleConnectionProofExpiry",
    "invalidateConnectionTest", "connectionRequestPayload", "cancelAutosave", "autosaveChanged",
    "refreshAutosaveStatus", "holdProfileAutosaveForConnection", "releaseProfileAutosaveFromConnection",
    "reportMutationError"
  ]);
}

test("stored-auth testing follows operational policy while secret writes remain frozen", () => {
  const component = loadAppComponent();
  const context = {
    profileEditorOpen: true,
    canMutate: true,
    operationBusy: false,
    capabilities: { profile_connection_test: true, secrets: true },
    securityPolicy: { allow_operational_actions: true, allow_secret_changes: false },
    profileConnectionState: "idle",
    profileSaveState: "idle"
  };
  assert.equal(component.computed.canManageSecrets.call(context), false);
  assert.equal(component.computed.canTestProfileAuthentication.call(context), true);
  context.securityPolicy.allow_operational_actions = false;
  assert.equal(component.computed.canTestProfileAuthentication.call(context), false);
});

test("new-profile authentication uses transient credentials and unlocks only the unchanged draft", async () => {
  const posts = [];
  const expires = Math.floor(Date.now() / 1000) + 300;
  const proof = `v1.${expires}.${"a".repeat(64)}.${"b".repeat(64)}`;
  const component = loadAppComponent({
    post: async (_auth, _csrf, action, payload) => {
      posts.push({ action, payload });
      return { connection_proof: proof, connection_proof_expires_at_epoch: expires };
    }
  });
  const context = connectionContext(component.methods);

  await component.methods.testProfileAuthentication.call(context, { preventDefault() {} });

  assert.equal(posts.length, 1);
  assert.equal(posts[0].action, "test-profile-auth");
  assert.equal(posts[0].payload.profile, null);
  assert.equal(posts[0].payload.password_source, "provided");
  assert.equal(posts[0].payload.password, "fixture-password");
  assert.equal(context.secretValues.password, "fixture-password", "testing must not consume the create-stage password");
  assert.equal(context.profileConnectionState, "success");
  assert.equal(context.connectionProof, proof);
  assert.equal(component.computed.connectionTestReady.call(context), true);

  component.methods.invalidateConnectionTest.call(context);
  assert.equal(context.connectionProof, "");
  assert.equal(context.profileConnectionState, "idle");
});

test("authentication rejects a response whose proof epoch and expiry field disagree", async () => {
  const expires = Math.floor(Date.now() / 1000) + 300;
  const proof = `v1.${expires}.${"a".repeat(64)}.${"b".repeat(64)}`;
  const component = loadAppComponent({
    post: async () => ({
      connection_proof: proof,
      connection_proof_expires_at_epoch: expires + 1
    })
  });
  const context = connectionContext(component.methods);

  await component.methods.testProfileAuthentication.call(context, { preventDefault() {} });

  assert.equal(context.profileConnectionState, "error");
  assert.equal(context.connectionProof, "");
  assert.match(context.profileConnectionMessage, /invalid authentication proof/);
});

test("ordinary authentication rejection is retryable without changing the profile or secret draft", async () => {
  let posts = 0;
  const expires = Math.floor(Date.now() / 1000) + 300;
  const proof = `v1.${expires}.${"e".repeat(64)}.${"f".repeat(64)}`;
  const component = loadAppComponent({
    post: async () => {
      posts += 1;
      if (posts === 1) throw new Error("DSM rejected these credentials");
      return { connection_proof: proof, connection_proof_expires_at_epoch: expires };
    }
  });
  const context = connectionContext(component.methods);
  const formBefore = structuredClone(context.profileForm);
  const modesBefore = structuredClone(context.secretModes);
  const valuesBefore = structuredClone(context.secretValues);

  await component.methods.testProfileAuthentication.call(context, { preventDefault() {} });
  assert.equal(context.profileConnectionState, "error");
  assert.equal(context.isolatedIncidents.connection.active, false);
  assert.deepEqual(context.profileForm, formBefore);
  assert.deepEqual(context.secretModes, modesBefore);
  assert.deepEqual(context.secretValues, valuesBefore);

  await component.methods.testProfileAuthentication.call(context, { preventDefault() {} });
  assert.equal(posts, 2);
  assert.equal(context.profileConnectionState, "success");
  assert.deepEqual(context.profileForm, formBefore);
  assert.deepEqual(context.secretValues, valuesBefore);
});

test("authentication owns the operation boundary and serializes secret save and profile deletion", async () => {
  const response = deferred();
  const posts = [];
  const expires = Math.floor(Date.now() / 1000) + 300;
  const proof = `v1.${expires}.${"a".repeat(64)}.${"b".repeat(64)}`;
  const component = loadAppComponent({
    post: async (_auth, _csrf, action, payload) => {
      posts.push({ action, payload });
      return response.promise;
    }
  });
  let confirmations = 0;
  const context = connectionContext(component.methods, {
    selectedProfile: "nightly",
    selectedProfileModel: { name: "nightly", has_password: true, has_totp: false },
    secretModes: { password: "keep", totp: "keep", remote_log_token: "keep" },
    canChangeProfiles: true,
    confirmAction: async () => { confirmations += 1; return true; }
  });

  const authentication = component.methods.testProfileAuthentication.call(context, { preventDefault() {} });
  assert.equal(context.operationBusy, true);
  await component.methods.saveProfileSecrets.call(context, { preventDefault() {} });
  await component.methods.removeProfile.call(context);
  assert.equal(posts.length, 1, "concurrent profile mutations must stop before transport");
  assert.equal(confirmations, 0, "delete confirmation must not open during an authentication test");

  response.resolve({ connection_proof: proof, connection_proof_expires_at_epoch: expires });
  await authentication;
  assert.equal(context.operationBusy, false);
  assert.equal(context.profileConnectionState, "success");
});

test("unknown authentication evidence stays connection-scoped and correlated after deliberate retry", async () => {
  let posts = 0;
  const expires = Math.floor(Date.now() / 1000) + 300;
  const proof = `v1.${expires}.${"a".repeat(64)}.${"b".repeat(64)}`;
  const requestId = "1".repeat(32);
  const jobId = "2".repeat(48);
  const unknown = Object.assign(new Error("Authentication outcome is unknown"), {
    outcomeUnknown: true, trustedRequestId: true, requestId, trustedJobId: true, jobId
  });
  const component = loadAppComponent({
    post: async () => {
      posts += 1;
      if (posts === 1) throw unknown;
      return { connection_proof: proof, connection_proof_expires_at_epoch: expires };
    }
  });
  const context = connectionContext(component.methods);

  await component.methods.testProfileAuthentication.call(context, { preventDefault() {} });
  assert.equal(context.isolatedIncidents.connection.active, true);
  assert.match(context.profileConnectionMessage, new RegExp(requestId));
  assert.match(context.profileConnectionMessage, new RegExp(jobId));
  assert.equal(context.autosaveFailureScopes.profile, false, "a connection incident must not poison profile autosave");
  context.connectionIncidentEvidence = component.computed.connectionIncidentEvidence.call(context);
  await component.methods.testProfileAuthentication.call(context, { preventDefault() {} });

  assert.equal(posts, 2);
  assert.equal(context.isolatedIncidents.connection.active, true);
  assert.equal(context.isolatedIncidents.connection.requestId, requestId);
  assert.equal(context.isolatedIncidents.connection.jobId, jobId);
  assert.match(context.profileConnectionMessage, /prior unresolved connection evidence remains/i);
  assert.equal(context.operationBusy, false);
  assert.equal(context.profileConnectionState, "success");
});

test("cleanup-required File Station evidence survives later success until explicit reconciliation", async () => {
  const api = await loadApi();
  const codes = [
    "file_station_logout_failed",
    "file_station_denied_logout_failed",
    "file_station_listing_logout_failed",
    "file_station_operation_logout_failed"
  ];
  for (const code of codes) {
    const error = new api.DsmApiError("File Station logout failed", 502, code, "file_station_logout");
    assert.equal(error.outcomeUnknown, undefined);
    assert.equal(error.requiresInspection, true, `${code} must be classified as inspection-required`);
  }

  let authenticationPosts = 0;
  const expires = Math.floor(Date.now() / 1000) + 300;
  const proof = `v1.${expires}.${"c".repeat(64)}.${"d".repeat(64)}`;
  const cleanupRequestId = "7".repeat(32);
  const authentication = loadAppComponent({
    post: async () => {
      authenticationPosts += 1;
      if (authenticationPosts === 1) {
        throw Object.assign(new api.DsmApiError("Temporary session logout failed", 502, codes[0]), {
          trustedRequestId: true, requestId: cleanupRequestId
        });
      }
      return { connection_proof: proof, connection_proof_expires_at_epoch: expires };
    }
  });
  const authenticationContext = connectionContext(authentication.methods);
  await authentication.methods.testProfileAuthentication.call(authenticationContext, { preventDefault() {} });
  assert.equal(authenticationContext.isolatedIncidents.connection.requiresInspection, true);
  assert.equal(authenticationContext.toasts[0].title, "Authentication cleanup needs inspection");
  authenticationContext.connectionIncidentEvidence = authentication.computed.connectionIncidentEvidence.call(authenticationContext);
  await authentication.methods.testProfileAuthentication.call(authenticationContext, { preventDefault() {} });
  assert.equal(authenticationPosts, 2);
  assert.equal(authenticationContext.isolatedIncidents.connection.active, true);
  assert.equal(authenticationContext.isolatedIncidents.connection.requestId, cleanupRequestId);
  assert.match(authentication.computed.incidentGuidance.call(authenticationContext), new RegExp(cleanupRequestId));
  assert.match(authenticationContext.profileConnectionMessage, /prior unresolved connection evidence remains/i);

  for (const code of codes.slice(1)) {
    let browsePosts = 0;
    const component = loadAppComponent({
      post: async () => {
        browsePosts += 1;
        if (browsePosts === 1) throw new api.DsmApiError("Temporary session logout failed", 502, code);
        return { directory_schema: "sdsync.dsm-remote-directories.v1", current: "/home/Drive", directories: [], truncated: false };
      }
    });
    const context = connectionContext(component.methods, {
      connectionTestReady: true,
      connectionProof: `v1.${Math.floor(Date.now() / 1000) + 300}.${"a".repeat(64)}.${"b".repeat(64)}`,
      connectionProofExpires: Math.floor(Date.now() / 1000) + 300,
      pathBrowser: { visible: true, kind: "remote", current: "/home/Drive", parent: "/home", directories: [], truncated: false, loading: false, error: "", request: 0 }
    });
    context.browserParent = (...args) => component.methods.browserParent.call(context, ...args);
    await component.methods.browsePath.call(context, "/home/Drive");
    assert.equal(context.isolatedIncidents.connection.requiresInspection, true);
    assert.match(context.pathBrowser.error, /logout failed|cleanup/i);
    context.connectionIncidentEvidence = component.computed.connectionIncidentEvidence.call(context);
    await component.methods.browsePath.call(context, "/home/Drive");
    assert.equal(browsePosts, 2, `${code} must permit a deliberate connection-only retry`);
    assert.equal(context.isolatedIncidents.connection.active, true, `${code} cleanup evidence must outlive an unrelated later success`);
    assert.match(context.pathBrowser.error, /prior unresolved connection evidence remains/i);
  }
});

test("remote browsing is auth-gated and sends the proof with the exact transient draft", async () => {
  const posts = [];
  const component = loadAppComponent({
    post: async (_auth, _csrf, action, payload) => {
      posts.push({ action, payload });
      return {
        directory_schema: "sdsync.dsm-remote-directories.v1",
        current: payload.parent,
        directories: [{ name: "Child", path: `${payload.parent}/Child` }],
        truncated: false
      };
    }
  });
  const blocked = connectionContext(component.methods, { connectionTestReady: false });
  component.methods.openRemotePathBrowser.call(blocked, { preventDefault() {} });
  assert.equal(posts.length, 0);
  assert.equal(blocked.toasts.at(-1).error, true);

  const context = connectionContext(component.methods, {
    connectionTestReady: true,
    connectionProof: `v1.${Math.floor(Date.now() / 1000) + 300}.${"a".repeat(64)}.${"b".repeat(64)}`,
    connectionProofExpires: Math.floor(Date.now() / 1000) + 300,
    pathBrowser: { visible: true, kind: "remote", current: "/home/Drive", parent: "/home", directories: [], truncated: false, loading: false, error: "", request: 0 }
  });
  context.browserParent = (...args) => component.methods.browserParent.call(context, ...args);
  await component.methods.browsePath.call(context, "/home/Drive");

  assert.equal(posts.length, 1);
  assert.equal(posts[0].action, "browse-remote");
  assert.equal(posts[0].payload.connection_proof, context.connectionProof);
  assert.equal(posts[0].payload.password, "fixture-password");
  assert.deepEqual(context.pathBrowser.directories, [{ name: "Child", path: "/home/Drive/Child" }]);
});

test("unknown File Station browse evidence remains isolated after a trusted new retry", async () => {
  let posts = 0;
  const unknown = Object.assign(new Error("Browse outcome is unknown"), { outcomeUnknown: true });
  const component = loadAppComponent({
    post: async (_auth, _csrf, _action, payload) => {
      posts += 1;
      if (posts === 1) throw unknown;
      return { directory_schema: "sdsync.dsm-remote-directories.v1", current: payload.parent, directories: [], truncated: false };
    }
  });
  const context = connectionContext(component.methods, {
    connectionTestReady: true,
    connectionProof: `v1.${Math.floor(Date.now() / 1000) + 300}.${"a".repeat(64)}.${"b".repeat(64)}`,
    connectionProofExpires: Math.floor(Date.now() / 1000) + 300,
    pathBrowser: { visible: true, kind: "remote", current: "/home/Drive", parent: "/home", directories: [], truncated: false, loading: false, error: "", request: 0 }
  });
  context.browserParent = (...args) => component.methods.browserParent.call(context, ...args);

  await component.methods.browsePath.call(context, "/home/Drive");
  assert.equal(context.isolatedIncidents.connection.active, true);
  assert.equal(context.autosaveFailureScopes.profile, false);
  context.connectionIncidentEvidence = component.computed.connectionIncidentEvidence.call(context);
  await component.methods.browsePath.call(context, "/home/Drive");

  assert.equal(posts, 2);
  assert.equal(context.isolatedIncidents.connection.active, true);
  assert.match(context.pathBrowser.error, /prior unresolved connection evidence remains/i);
});

test("authentication proof expiry closes remote browsing and prevents another browse dispatch", async () => {
  let timerCallback = null;
  let posts = 0;
  const previousWindow = globalThis.window;
  const previousNow = Date.now;
  globalThis.window = {
    setTimeout(callback) { timerCallback = callback; return 41; },
    clearTimeout() {}
  };
  Date.now = () => 1_000_000;
  try {
    const component = loadAppComponent({ post: async () => { posts += 1; return {}; } });
    const context = connectionContext(component.methods, {
      connectionProof: `v1.1001.${"a".repeat(64)}.${"b".repeat(64)}`,
      connectionProofExpires: 1001,
      profileConnectionState: "success",
      pathBrowser: { visible: true, kind: "remote", current: "/home", parent: "/", directories: [], truncated: false, loading: false, error: "", request: 0 },
      closePathBrowser() { this.pathBrowser.visible = false; }
    });
    component.methods.scheduleConnectionProofExpiry.call(context, 1001);
    assert.equal(typeof timerCallback, "function");
    Date.now = () => 1_001_001;
    timerCallback();
    assert.equal(context.profileConnectionState, "expired");
    assert.equal(context.connectionProof, "");
    assert.equal(context.pathBrowser.visible, false);

    context.pathBrowser.visible = true;
    context.connectionTestReady = false;
    await component.methods.browsePath.call(context, "/home");
    assert.equal(posts, 0);
  } finally {
    Date.now = previousNow;
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
  }
});

test("local browser lists package-visible directories and preserves an Up route after errors", async () => {
  const requests = [];
  const component = loadAppComponent({
    get: async (_auth, action, query) => {
      requests.push({ action, query });
      if (query.parent === "/volume1/missing") throw new Error("Folder is not visible to the package identity.");
      return {
        schema: "sdsync.dsm-source-directories.v1", current: query.parent,
        parent: query.parent === "/" ? null : "/",
        directories: [{ name: "Source", path: "/volume1/Source" }], truncated: false
      };
    }
  });
  const context = {
    disposed: false,
    auth: {},
    connectionTestReady: false,
    pathBrowser: { visible: true, kind: "local", current: "/volume1/missing", parent: null, directories: [], truncated: false, loading: false, error: "", request: 0 }
  };
  context.browserParent = (...args) => component.methods.browserParent.call(context, ...args);

  await component.methods.browsePath.call(context, "/volume1/missing");
  assert.equal(context.pathBrowser.parent, "/volume1");
  assert.match(context.pathBrowser.error, /not visible/);

  await component.methods.browsePath.call(context, "/");
  assert.equal(context.pathBrowser.error, "");
  assert.deepEqual(context.pathBrowser.directories, [{ name: "Source", path: "/volume1/Source" }]);
  assert.deepEqual(requests.map((request) => request.action), ["source-directories", "source-directories"]);
});

function saveContext(methods, secretModes, secretValues) {
  const payload = {
    name: "new-profile", source: "/volume1/source", allow_http: false,
    allow_empty_source: false, danger_accept_invalid_certs: false, delete: false
  };
  const reports = [];
  const context = bind({
    profileEditorOpen: true, canChangeProfiles: true, operationBusy: false, disposed: false,
    profileSaveState: "idle", profileSaveMessage: "", selectedProfile: "",
    secretModes: { ...secretModes }, secretValues: { ...secretValues },
    auth: {}, csrfToken: "csrf", reports, closed: false, refreshed: 0,
    cancelAutosave() {}, profileAutosavePayload: () => payload, validateProfile: () => "",
    confirmAction: async () => true, clearProfileConfigurationFailure() {}, hydrateAutosave() {},
    pauseAutosave() {}, reportMutationError(error) { reports.push(error); },
    toast() {}, clearSecrets() { this.secretValues = { password: "", totp: "", remote_log_token: "" }; },
    closeProfile(options) { this.closed = options; },
    refreshSnapshot: async function () { this.refreshed += 1; return true; },
    clearProfileSecretFailures() {}, refreshAutosaveStatus() {}
  }, methods, ["secretOperations", "applyTrustedSecretPresence"]);
  return { context, payload };
}

test("profile creation dispatches and settles with visible progress", async () => {
  const posts = [];
  const component = loadAppComponent({
    get: async (_auth, action, query) => ({
      schema: "sdsync.dsm-source-path.v1", path: query.path, valid: true
    }),
    post: async (_auth, _csrf, action, payload) => { posts.push({ action, payload }); return { ok: true }; }
  });
  const { context } = saveContext(component.methods,
    { password: "replace", totp: "keep", remote_log_token: "keep" },
    { password: "fixture-password", totp: "", remote_log_token: "" });

  await component.methods.saveProfile.call(context, { preventDefault() {} });

  assert.deepEqual(posts.map((entry) => entry.action), ["configure-profile", "set-secret"]);
  assert.equal(context.operationBusy, false);
  assert.equal(context.profileSaveState, "success");
  assert.match(context.profileSaveMessage, /successfully/);
  assert.deepEqual(context.closed, { refresh: false });
  assert.equal(context.refreshed, 1);
});

test("trusted secret-only success refreshes stored-presence state for immediate authentication", async () => {
  const component = loadAppComponent({
    post: async () => ({ has_password: true, has_totp: false, has_remote_log_token: false })
  });
  const model = { name: "nightly", has_password: false, has_totp: false, has_remote_log_token: false };
  const context = connectionContext(component.methods, {
    selectedProfile: "nightly",
    selectedProfileModel: model,
    profileSaveState: "idle",
    profileSaveMessage: "",
    secretModes: { password: "replace", totp: "keep", remote_log_token: "keep" },
    secretValues: { password: "fixture-password", totp: "", remote_log_token: "" },
    secretOperations: () => [{ profile: "nightly", kind: "password", mode: "replace", value: "fixture-password" }],
    validateSecretOperations: () => "",
    clearProfileSecretFailures() {},
    refreshAutosaveStatus() {}
  });
  context.applyTrustedSecretPresence = (...args) => component.methods.applyTrustedSecretPresence.call(context, ...args);

  await component.methods.saveProfileSecrets.call(context, { preventDefault() {} });

  assert.equal(model.has_password, true);
  assert.equal(context.secretModes.password, "keep");
  assert.equal(context.secretValues.password, "");
  const payload = component.methods.connectionRequestPayload.call(context);
  assert.equal(payload.error, undefined);
  assert.equal(payload.password_source, "stored");
  assert.equal(payload.password, null);
});

test("configure rejection keeps every draft secret and leaves the editor recoverable", async () => {
  const component = loadAppComponent({
    get: async (_auth, _action, query) => ({
      schema: "sdsync.dsm-source-path.v1", path: query.path, valid: true
    }),
    post: async () => { throw new Error("configuration rejected"); }
  });
  const { context } = saveContext(component.methods,
    { password: "replace", totp: "replace", remote_log_token: "keep" },
    { password: "fixture-password", totp: "JBSWY3DPEHPK3PXP", remote_log_token: "" });

  await component.methods.saveProfile.call(context, { preventDefault() {} });

  assert.equal(context.profileSaveState, "error");
  assert.equal(context.closed, false);
  assert.deepEqual(context.secretModes, { password: "replace", totp: "replace", remote_log_token: "keep" });
  assert.equal(context.secretValues.password, "fixture-password");
  assert.equal(context.secretValues.totp, "JBSWY3DPEHPK3PXP");
});

test("partial secret failure clears only applied values and retains the unapplied stage", async () => {
  let post = 0;
  const component = loadAppComponent({
    get: async (_auth, _action, query) => ({
      schema: "sdsync.dsm-source-path.v1", path: query.path, valid: true
    }),
    post: async () => {
      post += 1;
      if (post === 3) throw new Error("totp rejected");
      return { ok: true };
    }
  });
  const { context } = saveContext(component.methods,
    { password: "replace", totp: "replace", remote_log_token: "keep" },
    { password: "fixture-password", totp: "JBSWY3DPEHPK3PXP", remote_log_token: "" });

  await component.methods.saveProfile.call(context, { preventDefault() {} });

  assert.equal(context.profileSaveState, "error");
  assert.equal(context.secretModes.password, "keep");
  assert.equal(context.secretValues.password, "");
  assert.equal(context.secretModes.totp, "replace");
  assert.equal(context.secretValues.totp, "JBSWY3DPEHPK3PXP");
  assert.equal(context.closed, false);
});

test("profile-owned drafts block manual refresh and fence an already in-flight snapshot", async () => {
  const snapshot = deferred();
  let reads = 0;
  const component = loadAppComponent({ get: async () => { reads += 1; return snapshot.promise; } });
  const previousDocument = globalThis.document;
  globalThis.document = { hidden: false };
  try {
    const stale = {
      disposed: false, snapshotRefreshBlocked: false, snapshotPromise: null,
      snapshotRefreshQueued: false, snapshotLoading: false, snapshotGeneration: 7,
      csrfToken: "csrf", auth: {}, snapshot: null, connected: false,
      scheduleSnapshot() {}, toast() {}
    };
    const pending = component.methods.refreshSnapshot.call(stale, false);
    stale.snapshotGeneration += 1;
    stale.snapshotRefreshBlocked = true;
    snapshot.resolve({ schema: "sdsync.dsm-api.v1", revision: "stale" });
    assert.equal(await pending, false);
    assert.equal(stale.snapshot, null, "late status must not replace editor-owned state");

    const toasts = [];
    const blocked = {
      disposed: false, snapshotRefreshBlocked: true, snapshotPromise: null,
      toast(title, message, error) { toasts.push({ title, message, error }); }
    };
    assert.equal(await component.methods.refreshSnapshot.call(blocked, true), false);
    assert.equal(reads, 1, "manual refresh must not dispatch while the profile owns the draft");
    assert.match(toasts[0].message, /will not be overwritten/);
  } finally {
    if (previousDocument === undefined) delete globalThis.document;
    else globalThis.document = previousDocument;
  }
});

test("profile recovery preserves the editor and transient secrets across Activity evidence reads", async () => {
  const freshSnapshot = {
    schema: "sdsync.dsm-api.v1",
    profiles: [{ name: "nightly", source: "/volume1/authoritative" }],
    capabilities: { mutations: true }
  };
  const component = loadAppComponent({ get: async () => freshSnapshot });
  const previousDocument = globalThis.document;
  const previousWindow = globalThis.window;
  globalThis.document = { hidden: false };
  globalThis.window = { clearTimeout() {} };
  try {
    const profileForm = { ...connectionForm(), name: "nightly", source: "/volume1/draft" };
    const secretValues = { password: "draft-password", totp: "JBSWY3DPEHPK3PXP", remote_log_token: "draft-token" };
    const secretModes = { password: "replace", totp: "replace", remote_log_token: "replace" };
    const refreshContext = {
      disposed: false, snapshotRefreshBlocked: false, snapshotPromise: null,
      snapshotRefreshQueued: false, snapshotLoading: false, snapshotGeneration: 4,
      csrfToken: "csrf", auth: {}, snapshot: null, connected: false, canMutate: true,
      profileEditorOpen: true, profileForm, secretValues, secretModes,
      bridgeIssue: { title: "", message: "" }, connectionLabel: "", freshness: "",
      hydrateAlerts() {}, hydrateSecurityPolicy() {}, maybeNotifyFailure() {}, scheduleSnapshot() {}, toast() {}
    };

    assert.equal(await component.methods.refreshSnapshot.call(refreshContext, true, true), true);
    assert.equal(refreshContext.snapshot, freshSnapshot);
    assert.equal(refreshContext.profileForm, profileForm, "snapshot evidence must not replace the editor-owned form object");
    assert.equal(refreshContext.secretValues, secretValues, "snapshot evidence must not replace transient secret values");
    assert.deepEqual(refreshContext.secretValues, {
      password: "draft-password", totp: "JBSWY3DPEHPK3PXP", remote_log_token: "draft-token"
    });

    const navigation = {
      routes: [{ id: "profiles" }, { id: "activity" }], route: "profiles",
      profileSaveState: "error", profileConnectionState: "idle", profileRecoveryActive: true,
      profileEditorOpen: true, profileForm, secretValues, secretModes, logTimer: 0,
      autosaveOutcomeUnknownScopes: { profile: true, routine: false, alerts: false, security: false, interface: false },
      autosaveInspectionScopes: { profile: true, routine: false, alerts: false, security: false, interface: false },
      autosaveIncidents: {
        profile: { active: true, outcomeUnknown: true, requiresInspection: true, message: "uncertain", requestId: "8".repeat(32), jobId: "9".repeat(48), subject: "nightly" }
      },
      isolatedIncidents: {
        connection: { active: false }, operations: { active: false }
      },
      closeProfile() { assert.fail("recovery navigation must not close or clear the profile editor"); },
      closeRoutine() {}, refreshLogsCalls: 0, refreshes: 0,
      refreshLogs() { this.refreshLogsCalls += 1; },
      refreshSnapshot() { this.refreshes += 1; return Promise.resolve(true); },
      toast() {}
    };
    const correlationBefore = component.computed.incidentGuidance.call(navigation);
    component.methods.navigate.call(navigation, "activity");
    assert.equal(navigation.route, "activity");
    assert.equal(navigation.refreshLogsCalls, 1, "Activity and Logs reads remain available");
    assert.equal(navigation.refreshes, 1, "entering Activity requests fresh package evidence");
    assert.equal(navigation.profileForm, profileForm);
    assert.equal(navigation.secretValues, secretValues);
    assert.equal(component.computed.incidentGuidance.call(navigation), correlationBefore);
    assert.match(correlationBefore, new RegExp("8".repeat(32)));
    assert.match(correlationBefore, new RegExp("9".repeat(48)));

    component.methods.navigate.call(navigation, "profiles");
    assert.equal(navigation.route, "profiles");
    assert.equal(navigation.profileEditorOpen, true);
    assert.equal(navigation.profileForm.source, "/volume1/draft");
    assert.equal(navigation.secretValues.password, "draft-password");
  } finally {
    if (previousDocument === undefined) delete globalThis.document;
    else globalThis.document = previousDocument;
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
  }
});

test("Local source visibly explains DSM system-internal-user permissions", () => {
  assert.match(appSource, /class="sdsync-permission-callout span-2"/);
  assert.match(appSource, /Control Panel → Shared Folder → Permissions → System internal user/);
  assert.match(appSource, /grant list, traverse, and read access to the exact package identity DSM displays/);
  assert.match(appSource, /DSM can collision-rename that identity/);
  assert.match(appSource, /package cannot grant itself access/);
});

test("folder explorer exposes breadcrumbs, explicit current-folder selection, and complete async states", () => {
  assert.match(appSource, /class="sdsync-path-browser-breadcrumbs" aria-label="Current folder"/);
  assert.match(appSource, /:aria-current="crumb\.current \? 'location' : null"/);
  assert.match(appSource, />Up one level<\/v-button>/);
  assert.match(appSource, /class="sdsync-path-browser-current" aria-live="polite"/);
  assert.match(appSource, /class="sdsync-path-browser-folder-icon"/);
  assert.match(appSource, /:aria-label="'Open folder ' \+ directory\.name"/);
  assert.match(appSource, /:aria-label="'Select folder ' \+ directory\.name"/);
  assert.match(appSource, /<strong>Opening folder<\/strong>/);
  assert.match(appSource, /<strong>Folder listing unavailable<\/strong>/);
  assert.match(appSource, /<strong>No child folders visible<\/strong>/);
  assert.match(appSource, />Select this folder<\/v-button>/);

  const component = loadAppComponent();
  const local = component.methods.pathBrowserBreadcrumbs.call(
    { pathBrowser: { kind: "local" } },
    "/volume1/Shared/Project"
  );
  assert.deepEqual(local, [
    { label: "NAS", path: "/", current: false },
    { label: "volume1", path: "/volume1", current: false },
    { label: "Shared", path: "/volume1/Shared", current: false },
    { label: "Project", path: "/volume1/Shared/Project", current: true }
  ]);
  const remoteRoot = component.methods.pathBrowserBreadcrumbs.call(
    { pathBrowser: { kind: "remote" } },
    "/"
  );
  assert.deepEqual(remoteRoot, [{ label: "File Station", path: "/", current: true }]);
});

test("starting folder navigation clears stale rows before awaiting the bounded listing", async () => {
  const pending = deferred();
  const component = loadAppComponent({ get: async () => pending.promise });
  const context = {
    disposed: false,
    auth: {},
    connectionTestReady: false,
    pathBrowser: {
      visible: true, kind: "local", current: "/volume1/Old", parent: "/volume1",
      directories: [{ name: "Stale", path: "/volume1/Old/Stale" }], truncated: true,
      loading: false, error: "", request: 0
    }
  };
  context.browserParent = (...args) => component.methods.browserParent.call(context, ...args);

  const navigation = component.methods.browsePath.call(context, "/volume1/New");
  assert.equal(context.pathBrowser.loading, true);
  assert.deepEqual(context.pathBrowser.directories, []);
  assert.equal(context.pathBrowser.truncated, false);
  pending.resolve({
    schema: "sdsync.dsm-source-directories.v1",
    current: "/volume1/New",
    parent: "/volume1",
    directories: [{ name: "Fresh", path: "/volume1/New/Fresh" }],
    truncated: false
  });
  await navigation;
  assert.equal(context.pathBrowser.loading, false);
  assert.deepEqual(context.pathBrowser.directories, [{ name: "Fresh", path: "/volume1/New/Fresh" }]);
});

test("closing and reopening the folder explorer fences a late listing from the prior dialog", async () => {
  const firstListing = deferred();
  let reads = 0;
  const component = loadAppComponent({
    get: async (_auth, _action, parameters) => {
      reads += 1;
      if (reads === 1) return firstListing.promise;
      return {
        schema: "sdsync.dsm-source-directories.v1",
        current: parameters.parent,
        parent: "/",
        directories: [{ name: "Current child", path: `${parameters.parent}/Current child` }],
        truncated: false
      };
    }
  });
  const context = bind({
    disposed: false,
    auth: {},
    pathBrowserPriorFocus: null,
    pathBrowserKeyHandler: null,
    pathBrowser: {
      visible: false, kind: "", current: "/", parent: null,
      directories: [], truncated: false, loading: false, error: "", request: 0
    }
  }, component.methods, [
    "removePathBrowserKeyHandler", "handlePathBrowserKeydown", "browserParent", "browsePath"
  ]);

  const staleRequest = component.methods.showPathBrowser.call(context, "local", "/volume1/Old");
  assert.equal(context.pathBrowser.request, 1);
  component.methods.closePathBrowser.call(context);
  assert.equal(context.pathBrowser.request, 2);
  assert.equal(context.pathBrowser.visible, false);

  await component.methods.showPathBrowser.call(context, "local", "/volume2/New");
  assert.equal(context.pathBrowser.request, 3);
  assert.equal(context.pathBrowser.current, "/volume2/New");
  assert.deepEqual(context.pathBrowser.directories, [
    { name: "Current child", path: "/volume2/New/Current child" }
  ]);

  firstListing.resolve({
    schema: "sdsync.dsm-source-directories.v1",
    current: "/volume1/Old",
    parent: "/volume1",
    directories: [{ name: "Stale child", path: "/volume1/Old/Stale child" }],
    truncated: false
  });
  await staleRequest;

  assert.equal(context.pathBrowser.request, 3);
  assert.equal(context.pathBrowser.current, "/volume2/New");
  assert.equal(context.pathBrowser.loading, false);
  assert.deepEqual(context.pathBrowser.directories, [
    { name: "Current child", path: "/volume2/New/Current child" }
  ]);
});

test("closing a remote explorer preserves late cleanup evidence without repainting modal state", async () => {
  const pending = deferred();
  const api = await loadApi();
  const requestId = "c".repeat(32);
  const component = loadAppComponent({ post: async () => pending.promise });
  const context = connectionContext(component.methods, {
    selectedProfile: "nightly",
    connectionTestReady: true,
    connectionProof: `v1.${Math.floor(Date.now() / 1000) + 300}.${"a".repeat(64)}.${"b".repeat(64)}`,
    connectionProofExpires: Math.floor(Date.now() / 1000) + 300,
    pathBrowserPriorFocus: null,
    pathBrowserKeyHandler: null,
    pathBrowser: {
      visible: true, kind: "remote", current: "/home/Drive", parent: "/home",
      directories: [], truncated: false, loading: false, error: "", request: 0
    }
  });
  context.browserParent = (...args) => component.methods.browserParent.call(context, ...args);
  context.removePathBrowserKeyHandler = (...args) => component.methods.removePathBrowserKeyHandler.call(context, ...args);

  const browse = component.methods.browsePath.call(context, "/home/Drive");
  assert.equal(context.pathBrowser.loading, true);
  component.methods.closePathBrowser.call(context);
  context.selectedProfile = "different-draft";
  assert.deepEqual(context.pathBrowser, {
    visible: false, kind: "", current: "/", parent: null, directories: [],
    loading: false, error: "", truncated: false, request: 2
  });

  pending.reject(Object.assign(
    new api.DsmApiError(
      "The File Station listing failed and its temporary session could not be closed.",
      502,
      "file_station_listing_logout_failed",
      "file_station_logout"
    ),
    { trustedRequestId: true, requestId }
  ));
  await browse;

  assert.deepEqual(context.pathBrowser, {
    visible: false, kind: "", current: "/", parent: null, directories: [],
    loading: false, error: "", truncated: false, request: 2
  });
  assert.equal(context.isolatedIncidents.connection.active, true);
  assert.equal(context.isolatedIncidents.connection.requiresInspection, true);
  assert.equal(context.isolatedIncidents.connection.requestId, requestId);
  assert.equal(context.isolatedIncidents.connection.subject, "nightly · /home/Drive");
  assert.equal(context.toasts.at(-1).title, "File Station browse cleanup needs inspection");
  assert.equal(context.operationBusy, false);
  assert.equal(context.profileConnectionAutosaveHeld, false);
});

test("closing a remote explorer ignores a late ordinary rejection but releases its operation hold", async () => {
  const pending = deferred();
  const component = loadAppComponent({ post: async () => pending.promise });
  const context = connectionContext(component.methods, {
    connectionTestReady: true,
    connectionProof: `v1.${Math.floor(Date.now() / 1000) + 300}.${"a".repeat(64)}.${"b".repeat(64)}`,
    connectionProofExpires: Math.floor(Date.now() / 1000) + 300,
    pathBrowserPriorFocus: null,
    pathBrowserKeyHandler: null,
    pathBrowser: {
      visible: true, kind: "remote", current: "/home/Drive", parent: "/home",
      directories: [], truncated: false, loading: false, error: "", request: 0
    }
  });
  context.browserParent = (...args) => component.methods.browserParent.call(context, ...args);
  context.removePathBrowserKeyHandler = (...args) => component.methods.removePathBrowserKeyHandler.call(context, ...args);

  const browse = component.methods.browsePath.call(context, "/home/Drive");
  assert.equal(context.profileConnectionAutosaveHeld, true);
  component.methods.closePathBrowser.call(context);
  pending.reject(new Error("The folder is not available to this account."));
  await browse;

  assert.deepEqual(context.pathBrowser, {
    visible: false, kind: "", current: "/", parent: null, directories: [],
    loading: false, error: "", truncated: false, request: 2
  });
  assert.equal(context.toasts.length, 0);
  assert.equal(context.isolatedIncidents.connection.active, false);
  assert.equal(context.operationBusy, false);
  assert.equal(context.profileConnectionAutosaveHeld, false);
});

test("closing an editor always requests one fresh snapshot and active operations block navigation", () => {
  const component = loadAppComponent();
  const refreshes = [];
  const context = {
    profileSaveState: "idle", profileConnectionState: "idle", profileEditorOpen: true,
    selectedProfile: "nightly", snapshotGeneration: 1, profileConnectionRequest: 1,
    secretModes: {}, secretValues: {}, connectionProof: "proof", connectionProofExpires: 1, connectionProofTimer: 0,
    profileSaveMessage: "", pathBrowser: { request: 0 }, disposed: false,
    cancelAutosave() {}, closePathBrowser() {}, clearSecrets() {}, clearConnectionProofTimer() {},
    refreshSnapshot(...args) { refreshes.push(args); }
  };
  component.methods.closeProfile.call(context);
  assert.deepEqual(refreshes, [[false, true]]);

  const toasts = [];
  const navigating = {
    routes: [{ id: "profiles" }, { id: "activity" }], route: "profiles",
    profileSaveState: "saving", profileConnectionState: "idle",
    toast(title, message, error) { toasts.push({ title, message, error }); },
    closeProfile() { assert.fail("an active save must not hide the editor"); }
  };
  component.methods.navigate.call(navigating, "activity");
  assert.equal(navigating.route, "profiles");
  assert.equal(toasts[0].error, true);
});

test("path chooser is a contained keyboard dialog with focus restoration", () => {
  assert.match(appSource, /ref="pathBrowserDialog"[\s\S]*?role="dialog"[\s\S]*?aria-modal="true"/);
  assert.match(appSource, /document\.addEventListener\("keydown", this\.pathBrowserKeyHandler, true\)/);
  assert.match(appSource, /event\.key === "Escape"[\s\S]*?this\.closePathBrowser\(\)/);
  assert.match(appSource, /pathBrowserFocusables\(\)[\s\S]*?event\.key !== "Tab"/);
  assert.match(appSource, /priorFocus && priorFocus\.isConnected && priorFocus\.focus/);

  const component = loadAppComponent();
  let closed = 0;
  const escape = {
    pathBrowser: { visible: true },
    closePathBrowser() { closed += 1; }
  };
  const event = {
    key: "Escape", preventDefault() {}, stopPropagation() {}
  };
  component.methods.handlePathBrowserKeydown.call(escape, event);
  assert.equal(closed, 1);
});
