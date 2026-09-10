export const API_URL = "/webman/3rdparty/synology-drive-sync/api.cgi";
export const DSM_TOKEN_URL = "/webapi/entry.cgi?api=SYNO.API.Auth&version=6&method=token";
export const SNAPSHOT_SCHEMA = "sdsync.dsm-api.v1";
export const REQUEST_SCHEMA = "sdsync.dsm-request.v1";
export const QUEUED_SCHEMA = "sdsync.dsm-queued.v1";
export const RESULT_STATUS_SCHEMA = "sdsync.dsm-result-status.v1";
export const RESULT_SCHEMA = "sdsync.dsm-result.v1";
export const REQUEST_STATUS_SCHEMA = "sdsync.dsm-request-status.v1";
export const MAX_RESPONSE_BYTES = 1024 * 1024;
export const AUTOSAVE_API_LIMITS = Object.freeze({
  csrfReissueTimeoutMs: 10000,
  // The CGI relay has a 30-second I/O ceiling. The browser must not cancel a
  // request while that bounded backend hand-off can still complete.
  postRequestTimeoutMs: 45000,
  postResponseTimeoutMs: 10000,
  readTimeoutMs: 10000,
  resultRequestTimeoutMs: 10000,
  resultObservationTimeoutMs: 30000,
  requestReconciliationTimeoutMs: 30000,
  requestReconciliationPollIntervalMs: 1000
});

const TERMINAL_API_ATTEMPT_TIMEOUTS = Object.freeze({
  csrfReissueTimeoutMs: 10000,
  postRequestTimeoutMs: 45000,
  postResponseTimeoutMs: 10000,
  readTimeoutMs: 10000,
  resultRequestTimeoutMs: 10000,
  requestReconciliationTimeoutMs: 30000,
  requestReconciliationPollIntervalMs: 1000
});

const CSRF_SCHEMA = "sdsync.dsm-csrf.v1";
// Terminal reads are side-effect free, so the first observations can be cheap and
// frequent and then decay toward the bounded long-operation cadence that Doctor and
// other slow jobs need. The previous two-step ladder (500 ms once, then 2000 ms flat)
// meant a job finishing at 601 ms was invisible until 2500 ms; a job that finished
// just after the first poll paid the full long-operation interval for it.
const RESULT_POLL_RAMP_MS = Object.freeze([150, 300, 600, 1200, 2000]);
const RESULT_POLL_INTERVAL_MS = RESULT_POLL_RAMP_MS[RESULT_POLL_RAMP_MS.length - 1];
// How long observation may go on failing before the outcome is declared unknown.
//
// This is a duration and not an attempt count, and the distinction is the whole
// point. A count is only a proxy for time, and it is a proxy whose scale moves with
// the poll interval: five attempts was ten seconds of tolerance at 2000 ms, but would
// be 750 ms once the ramp starts at 150 ms -- a thirteen-fold cut in how much
// transport trouble is survivable, arriving silently as a side effect of polling
// faster. Ten seconds preserves the tolerance the count was chosen to express.
const RESULT_OBSERVATION_FAILURE_WINDOW_MS = 10000;
// ...but a single anomalous request must not end observation on its own, however long
// it took. A request may itself burn ten seconds before failing, so the window alone
// could be spent by one hung read; requiring a second independent failure keeps that
// from being mistaken for sustained loss of observability.
const RESULT_OBSERVATION_MIN_FAILURES = 2;
const POST_DISPATCH_REPLAY_DELAYS_MS = Object.freeze([250, 1000]);
const POST_DISPATCH_MAX_ATTEMPTS = POST_DISPATCH_REPLAY_DELAYS_MS.length + 1;
const MAX_DSM_TOKEN_RESPONSE_BYTES = 16 * 1024;
const MAX_DSM_TOKEN_BYTES = 1024;
const MAX_CSRF_TOKEN_BYTES = 4096;
const DSM_TOKEN_BOOTSTRAP_TIMEOUT_MS = 5000;
const DSM_TOKEN_RETRY_DELAY_MS = 30000;
const MAX_RECONCILIATION_AUTH_ENTRIES = 64;
// The backend permits result retention as low as 300 seconds. A request's DSM
// token snapshot must never remain eligible beyond that observable lifetime.
const RECONCILIATION_AUTH_TTL_MS = 5 * 60 * 1000;
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
const reconciliationAuthByOwner = new WeakMap();

function validClientRequestId(value) {
  return typeof value === "string" && CLIENT_REQUEST_ID_PATTERN.test(value) ? value : "";
}

function validJobId(value) {
  return typeof value === "string" && JOB_ID_PATTERN.test(value) ? value : "";
}

function reconciliationAuthOwner(auth) {
  return auth && (typeof auth === "object" || typeof auth === "function") ? auth : null;
}

function reconciliationAuthNow() {
  const monotonic = typeof window === "object"
    && window.performance
    && typeof window.performance.now === "function"
    ? Number(window.performance.now())
    : NaN;
  return Number.isFinite(monotonic) && monotonic >= 0 ? monotonic : Date.now();
}

function reconciliationAuthExpired(remembered, now) {
  return !remembered
    || !Number.isFinite(remembered.createdAt)
    || !Number.isFinite(remembered.expiresAt)
    || now < remembered.createdAt
    || now >= remembered.expiresAt;
}

function clearReconciliationAuthEntry(entries, requestId) {
  const remembered = entries.get(requestId);
  if (!remembered) return;
  remembered.token = "";
  entries.delete(requestId);
}

function rememberReconciliationAuth(auth, requestId, dsmAuth) {
  const owner = reconciliationAuthOwner(auth);
  if (!owner || !validClientRequestId(requestId) || !dsmAuth || typeof dsmAuth !== "object") return;
  let entries = reconciliationAuthByOwner.get(owner);
  if (!entries) {
    entries = new Map();
    reconciliationAuthByOwner.set(owner, entries);
  }
  const now = reconciliationAuthNow();
  for (const [retainedRequestId, remembered] of entries) {
    if (reconciliationAuthExpired(remembered, now)) {
      clearReconciliationAuthEntry(entries, retainedRequestId);
    }
  }
  clearReconciliationAuthEntry(entries, requestId);
  entries.set(requestId, {
    token: typeof dsmAuth.token === "string" ? dsmAuth.token : "",
    generation: Number.isSafeInteger(dsmAuth.generation) ? dsmAuth.generation : 0,
    createdAt: now,
    expiresAt: now + RECONCILIATION_AUTH_TTL_MS
  });
  while (entries.size > MAX_RECONCILIATION_AUTH_ENTRIES) {
    const oldest = entries.keys().next().value;
    clearReconciliationAuthEntry(entries, oldest);
  }
}

function rememberedReconciliationAuth(auth, requestId) {
  const owner = reconciliationAuthOwner(auth);
  const entries = owner ? reconciliationAuthByOwner.get(owner) : null;
  const remembered = entries ? entries.get(requestId) : null;
  if (remembered && reconciliationAuthExpired(remembered, reconciliationAuthNow())) {
    clearReconciliationAuthEntry(entries, requestId);
    if (entries.size === 0) reconciliationAuthByOwner.delete(owner);
    return null;
  }
  return remembered ? { token: remembered.token, generation: remembered.generation } : null;
}

function forgetReconciliationAuth(auth, requestId) {
  const owner = reconciliationAuthOwner(auth);
  const entries = owner ? reconciliationAuthByOwner.get(owner) : null;
  if (!entries) return;
  clearReconciliationAuthEntry(entries, requestId);
  if (entries.size === 0) reconciliationAuthByOwner.delete(owner);
}

export function purgeReconciliationAuth(auth) {
  const owner = reconciliationAuthOwner(auth);
  const entries = owner ? reconciliationAuthByOwner.get(owner) : null;
  if (!entries) return;
  for (const requestId of [...entries.keys()]) {
    clearReconciliationAuthEntry(entries, requestId);
  }
  reconciliationAuthByOwner.delete(owner);
}

export class QueuedOutcomeUnknownError extends Error {
  constructor(jobId, message, requestId = "", operation = "", stage = "result_observation") {
    super(message);
    this.name = "QueuedOutcomeUnknownError";
    this.jobId = validJobId(jobId);
    this.requestId = validClientRequestId(requestId);
    this.trustedJobId = Boolean(this.jobId);
    this.trustedRequestId = Boolean(this.requestId);
    this.operation = typeof operation === "string" && ARGUMENT_KEYS[operation] ? operation : "";
    this.stage = boundedText(stage, "result_observation").slice(0, 128);
    this.outcomeUnknown = true;
    this.accepted = true;
  }
}

/**
 * The observation deadline elapsed while the package was still, verifiably,
 * working through its queue.
 *
 * Deliberately not a `QueuedOutcomeUnknownError`. The two describe different
 * facts and call for opposite actions. Outcome-unknown means the result could
 * not be observed: something may have gone wrong and a person should go and
 * look. This means the last *trusted* read said `pending` -- the package
 * answered, repeatedly and successfully, that the job is queued -- and the only
 * thing that ran out was the browser's thirty seconds of patience. A save that
 * queues behind a running sync is the ordinary case, and telling an operator of
 * a backup tool that we do not know whether their configuration was saved, when
 * the answer is "yes, in about a minute", is an expensive thing to say wrongly.
 *
 * `accepted` is true and `outcomeUnknown` is false: the mutation is on the
 * queue with a known job ID, and nothing may re-send it.
 */
export class QueuedStillPendingError extends Error {
  constructor(jobId, message, requestId = "", operation = "", progress = null) {
    super(message);
    this.name = "QueuedStillPendingError";
    this.jobId = validJobId(jobId);
    this.requestId = validClientRequestId(requestId);
    this.trustedJobId = Boolean(this.jobId);
    this.trustedRequestId = Boolean(this.requestId);
    this.operation = typeof operation === "string" && ARGUMENT_KEYS[operation] ? operation : "";
    this.stage = "result_observation";
    this.progress = progress && typeof progress === "object" ? progress : null;
    this.accepted = true;
    this.outcomeUnknown = false;
    this.stillPending = true;
  }
}

export class MutationOutcomeUnknownError extends Error {
  constructor(requestId, message, operation = "", stage = "post_dispatch_observation") {
    super(message);
    this.name = "MutationOutcomeUnknownError";
    this.requestId = validClientRequestId(requestId);
    this.trustedRequestId = Boolean(this.requestId);
    this.operation = typeof operation === "string" && ARGUMENT_KEYS[operation] ? operation : "";
    this.stage = boundedText(stage, "post_dispatch_observation").slice(0, 128);
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
  // Both of these are queued jobs rather than reads. A status query walks the
  // local tree and enumerates the remote one over the network, so it is a job
  // wearing a read's clothes: on the synchronous path it would occupy a worker
  // for as long as the destination takes to answer, and block the queue behind
  // it. See ARGUMENT_KEYS below for the two-phase resync contract.
  syncStatus: "sync-status",
  resync: "resync",
  // Emptying a bounded log is destructive package state, so it travels the
  // queued mutation path and is audited like any other, rather than riding on
  // the read-only `logs` action it refreshes.
  clearLogs: "clear-logs",
  execute: "action"
});

const GET_ACTIONS = Object.freeze(["csrf", "snapshot", "source-directories", "source-path", "logs", "activity", "status-rollup", "result", "request-status"]);
const GET_ARGUMENT_KEYS = Object.freeze({
  csrf: Object.freeze([]),
  snapshot: Object.freeze([]),
  "source-directories": Object.freeze(["parent"]),
  "source-path": Object.freeze(["path"]),
  logs: Object.freeze(["lines", "source"]),
  activity: Object.freeze(["lines"]),
  // No arguments: the manager derives the expected profile set, so the browser
  // cannot narrow what the combined total is taken over.
  "status-rollup": Object.freeze([]),
  result: Object.freeze(["job_id"]),
  "request-status": Object.freeze(["request_id"])
});
// A status page can never exceed this many rows: the query engine enforces the
// same ceiling, so no request the AppWindow can compose returns an unbounded
// listing that the browser would then have to render.
export const SYNC_STATUS_MAX_LIMIT = 200;

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
  // Every key is always sent. `state` selects one status state or "all", and
  // the optional narrowings are sent empty when unused, because the bridge
  // contract is an exact key set rather than a subset.
  "sync-status": Object.freeze(["cursor", "filter", "include_excluded", "limit", "profile", "scope", "state"]),
  // Two-phase by construction: an empty `confirm` only plans and returns a
  // ticket, and the ticket a caller sends back is the proof that the exact
  // overwrite list it describes was the one displayed.
  resync: Object.freeze(["confirm", "profile", "scope"]),
  // One category per call. "all" is deliberately not accepted: the bridge
  // would have to decide what "all" means for the one source it must refuse,
  // and a single button that quietly skips the audit log is worse than five
  // that each say what they clear.
  "clear-logs": Object.freeze(["source"]),
  action: Object.freeze(["allow_delete", "kind", "level", "max_total_delete", "scope", "write_test"])
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

function boundedTerminalOutput(value, fallback = "") {
  const text = typeof value === "string" ? value : fallback;
  // responseJson already enforces the one-MiB response contract before this
  // value is reachable. Keep terminal machine evidence intact up to that same
  // boundary instead of applying the much smaller general-purpose UI text cap.
  return String(text || fallback || "").slice(0, MAX_RESPONSE_BYTES);
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

function linkedAbortAttempt(parentSignal) {
  const AbortControllerClass = typeof window === "object"
    && typeof window.AbortController === "function"
    ? window.AbortController
    : null;
  if (!AbortControllerClass) {
    return { signal: parentSignal, abort() {}, release() {} };
  }
  const controller = new AbortControllerClass();
  let listening = false;
  const abort = () => {
    if (!controller.signal.aborted) controller.abort();
  };
  if (parentSignal) {
    if (parentSignal.aborted) abort();
    else if (typeof parentSignal.addEventListener === "function") {
      parentSignal.addEventListener("abort", abort, { once: true });
      listening = true;
    }
  }
  return {
    signal: controller.signal,
    abort,
    release() {
      if (!listening || !parentSignal || typeof parentSignal.removeEventListener !== "function") return;
      parentSignal.removeEventListener("abort", abort);
      listening = false;
    }
  };
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
    "readTimeoutMs", "resultRequestTimeoutMs", "resultObservationTimeoutMs",
    "requestReconciliationTimeoutMs", "requestReconciliationPollIntervalMs"
  ];
  const supportedKeys = new Set([...timeoutKeys, "setTimeout", "clearTimeout", "now"]);
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
  if (options.now !== undefined && typeof options.now !== "function") {
    throw new TypeError("API request now option must be a function");
  }
  normalized.setTimer = typeof options.setTimeout === "function"
    ? options.setTimeout
    : (callback, milliseconds) => window.setTimeout(callback, milliseconds);
  normalized.clearTimer = typeof options.clearTimeout === "function"
    ? options.clearTimeout
    : (timer) => window.clearTimeout(timer);
  // The observation ceiling is a duration, so whoever supplies the timers must be able
  // to supply the clock as well. A caller that fakes setTimeout but not the clock would
  // advance timers against a deadline that never moves -- the two would disagree and
  // the ceiling would be untestable. Defaults to the real clock, as production wants.
  normalized.now = typeof options.now === "function" ? options.now : () => Date.now();
  if ((options.setTimeout === undefined) !== (options.clearTimeout === undefined)) {
    throw new TypeError("API request limits must provide both timer functions or neither");
  }
  return normalized;
}

function terminalAttemptLimits() {
  return {
    ...TERMINAL_API_ATTEMPT_TIMEOUTS,
    resultObservationTimeoutMs: null,
    setTimer: (callback, milliseconds) => window.setTimeout(callback, milliseconds),
    clearTimer: (timer) => window.clearTimeout(timer),
    // Same injection point as the timers above. A harness that fakes window.setTimeout
    // but leaves the clock real would advance timers against a deadline that never
    // moves, so the unbounded observation ceiling could not be exercised at all.
    // Production has no window.now and falls through to the real clock.
    now: () => (typeof window.now === "function" ? window.now() : Date.now())
  };
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
      if (onTimeout) onTimeout();
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
      if (onTimeout) onTimeout();
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

function queuedObservationTimeout(jobId, requestId, detail, operation = "") {
  return new QueuedOutcomeUnknownError(
    jobId,
    `${detail} Do not retry it; inspect Activity and Logs.`,
    requestId,
    operation
  );
}

function queuedStillPending(jobId, requestId, operation = "", progress = null) {
  return new QueuedStillPendingError(
    jobId,
    "The package accepted this change and is still working through its queue. It will apply when the running operation finishes; do not send it again.",
    requestId,
    operation,
    progress
  );
}

/**
 * Which of the two the observation deadline means, for this observation.
 *
 * The deadline bounds the browser's patience, not the package's work. When the
 * last trusted read said `pending`, the package has told us what it is doing and
 * the honest report is "still queued". When the last read failed, or none
 * succeeded at all, the outcome genuinely is unknown.
 */
function observationDeadlineError(observation, jobId, requestId, operation) {
  if (observation && observation.lastReadPending === true) {
    return queuedStillPending(jobId, requestId, operation, observation.lastProgress);
  }
  return queuedObservationTimeout(
    jobId,
    requestId,
    "DSM accepted the operation, but terminal result observation exceeded the autosave limit.",
    operation
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

async function apiGetWithDsmAuth(
  auth,
  action,
  parameters,
  dsmAuth,
  rememberGeneration = true,
  requestSignal = auth && auth.signal ? auth.signal : undefined
) {
  const response = await fetch(endpoint(action, parameters), {
    method: "GET",
    credentials: "same-origin",
    cache: "no-store",
    redirect: "error",
    signal: requestSignal,
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
  const configuredLimits = normalizedRequestLimits(options);
  const limits = configuredLimits || terminalAttemptLimits();
  await ensureDsmToken();
  const requestDsmAuth = dsmAuthSnapshot();
  const deferredCsrfGeneration = action === "csrf";
  const attempt = linkedAbortAttempt(auth && auth.signal);
  let model;
  try {
    model = await withinLimit(
      apiGetWithDsmAuth(
        auth,
        action,
        parameters,
        requestDsmAuth,
        !deferredCsrfGeneration,
        attempt.signal
      ),
      limits.readTimeoutMs,
      limits,
      () => safeReadTimeout("read_observation", "DSM did not return a complete read response within the client read limit."),
      attempt.abort
    );
  } finally {
    attempt.release();
  }
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
  observation = null,
  expectedOperation = ""
) {
  if (!/^[0-9a-f]{48}$/.test(jobId)) {
    throw new Error("API returned an invalid queued job identifier");
  }
  const interval = Number.isFinite(Number(pollIntervalMs)) && Number(pollIntervalMs) >= 0
    ? Number(pollIntervalMs)
    : RESULT_POLL_INTERVAL_MS;
  let pendingResultReads = 0;
  let consecutiveObservationFailures = 0;
  let firstObservationFailureAt = 0;
  // Read the clock through the same injection point the timers use. The observation
  // ceiling is a duration, so a test that controls setTimeout but not the clock would
  // advance timers past a deadline that never moves -- two clocks disagreeing, and the
  // ceiling silently untestable. Production passes neither and gets Date.now.
  const readClock = limits && typeof limits.now === "function" ? limits.now : () => Date.now();
  for (;;) {
    if (observation && observation.expired) {
      throw observationDeadlineError(observation, jobId, requestId, expectedOperation);
    }
    let status;
    try {
      const attempt = linkedAbortAttempt(auth && auth.signal);
      try {
        const request = apiGetWithDsmAuth(
          auth,
          "result",
          { job_id: jobId },
          dsmAuth,
          true,
          attempt.signal
        );
        status = limits
          ? await withinLimit(
            request,
            limits.resultRequestTimeoutMs,
            limits,
            () => queuedObservationTimeout(
              jobId,
              requestId,
              "DSM accepted the operation, but a terminal result request exceeded the autosave limit.",
              expectedOperation
            ),
            attempt.abort,
            observation
          )
          : await request;
      } finally {
        attempt.release();
      }
    } catch (error) {
      if (auth && auth.signal && auth.signal.aborted) throw error;
      if (observation && observation.expired) {
        throw observationDeadlineError(observation, jobId, requestId, expectedOperation);
      }
      // This read did not come back, so nothing it might have said can be
      // trusted. Cleared after the expiry check above on purpose: a deadline
      // that fires while this read is in flight is still reported against the
      // last read that *did* answer.
      if (observation) observation.lastReadPending = false;
      consecutiveObservationFailures += 1;
      if (firstObservationFailureAt === 0) firstObservationFailureAt = readClock();
      // The POST was already accepted. Repeated transport/auth observation
      // failures are not evidence that the queued mutation failed, so surface
      // an explicit outcome-unknown state rather than inviting a duplicate.
      // Bounded callers own an overall observation deadline, so transient
      // request failures remain retryable until that deadline. Unbounded
      // callers give up after a bounded period of continuous failure -- measured
      // as elapsed time rather than as a number of attempts, so that changing the
      // poll cadence cannot quietly change how much trouble is survivable.
      const observationLostForTooLong =
        consecutiveObservationFailures >= RESULT_OBSERVATION_MIN_FAILURES
        && readClock() - firstObservationFailureAt >= RESULT_OBSERVATION_FAILURE_WINDOW_MS;
      if (!observation && observationLostForTooLong) {
        throw new QueuedOutcomeUnknownError(
          jobId,
          "DSM accepted the operation, but its result cannot currently be observed. Do not retry it; inspect Activity and Logs.",
          requestId,
          expectedOperation
        );
      }
      // Back off between failed observations, flooring the caller's interval with the
      // same ramp the pending path uses. Without this the time-based ceiling would have
      // no attempt bound at all: a caller polling at a very short interval against an
      // endpoint that fails instantly would retry for the whole window as fast as the
      // network allowed. The count-based ceiling used to bound that as a side effect;
      // now it is bounded on purpose, and a caller asking for a longer interval keeps it.
      // Only floor the retry where nothing else bounds it. A bounded caller already
      // owns an overall observation deadline, so its interval is its own to choose --
      // flooring that would let this backoff spend a budget the caller set deliberately.
      // An unbounded caller has no such deadline, and the time-based ceiling below is a
      // duration rather than an attempt count, so without a floor it would retry as fast
      // as the network allowed for the whole window.
      const retryInterval = observation
        ? interval
        : Math.max(
          interval,
          RESULT_POLL_RAMP_MS[
            Math.min(consecutiveObservationFailures - 1, RESULT_POLL_RAMP_MS.length - 1)
          ]
        );
      await delay(retryInterval, auth && auth.signal, limits, observation);
      continue;
    }
    if (observation && observation.expired) {
      throw observationDeadlineError(observation, jobId, requestId, expectedOperation);
    }
    consecutiveObservationFailures = 0;
    firstObservationFailureAt = 0;
    if (status.schema !== RESULT_STATUS_SCHEMA || status.job_id !== jobId) {
      throw new QueuedOutcomeUnknownError(
        jobId,
        "The queued operation is still outcome-unknown because DSM returned an invalid result document. Do not retry it; inspect Activity and Logs.",
        requestId,
        expectedOperation
      );
    }
    if (status.state === "pending") {
      // A trusted read that says the job is queued. Recorded so that a deadline
      // firing later can report what the package actually told us rather than
      // attributing the browser's impatience to it.
      if (observation) {
        observation.lastReadPending = true;
        observation.lastProgress = status.progress && typeof status.progress === "object" ? status.progress : null;
      }
      // A caller that named its own interval keeps it; otherwise walk the ramp, so
      // the answer arrives at roughly the speed of the job for short jobs without
      // hammering the endpoint for long ones.
      const pendingInterval = interval === RESULT_POLL_INTERVAL_MS
        ? RESULT_POLL_RAMP_MS[Math.min(pendingResultReads, RESULT_POLL_RAMP_MS.length - 1)]
        : interval;
      pendingResultReads += 1;
      await delay(pendingInterval, auth && auth.signal, limits, observation);
      continue;
    }
    if (status.state === "expired_or_missing") {
      throw new QueuedOutcomeUnknownError(
        jobId,
        boundedText(
          status.result && status.result.message,
          "The queued result is no longer available. Do not retry it; inspect Activity and Logs."
        ),
        requestId,
        expectedOperation
      );
    }
    if (status.state !== "complete"
      || !status.result
      || typeof status.result !== "object"
      || Array.isArray(status.result)
      || status.result.schema !== RESULT_SCHEMA
      || (validClientRequestId(requestId) && status.client_request_id !== requestId)
      || (status.result.ok !== true && status.result.ok !== false)) {
      throw new QueuedOutcomeUnknownError(
        jobId,
        "The queued operation is outcome-unknown because DSM returned an invalid terminal result. Do not retry it; inspect Activity and Logs.",
        requestId,
        expectedOperation
      );
    }
    if (status.result.ok === false) {
      const failure = new DsmApiError(
        boundedText(status.result.message, "Package operation failed"),
        200,
        boundedText(status.result.code, "operation_failed")
      );
      failure.resultOutput = boundedTerminalOutput(
        status.result.output,
        failure.message
      );
      failure.jobId = jobId;
      failure.requestId = validClientRequestId(requestId);
      failure.trustedJobId = true;
      failure.trustedRequestId = Boolean(failure.requestId);
      failure.accepted = true;
      failure.operation = typeof expectedOperation === "string" && ARGUMENT_KEYS[expectedOperation]
        ? expectedOperation
        : "";
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

function dispatchedOutcomeUnknown(id, operation = "", stage = "post_dispatch_observation") {
  return new MutationOutcomeUnknownError(
    id,
    `Automatic exact-request recovery for client request ${id} could not obtain a trustworthy rejection or queue acknowledgement. DSM may already have accepted it; do not start a new request, and inspect Activity and Logs using this request ID.`,
    operation,
    stage
  );
}

function exactRequestStatusKeys(model, expected) {
  if (!model || typeof model !== "object" || Array.isArray(model)) return false;
  const actual = Object.keys(model).sort();
  const keys = expected.slice().sort();
  return actual.length === keys.length && actual.every((key, index) => key === keys[index]);
}

// A pending job publishes bounded progress from the reviewed section catalogue,
// and the bridge delivers it as exactly one additional key on the status
// document. Both shapes are enumerated rather than tolerated: an unreviewed
// seventh key still fails closed.
//
// Demanding an exact five-key match made the six-key form unreadable, and
// `pollRequestStatus` treats an untrusted document as fatal. A long Doctor run
// -- the one operation that publishes progress, and the slowest thing this
// package does -- therefore declared its own outcome unknown within a second of
// the first read, without consuming a single retry of its recovery window.
function trustedRequestProgress(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  if (!exactRequestStatusKeys(value, ["label", "step", "total", "updated_at"])) return null;
  const step = Number(value.step);
  const total = Number(value.total);
  const updatedAt = Number(value.updated_at);
  if (!Number.isInteger(step) || !Number.isInteger(total) || !Number.isInteger(updatedAt)
    || step < 1 || total < 1 || step > total || updatedAt < 1
    || typeof value.label !== "string" || !value.label || value.label.length > 128) {
    return null;
  }
  return { step, total, label: value.label, updatedAt };
}

function trustedRequestStatus(model, requestId, expectedOperation) {
  if (model.schema !== REQUEST_STATUS_SCHEMA || model.request_id !== requestId) return null;
  if (model.state === "unresolved"
    && exactRequestStatusKeys(model, ["request_id", "schema", "state"])) {
    return { state: "unresolved" };
  }
  const identityKeys = ["job_id", "operation", "request_id", "schema", "state"];
  const progress = model.progress === undefined ? null : trustedRequestProgress(model.progress);
  if (!["pending", "complete"].includes(model.state)
    || !exactRequestStatusKeys(
      model,
      model.progress === undefined ? identityKeys : identityKeys.concat("progress")
    )
    || !validJobId(model.job_id)
    || model.operation !== expectedOperation
    // Progress is published only while pending; once complete the result is the answer.
    || (model.progress !== undefined && (!progress || model.state !== "pending"))) {
    return null;
  }
  return {
    state: model.state,
    jobId: model.job_id,
    operation: model.operation,
    progress
  };
}

async function requestStatusOnce(
  auth,
  requestId,
  expectedOperation,
  dsmAuth,
  limits,
  observation = null
) {
  const attempt = linkedAbortAttempt(auth && auth.signal);
  try {
    const model = await withinLimit(
      apiGetWithDsmAuth(
        auth,
        "request-status",
        { request_id: requestId },
        dsmAuth,
        true,
        attempt.signal
      ),
      limits.readTimeoutMs,
      limits,
      () => dispatchedOutcomeUnknown(requestId, expectedOperation, "request_reconciliation"),
      attempt.abort,
      observation
    );
    const status = trustedRequestStatus(model, requestId, expectedOperation);
    if (!status) {
      const invalid = dispatchedOutcomeUnknown(requestId, expectedOperation, "request_reconciliation");
      invalid.invalidReconciliationDocument = true;
      throw invalid;
    }
    return status;
  } finally {
    attempt.release();
  }
}

async function pollRequestStatus(
  auth,
  requestId,
  expectedOperation,
  dsmAuth,
  limits,
  observation
) {
  for (;;) {
    if (observation.expired) {
      throw dispatchedOutcomeUnknown(requestId, expectedOperation, "request_reconciliation");
    }
    let model;
    try {
      model = await requestStatusOnce(
        auth,
        requestId,
        expectedOperation,
        dsmAuth,
        limits,
        observation
      );
    } catch (error) {
      if ((auth && auth.signal && auth.signal.aborted) || observation.expired) {
        throw dispatchedOutcomeUnknown(requestId, expectedOperation, "request_reconciliation");
      }
      if ((error && error.invalidReconciliationDocument === true)
        || (error instanceof DsmApiError && error.status >= 200 && error.status < 500)) {
        throw dispatchedOutcomeUnknown(requestId, expectedOperation, "request_reconciliation");
      }
      await delay(
        limits.requestReconciliationPollIntervalMs,
        auth && auth.signal,
        limits,
        observation
      );
      continue;
    }

    if (model.state !== "unresolved") return model;
    await delay(
      limits.requestReconciliationPollIntervalMs,
      auth && auth.signal,
      limits,
      observation
    );
  }
}

async function recoverQueuedRequest(auth, requestId, expectedOperation, dsmAuth, limits) {
  const observation = { expired: false, cancelCurrent: null };
  return withinLimit(
    pollRequestStatus(auth, requestId, expectedOperation, dsmAuth, limits, observation),
    limits.requestReconciliationTimeoutMs,
    limits,
    () => dispatchedOutcomeUnknown(requestId, expectedOperation, "request_reconciliation"),
    () => {
      observation.expired = true;
      if (observation.cancelCurrent) observation.cancelCurrent();
    }
  );
}

async function awaitQueuedResult(
  auth,
  queued,
  dsmAuth,
  pollIntervalMs,
  requestId,
  operation,
  limits,
  boundedObservationLimits
) {
  if (!boundedObservationLimits) {
    return pollJobResult(
      auth,
      queued.job_id,
      dsmAuth,
      pollIntervalMs,
      requestId,
      limits,
      null,
      operation
    );
  }
  const observation = { expired: false, cancelCurrent: null, lastReadPending: false, lastProgress: null };
  return withinLimit(
    pollJobResult(
      auth,
      queued.job_id,
      dsmAuth,
      pollIntervalMs,
      requestId,
      limits,
      observation,
      operation
    ),
    limits.resultObservationTimeoutMs,
    limits,
    // This factory races the poll's own expiry throw; both must reach the same
    // verdict, so both read it from the observation.
    () => observationDeadlineError(observation, queued.job_id, requestId, operation),
    () => {
      observation.expired = true;
      if (observation.cancelCurrent) observation.cancelCurrent();
    }
  );
}

export async function reconcileMutationRequest(
  auth,
  requestId,
  expectedOperation,
  pollIntervalMs = RESULT_POLL_INTERVAL_MS,
  options = undefined
) {
  const trustedRequestId = validClientRequestId(requestId);
  if (!trustedRequestId || !ARGUMENT_KEYS[expectedOperation]) {
    throw new TypeError("Mutation reconciliation requires a trusted request ID and operation");
  }
  const boundedObservationLimits = normalizedRequestLimits(options);
  const limits = boundedObservationLimits || terminalAttemptLimits();
  let requestDsmAuth = rememberedReconciliationAuth(auth, trustedRequestId);
  if (!requestDsmAuth) {
    await ensureDsmToken();
    requestDsmAuth = dsmAuthSnapshot();
  }
  try {
    const recovered = await recoverQueuedRequest(
      auth,
      trustedRequestId,
      expectedOperation,
      requestDsmAuth,
      limits
    );
    const queued = {
      schema: QUEUED_SCHEMA,
      state: "queued",
      request_id: trustedRequestId,
      job_id: recovered.jobId
    };
    const result = await awaitQueuedResult(
      auth,
      queued,
      requestDsmAuth,
      pollIntervalMs,
      trustedRequestId,
      expectedOperation,
      limits,
      boundedObservationLimits
    );
    forgetReconciliationAuth(auth, trustedRequestId);
    return {
      schema: "sdsync.dsm-reconciled-result.v1",
      request_id: trustedRequestId,
      job_id: recovered.jobId,
      operation: expectedOperation,
      result
    };
  } catch (error) {
    if (error
      && error.accepted === true
      && error.outcomeUnknown !== true
      && error.trustedJobId === true) {
      forgetReconciliationAuth(auth, trustedRequestId);
    }
    throw error;
  }
}

async function csrfForCurrentAuthGeneration(auth, csrfToken, dsmAuth, limits = null) {
  if (!auth || typeof auth !== "object") return csrfToken;
  const issuedGeneration = csrfGenerationByAuth.get(auth);
  if (issuedGeneration === undefined || issuedGeneration === dsmAuth.generation) return csrfToken;

  const previousToken = csrfToken;
  // A timed-out reissue may still settle later at the fetch layer. Do not let
  // that detached response advance the accepted generation without also
  // delivering its replacement token to the AppWindow.
  let model;
  if (limits) {
    const attempt = linkedAbortAttempt(auth.signal);
    try {
      model = await withinLimit(
        apiGetWithDsmAuth(auth, "csrf", {}, dsmAuth, false, attempt.signal),
        limits.csrfReissueTimeoutMs,
        limits,
        () => safeReadTimeout(
          "csrf_reissue",
          "DSM request authentication could not be refreshed within the autosave limit; no mutation request was sent."
        ),
        attempt.abort
      );
    } finally {
      attempt.release();
    }
  } else {
    model = await apiGetWithDsmAuth(auth, "csrf", {}, dsmAuth, false);
  }
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

export const REQUEST_PROBE_SCHEMA = "sdsync.dsm-request-probe.v1";
// The bridge writes one activity record per audited mutation state, keyed to the
// exact client request ID (`Module <operation> <state> [<transaction>]
// request_id=<id>` under code `audit.<state>`). That is the same evidence the
// outcome-unknown message asks the operator to go and read by hand.
const AUDIT_REQUEST_VERDICTS = Object.freeze({
  "audit.succeeded": "settled",
  "audit.failed": "settled",
  "audit.outcome_unknown": "unknown",
  "audit.requested": "accepted"
});
// Precedence when several records name the same request: a terminal state
// outranks the package's own unknown, which outranks a bare acceptance.
const AUDIT_VERDICT_RANK = Object.freeze({ settled: 3, unknown: 2, accepted: 1 });
const ACTIVITY_PROBE_LINES = 200;

function auditVerdictForRequest(model, requestId) {
  const events = model && Array.isArray(model.events) ? model.events : [];
  let best = null;
  for (const event of events) {
    if (!event || typeof event !== "object" || Array.isArray(event)) continue;
    if (validClientRequestId(event.client_request_id) !== requestId) continue;
    const verdict = AUDIT_REQUEST_VERDICTS[event.code];
    if (!verdict) continue;
    const epoch = Number(event.epoch);
    const record = {
      verdict,
      code: event.code,
      epoch: Number.isFinite(epoch) && epoch > 0 ? epoch : 0
    };
    if (!best
      || AUDIT_VERDICT_RANK[verdict] > AUDIT_VERDICT_RANK[best.verdict]
      || (AUDIT_VERDICT_RANK[verdict] === AUDIT_VERDICT_RANK[best.verdict] && record.epoch > best.epoch)) {
      best = record;
    }
  }
  return best;
}

/**
 * Establish what is currently knowable about one dispatched request.
 *
 * Deliberately a single bounded pass rather than a loop, and deliberately
 * incapable of rejecting: the caller polls this on a ramp and renders whatever
 * it establishes, so every failure has to arrive as a verdict rather than as an
 * exception that would end the account the operator is watching.
 *
 * Strictly read-only. It submits nothing, replays nothing, and never forgets the
 * reconciliation authentication that the manual recovery path still needs.
 *
 * The private queue answers first because it is authoritative and cheap. Activity
 * is consulted only when the queue holds no record, because that is the single
 * case the queue cannot resolve on its own: a request that was never accepted and
 * a request whose completed job has already been reaped both read `unresolved`.
 */
export async function probeRequestOutcome(auth, requestId, expectedOperation, options = undefined) {
  const trustedRequestId = validClientRequestId(requestId);
  if (!trustedRequestId || !ARGUMENT_KEYS[expectedOperation]) {
    throw new TypeError("Request outcome probes require a trusted request ID and operation");
  }
  const limits = normalizedRequestLimits(options) || terminalAttemptLimits();
  const observed = (verdict, detail = null) => ({
    schema: REQUEST_PROBE_SCHEMA,
    request_id: trustedRequestId,
    operation: expectedOperation,
    verdict,
    job_id: (detail && detail.jobId) || "",
    code: (detail && detail.code) || "",
    epoch: (detail && detail.epoch) || 0,
    progress: (detail && detail.progress) || null,
    checked_at: limits.now()
  });

  let queueRead = false;
  try {
    let requestDsmAuth = rememberedReconciliationAuth(auth, trustedRequestId);
    if (!requestDsmAuth) {
      await ensureDsmToken();
      requestDsmAuth = dsmAuthSnapshot();
    }
    const status = await requestStatusOnce(
      auth,
      trustedRequestId,
      expectedOperation,
      requestDsmAuth,
      limits
    );
    queueRead = true;
    if (status.state !== "unresolved") {
      return observed(
        status.state === "complete" ? "settled" : "accepted",
        { jobId: status.jobId, progress: status.progress }
      );
    }
  } catch (_error) {
    if (auth && auth.signal && auth.signal.aborted) return observed("unavailable");
  }

  try {
    const record = auditVerdictForRequest(
      await apiGet(auth, "activity", { lines: ACTIVITY_PROBE_LINES }, options),
      trustedRequestId
    );
    if (record) return observed(record.verdict, record);
    // Both sources agree there is no trace. That is an answer, not a silence:
    // nothing named this request ID, so nothing was accepted under it.
    return observed(queueRead ? "absent" : "unavailable");
  } catch (_error) {
    return observed("unavailable");
  }
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
  const boundedObservationLimits = normalizedRequestLimits(options);
  const limits = boundedObservationLimits || terminalAttemptLimits();

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
  // Manual recovery must retain the exact DSM token generation used when the
  // browser dispatched this request. The backend separately binds the stable
  // DSM session cookie. Keep the token snapshot only in this AppWindow's
  // memory and never copy it into an error, incident, log, or rendered status.
  rememberReconciliationAuth(auth, id, requestDsmAuth);
  let queued;
  let dispatchAmbiguous = false;
  let dispatchStage = "post_dispatch_observation";
  for (let attempt = 0; attempt < POST_DISPATCH_MAX_ATTEMPTS; attempt += 1) {
    const requestAttempt = linkedAbortAttempt(auth && auth.signal);
    let response;
    try {
      try {
        const dispatched = fetch(API_URL, {
          method: "POST",
          credentials: "same-origin",
          cache: "no-store",
          redirect: "error",
          signal: requestAttempt.signal,
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
            () => dispatchedOutcomeUnknown(id, action, "post_dispatch_observation"),
            requestAttempt.abort
          )
          : await dispatched;
      } catch (_error) {
        dispatchAmbiguous = true;
        dispatchStage = "post_dispatch_observation";
      }

      if (response) {
        try {
          const body = responseJson(response);
          queued = limits
            ? await withinLimit(
              body,
              limits.postResponseTimeoutMs,
              limits,
              () => dispatchedOutcomeUnknown(id, action, "post_ack_body_observation"),
              requestAttempt.abort
            )
            : await body;
        } catch (error) {
          dispatchStage = "post_ack_body_observation";
          if (!dispatchAmbiguous
            && error instanceof DsmApiError
            && error.trustedRejection === true) {
            error.preAcceptance = true;
            error.requestId = id;
            error.trustedRequestId = true;
            if (isExplicitCsrfRejection(error)) error.csrfRejected = true;
            forgetReconciliationAuth(auth, id);
            throw error;
          }
          if (dispatchAmbiguous
            && error instanceof DsmApiError
            && error.trustedRejection === true) {
            throw dispatchedOutcomeUnknown(id, action, dispatchStage);
          }
          dispatchAmbiguous = true;
        }
      }
    } finally {
      requestAttempt.release();
    }

    if (queued
      && queued.schema === QUEUED_SCHEMA
      && queued.state === "queued"
      && queued.request_id === id
      && validJobId(queued.job_id)) {
      break;
    }
    queued = null;
    dispatchAmbiguous = true;
    if (auth && auth.signal && auth.signal.aborted) {
      throw dispatchedOutcomeUnknown(id, action, dispatchStage);
    }

    // A response can be lost after DSM durably queued the request. Before
    // replaying the exact serialized body, ask the authenticated private queue
    // for the server job that owns this client request ID.
    let recovered;
    try {
      recovered = await requestStatusOnce(
        auth,
        id,
        action,
        requestDsmAuth,
        limits
      );
    } catch (_error) {
      if (auth && auth.signal && auth.signal.aborted) {
        throw dispatchedOutcomeUnknown(id, action, "request_reconciliation");
      }
      // A malformed, mismatched, rejected, or unavailable lookup is not an
      // authenticated negative acknowledgement. Stop replaying and let the
      // bounded read-only reconciliation loop settle or fail closed.
      break;
    }

    // Only this exact, schema-validated negative mapping authorizes replay of
    // the byte-identical request body.
    if (recovered.state !== "unresolved") {
      queued = {
        schema: QUEUED_SCHEMA,
        state: "queued",
        request_id: id,
        job_id: recovered.jobId
      };
      break;
    }

    if (attempt + 1 >= POST_DISPATCH_MAX_ATTEMPTS) {
      break;
    }
    try {
      await delay(
        POST_DISPATCH_REPLAY_DELAYS_MS[attempt],
        auth && auth.signal,
        limits
      );
    } catch (_error) {
      throw dispatchedOutcomeUnknown(id, action, dispatchStage);
    }
  }
  if (!queued) {
    const recovered = await recoverQueuedRequest(
      auth,
      id,
      action,
      requestDsmAuth,
      limits
    );
    queued = {
      schema: QUEUED_SCHEMA,
      state: "queued",
      request_id: id,
      job_id: recovered.jobId
    };
  }
  if (!awaitTerminal) {
    forgetReconciliationAuth(auth, id);
    return queued;
  }
  try {
    const result = await awaitQueuedResult(
      auth,
      queued,
      requestDsmAuth,
      pollIntervalMs,
      id,
      action,
      limits,
      boundedObservationLimits
    );
    forgetReconciliationAuth(auth, id);
    return result;
  } catch (error) {
    // A job that is merely still queued keeps its reconciliation auth, exactly
    // as an unknown one does: the operator may still want the Reconcile path to
    // observe how it finished, and that path needs the DSM token generation the
    // request was dispatched under.
    if (!error || (error.outcomeUnknown !== true && error.requiresInspection !== true && error.stillPending !== true)) {
      forgetReconciliationAuth(auth, id);
    }
    throw error;
  }
}
