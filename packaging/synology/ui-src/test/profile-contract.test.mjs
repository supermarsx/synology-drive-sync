import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
const bridgeSource = await readFile(new URL("../../../../src/dsm_api.rs", import.meta.url), "utf8");
const configSource = await readFile(new URL("../../../../src/config.rs", import.meta.url), "utf8");

async function loadApi() {
  return import(`data:text/javascript;base64,${Buffer.from(apiSource).toString("base64")}#${Date.now()}-${Math.random()}`);
}

function loadAppComponent(postSpy = async () => ({ ok: true })) {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "App.vue script block is missing");
  let executable = script[1]
    .replace(/import\s*\{[\s\S]*?\}\s*from\s*"\.\/api";\s*/, "")
    .replace(/import\s+SecurityPanel\s+from\s+"\.\/SecurityPanel\.vue";\s*/, "")
    .replace("export default {", "const AppComponent = {");
  executable += "\nreturn AppComponent;";
  const stubs = {
    ACTIONS: {
      configureProfile: "configure-profile", removeProfile: "remove-profile",
      setDefault: "set-default", setSecret: "set-secret", schedule: "schedule",
      routine: "routine", removeRoutine: "remove-routine", alertPolicy: "alert-policy",
      securityPolicy: "security-policy", clientEvent: "client-event", execute: "action"
    },
    MAX_RESPONSE_BYTES: 1024 * 1024,
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    apiGet: async () => ({}), apiPost: postSpy,
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback).slice(0, 65536),
    formatBytes: String, formatDate: String, formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    createAutosaveCoordinator: () => ({
      cancel() {}, dispose() {}, getState: () => ({ registered: false, dirty: false }),
      hydrate() {}, setGlobalBusy() {}, setScopeBlocked() {}, update: () => ({ dirty: false })
    }),
    installControlLayout: () => () => {},
    ActionIcon: { name: "ActionIcon" }, SecurityPanel: {}
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

function bind(context, methods, names) {
  for (const name of names) context[name] = (...args) => methods[name].call(context, ...args);
  return context;
}

function completeProfile(overrides = {}) {
  return {
    name: "nightly", source: "/volume1/source", url: "https://nas.example.invalid",
    username: "backup-user", remote: "/home/Drive/Backup", compare: "metadata", jobs: 4,
    delete: true, max_delete: 37, allow_http: false, allow_empty_source: true,
    excludes: ["@eaDir/", "**/@eaDir/", "#recycle/", "#snapshot/"], retries: 3,
    upload_timeout_seconds: 8000, connect_timeout_seconds: 45,
    max_rate_bytes_per_second: 1048576, ca_certificate: "/volume1/certs/ca.pem",
    danger_invalid_certs: false, verbosity: 1, quiet: false, log_level: "debug",
    log_format: "human", progress: "auto", output: "json",
    log_file: "/var/packages/synology-drive-sync/var/log/sync.log",
    remote_log_url: "https://logs.example.invalid/ingest", remote_log_mode: "required",
    default: true, has_password: true, has_totp: true, has_remote_log_token: true,
    ...overrides
  };
}

function validPayload(overrides = {}) {
  return {
    allow_empty_source: false, allow_http: false, ca_certificate: null, compare: "content",
    connect_timeout_seconds: 15, danger_accept_invalid_certs: false, delete: false,
    excludes: ["@eaDir/", "**/@eaDir/", "#recycle/", "#snapshot/"], jobs: 2,
    log_format: "json", log_level: "info", make_default: false, max_delete: 100,
    max_rate_bytes_per_second: null, name: "nightly", output: "human", progress: "never",
    quiet: false, remote: "/home/Drive/Backup", remote_log_mode: "best-effort",
    remote_log_url: null, retries: 2, source: "/volume1/source", timeout_seconds: 7200,
    url: "https://nas.example.invalid", username: "backup-user", verbosity: 0,
    ...overrides
  };
}

test("profile editor, browser API, and Rust bridge share one exact configurable field contract", async () => {
  const api = await loadApi();
  const block = bridgeSource.match(/struct ConfigureProfileArgs \{([\s\S]*?)\n\}/);
  assert.ok(block, "ConfigureProfileArgs is missing");
  const rustFields = Array.from(block[1].matchAll(/^\s+([a-z][a-z0-9_]*):/gm), (match) => match[1]).sort();
  assert.deepEqual([...api.ARGUMENT_KEYS["configure-profile"]].sort(), rustFields);

  const profileBlock = configSource.match(/pub struct Profile \{([\s\S]*?)\n\}/);
  assert.ok(profileBlock, "core Profile schema is missing");
  const coreFields = Array.from(profileBlock[1].matchAll(/^\s+pub ([a-z][a-z0-9_]*):/gm), (match) => match[1]);
  const mutationToCore = {
    timeout_seconds: "timeout", connect_timeout_seconds: "connect_timeout",
    max_rate_bytes_per_second: "max_rate", verbosity: "verbose"
  };
  const configuredCoreFields = new Set(rustFields
    .filter((field) => !["name", "make_default"].includes(field))
    .map((field) => mutationToCore[field] || field));
  assert.deepEqual(
    coreFields.filter((field) => !configuredCoreFields.has(field)).sort(),
    ["log_file", "no_vault", "password_file", "remote_log_token_env", "remote_log_token_file", "totp_secret_file"]
  );

  for (const field of rustFields) {
    assert.match(appSource, new RegExp(`(?:profileForm\\.|${field}:)[^\\n]*${field.replace("timeout_seconds", "timeout")}|${field}`));
  }
  for (const field of ["log_format", "progress", "output"]) {
    assert.match(appSource, new RegExp(`v-model="profileForm\\.${field}"`));
  }
  assert.match(appSource, /class="sdsync-readonly-value">\{\{ profileLogFile \}\}/);
  assert.doesNotMatch(apiSource, /"configure-profile"[\s\S]{0,700}"log_file"/);
  assert.doesNotMatch(apiSource, /"configure-profile"[\s\S]{0,700}"no_vault"/);
  for (const presence of ["has_password", "has_totp", "has_remote_log_token"]) {
    assert.match(appSource, new RegExp(`selectedProfileModel\\.${presence}`));
  }
  assert.doesNotMatch(appSource, /selectedProfileModel\.(?:password|totp|remote_log_token)\b/);
});

test("new profile defaults, explicit exclude clearing, snapshot hydration, and payload are lossless", async () => {
  const api = await loadApi();
  const component = loadAppComponent();
  const methods = component.methods;
  const context = bind({
    operationBusy: false, canChangeProfiles: true, profiles: [], selectedProfile: "",
    profileForm: {}, profileEditorOpen: false, secretModes: {}, secretValues: {},
    autosaveCoordinator: null
  }, methods, ["clearSecrets", "integer", "profilePayload", "hydrateAutosave"]);

  methods.openProfile.call(context, "");
  assert.equal(context.profileForm.excludes, "@eaDir/\n**/@eaDir/\n#recycle/\n#snapshot/");
  assert.deepEqual(
    [context.profileForm.log_format, context.profileForm.progress, context.profileForm.output],
    ["json", "never", "human"]
  );
  assert.equal(context.profileForm.max_delete, 100);

  Object.assign(context.profileForm, {
    name: "nightly", source: "/volume1/source", url: "https://nas.example.invalid",
    username: "backup-user", remote: "/home/Drive/Backup"
  });
  context.integer = (...args) => methods.integer.call(context, ...args);
  let payload = methods.profilePayload.call(context);
  assert.deepEqual(Object.keys(payload).sort(), [...api.ARGUMENT_KEYS["configure-profile"]].sort());
  assert.deepEqual(payload.excludes, ["@eaDir/", "**/@eaDir/", "#recycle/", "#snapshot/"]);
  assert.equal(payload.max_delete, 100);
  assert.equal(Object.keys(payload).some((key) => /password|totp|token|secret/.test(key)), false);

  context.profileForm.excludes = "";
  payload = methods.profilePayload.call(context);
  assert.deepEqual(payload.excludes, [], "clearing every line must remain an explicit supported action");

  const profile = completeProfile();
  context.profiles = [profile];
  methods.openProfile.call(context, profile.name);
  assert.deepEqual(
    {
      name: context.profileForm.name, source: context.profileForm.source,
      url: context.profileForm.url, username: context.profileForm.username,
      remote: context.profileForm.remote, log_format: context.profileForm.log_format,
      progress: context.profileForm.progress, output: context.profileForm.output,
      remote_log_url: context.profileForm.remote_log_url, remote_log_mode: context.profileForm.remote_log_mode
    },
    {
      name: profile.name, source: profile.source, url: profile.url, username: profile.username,
      remote: profile.remote, log_format: profile.log_format, progress: profile.progress,
      output: profile.output, remote_log_url: profile.remote_log_url,
      remote_log_mode: profile.remote_log_mode
    }
  );
  assert.equal(component.computed.profileLogFile.call({ selectedProfileModel: profile }), profile.log_file);
  assert.deepEqual(context.secretValues, { password: "", totp: "", remote_log_token: "" });
  assert.deepEqual(context.secretModes, { password: "keep", totp: "keep", remote_log_token: "keep" });
});

test("profile catalog-to-editor state transitions preserve filters and close safely", () => {
  const component = loadAppComponent();
  const methods = component.methods;
  const cancelled = [];
  const profile = completeProfile();
  const context = bind({
    operationBusy: false,
    canChangeProfiles: true,
    profiles: [profile],
    selectedProfile: "",
    profileForm: {},
    profileEditorOpen: false,
    profileFilter: "volume1",
    profileFilterStatus: "ready",
    secretModes: {},
    secretValues: {},
    autosaveCoordinator: null,
    cancelAutosave(scope) { cancelled.push(scope); }
  }, methods, ["clearSecrets", "integer", "profilePayload", "hydrateAutosave"]);

  methods.openProfile.call(context, "");
  assert.equal(context.profileEditorOpen, true, "New profile must leave the catalog for the editor view");
  assert.equal(context.selectedProfile, "");
  assert.deepEqual([context.profileFilter, context.profileFilterStatus], ["volume1", "ready"]);

  methods.closeProfile.call(context);
  assert.equal(context.profileEditorOpen, false, "Close must return to the catalog view");
  assert.equal(context.selectedProfile, "");
  assert.deepEqual(cancelled, ["profile"]);
  assert.deepEqual(context.secretValues, { password: "", totp: "", remote_log_token: "" });
  assert.deepEqual([context.profileFilter, context.profileFilterStatus], ["volume1", "ready"],
    "returning to the catalog must preserve the user's filters");

  methods.openProfile.call(context, profile.name);
  assert.equal(context.profileEditorOpen, true, "an existing profile must use the same dedicated editor view");
  assert.equal(context.selectedProfile, profile.name);
});

test("structured profile filters search useful fields and distinguish no matches from an empty catalog", () => {
  const component = loadAppComponent();
  const profiles = [
    completeProfile({ name: "alpha", source: "/volume1/source", has_password: true, default: true }),
    completeProfile({ name: "beta", source: "/volume2/media", has_password: false, default: false })
  ];
  const routines = [{ profile: "beta", enabled: true }];
  const filtered = (profileFilter, profileFilterStatus) => component.computed.filteredProfiles.call({
    profiles, routines, profileFilter, profileFilterStatus
  }).map((profile) => profile.name);

  assert.deepEqual(filtered("volume2", "all"), ["beta"]);
  assert.deepEqual(filtered("", "ready"), ["alpha"]);
  assert.deepEqual(filtered("", "needs-password"), ["beta"]);
  assert.deepEqual(filtered("", "default"), ["alpha"]);
  assert.deepEqual(filtered("", "automated"), ["beta"]);
  assert.deepEqual(filtered("does-not-exist", "all"), []);
  assert.match(
    appSource,
    /v-if="!filteredProfiles\.length"[^>]*>\{\{ profiles\.length \? 'No matching profiles\.' : 'No configured profiles\.' \}\}/
  );
});

test("profile validation mirrors target, path, safety, output, and remote logging bounds", () => {
  const component = loadAppComponent();
  const methods = component.methods;
  const context = bind({
    profileForm: { danger_invalid_confirm: true }, canAllowHttp: true, canAllowEmptySource: true,
    canAllowInvalidTls: true, canAllowDestructive: true, canAllowRemoteLogging: true,
    canManageSecrets: true, canReplaceRemoteLogToken: true
  }, methods, ["between", "validateSecretOperations"]);
  const validate = (payload, secrets = []) => methods.validateProfile.call(context, payload, secrets);

  assert.equal(validate(validPayload()), "");
  assert.match(validate(validPayload({ source: "volume1/source" })), /absolute NAS path/);
  assert.match(validate(validPayload({ source: "/volume1/../source" })), /dot segments/);
  assert.match(validate(validPayload({ url: "http://nas.example.invalid" })), /controlled-LAN exception/);
  assert.match(validate(validPayload({ remote: "/home//Backup" })), /Remote path/);
  assert.match(validate(validPayload({ ca_certificate: "relative/ca.pem" })), /CA certificate/);
  assert.match(validate(validPayload({ excludes: Array.from({ length: 65 }, (_, index) => `item-${index}`) })), /at most 64/);
  assert.match(validate(validPayload({ allow_empty_source: true, delete: false })), /requires deletion/);
  assert.match(validate(validPayload({ remote_log_mode: "required", remote_log_url: null })), /needs an HTTPS/);
  assert.equal(validate(validPayload({ output: "ndjson" })), "");
  assert.equal(validate(validPayload({ quiet: true, verbosity: 2 })), "");
  assert.match(validate(validPayload({ output: "xml" })), /newline-delimited JSON output/);
  assert.match(validate(validPayload({ progress: "sometimes" })), /supported progress/);
});

test("secret-only save is independent of profile edits and remote-token clear remains permitted", async () => {
  const posts = [];
  const component = loadAppComponent(async (_auth, _csrf, action, payload) => {
    posts.push({ action, payload });
    return { ok: true };
  });
  const methods = component.methods;
  function secretContext(secretModes, secretValues, canReplaceRemoteLogToken) {
    return bind({
      selectedProfile: "nightly", canManageSecrets: true, canChangeProfiles: false,
      canReplaceRemoteLogToken, operationBusy: false, disposed: false,
      auth: {}, csrfToken: "csrf", secretModes, secretValues, toasts: [], refreshes: 0,
      autosaveCoordinator: null, autosavePhase: "saved", autosaveMessage: "All changes saved",
      autosaveFailureScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
      autosaveOutcomeUnknownScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
      autosaveInspectionScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
      profileFailureRecords: {
        configuration: { active: false, outcomeUnknown: false, requiresInspection: false },
        secrets: {
          password: { active: false, outcomeUnknown: false, requiresInspection: false },
          totp: { active: false, outcomeUnknown: false, requiresInspection: false },
          "remote-log-token": { active: false, outcomeUnknown: false, requiresInspection: false }
        }
      },
      toast(title, message, error = false) { this.toasts.push({ title, message, error }); },
      confirmAction: async () => true,
      refreshSnapshot: async function () { this.refreshes += 1; return true; },
      reportMutationError() { throw new Error("unexpected mutation failure"); }
    }, methods, [
      "secretOperations", "validateSecretOperations", "clearSecrets", "refreshAutosaveStatus",
      "ensureProfileFailureRecords", "syncProfileFailureState", "clearProfileSecretFailures"
    ]);
  }

  const password = secretContext(
    { password: "replace", totp: "keep", remote_log_token: "keep" },
    { password: "fixture-password", totp: "", remote_log_token: "" },
    true
  );
  await methods.saveProfileSecrets.call(password, { preventDefault() {} });
  assert.deepEqual(posts.map((entry) => entry.action), ["set-secret"]);
  assert.deepEqual(posts[0].payload, {
    profile: "nightly", kind: "password", mode: "replace", value: "fixture-password"
  });
  assert.equal(posts.some((entry) => entry.action === "configure-profile"), false);
  assert.deepEqual(password.secretValues, { password: "", totp: "", remote_log_token: "" });
  assert.deepEqual(password.secretModes, { password: "keep", totp: "keep", remote_log_token: "keep" });

  const clearToken = secretContext(
    { password: "keep", totp: "keep", remote_log_token: "clear" },
    { password: "", totp: "", remote_log_token: "" },
    false
  );
  await methods.saveProfileSecrets.call(clearToken, { preventDefault() {} });
  assert.deepEqual(posts[1].payload, {
    profile: "nightly", kind: "remote-log-token", mode: "clear", value: null
  });

  const blockedReplace = secretContext(
    { password: "keep", totp: "keep", remote_log_token: "replace" },
    { password: "", totp: "", remote_log_token: "fixture-token" },
    false
  );
  await methods.saveProfileSecrets.call(blockedReplace, { preventDefault() {} });
  assert.equal(posts.length, 2);
  assert.match(blockedReplace.toasts[0].message, /may still clear/);
});

test("browser transport accepts the exact expanded profile payload and rejects drift before dispatch", async () => {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  const requests = [];
  globalThis.window = {
    crypto: globalThis.crypto || webcrypto, TextEncoder: globalThis.TextEncoder,
    setTimeout: globalThis.setTimeout, clearTimeout: globalThis.clearTimeout,
    fetch: async () => ({
      redirected: false, status: 200, ok: true,
      headers: { get: () => "application/json" },
      async text() { return JSON.stringify({ success: true, data: { synotoken: "token" } }); }
    })
  };
  globalThis.fetch = async (_url, options) => {
    const request = JSON.parse(options.body);
    requests.push(request);
    return {
      redirected: false, status: 200, ok: true,
      headers: { get: () => "application/json" },
      async text() {
        return JSON.stringify({
          schema: "sdsync.dsm-queued.v1", ok: true, state: "queued",
          request_id: request.request_id, job_id: "a".repeat(48)
        });
      }
    };
  };
  try {
    const api = await loadApi();
    const payload = validPayload({ log_format: "human", progress: "always", output: "ndjson" });
    await api.apiPost({}, "csrf", api.ACTIONS.configureProfile, payload, false);
    assert.deepEqual(requests[0].arguments, payload);
    await assert.rejects(
      api.apiPost({}, "csrf", api.ACTIONS.configureProfile, { ...payload, log_file: "/tmp/escape" }, false),
      /reviewed bridge contract/
    );
    assert.equal(requests.length, 1);
  } finally {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  }
});
