export const API_URL = "/webman/3rdparty/synology-drive-sync/api.cgi";
export const SNAPSHOT_SCHEMA = "sdsync.dsm-api.v1";
export const REQUEST_SCHEMA = "sdsync.dsm-request.v1";
export const QUEUED_SCHEMA = "sdsync.dsm-queued.v1";
export const RESULT_STATUS_SCHEMA = "sdsync.dsm-result-status.v1";
export const RESULT_SCHEMA = "sdsync.dsm-result.v1";
export const MAX_RESPONSE_BYTES = 1024 * 1024;

const RESULT_POLL_INTERVAL_MS = 2000;
const RESULT_POLL_OBSERVATION_FAILURES = 5;

export class QueuedOutcomeUnknownError extends Error {
  constructor(jobId, message) {
    super(message);
    this.name = "QueuedOutcomeUnknownError";
    this.jobId = jobId;
    this.outcomeUnknown = true;
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

async function responseJson(response, allowGoneResult = false) {
  if (response.redirected) throw new Error("DSM authentication redirected the API request");
  const contentType = response.headers.get("content-type") || "";
  if (!contentType.toLowerCase().includes("application/json")) {
    throw new Error("API did not return JSON");
  }
  const body = await response.text();
  const bodyBytes = typeof window.TextEncoder === "function"
    ? new window.TextEncoder().encode(body).byteLength
    : body.length * 2;
  if (bodyBytes > MAX_RESPONSE_BYTES) {
    throw new Error("API response exceeded the client limit");
  }
  let model;
  try {
    model = JSON.parse(body);
  } catch (_error) {
    throw new Error("API returned malformed JSON");
  }
  if (!model || typeof model !== "object" || Array.isArray(model)) {
    throw new Error("API returned an invalid document");
  }
  assertNoReturnedSecrets(model);
  const allowedGone = allowGoneResult && response.status === 410;
  if ((!response.ok && !allowedGone) || (model.ok === false && !allowedGone)) {
    throw new Error(boundedText(model.message, "API request failed"));
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
  return responseJson(response, action === "result");
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

async function pollJobResult(auth, jobId, pollIntervalMs = RESULT_POLL_INTERVAL_MS) {
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
          "DSM accepted the operation, but its result cannot currently be observed. Do not retry it; inspect Activity and Logs."
        );
      }
      await delay(interval, auth && auth.signal);
      continue;
    }
    consecutiveObservationFailures = 0;
    if (status.schema !== RESULT_STATUS_SCHEMA || status.job_id !== jobId) {
      throw new QueuedOutcomeUnknownError(
        jobId,
        "The queued operation is still outcome-unknown because DSM returned an invalid result document. Do not retry it; inspect Activity and Logs."
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
        )
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
        "The queued operation is outcome-unknown because DSM returned an invalid terminal result. Do not retry it; inspect Activity and Logs."
      );
    }
    if (status.result.ok === false) {
      const failure = new Error(boundedText(status.result.message, "Package operation failed"));
      failure.resultOutput = boundedText(
        status.result.output,
        failure.message
      );
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
  const response = await fetch(API_URL, {
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
  const queued = await responseJson(response);
  if (queued.schema !== QUEUED_SCHEMA
    || queued.state !== "queued"
    || queued.request_id !== id
    || !/^[0-9a-f]{48}$/.test(String(queued.job_id || ""))) {
    throw new Error("API returned an invalid queued-operation document");
  }
  return awaitTerminal ? pollJobResult(auth, queued.job_id, pollIntervalMs) : queued;
}
