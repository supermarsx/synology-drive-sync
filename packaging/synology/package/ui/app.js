(function () {
  "use strict";

  const API_URL = "./api.cgi";
  const SNAPSHOT_SCHEMA = "sdsync.dsm-api.v1";
  const REQUEST_SCHEMA = "sdsync.dsm-request.v1";
  const QUEUED_SCHEMA = "sdsync.dsm-queued.v1";
  const RESULT_STATUS_SCHEMA = "sdsync.dsm-result-status.v1";
  const RESULT_SCHEMA = "sdsync.dsm-result.v1";
  const MAX_RESPONSE_BYTES = 1024 * 1024;
  const RESULT_POLL_ATTEMPTS = 240;
  const RESULT_POLL_INTERVAL_MS = 500;
  const SETTINGS_KEY = "sdsync.ui.settings.v1";
  const ROUTE_TITLES = Object.freeze({
    overview: "Overview",
    profiles: "Profiles",
    routines: "Routines",
    health: "Health / Doctor",
    activity: "Activity / Logs",
    notifications: "Notifications",
    settings: "Settings"
  });
  const ACTIONS = Object.freeze({
    configureProfile: "configure-profile",
    removeProfile: "remove-profile",
    setDefault: "set-default",
    setSecret: "set-secret",
    schedule: "schedule",
    routine: "routine",
    removeRoutine: "remove-routine",
    alertPolicy: "alert-policy",
    execute: "action"
  });
  const GET_ACTIONS = Object.freeze(["csrf", "snapshot", "logs", "activity", "result"]);
  const GET_ARGUMENT_KEYS = Object.freeze({
    csrf: Object.freeze([]),
    snapshot: Object.freeze([]),
    logs: Object.freeze(["lines", "source"]),
    activity: Object.freeze(["lines"]),
    result: Object.freeze(["job_id"])
  });
  const ARGUMENT_KEYS = Object.freeze({
    "configure-profile": Object.freeze(["allow_empty_source", "allow_http", "ca_certificate", "compare", "connect_timeout_seconds", "danger_accept_invalid_certs", "delete", "excludes", "jobs", "log_level", "make_default", "max_delete", "max_rate_bytes_per_second", "name", "quiet", "remote", "remote_log_mode", "remote_log_url", "retries", "source", "timeout_seconds", "url", "username", "verbosity"]),
    "remove-profile": Object.freeze(["name"]),
    "set-default": Object.freeze(["name"]),
    "set-secret": Object.freeze(["kind", "mode", "profile", "value"]),
    "schedule": Object.freeze(["allow_delete", "enabled", "interval_seconds", "max_total_delete"]),
    "routine": Object.freeze(["action", "allow_delete", "debounce_seconds", "depends_on", "enabled", "interval_seconds", "max_total_delete", "mode", "poll_seconds", "profile", "retry_backoff_seconds", "retry_count", "time_window_end", "time_window_start", "weekdays"]),
    "remove-routine": Object.freeze(["name"]),
    "alert-policy": Object.freeze(["cooldown_seconds", "enabled", "failure_threshold", "on_failure", "on_success"]),
    "action": Object.freeze(["allow_delete", "kind", "max_total_delete", "scope", "write_test"])
  });

  const state = {
    snapshot: null,
    synoToken: consumeLaunchToken(),
    csrfToken: "",
    connected: false,
    selectedProfile: "",
    selectedRoutine: "",
    snapshotTimer: 0,
    logTimer: 0,
    pollingSnapshot: false,
    pollingLogs: false,
    logsPaused: false,
    clearedLogView: false,
    lastFailureKey: "",
    lastSnapshotAt: 0,
    settings: loadSettings()
  };

  function consumeLaunchToken() {
    const url = new URL(window.location.href);
    const hadTokenParameter = url.searchParams.has("SynoToken") || url.searchParams.has("synotoken");
    const token = url.searchParams.get("SynoToken") || url.searchParams.get("synotoken") || "";
    url.searchParams.delete("SynoToken");
    url.searchParams.delete("synotoken");
    if (hadTokenParameter) window.history.replaceState(null, "", url.pathname + (url.search ? url.search : "") + (url.hash ? url.hash : ""));
    if (!token || token.length > 1024 || /\s|[\u0000-\u001f\u007f]/.test(token)) return "";
    return token;
  }

  function one(selector, root) {
    return (root || document).querySelector(selector);
  }

  function all(selector, root) {
    return Array.from((root || document).querySelectorAll(selector));
  }

  function setText(target, value) {
    const element = typeof target === "string" ? one(target) : target;
    if (element) element.textContent = value === null || value === undefined || value === "" ? "—" : String(value);
  }

  function clearChildren(element) {
    while (element && element.firstChild) element.removeChild(element.firstChild);
  }

  function el(tag, className, text) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = String(text);
    return element;
  }

  function safeStorageGet(key) {
    try {
      return window.localStorage.getItem(key);
    } catch (_error) {
      return null;
    }
  }

  function safeStorageSet(key, value) {
    try {
      window.localStorage.setItem(key, value);
    } catch (_error) {
      toast("Preferences not persisted", "Browser storage is unavailable for this DSM session.", true);
    }
  }

  function loadSettings() {
    const defaults = {
      theme: "dark",
      status_refresh: 5000,
      log_refresh: 5000,
      desktop_notifications: false,
      audible: false
    };
    const raw = safeStorageGet(SETTINGS_KEY);
    if (!raw) return defaults;
    try {
      const parsed = JSON.parse(raw);
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return defaults;
      const theme = ["dark", "light", "system"].includes(parsed.theme) ? parsed.theme : defaults.theme;
      const statusRefresh = [3000, 5000, 10000, 30000].includes(Number(parsed.status_refresh)) ? Number(parsed.status_refresh) : defaults.status_refresh;
      const logRefresh = [5000, 10000, 30000].includes(Number(parsed.log_refresh)) ? Number(parsed.log_refresh) : defaults.log_refresh;
      return {
        theme,
        status_refresh: statusRefresh,
        log_refresh: logRefresh,
        desktop_notifications: parsed.desktop_notifications === true,
        audible: parsed.audible === true
      };
    } catch (_error) {
      return defaults;
    }
  }

  function saveSettings() {
    safeStorageSet(SETTINGS_KEY, JSON.stringify(state.settings));
  }

  function effectiveTheme() {
    if (state.settings.theme !== "system") return state.settings.theme;
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  }

  function applyTheme() {
    document.documentElement.dataset.theme = effectiveTheme();
  }

  function toast(title, message, isError) {
    const region = one("[data-toasts]");
    if (!region) return;
    const item = el("div", isError ? "toast is-error" : "toast");
    item.setAttribute("role", isError ? "alert" : "status");
    item.append(el("strong", "", title), el("span", "", message));
    region.appendChild(item);
    window.setTimeout(function () { item.remove(); }, 6000);
  }

  function boundedText(value, fallback) {
    const text = typeof value === "string" ? value : fallback;
    return String(text || fallback || "").slice(0, 65536);
  }

  function numberOr(value, fallback) {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : fallback;
  }

  function pick(model) {
    if (!model || typeof model !== "object") return undefined;
    for (let index = 1; index < arguments.length; index += 1) {
      const key = arguments[index];
      if (Object.prototype.hasOwnProperty.call(model, key)) return model[key];
    }
    return undefined;
  }

  function definedOr(value, fallback) {
    return value === undefined || value === null ? fallback : value;
  }

  function arrayOf(value) {
    return Array.isArray(value) ? value : [];
  }

  function formatDate(value) {
    const numeric = Number(value);
    if (!Number.isFinite(numeric) || numeric <= 0) return "Unavailable";
    const milliseconds = numeric < 100000000000 ? numeric * 1000 : numeric;
    try {
      return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(milliseconds));
    } catch (_error) {
      return "Unavailable";
    }
  }

  function formatDuration(milliseconds) {
    const value = Number(milliseconds);
    if (!Number.isFinite(value) || value < 0) return "Unavailable";
    if (value < 1000) return Math.round(value) + " ms";
    return (value / 1000).toFixed(value < 10000 ? 1 : 0) + " s";
  }

  function formatBytes(value) {
    const bytes = Number(value);
    if (!Number.isFinite(bytes) || bytes < 0) return "Unavailable";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let scaled = bytes;
    let index = 0;
    while (scaled >= 1024 && index < units.length - 1) {
      scaled /= 1024;
      index += 1;
    }
    return scaled.toFixed(index === 0 ? 0 : 1) + " " + units[index];
  }

  function assertNoReturnedSecrets(model) {
    const forbidden = new Set(["password", "totp", "remote_log_token", "remote-log-token", "secret", "secret_value"]);
    const pending = [model];
    let visited = 0;
    while (pending.length) {
      const value = pending.pop();
      visited += 1;
      if (visited > 20000) throw new Error("API response is too complex");
      if (!value || typeof value !== "object") continue;
      Object.keys(value).forEach(function (key) {
        if (forbidden.has(key.toLowerCase())) throw new Error("API returned forbidden secret material");
        const child = value[key];
        if (child && typeof child === "object") pending.push(child);
      });
    }
  }

  async function responseJson(response, allowGoneResult) {
    if (response.redirected) throw new Error("DSM authentication redirected the API request");
    const contentType = response.headers.get("content-type") || "";
    if (!contentType.toLowerCase().includes("application/json")) throw new Error("API did not return JSON");
    const body = await response.text();
    if (body.length > MAX_RESPONSE_BYTES) throw new Error("API response exceeded the client limit");
    let model;
    try {
      model = JSON.parse(body);
    } catch (_error) {
      throw new Error("API returned malformed JSON");
    }
    if (!model || typeof model !== "object" || Array.isArray(model)) throw new Error("API returned an invalid document");
    assertNoReturnedSecrets(model);
    if ((!response.ok && !(allowGoneResult === true && response.status === 410)) || model.ok === false) {
      throw new Error(boundedText(model.message, "API request failed"));
    }
    return model;
  }

  function endpoint(action, parameters) {
    const url = new URL(API_URL, window.location.href);
    url.searchParams.set("action", action);
    Object.keys(parameters || {}).forEach(function (key) {
      url.searchParams.set(key, String(parameters[key]));
    });
    return url.href;
  }

  async function apiGet(action, parameters) {
    if (!GET_ACTIONS.includes(action)) throw new Error("Unsupported API read action");
    const expectedKeys = GET_ARGUMENT_KEYS[action];
    const actualKeys = Object.keys(parameters || {}).sort();
    if (!expectedKeys || actualKeys.length !== expectedKeys.length || actualKeys.some(function (key, index) { return key !== expectedKeys[index]; })) {
      throw new Error("Read arguments do not match the reviewed bridge contract");
    }
    if (!state.synoToken) throw new Error("DSM launch token is unavailable; reopen the app from the DSM desktop");
    const response = await fetch(endpoint(action, parameters), {
      method: "GET",
      credentials: "same-origin",
      cache: "no-store",
      redirect: "error",
      headers: { "Accept": "application/json", "X-SDSYNC-Request": "1", "X-SYNO-TOKEN": state.synoToken }
    });
    return responseJson(response, action === "result");
  }

  function canMutate() {
    const capabilities = state.snapshot && state.snapshot.capabilities;
    return Boolean(capabilities && capabilities.mutations === true && state.csrfToken);
  }

  function hasCapability(name) {
    const capabilities = state.snapshot && state.snapshot.capabilities;
    return Boolean(capabilities && capabilities[name] === true);
  }

  async function pollJobResult(jobId) {
    if (!/^[0-9a-f]{48}$/.test(jobId)) throw new Error("API returned an invalid queued job identifier");
    for (let attempt = 0; attempt < RESULT_POLL_ATTEMPTS; attempt += 1) {
      const status = await apiGet("result", { job_id: jobId });
      if (status.schema !== RESULT_STATUS_SCHEMA || status.job_id !== jobId) {
        throw new Error("API returned an invalid job-result document");
      }
      if (status.state === "pending") {
        await delay(RESULT_POLL_INTERVAL_MS);
        continue;
      }
      if (status.state === "expired_or_missing") {
        if (!status.result || status.result.schema !== RESULT_SCHEMA || status.result.ok !== false || status.result.code !== "expired_or_missing") {
          throw new Error("API returned an invalid expired job result");
        }
        throw new Error(boundedText(status.result.message, "Queued result is no longer available"));
      }
      if (status.state !== "complete" || !status.result || typeof status.result !== "object" || Array.isArray(status.result) || status.result.schema !== RESULT_SCHEMA) {
        throw new Error("API returned an invalid terminal job result");
      }
      if (status.result.ok === false) throw new Error(boundedText(status.result.message, "Package operation failed"));
      return status.result;
    }
    throw new Error("Queued package operation did not complete within two minutes");
  }

  async function apiPost(action, payload, awaitTerminal) {
    if (!canMutate()) throw new Error("Authenticated DSM mutation bridge is unavailable");
    const expectedKeys = ARGUMENT_KEYS[action];
    if (!expectedKeys) throw new Error("Unsupported API mutation action");
    const actualKeys = Object.keys(payload || {}).sort();
    if (actualKeys.length !== expectedKeys.length || actualKeys.some(function (key, index) { return key !== expectedKeys[index]; })) {
      throw new Error("Mutation arguments do not match the reviewed bridge contract");
    }
    const random = new Uint8Array(16);
    window.crypto.getRandomValues(random);
    const requestId = Array.from(random).map(function (value) { return value.toString(16).padStart(2, "0"); }).join("");
    const request = JSON.stringify({ schema: REQUEST_SCHEMA, request_id: requestId, operation: action, arguments: payload });
    const response = await fetch(API_URL, {
      method: "POST",
      credentials: "same-origin",
      cache: "no-store",
      redirect: "error",
      headers: {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "X-SDSYNC-Request": "1",
        "X-SYNO-TOKEN": state.synoToken,
        "X-SDSYNC-CSRF": state.csrfToken
      },
      body: request
    });
    const queued = await responseJson(response);
    if (queued.schema !== QUEUED_SCHEMA || queued.state !== "queued" || queued.request_id !== requestId || !/^[0-9a-f]{48}$/.test(String(queued.job_id || ""))) {
      throw new Error("API returned an invalid queued-operation document");
    }
    if (awaitTerminal === false) return queued;
    return pollJobResult(queued.job_id);
  }

  function setConnected(connected, label) {
    state.connected = connected;
    const dot = one("[data-connection-dot]");
    if (dot) {
      dot.classList.toggle("is-online", connected);
      dot.classList.toggle("is-error", !connected);
    }
    setText("[data-connection-label]", label);
  }

  function setPill(element, label, stateName) {
    if (!element) return;
    element.textContent = label;
    element.classList.toggle("is-failed", stateName === "failed" || stateName === "error" || stateName === "untrusted");
    element.classList.toggle("is-paused", stateName === "disabled" || stateName === "stopped" || stateName === "unknown");
    element.classList.toggle("neutral", stateName === "disabled" || stateName === "stopped" || stateName === "unknown");
  }

  function profiles() {
    return arrayOf(state.snapshot && state.snapshot.profiles);
  }

  function routines() {
    return arrayOf(state.snapshot && state.snapshot.routines);
  }

  function healthRows() {
    const explicit = state.snapshot && state.snapshot.health;
    if (Array.isArray(explicit)) return explicit;
    return profiles().map(function (profile) {
      const health = profile.health && typeof profile.health === "object" ? profile.health : {};
      const routine = routineByProfile(profile.name) || {};
      return Object.assign({ profile: profile.name, last_success_epoch: routine.last_success_epoch }, health);
    });
  }

  function profileByName(name) {
    return profiles().find(function (profile) { return String(profile.name) === String(name); });
  }

  function routineByProfile(name) {
    return routines().find(function (routine) { return String(routine.profile) === String(name); });
  }

  function mutationControls() {
    const writable = canMutate();
    const profileForm = one("[data-profile-form]");
    if (profileForm) {
      all("input, select, textarea, button", profileForm).forEach(function (control) {
        if (control.hasAttribute("data-close-editor")) control.disabled = false;
        else if (control.hasAttribute("data-managed")) control.disabled = true;
        else control.disabled = !writable;
      });
      const secretWritable = writable && hasCapability("secrets");
      ["password_mode", "totp_mode", "remote_log_token_mode"].forEach(function (name) {
        if (profileForm.elements[name]) profileForm.elements[name].disabled = !secretWritable;
      });
    }
    const newButton = one("[data-new-profile]");
    if (newButton) newButton.disabled = !writable;
    const quickPlan = one("[data-quick-plan]");
    const quickRun = one("[data-quick-run]");
    if (quickPlan) quickPlan.disabled = !writable || profiles().length === 0;
    if (quickRun) quickRun.disabled = !writable || profiles().length === 0;

    const routineForm = one("[data-routine-form]");
    if (routineForm) all("input, select, button", routineForm).forEach(function (control) { control.disabled = !writable; });
    const doctorForm = one("[data-doctor-form]");
    if (doctorForm) {
      all("input, select, button", doctorForm).forEach(function (control) { control.disabled = !writable; });
      if (doctorForm.elements.write_test) doctorForm.elements.write_test.disabled = !writable || !hasCapability("write_test");
    }
    const alertForm = one("[data-alert-policy-form]");
    if (alertForm) all("input, select, button", alertForm).forEach(function (control) { control.disabled = !writable; });
    const banner = one("[data-readonly-banner]");
    if (banner) banner.classList.toggle("is-hidden", writable);
  }

  function renderProfileLists() {
    const list = one("[data-profile-list]");
    const compact = one("[data-overview-profiles]");
    if (list) clearChildren(list);
    if (compact) clearChildren(compact);
    if (profiles().length === 0) {
      if (list) list.appendChild(el("p", "empty-state", "No configured profiles."));
      if (compact) compact.appendChild(el("p", "empty-state", "No configured profiles."));
    }
    profiles().forEach(function (profile) {
      const name = boundedText(profile.name, "Unnamed profile");
      const remote = boundedText(pick(profile, "remote", "remote_path"), "Destination unavailable");
      if (list) {
        const row = el("button", "profile-row");
        row.type = "button";
        row.dataset.profile = name;
        row.classList.toggle("is-selected", state.selectedProfile === name);
        const copy = el("span");
        copy.append(el("strong", "", name), el("span", "", remote));
        const badges = el("span", "profile-badges");
        const ready = profile.has_password === true;
        badges.appendChild(el("span", ready ? "mini-badge ready" : "mini-badge", ready ? "Ready" : "Needs password"));
        if (profile.is_default === true || profile.default === true) badges.appendChild(el("span", "mini-badge", "Default"));
        row.append(copy, badges);
        list.appendChild(row);
      }
      if (compact) {
        const row = el("div", "compact-profile");
        const copy = el("div");
        copy.append(el("strong", "", name), el("span", "", remote));
        const evidence = profile.has_password === true ? "Credential stored" : "Password required";
        row.append(copy, el("span", "", evidence));
        compact.appendChild(row);
      }
    });
    refreshProfileSelectors();
  }

  function option(value, label) {
    const item = document.createElement("option");
    item.value = value;
    item.textContent = label;
    return item;
  }

  function refreshProfileSelectors() {
    const selectors = all("[data-profile-scope], [data-routine-profile]");
    selectors.forEach(function (select) {
      const previous = select.value;
      clearChildren(select);
      if (select.hasAttribute("data-profile-scope")) select.appendChild(option("all", "All profiles"));
      else select.appendChild(option("", "Choose a profile"));
      profiles().forEach(function (profile) { select.appendChild(option(String(profile.name), String(profile.name))); });
      if (Array.from(select.options).some(function (item) { return item.value === previous; })) select.value = previous;
    });
    const dependencies = one("[data-routine-dependencies]");
    if (dependencies) {
      const selected = new Set(Array.from(dependencies.selectedOptions).map(function (item) { return item.value; }));
      clearChildren(dependencies);
      profiles().forEach(function (profile) {
        if (String(profile.name) === String(one("[data-routine-profile]").value)) return;
        const item = option(String(profile.name), String(profile.name));
        item.selected = selected.has(item.value);
        dependencies.appendChild(item);
      });
    }
  }

  function renderRoutines() {
    const list = one("[data-routine-list]");
    if (list) clearChildren(list);
    if (routines().length === 0 && list) list.appendChild(el("p", "empty-state", "No configured routines."));
    routines().forEach(function (routine) {
      if (!list) return;
      const row = el("div", "routine-row");
      const button = el("button");
      button.type = "button";
      button.dataset.routineProfile = String(routine.profile || "");
      const effectiveBackend = boundedText(routine.backend, "fallback unreported");
      button.append(
        el("strong", "", boundedText(routine.profile, "Unknown profile")),
        el("span", "", boundedText(routine.mode, "interval") + " · " + effectiveBackend + " · " + boundedText(routine.state, routine.enabled ? "enabled" : "disabled"))
      );
      const next = routine.enabled === true ? formatDate(routine.next_run_epoch) : "Disabled";
      row.append(button, el("span", "", next));
      list.appendChild(row);
    });
    const realtime = routines().filter(function (routine) { return routine.enabled === true && routine.mode === "realtime"; });
    if (realtime.length) {
      setText("[data-realtime-state]", realtime.length + " active");
      const fallbacks = realtime.filter(function (routine) { return String(routine.backend || "").includes("poll"); }).length;
      setText("[data-realtime-detail]", fallbacks ? fallbacks + " using polling fallback" : "Native/fallback backend reported healthy");
    } else {
      setText("[data-realtime-state]", "Off");
      setText("[data-realtime-detail]", "No enabled realtime routine");
    }
  }

  function healthCell(row, value, kind) {
    const cell = el("td");
    if (kind === "boolean") {
      if (value === true) { cell.textContent = "Yes"; cell.className = "health-value-ok"; }
      else if (value === false) { cell.textContent = "No"; cell.className = "health-value-bad"; }
      else { cell.textContent = "Unavailable"; cell.className = "health-value-unknown"; }
    } else {
      cell.textContent = value || "Unavailable";
      if (!value || value === "Unavailable") cell.className = "health-value-unknown";
    }
    row.appendChild(cell);
  }

  function renderHealth() {
    const body = one("[data-target-health]");
    if (!body) return;
    clearChildren(body);
    if (healthRows().length === 0) {
      const row = el("tr");
      const cell = el("td", "health-value-unknown", "No cached target-health evidence.");
      cell.colSpan = 9;
      row.appendChild(cell);
      body.appendChild(row);
      setText("[data-health-freshness]", "Unavailable");
      return;
    }
    let newest = 0;
    healthRows().forEach(function (health) {
      const row = el("tr");
      const check = numberOr(pick(health, "last_check_epoch", "checked_at_epoch", "checked_epoch"), 0);
      newest = Math.max(newest, check);
      healthCell(row, boundedText(health.profile, "Unknown"));
      healthCell(row, formatDate(check));
      healthCell(row, health.reachable, "boolean");
      healthCell(row, pick(health, "authenticated", "auth"), "boolean");
      healthCell(row, health.writable, "boolean");
      healthCell(row, Number.isFinite(Number(health.latency_ms)) ? formatDuration(health.latency_ms) : "Unavailable");
      healthCell(row, formatDate(pick(health, "last_success_epoch", "last_successful_sync_epoch")));
      healthCell(row, boundedText(pick(health, "doctor_status", "last_doctor_status", "state"), "Unavailable"));
      const proven = health.free_space_proven === true;
      healthCell(row, proven ? formatBytes(health.free_space_bytes) : "Unavailable");
      body.appendChild(row);
    });
    setText("[data-health-freshness]", newest ? "Newest check " + formatDate(newest) : "Cached time unavailable");
  }

  function renderAlerts() {
    const alerts = state.snapshot && state.snapshot.alerts;
    const form = one("[data-alert-policy-form]");
    if (!form || !alerts || typeof alerts !== "object") {
      setPill(one("[data-alert-policy-state]"), "Unavailable", "unknown");
      return;
    }
    form.elements.enabled.checked = alerts.enabled === true;
    form.elements.on_success.checked = alerts.on_success === true;
    form.elements.on_failure.checked = alerts.on_failure !== false;
    form.elements.failure_threshold.value = String(numberOr(alerts.failure_threshold, 1));
    form.elements.cooldown_seconds.value = String(numberOr(alerts.cooldown_seconds, 3600));
    setPill(one("[data-alert-policy-state]"), alerts.enabled ? "Enabled" : "Disabled", alerts.enabled ? "running" : "disabled");
  }

  function runModel() {
    if (!state.snapshot) return {};
    return state.snapshot.run && typeof state.snapshot.run === "object" ? state.snapshot.run : (state.snapshot.last_run || {});
  }

  function renderRun() {
    const run = runModel();
    const status = boundedText(pick(run, "status", "state", "result"), "Unavailable");
    const scope = boundedText(run.scope, "Unavailable");
    setText("[data-last-result]", status);
    setText("[data-last-detail]", run.finished_epoch ? formatDate(run.finished_epoch) : "No completion time");
    setText("[data-active-scope]", status === "running" ? scope : "Idle");
    setText("[data-active-detail]", status === "running" ? boundedText(run.operation, "Operation active") : "No active operation");
    const details = one("[data-run-details]");
    if (details) {
      const values = [boundedText(run.operation, "Unavailable"), status, scope, formatDate(run.started_epoch), formatDate(run.finished_epoch)];
      all("dd", details).forEach(function (item, index) { item.textContent = values[index] || "Unavailable"; });
    }
    maybeNotifyFailure(run);
  }

  function maybeNotifyFailure(run) {
    if (!run || String(pick(run, "status", "state", "result")) !== "failed") return;
    const key = [run.profile || run.scope || "unknown", run.finished_epoch || run.started_epoch || "unknown", run.exit_code || "unknown"].join(":");
    if (!state.lastFailureKey) { state.lastFailureKey = key; return; }
    if (key === state.lastFailureKey) return;
    state.lastFailureKey = key;
    if (state.settings.desktop_notifications && window.Notification && Notification.permission === "granted") {
      new Notification("Synology Drive Sync failed", { body: "A newly observed package run failed. Open DSM for details.", icon: "images/icon_64.png", tag: "sdsync-run-failure" });
    }
    if (state.settings.audible) playCue();
  }

  function playCue() {
    try {
      const AudioContext = window.AudioContext || window.webkitAudioContext;
      if (!AudioContext) return;
      const context = new AudioContext();
      const oscillator = context.createOscillator();
      const gain = context.createGain();
      oscillator.frequency.value = 440;
      gain.gain.setValueAtTime(0.0001, context.currentTime);
      gain.gain.exponentialRampToValueAtTime(0.08, context.currentTime + 0.02);
      gain.gain.exponentialRampToValueAtTime(0.0001, context.currentTime + 0.18);
      oscillator.connect(gain);
      gain.connect(context.destination);
      oscillator.start();
      oscillator.stop(context.currentTime + 0.2);
      oscillator.addEventListener("ended", function () { context.close(); }, { once: true });
    } catch (_error) {
      // Audio is best-effort and never changes package state.
    }
  }

  function renderSnapshot(snapshot) {
    if (snapshot.schema !== SNAPSHOT_SCHEMA) throw new Error("Unsupported DSM API schema");
    state.snapshot = snapshot;
    if (typeof snapshot.csrf_token === "string" && snapshot.csrf_token) state.csrfToken = snapshot.csrf_token;
    state.lastSnapshotAt = Date.now();
    const serviceObject = snapshot.service && typeof snapshot.service === "object" ? snapshot.service : {};
    const service = boundedText(pick(serviceObject, "state", "status") || snapshot.service, "unknown");
    setPill(one("[data-service-pill]"), service, service);
    setText("[data-overview-summary]", service === "running" ? "The package controller is running. Status and logs update while this window remains open." : "The package controller reports " + service + ". Review Health and Activity before relying on automation.");
    setText("[data-profile-count]", profiles().length);
    const readyCount = profiles().filter(function (profile) { return profile.has_password === true; }).length;
    setText("[data-profile-detail]", readyCount + " with protected password material");
    const enabledRoutines = routines().filter(function (routine) { return routine.enabled === true; });
    const nextEpochs = enabledRoutines.map(function (routine) { return Number(routine.next_run_epoch); }).filter(function (value) { return Number.isFinite(value) && value > 0; });
    const nextEpoch = nextEpochs.length ? Math.min.apply(Math, nextEpochs) : 0;
    setText("[data-next-run]", nextEpoch ? formatDate(nextEpoch) : "None");
    setText("[data-schedule-detail]", enabledRoutines.length + " enabled profile routine" + (enabledRoutines.length === 1 ? "" : "s"));
    setText("[data-freshness]", "Updated " + new Intl.DateTimeFormat(undefined, { timeStyle: "medium" }).format(new Date()));
    setConnected(true, canMutate() ? "Authenticated control bridge" : "Package status · read-only");
    renderProfileLists();
    renderRoutines();
    renderRun();
    renderHealth();
    renderAlerts();
    mutationControls();
  }

  async function refreshSnapshot(manual) {
    if (state.pollingSnapshot || document.hidden) return;
    state.pollingSnapshot = true;
    try {
      if (!state.csrfToken) await refreshCsrf();
      const snapshot = await apiGet("snapshot");
      renderSnapshot(snapshot);
      if (manual) toast("Status refreshed", "The latest package snapshot is displayed.", false);
    } catch (error) {
      state.snapshot = null;
      state.csrfToken = "";
      setConnected(false, "Control bridge unavailable");
      setPill(one("[data-service-pill]"), "Unavailable", "failed");
      setText("[data-freshness]", "Status unavailable");
      const banner = one("[data-readonly-banner]");
      if (banner) banner.classList.remove("is-hidden");
      mutationControls();
      if (manual) toast("Refresh failed", boundedText(error.message, "Unable to read package status."), true);
    } finally {
      state.pollingSnapshot = false;
      scheduleSnapshot();
    }
  }

  async function refreshCsrf() {
    state.csrfToken = "";
    const model = await apiGet("csrf");
    if (typeof model.csrf_token !== "string" || !model.csrf_token || model.csrf_token.length > 4096) throw new Error("Authenticated bridge did not issue a valid CSRF token");
    state.csrfToken = model.csrf_token;
  }

  function scheduleSnapshot() {
    window.clearTimeout(state.snapshotTimer);
    if (!document.hidden) state.snapshotTimer = window.setTimeout(function () { refreshSnapshot(false); }, state.settings.status_refresh);
  }

  function logsFrom(model) {
    if (Array.isArray(model.logs)) {
      return model.logs.map(function (entry) {
        if (typeof entry === "string") return entry;
        if (entry && typeof entry === "object") {
          if (Array.isArray(entry.lines)) {
            return entry.lines.map(function (line) { return "[" + boundedText(entry.source, "log") + "] " + boundedText(line, ""); }).join("\n");
          }
          const prefix = entry.timestamp ? "[" + entry.timestamp + "] " : "";
          const source = entry.source ? "[" + entry.source + "] " : "";
          return prefix + source + boundedText(entry.message, "");
        }
        return "";
      }).join("\n");
    }
    return boundedText(model.text || model.output, "No log data yet.");
  }

  function renderActivity(model) {
    const feed = one("[data-activity-feed]");
    if (!feed) return;
    clearChildren(feed);
    const events = arrayOf(model && model.events);
    if (!events.length) feed.appendChild(el("li", "empty-state", "No recorded package events."));
    events.slice().reverse().forEach(function (event) {
      const item = el("li", "activity-item");
      const time = el("time", "", formatDate(event.epoch));
      if (event.epoch) time.dateTime = new Date(Number(event.epoch) * 1000).toISOString();
      item.append(time, el("strong", "", boundedText(event.code, "unknown.event")), el("small", "", boundedText(event.profile, "none") + " · " + boundedText(event.state, "unknown")));
      feed.appendChild(item);
    });
    setText("[data-activity-state]", events.length + " event" + (events.length === 1 ? "" : "s"));
  }

  async function refreshLogs() {
    if (state.pollingLogs || state.logsPaused || document.hidden || currentRoute() !== "activity") return;
    state.pollingLogs = true;
    try {
      const source = one("[data-log-source]").value;
      const lines = Math.min(1000, Math.max(1, Number(one("[data-log-lines]").value) || 200));
      const results = await Promise.all([apiGet("logs", { source, lines }), apiGet("activity", { lines })]);
      const model = results[0];
      renderActivity(results[1]);
      const output = one("[data-log-output]");
      if (output && !state.clearedLogView) output.textContent = logsFrom(model).slice(0, MAX_RESPONSE_BYTES);
      state.clearedLogView = false;
      setText("[data-log-state]", "Live · " + lines + " line limit");
    } catch (_error) {
      setText("[data-log-state]", "Logs unavailable");
    } finally {
      state.pollingLogs = false;
      scheduleLogs();
    }
  }

  function scheduleLogs() {
    window.clearTimeout(state.logTimer);
    if (!document.hidden && currentRoute() === "activity" && !state.logsPaused) state.logTimer = window.setTimeout(refreshLogs, state.settings.log_refresh);
  }

  function currentRoute() {
    const hash = window.location.hash.replace(/^#\/?/, "");
    return Object.prototype.hasOwnProperty.call(ROUTE_TITLES, hash) ? hash : "overview";
  }

  function navigate(route, updateHash) {
    const selected = Object.prototype.hasOwnProperty.call(ROUTE_TITLES, route) ? route : "overview";
    all("[data-page]").forEach(function (page) {
      const active = page.dataset.page === selected;
      page.hidden = !active;
      page.classList.toggle("is-active", active);
    });
    all("[data-route]").forEach(function (button) {
      const active = button.dataset.route === selected;
      button.classList.toggle("is-active", active);
      if (active) button.setAttribute("aria-current", "page");
      else button.removeAttribute("aria-current");
    });
    setText("[data-page-title]", ROUTE_TITLES[selected]);
    if (updateHash) window.history.replaceState(null, "", "#" + selected);
    if (selected === "activity") refreshLogs();
    else scheduleLogs();
  }

  function setFormValue(form, name, value) {
    const control = form.elements[name];
    if (!control) return;
    if (control.type === "checkbox") control.checked = value === true;
    else control.value = value === null || value === undefined ? "" : String(value);
  }

  function resetSecretEditor(form) {
    ["password", "totp", "remote_log_token"].forEach(function (name) { if (form.elements[name]) form.elements[name].value = ""; });
    ["password_mode", "totp_mode", "remote_log_token_mode"].forEach(function (name) { if (form.elements[name]) form.elements[name].value = "keep"; });
    updateSecretVisibility(form, "password");
    updateSecretVisibility(form, "totp");
    updateSecretVisibility(form, "remote_log_token");
  }

  function openProfile(name) {
    const form = one("[data-profile-form]");
    if (!form) return;
    const profile = name ? profileByName(name) : null;
    state.selectedProfile = profile ? String(profile.name) : "";
    form.reset();
    resetSecretEditor(form);
    setText("[data-profile-form-title]", profile ? "Edit " + profile.name : "New profile");
    const deleteButton = one("[data-delete-profile]", form);
    if (deleteButton) deleteButton.classList.toggle("is-hidden", !profile);
    if (profile) {
      const mapping = {
        name: pick(profile, "name"), source: pick(profile, "source"), url: pick(profile, "url"), username: pick(profile, "username"),
        remote: pick(profile, "remote", "remote_path"), compare: pick(profile, "compare"), jobs: pick(profile, "jobs"), allow_http: pick(profile, "allow_http"),
        delete: pick(profile, "delete"), max_delete: pick(profile, "max_delete"), make_default: pick(profile, "is_default", "default"),
        allow_empty_source: pick(profile, "allow_empty_source"), retries: pick(profile, "retries"), timeout: pick(profile, "timeout", "upload_timeout_seconds"),
        connect_timeout: pick(profile, "connect_timeout", "connect_timeout_seconds"), max_rate: pick(profile, "max_rate", "max_rate_bytes_per_second"),
        ca_certificate: pick(profile, "ca_certificate"), danger_invalid_certs: pick(profile, "danger_invalid_certs", "danger_accept_invalid_certs"),
        verbosity: pick(profile, "verbosity"), quiet: pick(profile, "quiet"), log_level: pick(profile, "log_level"),
        remote_log_url: pick(profile, "remote_log_url"), remote_log_mode: pick(profile, "remote_log_mode")
      };
      Object.keys(mapping).forEach(function (key) { if (mapping[key] !== undefined) setFormValue(form, key, mapping[key]); });
      setFormValue(form, "excludes", arrayOf(profile.excludes).join("\n"));
      setText("[data-password-state]", profile.has_password === true ? "Stored · masked" : "Not stored");
      setText("[data-totp-state]", profile.has_totp === true ? "Stored · masked" : "Not stored");
      setText("[data-remote-token-state]", profile.has_remote_log_token === true ? "Stored · masked" : "Not stored");
    } else {
      setText("[data-password-state]", "Not stored");
      setText("[data-totp-state]", "Not stored");
      setText("[data-remote-token-state]", "Not stored");
    }
    form.elements.name.readOnly = Boolean(profile);
    form.elements.name.setAttribute("aria-readonly", profile ? "true" : "false");
    if (form.elements.danger_invalid_confirm) form.elements.danger_invalid_confirm.checked = false;
    updateInvalidCertificateWarning(form);
    form.classList.remove("is-hidden");
    renderProfileLists();
    mutationControls();
    const first = form.elements.name;
    if (first) first.focus();
  }

  function closeProfile() {
    const form = one("[data-profile-form]");
    if (!form) return;
    clearSecretInputs();
    form.classList.add("is-hidden");
    state.selectedProfile = "";
    renderProfileLists();
  }

  function updateSecretVisibility(form, name) {
    const mode = form.elements[name + "_mode"];
    const wrapper = one("[data-" + name.replaceAll("_", "-") + "-input]", form);
    const input = form.elements[name];
    if (!mode || !wrapper || !input) return;
    const replacing = mode.value === "replace";
    wrapper.classList.toggle("is-hidden", !replacing);
    input.required = replacing;
    if (!replacing) input.value = "";
  }

  function updateInvalidCertificateWarning(form) {
    const enabled = form.elements.danger_invalid_certs && form.elements.danger_invalid_certs.checked;
    const wrapper = one("[data-invalid-cert-warning]", form);
    if (wrapper) wrapper.classList.toggle("is-hidden", !enabled);
    if (!enabled && form.elements.danger_invalid_confirm) form.elements.danger_invalid_confirm.checked = false;
  }

  function clearSecretInputs() {
    const form = one("[data-profile-form]");
    if (!form) return;
    ["password", "totp", "remote_log_token"].forEach(function (name) { if (form.elements[name]) form.elements[name].value = ""; });
  }

  function integer(form, name, fallback) {
    const value = Number(form.elements[name].value);
    return Number.isInteger(value) ? value : fallback;
  }

  function collectProfile(form) {
    const maxRate = integer(form, "max_rate", 0);
    return {
      name: form.elements.name.value,
      source: form.elements.source.value,
      url: form.elements.url.value,
      username: form.elements.username.value,
      remote: form.elements.remote.value,
      compare: form.elements.compare.value,
      jobs: integer(form, "jobs", 2),
      allow_http: form.elements.allow_http.checked,
      delete: form.elements.delete.checked,
      max_delete: integer(form, "max_delete", 5),
      make_default: form.elements.make_default.checked,
      excludes: form.elements.excludes.value.split(/\r?\n/).map(function (item) { return item.trim(); }).filter(Boolean),
      allow_empty_source: form.elements.allow_empty_source.checked,
      retries: integer(form, "retries", 2),
      timeout_seconds: integer(form, "timeout", 7200),
      connect_timeout_seconds: integer(form, "connect_timeout", 15),
      max_rate_bytes_per_second: maxRate === 0 ? null : maxRate,
      ca_certificate: form.elements.ca_certificate.value || null,
      danger_accept_invalid_certs: form.elements.danger_invalid_certs.checked,
      verbosity: integer(form, "verbosity", 0),
      quiet: form.elements.quiet.checked,
      log_level: form.elements.log_level.value,
      remote_log_url: form.elements.remote_log_url.value || null,
      remote_log_mode: form.elements.remote_log_mode.value
    };
  }

  function collectSecretOperations(form, profile) {
    const operations = [];
    [
      ["password", "password"],
      ["totp", "totp"],
      ["remote_log_token", "remote-log-token"]
    ].forEach(function (entry) {
      const field = entry[0];
      const kind = entry[1];
      const mode = form.elements[field + "_mode"].value;
      if (mode === "keep") return;
      const operation = { profile, kind, mode };
      if (mode === "replace") operation.value = form.elements[field].value;
      else operation.value = null;
      operations.push(operation);
    });
    return operations;
  }

  function delay(milliseconds) {
    return new Promise(function (resolve) { window.setTimeout(resolve, milliseconds); });
  }

  async function waitForNewProfile(name) {
    for (let attempt = 0; attempt < 40; attempt += 1) {
      await delay(500);
      const snapshot = await apiGet("snapshot");
      renderSnapshot(snapshot);
      if (profileByName(name)) return;
    }
    throw new Error("Profile configuration completed but was not observed before the secret handoff deadline");
  }

  async function saveProfile(event) {
    event.preventDefault();
    const form = event.currentTarget;
    if (!canMutate()) return toast("Read-only", "The authenticated DSM mutation bridge is unavailable.", true);
    if (!form.reportValidity()) return;
    if (form.elements.quiet.checked && Number(form.elements.verbosity.value) !== 0) {
      toast("Profile not saved", "Quiet terminal output cannot be combined with verbose output. Select Normal verbosity or turn off Quiet.", true);
      form.elements.verbosity.focus();
      return;
    }
    if (form.elements.danger_invalid_certs.checked && !form.elements.danger_invalid_confirm.checked) {
      toast("Confirmation required", "Explicitly accept the TLS interception risk before saving.", true);
      form.elements.danger_invalid_confirm.focus();
      return;
    }
    const profile = collectProfile(form);
    if (profile.remote_log_url && !profile.remote_log_url.startsWith("https://")) {
      toast("Profile not saved", "Remote log delivery requires an HTTPS URL.", true);
      form.elements.remote_log_url.focus();
      return;
    }
    const risky = profile.allow_empty_source || profile.danger_accept_invalid_certs || profile.delete;
    if (risky && !await confirmAction("Save dangerous profile settings?", "This profile changes one or more safety guards. Review deletion, empty-source, and TLS settings before continuing.", "Save profile")) return;
    const isNewProfile = !profileByName(profile.name);
    const secretOperations = collectSecretOperations(form, profile.name);
    clearSecretInputs();
    try {
      await apiPost(ACTIONS.configureProfile, profile);
      if (isNewProfile && secretOperations.length) await waitForNewProfile(profile.name);
      for (const operation of secretOperations) await apiPost(ACTIONS.setSecret, operation);
      toast("Profile saved", "The controller applied the validated configuration and protected credential operations.", false);
      closeProfile();
      await refreshSnapshot(false);
    } catch (error) {
      toast("Profile not saved", boundedText(error.message, "The package rejected the change."), true);
    }
  }

  async function removeProfile() {
    if (!canMutate() || !state.selectedProfile) return;
    const name = state.selectedProfile;
    if (!await confirmAction("Delete profile " + name + "?", "This removes its package-owned configuration and protected credentials. Synced files are not deleted by this action.", "Delete profile")) return;
    try {
      await apiPost(ACTIONS.removeProfile, { name });
      toast("Profile deleted", "The controller removed " + name + " and its stored credentials.", false);
      closeProfile();
      await refreshSnapshot(false);
    } catch (error) {
      toast("Profile not deleted", boundedText(error.message, "The package rejected the change."), true);
    }
  }

  function populateRoutine(profileName) {
    const form = one("[data-routine-form]");
    if (!form) return;
    const routine = routineByProfile(profileName);
    state.selectedRoutine = profileName || "";
    form.elements.profile.value = profileName || "";
    setFormValue(form, "enabled", routine && routine.enabled === true);
    setFormValue(form, "action", pick(routine, "action") || "sync");
    setFormValue(form, "mode", pick(routine, "mode") || "interval");
    setFormValue(form, "interval_seconds", pick(routine, "interval_seconds") || 3600);
    setFormValue(form, "window_start", pick(routine, "time_window_start", "window_start") || "00:00");
    setFormValue(form, "window_end", pick(routine, "time_window_end", "window_end") || "23:59");
    setFormValue(form, "debounce_seconds", pick(routine, "debounce_seconds") || 30);
    setFormValue(form, "poll_seconds", pick(routine, "poll_seconds") || 30);
    setFormValue(form, "retry_count", definedOr(pick(routine, "retry_count"), 2));
    setFormValue(form, "retry_backoff_seconds", pick(routine, "retry_backoff_seconds") || 60);
    setFormValue(form, "allow_delete", routine && routine.allow_delete === true);
    setFormValue(form, "max_total_delete", definedOr(pick(routine, "max_total_delete"), 100));
    const weekdayValue = routine && routine.weekdays;
    const weekdayList = Array.isArray(weekdayValue) ? weekdayValue : (typeof weekdayValue === "string" ? weekdayValue.split(",") : []);
    const weekdays = new Set(weekdayList.map(String));
    all('input[name="weekday"]', form).forEach(function (item) { item.checked = routine ? weekdays.has(item.value) : true; });
    refreshProfileSelectors();
    const dependencies = new Set(arrayOf(routine && routine.depends_on).map(String));
    all("option", form.elements.depends_on).forEach(function (item) { item.selected = dependencies.has(item.value); });
    setPill(one("[data-routine-pill]"), routine ? boundedText(routine.state, routine.enabled ? "Enabled" : "Disabled") : "New", routine ? routine.state : "unknown");
    mutationControls();
  }

  async function saveRoutine(event) {
    event.preventDefault();
    const form = event.currentTarget;
    if (!canMutate()) return toast("Read-only", "The authenticated DSM mutation bridge is unavailable.", true);
    if (!form.reportValidity()) return;
    const weekdays = all('input[name="weekday"]:checked', form).map(function (item) { return Number(item.value); });
    if (!weekdays.length) return toast("Routine not saved", "Select at least one active weekday.", true);
    const payload = {
      profile: form.elements.profile.value,
      enabled: form.elements.enabled.checked,
      action: form.elements.action.value,
      mode: form.elements.mode.value,
      interval_seconds: integer(form, "interval_seconds", 3600),
      weekdays,
      time_window_start: form.elements.window_start.value,
      time_window_end: form.elements.window_end.value,
      debounce_seconds: integer(form, "debounce_seconds", 30),
      poll_seconds: integer(form, "poll_seconds", 30),
      retry_count: integer(form, "retry_count", 2),
      retry_backoff_seconds: integer(form, "retry_backoff_seconds", 60),
      allow_delete: form.elements.allow_delete.checked,
      max_total_delete: integer(form, "max_total_delete", 100),
      depends_on: Array.from(form.elements.depends_on.selectedOptions).map(function (item) { return item.value; })
    };
    if (!payload.profile) return toast("Routine not saved", "Choose a profile first.", true);
    try {
      await apiPost(ACTIONS.routine, payload);
      toast("Routine saved", "The controller applied the per-profile policy.", false);
      await refreshSnapshot(false);
    } catch (error) {
      toast("Routine not saved", boundedText(error.message, "The package rejected the routine."), true);
    }
  }

  async function removeRoutine() {
    const profile = one("[data-routine-form]").elements.profile.value;
    if (!canMutate() || !profile || !routineByProfile(profile)) return;
    if (!await confirmAction("Remove routine for " + profile + "?", "The profile remains configured, but package automation for it will stop.", "Remove routine")) return;
    try {
      await apiPost(ACTIONS.removeRoutine, { name: profile });
      toast("Routine removed", "The controller removed automation for " + profile + ".", false);
      await refreshSnapshot(false);
      populateRoutine(profile);
    } catch (error) {
      toast("Routine not removed", boundedText(error.message, "The package rejected the change."), true);
    }
  }

  async function saveAlerts(event) {
    event.preventDefault();
    const form = event.currentTarget;
    if (!canMutate()) return toast("Read-only", "The authenticated DSM mutation bridge is unavailable.", true);
    const payload = {
      enabled: form.elements.enabled.checked,
      on_success: form.elements.on_success.checked,
      on_failure: form.elements.on_failure.checked,
      failure_threshold: integer(form, "failure_threshold", 1),
      cooldown_seconds: integer(form, "cooldown_seconds", 3600)
    };
    try {
      await apiPost(ACTIONS.alertPolicy, payload);
      toast("Alert policy saved", "The controller applied the DSM desktop alert policy.", false);
      await refreshSnapshot(false);
    } catch (error) {
      toast("Alert policy not saved", boundedText(error.message, "The package rejected the policy."), true);
    }
  }

  async function executeOperation(operation, payload) {
    if (!canMutate()) return toast("Read-only", "The authenticated DSM mutation bridge is unavailable.", true);
    try {
      const result = await apiPost(ACTIONS.execute, Object.assign({ kind: operation }, payload || {}), false);
      const queued = result.state === "queued";
      const message = boundedText(result.message || result.output, queued ? "Queued safely; follow Activity and Logs for the final result." : "The operation was accepted.");
      if (operation === "doctor") {
        setText("[data-diagnostic-title]", queued ? "Doctor queued" : (result.ok === false ? "Doctor failed" : "Doctor completed"));
        setText("[data-diagnostic-output]", message);
      }
      toast(operation.charAt(0).toUpperCase() + operation.slice(1) + (queued ? " queued" : " accepted"), message, false);
      await refreshSnapshot(false);
    } catch (error) {
      if (operation === "doctor") {
        setText("[data-diagnostic-title]", "Doctor failed");
        setText("[data-diagnostic-output]", boundedText(error.message, "Diagnostic failed."));
      }
      toast(operation + " failed", boundedText(error.message, "The package rejected the operation."), true);
    }
  }

  async function submitDoctor(event) {
    event.preventDefault();
    const form = event.currentTarget;
    const writeTest = form.elements.write_test.checked;
    if (writeTest && !form.elements.write_confirm.checked) return toast("Write-test confirmation required", "Approve the disposable probe and cleanup before running.", true);
    if (writeTest && !await confirmAction("Run the disposable target probe?", "The doctor will briefly create, verify, and remove a unique probe in the selected destination.", "Run write test")) return;
    setText("[data-diagnostic-title]", "Doctor running");
    setText("[data-diagnostic-output]", "Waiting for the package controller…");
    await executeOperation("doctor", { scope: form.elements.scope.value, write_test: writeTest, allow_delete: null, max_total_delete: null });
  }

  function confirmAction(title, message, buttonLabel) {
    const dialog = one("[data-confirm-dialog]");
    if (!dialog || typeof dialog.showModal !== "function") return Promise.resolve(false);
    setText("[data-confirm-title]", title);
    setText("[data-confirm-message]", message);
    setText("[data-confirm-button]", buttonLabel);
    return new Promise(function (resolve) {
      dialog.addEventListener("close", function () { resolve(dialog.returnValue === "confirm"); }, { once: true });
      dialog.showModal();
    });
  }

  async function saveNotificationPreferences(event) {
    event.preventDefault();
    const form = event.currentTarget;
    if (form.elements.desktop_notifications.checked && window.Notification && Notification.permission === "default") {
      const permission = await Notification.requestPermission();
      if (permission !== "granted") form.elements.desktop_notifications.checked = false;
    }
    state.settings.desktop_notifications = form.elements.desktop_notifications.checked;
    state.settings.audible = form.elements.audible.checked;
    saveSettings();
    renderNotificationPermission();
    toast("Session preferences saved", "These non-secret browser preferences are stored locally.", false);
  }

  function renderNotificationPermission() {
    const form = one("[data-notification-form]");
    if (form) {
      form.elements.desktop_notifications.checked = state.settings.desktop_notifications;
      form.elements.audible.checked = state.settings.audible;
    }
    const permission = window.Notification ? Notification.permission : "unsupported";
    setPill(one("[data-notification-permission]"), permission, permission === "denied" ? "failed" : (permission === "granted" ? "running" : "unknown"));
  }

  function saveInterfaceSettings(event) {
    event.preventDefault();
    const form = event.currentTarget;
    state.settings.theme = form.elements.theme.value;
    state.settings.status_refresh = Number(form.elements.status_refresh.value);
    state.settings.log_refresh = Number(form.elements.log_refresh.value);
    saveSettings();
    applyTheme();
    scheduleSnapshot();
    scheduleLogs();
    toast("Interface settings saved", "Theme and refresh cadence were updated locally.", false);
  }

  function hydrateSettingsForms() {
    const form = one("[data-settings-form]");
    if (form) {
      form.elements.theme.value = state.settings.theme;
      form.elements.status_refresh.value = String(state.settings.status_refresh);
      form.elements.log_refresh.value = String(state.settings.log_refresh);
    }
    renderNotificationPermission();
  }

  function bindEvents() {
    all("[data-route]").forEach(function (button) { button.addEventListener("click", function () { navigate(button.dataset.route, true); }); });
    all("[data-goto]").forEach(function (button) { button.addEventListener("click", function () { navigate(button.dataset.goto, true); }); });
    one("[data-refresh]").addEventListener("click", function () { refreshSnapshot(true); });
    one("[data-new-profile]").addEventListener("click", function () { openProfile(""); });
    all("[data-close-editor]").forEach(function (button) { button.addEventListener("click", closeProfile); });
    one("[data-profile-list]").addEventListener("click", function (event) {
      const row = event.target.closest("[data-profile]");
      if (row) openProfile(row.dataset.profile);
    });
    one("[data-profile-filter]").addEventListener("input", function (event) {
      const query = event.target.value.trim().toLowerCase();
      all("[data-profile]", one("[data-profile-list]")).forEach(function (row) { row.hidden = Boolean(query) && !row.dataset.profile.toLowerCase().includes(query); });
    });
    const profileForm = one("[data-profile-form]");
    profileForm.addEventListener("submit", saveProfile);
    profileForm.elements.password_mode.addEventListener("change", function () { updateSecretVisibility(profileForm, "password"); });
    profileForm.elements.totp_mode.addEventListener("change", function () { updateSecretVisibility(profileForm, "totp"); });
    profileForm.elements.remote_log_token_mode.addEventListener("change", function () { updateSecretVisibility(profileForm, "remote_log_token"); });
    profileForm.elements.danger_invalid_certs.addEventListener("change", function () { updateInvalidCertificateWarning(profileForm); });
    one("[data-delete-profile]").addEventListener("click", removeProfile);

    const routineForm = one("[data-routine-form]");
    routineForm.addEventListener("submit", saveRoutine);
    routineForm.elements.profile.addEventListener("change", function () { populateRoutine(routineForm.elements.profile.value); });
    one("[data-remove-routine]").addEventListener("click", removeRoutine);
    one("[data-routine-list]").addEventListener("click", function (event) {
      const button = event.target.closest("[data-routine-profile]");
      if (button) populateRoutine(button.dataset.routineProfile);
    });

    one("[data-doctor-form]").addEventListener("submit", submitDoctor);
    const writeTest = one('[data-doctor-form] [name="write_test"]');
    writeTest.addEventListener("change", function () {
      one("[data-write-warning]").classList.toggle("is-hidden", !writeTest.checked);
      if (!writeTest.checked) one('[data-doctor-form] [name="write_confirm"]').checked = false;
    });
    one("[data-quick-plan]").addEventListener("click", function () { executeOperation("plan", { scope: "all", write_test: null, allow_delete: false, max_total_delete: 0 }); });
    one("[data-quick-run]").addEventListener("click", async function () {
      if (await confirmAction("Run all configured profiles?", "This starts a real one-way sync. Remote deletion remains disabled for this quick action.", "Run all")) executeOperation("run", { scope: "all", write_test: null, allow_delete: false, max_total_delete: 0 });
    });
    one("[data-alert-policy-form]").addEventListener("submit", saveAlerts);
    one("[data-notification-form]").addEventListener("submit", saveNotificationPreferences);
    one("[data-settings-form]").addEventListener("submit", saveInterfaceSettings);
    one("[data-pause-logs]").addEventListener("click", function (event) {
      state.logsPaused = !state.logsPaused;
      event.currentTarget.textContent = state.logsPaused ? "Resume live updates" : "Pause live updates";
      setText("[data-log-state]", state.logsPaused ? "Paused" : "Resuming");
      if (!state.logsPaused) refreshLogs();
    });
    one("[data-clear-view]").addEventListener("click", function () {
      state.clearedLogView = true;
      setText("[data-log-output]", "View cleared. The package log was not deleted.");
    });
    one("[data-log-source]").addEventListener("change", refreshLogs);
    one("[data-log-lines]").addEventListener("change", refreshLogs);
    window.addEventListener("hashchange", function () { navigate(currentRoute(), false); });
    window.addEventListener("pagehide", function () {
      clearSecretInputs();
      state.csrfToken = "";
      state.synoToken = "";
    });
    document.addEventListener("visibilitychange", function () {
      if (document.hidden) {
        window.clearTimeout(state.snapshotTimer);
        window.clearTimeout(state.logTimer);
        clearSecretInputs();
      } else {
        refreshSnapshot(false);
        if (currentRoute() === "activity") refreshLogs();
      }
    });
    if (window.matchMedia) {
      const colorPreference = window.matchMedia("(prefers-color-scheme: light)");
      if (typeof colorPreference.addEventListener === "function") colorPreference.addEventListener("change", function () { if (state.settings.theme === "system") applyTheme(); });
    }
  }

  async function init() {
    applyTheme();
    hydrateSettingsForms();
    bindEvents();
    navigate(currentRoute(), false);
    mutationControls();
    try {
      await refreshCsrf();
    } catch (error) {
      setConnected(false, "DSM launch authentication unavailable");
      toast("Read-only launch", boundedText(error.message, "Reopen this app from the DSM desktop."), true);
    }
    refreshSnapshot(false);
  }

  window.SDSyncUI = Object.freeze({
    schemas: Object.freeze({ snapshot: SNAPSHOT_SCHEMA, request: REQUEST_SCHEMA }),
    actions: ACTIONS,
    getActions: GET_ACTIONS,
    argumentKeys: ARGUMENT_KEYS,
    formatBytes,
    formatDate
  });

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init, { once: true });
  else init();
}());
