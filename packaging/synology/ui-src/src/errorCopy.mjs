/**
 * What to say about a read that failed.
 *
 * Every failure that was not a 401, a 403 or a named 503 used to render as "the
 * package endpoint could not be reached", including the ones where the package
 * answered with a 500 and a message, and the one where a response guard tripped.
 * The cause is now part of the text: the HTTP status, the bridge's own code, and
 * the message when it is safe to show.
 *
 * The message is only trusted when the bridge sent it in a well-formed
 * `sdsync.dsm-error.v1` document (api.js marks that `trustedRejection`), or when
 * the error was raised by this UI's own transport code, whose strings it owns.
 * Anything else is a proxy's or a browser's text and only its status and code
 * are shown. Imports only textSafety.mjs, which imports nothing.
 */

import { plainText } from "./textSafety.mjs";

// Errors api.js raises itself, so their messages are this UI's own copy.
const CLIENT_OWNED_CODES = new Set([
  "authentication_redirect", "non_json_response", "response_too_large", "malformed_json",
  "invalid_document", "client_timeout"
]);

const SAFE_CODE = /^[A-Za-z0-9_.:-]{1,64}$/;
const MESSAGE_LIMIT = 200;

function boundedMessage(value) {
  return plainText(String(value || ""), MESSAGE_LIMIT);
}

/** { status, code, message, text } where `text` is "HTTP 502 · non_json_response · API did not return JSON". */
export function errorCause(error) {
  const status = Number(error && error.status);
  const httpStatus = Number.isInteger(status) && status > 0 ? status : 0;
  const rawCode = String((error && error.code) || "");
  // api.js names a failure with no code of its own `http_<status>`; that adds nothing to "HTTP <status>".
  const code = SAFE_CODE.test(rawCode) && rawCode !== `http_${httpStatus}` ? rawCode : "";
  // Only an Error object can be trusted at all: a plain object is whatever the
  // caller built. Among Errors, a DsmApiError carries the bridge's text only when
  // api.js saw a well-formed error document (trustedRejection) or raised the error
  // itself; anything else (TypeError "Failed to fetch", a guard's own Error) is
  // this window's or the browser's text.
  const isError = error instanceof Error;
  const isApiError = isError && (error.name === "DsmApiError" || error.name === "ClientRequestTimeoutError");
  const trusted = isError && (isApiError ? error.trustedRejection === true || CLIENT_OWNED_CODES.has(code) : true);
  const message = trusted ? boundedMessage(error && error.message) : "";
  const parts = [];
  if (httpStatus) parts.push(`HTTP ${httpStatus}`);
  if (code) parts.push(code);
  if (message) parts.push(message);
  return { status: httpStatus, code, message, text: parts.join(" · ") };
}

/** "Detail: HTTP 500 · http_500 · message." or "" when there is nothing to add. */
export function causeSentence(error) {
  const { text } = errorCause(error);
  return text ? `Detail: ${text}.` : "";
}
