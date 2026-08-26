import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appUrl = new URL("../src/App.vue", import.meta.url);
const panelUrl = new URL("../src/SecurityPanel.vue", import.meta.url);
const appConfigUrl = new URL("../app.config", import.meta.url);
const apiUrl = new URL("../src/api.js", import.meta.url);
const packageUrl = new URL("../package.json", import.meta.url);
const helpRoot = new URL("../../package/ui/", import.meta.url);
const cargoUrl = new URL("../../../../Cargo.toml", import.meta.url);
const cargoLockUrl = new URL("../../../../Cargo.lock", import.meta.url);
const infoUrl = new URL("../../INFO.template", import.meta.url);
const dsmUiNoticeUrl = new URL("../../licenses/DSM_UI_THIRD_PARTY_LICENSES.txt", import.meta.url);
const nodeModulesRoot = new URL("../node_modules/", import.meta.url);

const app = await readFile(appUrl, "utf8");
const panel = await readFile(panelUrl, "utf8");
const apiSource = await readFile(apiUrl, "utf8");
const appConfig = JSON.parse(await readFile(appConfigUrl, "utf8"));
const uiPackage = JSON.parse(await readFile(packageUrl, "utf8"));
const cargo = await readFile(cargoUrl, "utf8");
const cargoLock = await readFile(cargoLockUrl, "utf8");
const info = await readFile(infoUrl, "utf8");
const dsmUiNotice = await readFile(dsmUiNoticeUrl, "utf8");
const toc = JSON.parse(await readFile(new URL("helptoc.conf", helpRoot), "utf8"));
const strings = await readFile(new URL("texts/enu/strings", helpRoot), "utf8");

const APP_CLASS = "SYNO.SDS.App.SynologyDriveSync.Instance";
const HELP_PAGES = [
  "overview", "profiles", "routines", "health", "activity", "notifications", "security", "settings", "about"
];

function componentTags(source) {
  return source.match(/<v-(?:input|single-select|checkbox)\b[^>]*>/g) || [];
}

async function loadAppComponent(postSpy, trace) {
  const script = app.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "App.vue script block is missing");
  let executable = script[1]
    .replace(/import\s*\{[\s\S]*?\}\s*from\s*"\.\/api";\s*/, "")
    .replace(/import\s+SecurityPanel\s+from\s+"\.\/SecurityPanel\.vue";\s*/, "")
    .replace("export default {", "const AppComponent = {");
  executable += "\nreturn AppComponent;";

  const stubs = {
    ACTIONS: {
      securityPolicy: "security-policy",
      clientEvent: "client-event",
      execute: "action"
    },
    MAX_RESPONSE_BYTES: 1024 * 1024,
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    apiGet: async () => ({}),
    apiPost: async (...args) => {
      trace.push("post");
      return postSpy(...args);
    },
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" && value ? value : fallback).slice(0, 65536),
    formatBytes: String,
    formatDate: String,
    formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    SecurityPanel: {}
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

test("native AppWindow title is literal and contextual DSM Help covers every route", async () => {
  assert.deepEqual(Object.keys(appConfig), [APP_CLASS]);
  assert.equal(appConfig[APP_CLASS].type, "app");
  assert.equal(appConfig[APP_CLASS].title, "Synology Drive Sync");
  assert.equal(appConfig[APP_CLASS].appWindow, APP_CLASS);
  assert.equal(appConfig[APP_CLASS].url, undefined);

  assert.match(app, /<v-app-window[\s\S]{0,180}?title="Synology Drive Sync"/);
  assert.doesNotMatch(app, /:title="windowTitle"|APP_TITLE_FALLBACK|resolvedWindowTitle/);
  assert.equal((app.match(/<h1\s+id="sdsync-page-title"/g) || []).length, 1);
  assert.equal((app.match(/aria-labelledby="sdsync-page-title"/g) || []).length, HELP_PAGES.length);
  assert.doesNotMatch(app, /control plane/i);
  assert.doesNotMatch(app, /type="round-border"/);

  for (const page of HELP_PAGES) {
    assert.match(app, new RegExp(`${page}: "${page}\\.html"`));
  }
  assert.match(app, /const HELP_APPLICATION = "SYNO\.SDS\.HelpBrowser\.Application";/);
  assert.match(app, /SYNO\.SDS\.AppLaunch/);
  assert.match(app, /content: HELP_CONTENT\[this\.route\] \|\| HELP_CONTENT\.overview/);
});

test("official DSM Help tree and every contextual page are staged from local assets", async () => {
  assert.deepEqual(toc, {
    app: APP_CLASS,
    title: "app:title",
    content: "overview.html",
    toc: HELP_PAGES.map((page) => ({ title: `help:${page}`, content: `${page}.html` }))
  });
  for (const page of HELP_PAGES) {
    const document = await readFile(new URL(`help/enu/${page}.html`, helpRoot), "utf8");
    for (const marker of [
      '<html class="img-no-display">',
      '../../../../help/help.css',
      '../../../../help/scrollbar/flexcroll.css',
      '../../../../help/scrollbar/flexcroll.js',
      '../../../../help/scrollbar/initFlexcroll.js',
      "<h1>",
      "<h2>"
    ]) assert.ok(document.includes(marker), `${page}.html lacks ${marker}`);
    const remoteTags = document.match(/<[^>]+(?:href|src)=["'](?:https?:)?\/\/[^>]*>/gi) || [];
    if (page !== "about") assert.equal(remoteTags.length, 0, `${page}.html contains a remote reference`);
    for (const tag of remoteTags) {
      assert.match(tag, /^<a\b/i);
      assert.match(tag, /href=["']https:\/\//i);
      assert.match(tag, /target=["']_blank["']/i);
      assert.match(tag, /rel=["'][^"']*\bnoopener\b[^"']*\bnoreferrer\b[^"']*["']/i);
    }
    assert.match(strings, new RegExp(`^${page}="[^"\\r\\n]+"$`, "m"));
  }
  const security = await readFile(new URL("help/enu/security.html", helpRoot), "utf8");
  for (const marker of [
    "Require HTTPS", "Interface changes", "Profile and secret changes", "Empty source",
    "CSRF lifetime", "60 through 900", "Result retention", "300 through 86400",
    "Maximum outstanding jobs", "1 through 256", "Mandatory minimal action audit"
  ]) assert.ok(security.includes(marker), `security help lacks ${marker}`);
  const settingsHelp = await readFile(new URL("help/enu/settings.html", helpRoot), "utf8");
  for (const marker of [
    "writes the candidate browser preference first",
    "no audit event is submitted",
    "restores the exact prior browser value",
    "validated client request and queued job correlation"
  ]) assert.ok(settingsHelp.includes(marker), `settings help lacks ${marker}`);
});

test("About metadata, dependency versions, and update links match repository sources", async () => {
  const cargoPackage = cargo.match(/^\[package\]\r?\n([\s\S]*?)(?=^\[)/m);
  assert.ok(cargoPackage, "Cargo package metadata is missing");
  const cargoValue = (key) => {
    const match = cargoPackage[1].match(new RegExp(`^${key}\\s*=\\s*"([^"]+)"`, "m"));
    assert.ok(match, `Cargo package field ${key} is missing`);
    return match[1];
  };
  const maintainer = info.match(/^maintainer="([^"]+)"$/m)?.[1];
  const maintainerUrl = info.match(/^maintainer_url="([^"]+)"$/m)?.[1];
  assert.equal(maintainer, "supermarsx");
  for (const [field, value] of [
    ["project", cargoValue("name")],
    ["author", "Mariana"],
    ["authorUrl", "https://github.com/supermarsx"],
    ["maintainer", maintainer],
    ["maintainerUrl", maintainerUrl],
    ["repository", cargoValue("repository")],
    ["license", cargoValue("license")],
    ["coreVersion", cargoValue("version")],
    ["uiVersion", uiPackage.version]
  ]) {
    assert.match(app, new RegExp(`${field}: "${value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}"`));
  }
  assert.match(app, /apiSchema: SNAPSHOT_SCHEMA/);
  assert.match(app, /installedPackageVersion\(\) \{ return boundedText\(this\.snapshot && this\.snapshot\.package && this\.snapshot\.package\.version, "Not reported by package API"\); \}/);

  const lockedCargoVersions = new Map();
  for (const block of cargoLock.split(/(?=^\[\[package\]\]\r?$)/m)) {
    const name = block.match(/^name\s*=\s*"([^"]+)"\r?$/m)?.[1];
    const version = block.match(/^version\s*=\s*"([^"]+)"\r?$/m)?.[1];
    if (!name || !version) continue;
    const versions = lockedCargoVersions.get(name) || [];
    versions.push(version);
    lockedCargoVersions.set(name, versions);
  }
  const lockedCargoVersion = (name) => {
    const versions = lockedCargoVersions.get(name) || [];
    assert.equal(versions.length, 1, `direct Rust dependency ${name} must have one Cargo.lock resolution`);
    return versions[0];
  };

  const parseCargoSection = (body, scope) => {
    const parsed = [];
    const declarations = body.matchAll(/^([A-Za-z0-9_-]+)\s*=\s*(?:"([^"]+)"|\{[^\r\n]*?\bversion\s*=\s*"([^"]+)")/gm);
    for (const declaration of declarations) {
      const name = declaration[1];
      parsed.push([name, [lockedCargoVersion(name), scope, `https://crates.io/crates/${name}`]]);
    }
    return parsed;
  };
  const commonSection = cargo.match(/^\[dependencies\]\r?\n([\s\S]*?)(?=^\[|(?![\s\S]))/m);
  assert.ok(commonSection, "Cargo direct dependency section is missing");
  const rustDependencies = new Map(parseCargoSection(commonSection[1], "All platforms"));
  const targetScopes = { windows: "Windows", macos: "macOS", linux: "Linux" };
  const targetSections = cargo.matchAll(/^\[target\.'cfg\(target_os = "([^"]+)"\)'\.dependencies\]\r?\n([\s\S]*?)(?=^\[|(?![\s\S]))/gm);
  const foundTargets = new Set();
  for (const targetSection of targetSections) {
    const target = targetSection[1];
    assert.ok(targetScopes[target], `Unexpected Cargo target dependency scope ${target}`);
    foundTargets.add(target);
    for (const [name, values] of parseCargoSection(targetSection[2], targetScopes[target])) {
      assert.ok(!rustDependencies.has(name), `Duplicate direct Rust dependency ${name}`);
      rustDependencies.set(name, values);
    }
  }
  assert.deepEqual([...foundTargets].sort(), Object.keys(targetScopes).sort());

  const uiDependencies = new Map(Object.entries(uiPackage.devDependencies).map(([name, version]) => [
    name,
    [version, "devDependency", `https://www.npmjs.com/package/${name}`]
  ]));
  const packageManagerName = uiPackage.packageManager.split("@", 1)[0];
  uiDependencies.set(packageManagerName, [uiPackage.packageManager, "packageManager", "https://pnpm.io/"]);

  const appCatalog = (constant) => {
    const block = app.match(new RegExp(`const ${constant} = Object\\.freeze\\(\\[([\\s\\S]*?)\\]\\);`));
    assert.ok(block, `${constant} is missing`);
    const entries = [...block[1].matchAll(/\{ name: "([^"]+)", pin: "([^"]+)", scope: "([^"]+)", url: "([^"]+)" \}/g)];
    assert.equal(entries.length, new Set(entries.map((entry) => entry[1])).size, `${constant} has duplicate packages`);
    return new Map(entries.map((entry) => [entry[1], [entry[2], entry[3], entry[4]]]));
  };
  assert.deepEqual(appCatalog("ABOUT_RUST_DEPENDENCIES"), rustDependencies);
  assert.deepEqual(appCatalog("ABOUT_UI_DEPENDENCIES"), uiDependencies);
  assert.match(app, /Exact direct versions resolved by the frozen <code>Cargo\.lock<\/code>/);

  const aboutHelp = await readFile(new URL("help/enu/about.html", helpRoot), "utf8");
  assert.match(aboutHelp, />Mariana<\/a>/);
  const helpEntries = [...aboutHelp.matchAll(/<li data-package="([^"]+)" data-version="([^"]+)" data-scope="([^"]+)"><a href="([^"]+)"[^>]*>([^<]+)<\/a> — ([^<]+) — <code>([^<]+)<\/code><\/li>/g)];
  assert.equal(helpEntries.length, new Set(helpEntries.map((entry) => entry[1])).size, "About Help has duplicate packages");
  for (const entry of helpEntries) assert.deepEqual([entry[5], entry[6], entry[7]], [entry[1], entry[3], entry[2]]);
  assert.deepEqual(
    new Map(helpEntries.map((entry) => [entry[1], [entry[2], entry[3], entry[4]]])),
    new Map([...rustDependencies, ...uiDependencies])
  );
  assert.match(aboutHelp, /exact version resolved by the frozen <code>Cargo\.lock<\/code>/);
  for (const document of [app, aboutHelp]) {
    assert.ok(document.includes("complete transitive Rust release-dependency license inventory"));
    assert.ok(document.includes("DSM_UI_THIRD_PARTY_LICENSES.txt"));
    assert.ok(document.includes("Vue is supplied by DSM and is not bundled"));
    assert.ok(document.includes("other pnpm packages whose code is not named in that notice are used only during the build"));
  }
  assert.ok(!aboutHelp.includes("complete transitive license inventory ships as"));

  const normalizedNotice = dsmUiNotice.replace(/\r\n?/g, "\n").trim();
  for (const name of ["vue-loader", "webpack"]) {
    const pin = uiPackage.devDependencies[name];
    assert.match(pin, /^\d+\.\d+\.\d+$/, `${name} must have an exact package.json pin`);
    const installedMetadata = JSON.parse(
      await readFile(new URL(`${name}/package.json`, nodeModulesRoot), "utf8")
    );
    assert.equal(installedMetadata.version, pin, `${name} installed version differs from package.json`);
    const installedLicense = (
      await readFile(new URL(`${name}/LICENSE`, nodeModulesRoot), "utf8")
    ).replace(/\r\n?/g, "\n").trim();
    assert.ok(normalizedNotice.includes(`${name} ${pin}`), `${name} exact pin is missing from DSM UI notices`);
    assert.ok(normalizedNotice.includes(installedLicense), `${name} complete license text is missing from DSM UI notices`);
  }
  for (const marker of [
    "lib/runtime/componentNormalizer.js",
    "lib/runtime/CompatGetDefaultExportRuntimeModule.js",
    "lib/runtime/DefinePropertyGettersRuntimeModule.js",
    "lib/runtime/HasOwnPropertyRuntimeModule.js",
    "Sergey Melyukov (@smelukov)"
  ]) assert.ok(normalizedNotice.includes(marker), `DSM UI notices lack bundled-code marker ${marker}`);

  for (const marker of [
    "https://github.com/supermarsx/synology-drive-sync/releases",
    "https://supermarsx.github.io/synology-drive-sync/release-selector.html",
    "Package Center <strong>Manual Install</strong>",
    "does not fetch or install updates",
    "does not configure Package Source discovery"
  ]) assert.ok(app.includes(marker), `About route lacks ${marker}`);
  const appAnchors = app.match(/<a\b[^>]*>/g) || [];
  assert.ok(appAnchors.length >= 8);
  for (const anchor of appAnchors) {
    assert.match(anchor, /target="_blank"/);
    assert.match(anchor, /rel="noopener noreferrer"/);
  }
  assert.doesNotMatch(app, /fetch\s*\(\s*["']https?:\/\//i);
});

test("operational pages contain no marketing hero, placeholder card, or Help-only filler card", () => {
  for (const forbidden of [
    "Your sync estate, at a glance.", "sdsync-hero", "sdsync-check-grid",
    "sdsync-editor-placeholder", "Select a profile or create one",
    "Fixed, non-secret messages", "sdsync-section-heading"
  ]) assert.ok(!app.includes(forbidden), `App.vue retains filler marker ${forbidden}`);
  assert.match(app, /class="sdsync-overview-status" aria-label="Service status and actions"/);
  assert.match(app, /<span>Service<\/span>[\s\S]*?\{\{ serviceState \}\}/);
  assert.match(app, />Plan all profiles<\/v-button>/);
  assert.match(app, />Run all profiles<\/v-button>/);
});

test("configurable DSM controls use real accessible help markup and themed portal menus", () => {
  for (const [name, source] of [["App.vue", app], ["SecurityPanel.vue", panel]]) {
    const tags = componentTags(source);
    assert.ok(tags.length > 0, `${name} has no DSM controls`);
    for (const tag of tags) {
      assert.match(tag, /:?(?:aria-describedby)="[^"]+"/, `${name} control lacks aria-describedby: ${tag}`);
      assert.doesNotMatch(tag, /\stooltip=/, `${name} relies on an unsupported input tooltip prop: ${tag}`);
      if (tag.startsWith("<v-single-select")) {
        assert.match(tag, /:custom-dropdown-cls="'sdsync-select-dropdown ' \+ themeClass"/);
      }
    }
  }

  const described = new Set([...app.matchAll(/aria-describedby="sdsync-help-([a-z0-9-]+)"/g)].map((match) => match[1]));
  const helpKeys = new Set([...app.matchAll(/<control-help\b[^>]*help-key="([a-z0-9-]+)"/g)].map((match) => match[1]));
  for (const id of described) assert.ok(helpKeys.has(id), `missing ControlHelp for ${id}`);
  for (const source of [app, panel]) {
    assert.match(source, /<button type="button" class="sdsync-field-tip-trigger"/);
    assert.match(source, /role="tooltip"/);
    assert.match(source, /:title="text"/);
    assert.match(source, /@keydown\.esc="\$event\.currentTarget\.blur\(\)"/);
  }
  assert.match(app, /fieldset class="sdsync-weekday-fieldset" aria-describedby="sdsync-help-routine-weekdays"/);
  for (const truthfulProducer of [
    "accepted and rejected dashboard mutation bridge events",
    "authenticated identity events emitted for accepted dashboard mutations",
    "security-policy changes and policy or security mutation rejections",
    "controller queue-processing and lifecycle diagnostics"
  ]) assert.ok(panel.includes(truthfulProducer), `SecurityPanel tooltip lacks ${truthfulProducer}`);
  assert.doesNotMatch(panel, /CGI, socket, queue, and response|authentication and authorization diagnostics/);
});

test("complete security policy, client-event auditing, activity filters, and stale evidence are wired", async () => {
  const api = await import(`data:text/javascript;base64,${Buffer.from(apiSource).toString("base64")}#${Date.now()}`);
  const policyKeys = api.ARGUMENT_KEYS[api.ACTIONS.securityPolicy];
  assert.equal(policyKeys.length, 28);
  for (const key of [
    "allow_empty_source", "allow_destructive_sync", "allow_doctor_write_test",
    "allow_http_targets", "allow_invalid_tls", "allow_remote_logging",
    "csrf_lifetime_seconds", "result_retention_seconds", "max_outstanding_jobs"
  ]) assert.ok(policyKeys.includes(key));

  for (const marker of [
    "securityPayload()", "validateSecurityPayload(payload)",
    "ACTIONS.securityPolicy, payload", "ACTIONS.clientEvent, { event: \"interface-settings\" }",
    "ACTIONS.clientEvent, { event: \"session-notifications\" }",
    "canChangeProfiles", "canChangeRoutines", "canChangeNotifications", "canRunOperations",
    "activityCategory", "activityLevel", '["audit", "Audit"]',
    "Stale · last successful snapshot retained", "this.snapshot ?"
  ]) assert.ok(app.includes(marker), `App.vue lacks ${marker}`);
  assert.doesNotMatch(app, /this\.snapshot\s*=\s*null/);
  assert.match(app, />Retry<\/v-button>/);
  assert.match(panel, /:disabled="disabled \|\| busy \|\| !dirty"/);
  assert.match(panel, /@input="updateField\(control\.key, \$event === true\)"/);
  assert.match(panel, /<v-form-item label="Policy version">[\s\S]*?:value="policyVersionLabel"[\s\S]*?readonly/);
  assert.match(panel, /updates are managed only by package migrations/);
  assert.match(app, /policy_version: null/);

  const settingsSection = app.match(/<section v-else-if="route === 'settings'"[\s\S]*?<\/section>/);
  assert.ok(settingsSection);
  assert.match(settingsSection[0], /<v-form/);
  assert.doesNotMatch(settingsSection[0], /<article\b/);
});

test("security save and browser preference saves are behaviorally audited", async () => {
  const calls = [];
  const trace = [];
  const component = await loadAppComponent(async (_auth, _csrf, action, payload) => {
    calls.push({ action, payload });
    return { ok: true };
  }, trace);
  const methods = component.methods;
  const basePolicy = {
    policy_version: 1,
    require_https: false,
    allow_interface_changes: true,
    allow_profile_changes: true,
    allow_secret_changes: true,
    allow_routine_changes: true,
    allow_notification_changes: true,
    allow_operational_actions: true,
    allow_http_targets: true,
    allow_empty_source: true,
    allow_invalid_tls: true,
    allow_destructive_sync: true,
    allow_doctor_write_test: true,
    allow_remote_logging: true,
    csrf_lifetime_seconds: 300,
    result_retention_seconds: 3600,
    max_outstanding_jobs: 64,
    log_levels: Object.fromEntries([
      "audit", "bridge", "authentication", "security", "configuration", "secrets",
      "routines", "operations", "notifications", "sync", "controller", "scheduler"
    ].map((category) => [category, "info"]))
  };
  const context = {
    canMutate: true,
    securityDirty: true,
    operationBusy: false,
    disposed: false,
    auth: {},
    csrfToken: "csrf",
    securityForm: structuredClone(basePolicy),
    securityPolicy: structuredClone(basePolicy),
    connected: true,
    toasts: [],
    toast(title, message, error) { this.toasts.push({ title, message, error }); },
    confirmAction: async () => true,
    refreshSnapshot: async () => {},
    hydrateSecurityPolicy: () => {}
  };
  for (const method of [
    "between", "securityPayload", "validateSecurityPayload", "securityRelaxed",
    "saveSecurityPolicy", "reportMutationError"
  ]) context[method] = (...args) => methods[method].apply(context, args);
  await context.saveSecurityPolicy({ preventDefault() {} });
  assert.equal(calls[0].action, "security-policy");
  assert.equal(Object.keys(calls[0].payload).length, 28);
  assert.equal(Object.prototype.hasOwnProperty.call(calls[0].payload, "policy_version"), false);
  assert.equal(calls[0].payload.allow_empty_source, true);
  assert.equal(context.securityDirty, false);
  assert.equal(context.operationBusy, false);

  const previousWindow = globalThis.window;
  const stored = [];
  let storedRaw = null;
  globalThis.window = {
    localStorage: {
      getItem: () => storedRaw,
      setItem: (key, value) => { trace.push("persist"); stored.push({ key, value }); storedRaw = value; },
      removeItem: () => { storedRaw = null; }
    },
    Notification: null,
    clearTimeout() {},
    setTimeout() {}
  };
  try {
    const preference = {
      canChangeInterface: true,
      canChangeNotifications: true,
      operationBusy: false,
      disposed: false,
      auth: {},
      csrfToken: "csrf",
      settings: { theme: "dark", status_refresh: 5000, log_refresh: 5000, desktop_notifications: false, audible: false },
      notificationForm: { desktop_notifications: false, audible: true },
      toast() {},
      scheduleSnapshot() {},
      scheduleLogs() {},
      reportMutationError() {}
    };
    for (const method of [
      "persistSettings", "captureSettingsTransaction", "applySettingsState",
      "restoreSettingsTransaction", "preferenceAuditWasRejected",
      "saveInterfaceSettings", "saveNotificationPreferences"
    ]) {
      preference[method] = (...args) => methods[method].apply(preference, args);
    }
    await preference.saveInterfaceSettings({ preventDefault() {} });
    await preference.saveNotificationPreferences({ preventDefault() {} });
    assert.deepEqual(calls.slice(1).map((call) => call.payload.event), ["interface-settings", "session-notifications"]);
    assert.equal(stored.length, 2);
    assert.deepEqual(trace.slice(-4), ["persist", "post", "persist", "post"]);
  } finally {
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
  }
});

test("browser preference persistence and audit ordering preserve truthful outcomes", async () => {
  const previousWindow = globalThis.window;
  const prior = {
    theme: "dark",
    status_refresh: 5000,
    log_refresh: 5000,
    desktop_notifications: false,
    audible: false
  };
  const priorRaw = `{\n  "theme": "dark",\n  "status_refresh": 5000,\n  "log_refresh": 5000,\n  "desktop_notifications": false,\n  "audible": false,\n  "preserve_exactly": true\n}`;

  function preferenceContext(component, overrides = {}) {
    const methods = component.methods;
    const context = Object.assign({
      canChangeInterface: true,
      canChangeNotifications: true,
      operationBusy: false,
      disposed: false,
      auth: {},
      csrfToken: "csrf",
      settings: Object.assign({}, prior),
      notificationForm: { desktop_notifications: false, audible: false },
      toasts: [],
      toast(title, message, error = false) { this.toasts.push({ title, message, error }); },
      scheduleSnapshot() {},
      scheduleLogs() {}
    }, overrides);
    for (const method of [
      "persistSettings", "captureSettingsTransaction", "applySettingsState",
      "restoreSettingsTransaction", "preferenceAuditWasRejected",
      "saveInterfaceSettings", "saveNotificationPreferences", "reportMutationError"
    ]) context[method] = (...args) => methods[method].apply(context, args);
    return context;
  }

  try {
    let postCount = 0;
    const storageFailureComponent = await loadAppComponent(async () => {
      postCount += 1;
      return { ok: true };
    }, []);
    globalThis.window = {
      localStorage: {
        getItem: () => priorRaw,
        setItem: () => { throw new Error("quota unavailable"); },
        removeItem() {}
      },
      Notification: null,
      clearTimeout() {},
      setTimeout() {}
    };
    const storageFailure = preferenceContext(storageFailureComponent, {
      settings: Object.assign({}, prior, { theme: "light", status_refresh: 10000 })
    });
    await storageFailure.saveInterfaceSettings({ preventDefault() {} });
    assert.equal(postCount, 0, "failed local persistence must not submit a success audit event");
    assert.deepEqual(storageFailure.settings, prior);
    assert.equal(storageFailure.operationBusy, false);
    assert.equal(storageFailure.toasts.at(-1).title, "Preferences not persisted");

    const rejectedRequestId = "1".repeat(32);
    const rejectedError = Object.assign(new Error("The package rejected the preference audit."), {
      preAcceptance: true,
      trustedRejection: true,
      requestId: rejectedRequestId,
      trustedRequestId: true
    });
    const rejectedComponent = await loadAppComponent(async () => { throw rejectedError; }, []);
    let rejectedRaw = priorRaw;
    const rejectedWrites = [];
    globalThis.window = {
      localStorage: {
        getItem: () => rejectedRaw,
        setItem: (_key, value) => { rejectedWrites.push(value); rejectedRaw = value; },
        removeItem: () => { rejectedWrites.push(null); rejectedRaw = null; }
      },
      Notification: null,
      clearTimeout() {},
      setTimeout() {}
    };
    const rejected = preferenceContext(rejectedComponent, {
      notificationForm: { desktop_notifications: false, audible: true }
    });
    await rejected.saveNotificationPreferences({ preventDefault() {} });
    assert.equal(rejectedWrites.length, 2, "candidate write must be followed by one exact rollback write");
    assert.notEqual(rejectedWrites[0], priorRaw);
    assert.equal(rejectedWrites[1], priorRaw);
    assert.equal(rejectedRaw, priorRaw);
    assert.deepEqual(rejected.settings, prior);
    assert.deepEqual(rejected.notificationForm, { desktop_notifications: false, audible: false });
    assert.equal(rejected.toasts.at(-1).title, "Session preferences not saved");
    assert.match(rejected.toasts.at(-1).message, new RegExp(rejectedRequestId));

    const unknownRequestId = "2".repeat(32);
    const unknownError = Object.assign(new Error("DSM may have accepted the preference audit."), {
      outcomeUnknown: true,
      acceptanceUnknown: true,
      requestId: unknownRequestId,
      trustedRequestId: true
    });
    const unknownComponent = await loadAppComponent(async () => { throw unknownError; }, []);
    let unknownRaw = priorRaw;
    const unknownWrites = [];
    globalThis.window = {
      localStorage: {
        getItem: () => unknownRaw,
        setItem: (_key, value) => { unknownWrites.push(value); unknownRaw = value; },
        removeItem: () => { unknownWrites.push(null); unknownRaw = null; }
      },
      Notification: null,
      clearTimeout() {},
      setTimeout() {}
    };
    const unknown = preferenceContext(unknownComponent, {
      settings: Object.assign({}, prior, { theme: "light", status_refresh: 10000 })
    });
    await unknown.saveInterfaceSettings({ preventDefault() {} });
    assert.equal(unknownWrites.length, 1, "outcome-unknown must retain the candidate without rollback");
    assert.equal(JSON.parse(unknownRaw).theme, "light");
    assert.equal(unknown.settings.theme, "light");
    assert.equal(unknown.settings.status_refresh, 10000);
    assert.equal(unknown.toasts.at(-1).title, "Interface settings stored · audit outcome unknown");
    assert.match(unknown.toasts.at(-1).message, new RegExp(unknownRequestId));
  } finally {
    if (previousWindow === undefined) delete globalThis.window;
    else globalThis.window = previousWindow;
  }
});

test("security policy version is normalized from snapshots but never enters mutations", async () => {
  const component = await loadAppComponent(async () => ({ ok: true }), []);
  const source = {
    policy_version: 7,
    require_https: true,
    csrf_lifetime_seconds: 300,
    result_retention_seconds: 3600,
    max_outstanding_jobs: 64,
    log_levels: {}
  };
  const normalized = component.computed.securityPolicy.call({
    snapshot: { security_policy: source }
  });
  assert.equal(normalized.policy_version, 7);
  assert.equal(source.policy_version, 7);

  const context = {
    securityForm: normalized
  };
  const payload = component.methods.securityPayload.call(context);
  assert.equal(Object.keys(payload).length, 28);
  assert.equal(Object.prototype.hasOwnProperty.call(payload, "policy_version"), false);
  assert.deepEqual(Object.keys(payload).sort(), [...(await import(`data:text/javascript;base64,${Buffer.from(apiSource).toString("base64")}#policy-version`)).ARGUMENT_KEYS["security-policy"]].sort());
});

test("Activity bounds messages, renders correlation, and searches text or request IDs", async () => {
  const component = await loadAppComponent(async () => ({ ok: true }), []);
  const requestId = "a".repeat(32);
  const activityEvents = [
    {
      epoch: 1,
      code: "legacy.event",
      profile: "legacy",
      state: "succeeded",
      category: "operations",
      level: "info",
      message: "Legacy records remain visible."
    },
    {
      epoch: 2,
      code: "audit.succeeded",
      profile: "office",
      state: "succeeded",
      category: "audit",
      level: "info",
      message: `Configuration changed ${"x".repeat(3000)}`,
      client_request_id: requestId
    },
    {
      epoch: 3,
      code: "bridge.rejected",
      profile: "none",
      state: "rejected",
      category: "bridge",
      level: "warn",
      message: "Malformed correlation was suppressed.",
      client_request_id: "<script>not-a-request-id</script>"
    }
  ];
  const filtered = (activitySearch = "", activityCategory = "all", activityLevel = "all") => (
    component.computed.reversedActivity.call({
      activityEvents,
      activitySearch,
      activityCategory,
      activityLevel
    })
  );

  const all = filtered();
  assert.equal(all.length, 3, "legacy records without correlation remain usable");
  assert.equal(all.find((event) => event.code === "legacy.event").client_request_id, "");
  assert.equal(all.find((event) => event.code === "bridge.rejected").client_request_id, "");
  assert.equal(all.find((event) => event.code === "audit.succeeded").message.length, 2048);
  assert.deepEqual(filtered("configuration changed").map((event) => event.code), ["audit.succeeded"]);
  assert.deepEqual(filtered(requestId).map((event) => event.code), ["audit.succeeded"]);
  assert.deepEqual(filtered("", "audit", "info").map((event) => event.code), ["audit.succeeded"]);
  assert.equal(filtered("<script>").length, 0);

  assert.match(app, /placeholder="Search event text or request ID"/);
  assert.match(app, /\{\{ event\.message \}\}/);
  assert.match(app, /Client request ID: \{\{ event\.client_request_id \}\}/);
  assert.doesNotMatch(app, /v-html/);
});
