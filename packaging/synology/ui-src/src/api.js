export const API_URL = "/webman/3rdparty/synology-drive-sync/api.cgi";
export const SNAPSHOT_SCHEMA = "sdsync.dsm-api.v1";
export const REQUEST_SCHEMA = "sdsync.dsm-request.v1";
export const QUEUED_SCHEMA = "sdsync.dsm-queued.v1";
export const RESULT_STATUS_SCHEMA = "sdsync.dsm-result-status.v1";
export const RESULT_SCHEMA = "sdsync.dsm-result.v1";
export const MAX_RESPONSE_BYTES = 1024 * 1024;

const RESULT_POLL_INTERVAL_MS = 2000;
const RESULT_POLL_OBSERVATION_FAILURES = 5;
const CLIENT_REQUEST_ID_PATTERN = /^[0-9a-f]{32}$/;
const JOB_ID_PATTERN = /^[0-9a-f]{48}$/;

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
  }
}

export const ACTIONS = Object.freeze({
  configureProfile: "configure-profile",
  removeProfile: "remove-profile",
  setDefault: "set-default",
  setSecret: "set-secret",
  schedule: "schedule",
  routine: "routine",
  removeRoutine: "remove-routine",
  alertPolicy: "alert-policy",
  securityPolicy: "security-policy",
  clientEvent: "client-event",
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

export const ARGUMENT_KEYS = Object.freeze({
  "configure-profile": Object.freeze([
    "allow_empty_source", "allow_http", "ca_certificate", "compare",
    "connect_timeout_seconds", "danger_accept_invalid_certs", "delete",
    "excludes", "jobs", "log_level", "make_default", "max_delete",
    "max_rate_bytes_per_second", "name", "quiet", "remote",
    "remote_log_mode", "remote_log_url", "retries", "source",
    "timeout_seconds", "url", "username", "verbosity"
  ]),
  "remove-profile": Object.freeze(["name"]),
  "set-default": Object.freeze(["name"]),
  "set-secret": Object.freeze(["kind", "mode", "profile", "value"]),
  schedule: Object.freeze(["allow_delete", "enabled", "interval_seconds", "max_total_delete"]),
  routine: Object.freeze([
    "action", "allow_delete", "debounce_seconds", "depends_on", "enabled",
    "interval_seconds", "max_total_delete", "mode", "poll_seconds", "profile",
    "retry_backoff_seconds", "retry_count", "time_window_end",
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

function authenticatedHeaders(headers) {
  return Object.assign({}, headers, { "X-SDSYNC-Request": "1" });
}

function exactKeys(actual, expected, label) {
  const keys = Object.keys(actual || {}).sort();
  if (!expected || keys.length !== expected.length || keys.some((key, index) => key !== expected[index])) {
    throw new Error(`${label} arguments do not match the reviewed bridge contract`);
  }
}

function endpoint(action, parameters) {
  const query = new URLSearchParams();
  query.set("action", action);
  Object.keys(parameters || {}).forEach((key) => query.set(key, String(parameters[key])));
  return `${API_URL}?${query.toString()}`;
}

export async function apiGet(auth, action, parameters = {}) {
  if (!GET_ACTIONS.includes(action)) throw new Error("Unsupported API read action");
  exactKeys(parameters, GET_ARGUMENT_KEYS[action], "Read");
  const response = await fetch(endpoint(action, parameters), {
    method: "GET",
    credentials: "same-origin",
    cache: "no-store",
    redirect: "error",
    signal: auth && auth.signal ? auth.signal : undefined,
    headers: authenticatedHeaders({ Accept: "application/json" })
  });
  return responseJson(response, action === "result", true);
}

function delay(milliseconds, signal) {
  return new Promise((resolve, reject) => {
    if (signal && signal.aborted) {
      reject(new Error("DSM UI request was cancelled"));
      return;
    }
    const timer = window.setTimeout(() => {
      if (signal) signal.removeEventListener("abort", cancel);
      resolve();
    }, milliseconds);
    function cancel() {
      window.clearTimeout(timer);
      reject(new Error("DSM UI request was cancelled"));
    }
    if (signal) signal.addEventListener("abort", cancel, { once: true });
  });
}

async function pollJobResult(auth, jobId, pollIntervalMs = RESULT_POLL_INTERVAL_MS, requestId = "") {
  if (!/^[0-9a-f]{48}$/.test(jobId)) {
    throw new Error("API returned an invalid queued job identifier");
  }
  const interval = Number.isFinite(Number(pollIntervalMs)) && Number(pollIntervalMs) >= 0
    ? Number(pollIntervalMs)
    : RESULT_POLL_INTERVAL_MS;
  let consecutiveObservationFailures = 0;
  for (;;) {
    let status;
    try {
      status = await apiGet(auth, "result", { job_id: jobId });
    } catch (error) {
      if (auth && auth.signal && auth.signal.aborted) throw error;
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
      await delay(interval, auth && auth.signal);
      continue;
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
      await delay(interval, auth && auth.signal);
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

export async function apiPost(
  auth,
  csrfToken,
  action,
  payload,
  awaitTerminal = true,
  pollIntervalMs = RESULT_POLL_INTERVAL_MS
) {
  if (!csrfToken) throw new Error("Authenticated DSM mutation bridge is unavailable");
  const expectedKeys = ARGUMENT_KEYS[action];
  if (!expectedKeys) throw new Error("Unsupported API mutation action");
  exactKeys(payload, expectedKeys, "Mutation");

  const id = requestId();
  const request = JSON.stringify({
    schema: REQUEST_SCHEMA,
    request_id: id,
    operation: action,
    arguments: payload
  });
  let response;
  try {
    response = await fetch(API_URL, {
      method: "POST",
      credentials: "same-origin",
      cache: "no-store",
      redirect: "error",
      signal: auth && auth.signal ? auth.signal : undefined,
      headers: authenticatedHeaders({
        Accept: "application/json",
        "Content-Type": "application/json",
        "X-SDSYNC-CSRF": csrfToken
      }),
      body: request
    });
  } catch (_error) {
    throw dispatchedOutcomeUnknown(id);
  }

  let queued;
  try {
    queued = await responseJson(response);
  } catch (error) {
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
  return awaitTerminal ? pollJobResult(auth, queued.job_id, pollIntervalMs, id) : queued;
}
