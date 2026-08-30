export const API_URL = "/webman/3rdparty/synology-drive-sync/api.cgi";
export const DSM_TOKEN_URL = "/webapi/entry.cgi?api=SYNO.API.Auth&version=6&method=token";
export const SNAPSHOT_SCHEMA = "sdsync.dsm-api.v1";
export const REQUEST_SCHEMA = "sdsync.dsm-request.v1";
export const QUEUED_SCHEMA = "sdsync.dsm-queued.v1";
export const RESULT_STATUS_SCHEMA = "sdsync.dsm-result-status.v1";
export const RESULT_SCHEMA = "sdsync.dsm-result.v1";
export const MAX_RESPONSE_BYTES = 1024 * 1024;
export const AUTOSAVE_API_LIMITS = Object.freeze({
  csrfReissueTimeoutMs: 10000,
  postRequestTimeoutMs: 15000,
  postResponseTimeoutMs: 10000,
  readTimeoutMs: 10000,
  resultRequestTimeoutMs: 10000,
  resultObservationTimeoutMs: 30000
});

const CSRF_SCHEMA = "sdsync.dsm-csrf.v1";
const RESULT_POLL_INTERVAL_MS = 2000;
const RESULT_POLL_OBSERVATION_FAILURES = 5;
const MAX_DSM_TOKEN_RESPONSE_BYTES = 16 * 1024;
const MAX_DSM_TOKEN_BYTES = 1024;
const MAX_CSRF_TOKEN_BYTES = 4096;
const DSM_TOKEN_BOOTSTRAP_TIMEOUT_MS = 5000;
const DSM_TOKEN_RETRY_DELAY_MS = 30000;
const CLIENT_REQUEST_ID_PATTERN = /^[0-9a-f]{32}$/;
const JOB_ID_PATTERN = /^[0-9a-f]{48}$/;
const FILE_STATION_CLEANUP_INSPECTION_CODES = new Set([
  "file_station_logout_failed",
  "file_station_denied_logout_failed",
  "file_station_listing_logout_failed",
  "file_station_operation_logout_failed"
]);

let cachedDsmToken = "";
let dsmTokenBootstrapPromise = null;
let dsmTokenRetryAfter = 0;
let dsmAuthGeneration = 0;
const csrfGenerationByAuth = new WeakMap();

function validClientRequestId(value) {
  return typeof value === "string" && CLIENT_REQUEST_ID_PATTERN.test(value) ? value : "";
}

function validJobId(value) {
  return typeof value === "string" && JOB_ID_PATTERN.test(value) ? value : "";
}

export class QueuedOutcomeUnknownError extends Error {
  constructor(jobId, message, requestId = "") {
    super(message);
    this.name = "QueuedOutcomeUnknownError";
    this.jobId = validJobId(jobId);
    this.requestId = validClientRequestId(requestId);
    this.trustedJobId = Boolean(this.jobId);
    this.trustedRequestId = Boolean(this.requestId);
    this.outcomeUnknown = true;
    this.accepted = true;
  }
}

export class MutationOutcomeUnknownError extends Error {
  constructor(requestId, message) {
    super(message);
    this.name = "MutationOutcomeUnknownError";
    this.requestId = validClientRequestId(requestId);
    this.trustedRequestId = Boolean(this.requestId);
    this.outcomeUnknown = true;
    this.acceptanceUnknown = true;
  }
}

export class DsmApiError extends Error {
  constructor(message, status = 0, code = "api_error", stage = "") {
    super(message);
    this.name = "DsmApiError";
    this.status = Number.isInteger(Number(status)) ? Number(status) : 0;
    this.code = boundedText(code, "api_error").slice(0, 128);
    this.stage = boundedText(stage, "").slice(0, 128);
    this.requiresInspection = FILE_STATION_CLEANUP_INSPECTION_CODES.has(this.code);
  }
}

export class ClientRequestTimeoutError extends DsmApiError {
  constructor(message, stage) {
    super(message, 0, "client_timeout", stage);
    this.name = "ClientRequestTimeoutError";
    this.clientTimeout = true;
    this.preAcceptance = true;
  }
}

export const ACTIONS = Object.freeze({
  configureProfile: "configure-profile",
  removeProfile: "remove-profile",
  setDefault: "set-default",
  setSecret: "set-secret",
  testProfileAuth: "test-profile-auth",
  browseRemote: "browse-remote",
  schedule: "schedule",
  routine: "routine",
  removeRoutine: "remove-routine",
  alertPolicy: "alert-policy",
  securityPolicy: "security-policy",
  clientEvent: "client-event",
  execute: "action"
});

const GET_ACTIONS = Object.freeze(["csrf", "snapshot", "source-directories", "source-path", "logs", "activity", "result"]);
const GET_ARGUMENT_KEYS = Object.freeze({
  csrf: Object.freeze([]),
  snapshot: Object.freeze([]),
  "source-directories": Object.freeze(["parent"]),
  "source-path": Object.freeze(["path"]),
  logs: Object.freeze(["lines", "source"]),
  activity: Object.freeze(["lines"]),
  result: Object.freeze(["job_id"])
});

export const ARGUMENT_KEYS = Object.freeze({
  "configure-profile": Object.freeze([
    "allow_empty_source", "allow_http", "ca_certificate", "compare",
    "connect_timeout_seconds", "danger_accept_invalid_certs", "delete",
    "excludes", "jobs", "log_format", "log_level", "make_default", "max_delete",
    "max_rate_bytes_per_second", "name", "output", "progress", "quiet", "remote",
    "remote_log_mode", "remote_log_url", "retries", "source",
    "timeout_seconds", "url", "username", "verbosity"
  ]),
  "remove-profile": Object.freeze(["name"]),
  "set-default": Object.freeze(["name"]),
  "set-secret": Object.freeze(["kind", "mode", "profile", "value"]),
  "test-profile-auth": Object.freeze([
    "allow_http", "ca_certificate", "connect_timeout_seconds",
    "danger_accept_invalid_certs", "password", "password_source", "profile",
    "retries", "timeout_seconds", "totp", "totp_source", "url", "username"
  ]),
  "browse-remote": Object.freeze([
    "allow_http", "ca_certificate", "connect_timeout_seconds", "connection_proof",
    "danger_accept_invalid_certs", "parent", "password", "password_source", "profile",
    "retries", "timeout_seconds", "totp", "totp_source", "url", "username"
  ]),
  schedule: Object.freeze(["allow_delete", "enabled", "interval_seconds", "max_total_delete"]),
  routine: Object.freeze([
    "action", "allow_delete", "debounce_seconds", "depends_on", "enabled",
    "interval_seconds", "max_total_delete", "mode", "poll_seconds", "profile",
    "retry_backoff_seconds", "retry_count", "retry_exponential", "time_window_end",
    "time_window_start", "weekdays"
  ]),
  "remove-routine": Object.freeze(["name"]),
  "alert-policy": Object.freeze([
    "cooldown_seconds", "enabled", "failure_threshold", "on_failure", "on_success"
  ]),
  "security-policy": Object.freeze([
    "allow_destructive_sync", "allow_doctor_write_test", "allow_empty_source", "allow_http_targets",
    "allow_interface_changes", "allow_invalid_tls", "allow_notification_changes",
    "allow_operational_actions", "allow_profile_changes", "allow_remote_logging",
    "allow_routine_changes", "allow_secret_changes", "audit_log_level",
    "authentication_log_level", "bridge_log_level", "configuration_log_level",
    "controller_log_level", "csrf_lifetime_seconds", "max_outstanding_jobs",
    "notifications_log_level", "operations_log_level", "require_https",
    "result_retention_seconds", "routines_log_level", "scheduler_log_level",
    "secrets_log_level", "security_log_level", "sync_log_level"
  ]),
  "client-event": Object.freeze(["event"]),
  action: Object.freeze(["allow_delete", "kind", "max_total_delete", "scope", "write_test"])
});

const ROUTINE_COMMON_ARGUMENT_KEYS = Object.freeze([
  "action", "allow_delete", "depends_on", "enabled", "max_total_delete", "mode", "profile",
  "retry_backoff_seconds", "retry_count", "retry_exponential"
]);
const ROUTINE_MODE_ARGUMENT_KEYS = Object.freeze({
  interval: Object.freeze(["interval_seconds"]),
  daily: Object.freeze(["time_window_end", "time_window_start", "weekdays"]),
  realtime: Object.freeze(["debounce_seconds", "poll_seconds"])
});

export function boundedText(value, fallback = "") {
  const text = typeof value === "string" ? value : fallback;
  return String(text || fallback || "").slice(0, 65536);
}

export function arrayOf(value) {
  return Array.isArray(value) ? value : [];
}

export function numberOr(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

export function pick(model, ...keys) {
  if (!model || typeof model !== "object") return undefined;
  const key = keys.find((candidate) => Object.prototype.hasOwnProperty.call(model, candidate));
  return key === undefined ? undefined : model[key];
}

export function formatDate(value) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric) || numeric <= 0) return "Unavailable";
  const milliseconds = numeric < 100000000000 ? numeric * 1000 : numeric;
  try {
    return new Intl.DateTimeFormat(undefined, {
      dateStyle: "medium",
      timeStyle: "short"
    }).format(new Date(milliseconds));
  } catch (_error) {
    return "Unavailable";
  }
}

export function formatDuration(milliseconds) {
  const value = Number(milliseconds);
  if (!Number.isFinite(value) || value < 0) return "Unavailable";
  if (value < 1000) return `${Math.round(value)} ms`;
  return `${(value / 1000).toFixed(value < 10000 ? 1 : 0)} s`;
}

export function formatBytes(value) {
  const bytes = Number(value);
  if (!Number.isFinite(bytes) || bytes < 0) return "Unavailable";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let scaled = bytes;
  let index = 0;
  while (scaled >= 1024 && index < units.length - 1) {
    scaled /= 1024;
    index += 1;
  }
  return `${scaled.toFixed(index === 0 ? 0 : 1)} ${units[index]}`;
}

function assertNoReturnedSecrets(model) {
  const forbidden = new Set([
    "password", "totp", "remote_log_token", "remote-log-token", "secret", "secret_value"
  ]);
  const pending = [model];
  let visited = 0;
  while (pending.length) {
    const value = pending.pop();
    visited += 1;
    if (visited > 20000) throw new Error("API response is too complex");
    if (!value || typeof value !== "object") continue;
    Object.keys(value).forEach((key) => {
      if (forbidden.has(key.toLowerCase())) {
        throw new Error("API returned forbidden secret material");
      }
      const child = value[key];
      if (child && typeof child === "object") pending.push(child);
    });
  }
}

async function responseJson(response, allowGoneResult = false, preserveSemanticStatus = false) {
  if (response.redirected) {
    throw new DsmApiError("DSM authentication redirected the API request", response.status, "authentication_redirect");
  }
  const contentType = response.headers.get("content-type") || "";
  if (!contentType.toLowerCase().includes("application/json")) {
    throw new DsmApiError("API did not return JSON", response.status, "non_json_response");
  }
  const body = await response.text();
  const bodyBytes = typeof window.TextEncoder === "function"
    ? new window.TextEncoder().encode(body).byteLength
    : body.length * 2;
  if (bodyBytes > MAX_RESPONSE_BYTES) {
    throw new DsmApiError("API response exceeded the client limit", response.status, "response_too_large");
  }
  let model;
  try {
    model = JSON.parse(body);
  } catch (_error) {
    throw new DsmApiError("API returned malformed JSON", response.status, "malformed_json");
  }
  if (!model || typeof model !== "object" || Array.isArray(model)) {
    throw new DsmApiError("API returned an invalid document", response.status, "invalid_document");
  }
  assertNoReturnedSecrets(model);
  const semanticStatus = model.status;
  const validSemanticStatus = typeof semanticStatus === "number"
    && Number.isInteger(semanticStatus)
    && semanticStatus >= 400
    && semanticStatus < 600;
  const trustedErrorDocument = model.schema === "sdsync.dsm-error.v1"
    && model.ok === false
    && typeof model.code === "string"
    && typeof model.message === "string";
  const trustedSemanticError = preserveSemanticStatus && trustedErrorDocument;
  const effectiveStatus = preserveSemanticStatus
    && trustedSemanticError
    && validSemanticStatus
    ? semanticStatus
    : response.status;
  const allowedGone = allowGoneResult
    && response.status === 410
    && model.schema === RESULT_STATUS_SCHEMA
    && model.state === "expired_or_missing"
    && model.ok !== false;
  if ((!response.ok && !allowedGone) || (model.ok === false && !allowedGone)) {
    const error = new DsmApiError(
      boundedText(model.message, "API request failed"),
      effectiveStatus,
      boundedText(model.code, `http_${effectiveStatus || 0}`),
      boundedText(model.stage, "")
    );
    error.transportStatus = response.status;
    error.trustedRejection = effectiveStatus >= 400
      && effectiveStatus < 600
      && trustedErrorDocument;
    throw error;
  }
  return model;
}

function boundedUtf8Length(value) {
  if (window.TextEncoder && typeof window.TextEncoder === "function") {
    return new window.TextEncoder().encode(value).byteLength;
  }
  // A JavaScript UTF-16 code unit occupies at most three UTF-8 bytes for the
  // JSON syntax and token alphabet accepted below. This conservative fallback
  // can reject an unusual response early but can never relax the byte bound.
  return value.length * 3;
}

function normalizeDsmToken(value) {
  if (typeof value !== "string" || !value || boundedUtf8Length(value) > MAX_DSM_TOKEN_BYTES) {
    return "";
  }
  try {
    const encoded = encodeURIComponent(value);
    return encoded.length <= MAX_DSM_TOKEN_BYTES && /^[\x21-\x7e]+$/.test(encoded)
      ? encoded
      : "";
  } catch (_error) {
    return "";
  }
}

async function bootstrapDsmToken() {
  if (typeof window !== "object" || typeof window.fetch !== "function") return "";

  const controller = typeof window.AbortController === "function"
    ? new window.AbortController()
    : null;
  const timeout = controller && typeof window.setTimeout === "function"
    ? window.setTimeout(() => controller.abort(), DSM_TOKEN_BOOTSTRAP_TIMEOUT_MS)
    : null;
  try {
    const response = await window.fetch(DSM_TOKEN_URL, {
      method: "GET",
      credentials: "same-origin",
      cache: "no-store",
      redirect: "error",
      signal: controller ? controller.signal : undefined,
      headers: { Accept: "application/json" }
    });
    if (response.redirected || response.status !== 200 || !response.ok) return "";
    const contentType = response.headers.get("content-type") || "";
    if (!contentType.toLowerCase().includes("application/json")) return "";
    const declaredLength = response.headers.get("content-length");
    if (declaredLength !== null) {
      if (!/^[1-9][0-9]*$/.test(declaredLength)) return "";
      const length = Number(declaredLength);
      if (!Number.isSafeInteger(length) || length > MAX_DSM_TOKEN_RESPONSE_BYTES) return "";
    }
    const body = await response.text();
    if (!body || boundedUtf8Length(body) > MAX_DSM_TOKEN_RESPONSE_BYTES) return "";
    let model;
    try {
      model = JSON.parse(body);
    } catch (_error) {
      return "";
    }
    if (!model || typeof model !== "object" || Array.isArray(model) || model.success !== true) {
      return "";
    }
    const data = model.data;
    if (!data || typeof data !== "object" || Array.isArray(data)) return "";
    return normalizeDsmToken(data.synotoken);
  } catch (_error) {
    return "";
  } finally {
    if (timeout !== null && typeof window.clearTimeout === "function") {
      window.clearTimeout(timeout);
    }
  }
}

async function ensureDsmToken() {
  if (cachedDsmToken) return cachedDsmToken;
  if (dsmTokenBootstrapPromise) return dsmTokenBootstrapPromise;
  if (Date.now() < dsmTokenRetryAfter) return "";

  dsmTokenBootstrapPromise = bootstrapDsmToken()
    .then((token) => {
      if (token) {
        if (token !== cachedDsmToken) {
          cachedDsmToken = token;
          dsmAuthGeneration += 1;
        }
      } else dsmTokenRetryAfter = Date.now() + DSM_TOKEN_RETRY_DELAY_MS;
      return token;
    })
    .finally(() => { dsmTokenBootstrapPromise = null; });
  return dsmTokenBootstrapPromise;
}

function dsmAuthSnapshot() {
  return {
    token: cachedDsmToken,
    generation: dsmAuthGeneration
  };
}

function authenticatedHeaders(headers, dsmAuth) {
  const authenticated = Object.assign({}, headers, { "X-SDSYNC-Request": "1" });
  if (dsmAuth.token) authenticated["X-SYNO-TOKEN"] = dsmAuth.token;
  return authenticated;
}

function validCsrfModel(model) {
  return model
    && model.schema === CSRF_SCHEMA
    && typeof model.csrf_token === "string"
    && model.csrf_token.length > 0
    && model.csrf_token.length <= MAX_CSRF_TOKEN_BYTES;
}

function rememberCsrfGeneration(auth, model, generation) {
  if (auth && typeof auth === "object" && validCsrfModel(model)) {
    csrfGenerationByAuth.set(auth, generation);
  }
}

function exactKeys(actual, expected, label) {
  const keys = Object.keys(actual || {}).sort();
  if (!expected || keys.length !== expected.length || keys.some((key, index) => key !== expected[index])) {
    throw new Error(`${label} arguments do not match the reviewed bridge contract`);
  }
}

function normalizedRequestLimits(options) {
  if (options === undefined || options === null) return null;
  if (!options || typeof options !== "object" || Array.isArray(options)) {
    throw new TypeError("API request limits must be an object");
  }
  const timeoutKeys = [
    "csrfReissueTimeoutMs", "postRequestTimeoutMs", "postResponseTimeoutMs",
    "readTimeoutMs", "resultRequestTimeoutMs", "resultObservationTimeoutMs"
  ];
  const supportedKeys = new Set([...timeoutKeys, "setTimeout", "clearTimeout"]);
  if (Object.keys(options).some((key) => !supportedKeys.has(key))) {
    throw new TypeError("API request limits contain an unsupported option");
  }
  const normalized = {};
  timeoutKeys.forEach((key) => {
    const value = Number(options[key]);
    if (!Number.isInteger(value) || value < 1 || value > 600000) {
      throw new TypeError(`${key} must be an integer from 1 through 600000 milliseconds`);
    }
    normalized[key] = value;
  });
  if (options.setTimeout !== undefined && typeof options.setTimeout !== "function") {
    throw new TypeError("API request setTimeout option must be a function");
  }
  if (options.clearTimeout !== undefined && typeof options.clearTimeout !== "function") {
    throw new TypeError("API request clearTimeout option must be a function");
  }
  normalized.setTimer = typeof options.setTimeout === "function"
    ? options.setTimeout
    : (callback, milliseconds) => window.setTimeout(callback, milliseconds);
  normalized.clearTimer = typeof options.clearTimeout === "function"
    ? options.clearTimeout
    : (timer) => window.clearTimeout(timer);
  if ((options.setTimeout === undefined) !== (options.clearTimeout === undefined)) {
    throw new TypeError("API request limits must provide both timer functions or neither");
  }
  return normalized;
}

function withinLimit(
  promise,
  milliseconds,
  limits,
  errorFactory,
  onTimeout = null,
  cancellation = null
) {
  return new Promise((resolve, reject) => {
    if (cancellation && cancellation.expired) {
      reject(errorFactory());
      return;
    }
    let settled = false;
    const clearCancellation = () => {
      if (cancellation && cancellation.cancelCurrent === cancel) cancellation.cancelCurrent = null;
    };
    const cancel = () => {
      if (settled) return;
      settled = true;
      limits.clearTimer(timer);
      clearCancellation();
      reject(errorFactory());
    };
    const timer = limits.setTimer(() => {
      if (settled) return;
      settled = true;
      clearCancellation();
      if (onTimeout) onTimeout();
      reject(errorFactory());
    }, milliseconds);
    if (cancellation) cancellation.cancelCurrent = cancel;
    Promise.resolve(promise).then(
      (value) => {
        if (settled) return;
        settled = true;
        limits.clearTimer(timer);
        clearCancellation();
        resolve(value);
      },
      (error) => {
        if (settled) return;
        settled = true;
        limits.clearTimer(timer);
        clearCancellation();
        reject(error);
      }
    );
  });
}

function safeReadTimeout(stage, detail) {
  return new ClientRequestTimeoutError(detail, stage);
}

function queuedObservationTimeout(jobId, requestId, detail) {
  return new QueuedOutcomeUnknownError(
    jobId,
    `${detail} Do not retry it; inspect Activity and Logs.`,
    requestId
  );
}

function exactMutationKeys(action, payload) {
  if (action !== ACTIONS.routine) {
    exactKeys(payload, ARGUMENT_KEYS[action], "Mutation");
    return;
  }
  const modeKeys = payload && ROUTINE_MODE_ARGUMENT_KEYS[payload.mode];
  if (!modeKeys) {
    throw new Error("Mutation arguments do not match the reviewed bridge contract");
  }
  exactKeys(
    payload,
    [...ROUTINE_COMMON_ARGUMENT_KEYS, ...modeKeys].sort(),
    "Mutation"
  );
}

function endpoint(action, parameters) {
  const query = new URLSearchParams();
  query.set("action", action);
  Object.keys(parameters || {}).forEach((key) => query.set(key, String(parameters[key])));
  return `${API_URL}?${query.toString()}`;
}

async function apiGetWithDsmAuth(auth, action, parameters, dsmAuth, rememberGeneration = true) {
  const response = await fetch(endpoint(action, parameters), {
    method: "GET",
    credentials: "same-origin",
    cache: "no-store",
    redirect: "error",
    signal: auth && auth.signal ? auth.signal : undefined,
    headers: authenticatedHeaders({ Accept: "application/json" }, dsmAuth)
  });
  const model = await responseJson(response, action === "result", true);
  if (rememberGeneration && action === "csrf") {
    rememberCsrfGeneration(auth, model, dsmAuth.generation);
  }
  return model;
}

export async function apiGet(auth, action, parameters = {}, options = undefined) {
  if (!GET_ACTIONS.includes(action)) throw new Error("Unsupported API read action");
  exactKeys(parameters, GET_ARGUMENT_KEYS[action], "Read");
  const limits = normalizedRequestLimits(options);
  await ensureDsmToken();
  const requestDsmAuth = dsmAuthSnapshot();
  const deferredCsrfGeneration = Boolean(limits && action === "csrf");
  const request = apiGetWithDsmAuth(
    auth,
    action,
    parameters,
    requestDsmAuth,
    !deferredCsrfGeneration
  );
  if (!limits) return request;
  const model = await withinLimit(
      request,
      limits.readTimeoutMs,
      limits,
      () => safeReadTimeout("read_observation", "DSM did not return a complete read response within the autosave limit.")
  );
  if (deferredCsrfGeneration) {
    rememberCsrfGeneration(auth, model, requestDsmAuth.generation);
  }
  return model;
}

function delay(milliseconds, signal, limits = null, cancellation = null) {
  return new Promise((resolve, reject) => {
    if (signal && signal.aborted) {
      reject(new Error("DSM UI request was cancelled"));
      return;
    }
    if (cancellation && cancellation.expired) {
      reject(new Error("DSM result observation was cancelled after its autosave limit elapsed"));
      return;
    }
    const setTimer = limits ? limits.setTimer : (callback, delayMs) => window.setTimeout(callback, delayMs);
    const clearTimer = limits ? limits.clearTimer : (value) => window.clearTimeout(value);
    let settled = false;
    const cleanup = () => {
      if (signal) signal.removeEventListener("abort", cancel);
      if (cancellation && cancellation.cancelCurrent === cancelObservation) cancellation.cancelCurrent = null;
    };
    const timer = setTimer(() => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve();
    }, milliseconds);
    function cancel() {
      if (settled) return;
      settled = true;
      clearTimer(timer);
      cleanup();
      reject(new Error("DSM UI request was cancelled"));
    }
    function cancelObservation() {
      if (settled) return;
      settled = true;
      clearTimer(timer);
      cleanup();
      reject(new Error("DSM result observation was cancelled after its autosave limit elapsed"));
    }
    if (signal) signal.addEventListener("abort", cancel, { once: true });
    if (cancellation) cancellation.cancelCurrent = cancelObservation;
  });
}

async function pollJobResult(
  auth,
  jobId,
  dsmAuth,
  pollIntervalMs = RESULT_POLL_INTERVAL_MS,
  requestId = "",
  limits = null,
  observation = null
) {
  if (!/^[0-9a-f]{48}$/.test(jobId)) {
    throw new Error("API returned an invalid queued job identifier");
  }
  const interval = Number.isFinite(Number(pollIntervalMs)) && Number(pollIntervalMs) >= 0
    ? Number(pollIntervalMs)
    : RESULT_POLL_INTERVAL_MS;
  let consecutiveObservationFailures = 0;
  for (;;) {
    if (observation && observation.expired) {
      throw queuedObservationTimeout(
        jobId,
        requestId,
        "DSM accepted the operation, but terminal result observation exceeded the autosave limit."
      );
    }
    let status;
    try {
      const request = apiGetWithDsmAuth(auth, "result", { job_id: jobId }, dsmAuth);
      status = limits
        ? await withinLimit(
          request,
          limits.resultRequestTimeoutMs,
          limits,
          () => queuedObservationTimeout(
            jobId,
            requestId,
            "DSM accepted the operation, but a terminal result request exceeded the autosave limit."
          ),
          null,
          observation
        )
        : await request;
    } catch (error) {
      if (auth && auth.signal && auth.signal.aborted) throw error;
      if (error instanceof QueuedOutcomeUnknownError) throw error;
      consecutiveObservationFailures += 1;
      // The POST was already accepted. Repeated transport/auth observation
      // failures are not evidence that the queued mutation failed, so surface
      // an explicit outcome-unknown state rather than inviting a duplicate.
      if (consecutiveObservationFailures >= RESULT_POLL_OBSERVATION_FAILURES) {
        throw new QueuedOutcomeUnknownError(
          jobId,
          "DSM accepted the operation, but its result cannot currently be observed. Do not retry it; inspect Activity and Logs.",
          requestId
        );
      }
      await delay(interval, auth && auth.signal, limits, observation);
      continue;
    }
    if (observation && observation.expired) {
      throw queuedObservationTimeout(
        jobId,
        requestId,
        "DSM accepted the operation, but terminal result observation exceeded the autosave limit."
      );
    }
    consecutiveObservationFailures = 0;
    if (status.schema !== RESULT_STATUS_SCHEMA || status.job_id !== jobId) {
      throw new QueuedOutcomeUnknownError(
        jobId,
        "The queued operation is still outcome-unknown because DSM returned an invalid result document. Do not retry it; inspect Activity and Logs.",
        requestId
      );
    }
    if (status.state === "pending") {
      await delay(interval, auth && auth.signal, limits, observation);
      continue;
    }
    if (status.state === "expired_or_missing") {
      throw new QueuedOutcomeUnknownError(
        jobId,
        boundedText(
          status.result && status.result.message,
          "The queued result is no longer available. Do not retry it; inspect Activity and Logs."
        ),
        requestId
      );
    }
    if (status.state !== "complete"
      || !status.result
      || typeof status.result !== "object"
      || Array.isArray(status.result)
      || status.result.schema !== RESULT_SCHEMA
      || (status.result.ok !== true && status.result.ok !== false)) {
      throw new QueuedOutcomeUnknownError(
        jobId,
        "The queued operation is outcome-unknown because DSM returned an invalid terminal result. Do not retry it; inspect Activity and Logs.",
        requestId
      );
    }
    if (status.result.ok === false) {
      const failure = new DsmApiError(
        boundedText(status.result.message, "Package operation failed"),
        200,
        boundedText(status.result.code, "operation_failed")
      );
      failure.resultOutput = boundedText(
        status.result.output,
        failure.message
      );
      failure.jobId = jobId;
      failure.requestId = validClientRequestId(requestId);
      failure.trustedJobId = true;
      failure.trustedRequestId = Boolean(failure.requestId);
      failure.accepted = true;
      throw failure;
    }
    return status.result;
  }
}

function requestId() {
  if (!window.crypto || typeof window.crypto.getRandomValues !== "function") {
    throw new Error("Secure browser randomness is unavailable");
  }
  const random = new Uint8Array(16);
  window.crypto.getRandomValues(random);
  return Array.from(random)
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
}

function isExplicitCsrfRejection(error) {
  if (!(error instanceof DsmApiError) || error.status !== 403 || error.trustedRejection !== true) {
    return false;
  }
  const code = String(error.code || "").toLowerCase().replace(/-/g, "_");
  const message = String(error.message || "").toLowerCase();
  return ["csrf_rejected", "csrf_expired", "csrf_invalid", "invalid_csrf"].includes(code)
    || /\bcsrf\b|cross[- ]site request forgery|mutation token/.test(message);
}

function dispatchedOutcomeUnknown(id) {
  return new MutationOutcomeUnknownError(
    id,
    `DSM may have accepted client request ${id}, but no trustworthy rejection or queue acknowledgement was received. Do not retry it automatically; inspect Activity and Logs using this request ID.`
  );
}

async function csrfForCurrentAuthGeneration(auth, csrfToken, dsmAuth, limits = null) {
  if (!auth || typeof auth !== "object") return csrfToken;
  const issuedGeneration = csrfGenerationByAuth.get(auth);
  if (issuedGeneration === undefined || issuedGeneration === dsmAuth.generation) return csrfToken;

  const previousToken = csrfToken;
  // A timed-out reissue may still settle later at the fetch layer. Do not let
  // that detached response advance the accepted generation without also
  // delivering its replacement token to the AppWindow.
  const request = apiGetWithDsmAuth(auth, "csrf", {}, dsmAuth, false);
  const model = limits
    ? await withinLimit(
      request,
      limits.csrfReissueTimeoutMs,
      limits,
      () => safeReadTimeout(
        "csrf_reissue",
        "DSM request authentication could not be refreshed within the autosave limit; no mutation request was sent."
      )
    )
    : await request;
  if (!validCsrfModel(model)) {
    throw new Error("Authenticated bridge did not reissue a valid CSRF token");
  }
  rememberCsrfGeneration(auth, model, dsmAuth.generation);
  const replacementToken = model.csrf_token;
  if (typeof auth.onCsrfReissued === "function") {
    auth.onCsrfReissued(previousToken, replacementToken);
  }
  return replacementToken;
}

export async function apiPost(
  auth,
  csrfToken,
  action,
  payload,
  awaitTerminal = true,
  pollIntervalMs = RESULT_POLL_INTERVAL_MS,
  options = undefined
) {
  if (!csrfToken) throw new Error("Authenticated DSM mutation bridge is unavailable");
  const expectedKeys = ARGUMENT_KEYS[action];
  if (!expectedKeys) throw new Error("Unsupported API mutation action");
  exactMutationKeys(action, payload);
  const limits = normalizedRequestLimits(options);

  await ensureDsmToken();
  const requestDsmAuth = dsmAuthSnapshot();
  const effectiveCsrfToken = await csrfForCurrentAuthGeneration(auth, csrfToken, requestDsmAuth, limits);
  const id = requestId();
  const request = JSON.stringify({
    schema: REQUEST_SCHEMA,
    request_id: id,
    operation: action,
    arguments: payload
  });
  let response;
  try {
    const dispatched = fetch(API_URL, {
      method: "POST",
      credentials: "same-origin",
      cache: "no-store",
      redirect: "error",
      signal: auth && auth.signal ? auth.signal : undefined,
      headers: authenticatedHeaders({
        Accept: "application/json",
        "Content-Type": "application/json",
        "X-SDSYNC-CSRF": effectiveCsrfToken
      }, requestDsmAuth),
      body: request
    });
    response = limits
      ? await withinLimit(
        dispatched,
        limits.postRequestTimeoutMs,
        limits,
        () => dispatchedOutcomeUnknown(id)
      )
      : await dispatched;
  } catch (_error) {
    throw dispatchedOutcomeUnknown(id);
  }

  let queued;
  try {
    const body = responseJson(response);
    queued = limits
      ? await withinLimit(
        body,
        limits.postResponseTimeoutMs,
        limits,
        () => dispatchedOutcomeUnknown(id)
      )
      : await body;
  } catch (error) {
    if (error instanceof MutationOutcomeUnknownError) throw error;
    if (error instanceof DsmApiError && error.trustedRejection === true) {
      error.preAcceptance = true;
      error.requestId = id;
      error.trustedRequestId = true;
      if (isExplicitCsrfRejection(error)) error.csrfRejected = true;
      throw error;
    }
    throw dispatchedOutcomeUnknown(id);
  }
  if (queued.schema !== QUEUED_SCHEMA
    || queued.state !== "queued"
    || queued.request_id !== id
    || !/^[0-9a-f]{48}$/.test(String(queued.job_id || ""))) {
    throw dispatchedOutcomeUnknown(id);
  }
  if (!awaitTerminal) return queued;
  if (!limits) return pollJobResult(auth, queued.job_id, requestDsmAuth, pollIntervalMs, id);
  const observation = { expired: false, cancelCurrent: null };
  return withinLimit(
    pollJobResult(auth, queued.job_id, requestDsmAuth, pollIntervalMs, id, limits, observation),
    limits.resultObservationTimeoutMs,
    limits,
    () => queuedObservationTimeout(
      queued.job_id,
      id,
      "DSM accepted the operation, but terminal result observation exceeded the autosave limit."
    ),
    () => {
      observation.expired = true;
      if (observation.cancelCurrent) observation.cancelCurrent();
    }
  );
}
