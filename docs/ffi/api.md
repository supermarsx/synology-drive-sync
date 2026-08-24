# ABI surface

`include/sdsync.h` is the source of truth for the C ABI. Include it rather than copying declarations
into an application. The header supplies visibility/calling-convention macros for C and C++, an
`extern "C"` guard, ABI/status constants, opaque handle declarations, callback types, and the
size-versioned callback table.

## Exported functions

| Function | Purpose and ownership |
| --- | --- |
| `sdsync_abi_version_v1()` | Return numeric ABI major `1`; it cannot fail or allocate. |
| `sdsync_build_version_v1(data, length)` | Borrow the non-NUL-terminated calendar build-version bytes until the library unloads. |
| `sdsync_cancellation_new_v1(out)` | Allocate one cancellation handle. |
| `sdsync_cancellation_cancel_v1(handle)` | Signal cooperative cancellation; repeated calls and calls from another thread are allowed. |
| `sdsync_cancellation_free_v1(handle)` | Free once after all runs using the handle return; `NULL` is accepted. |
| `sdsync_run_v1(request, length, callbacks, cancellation, out_result)` | Run one complete blocking plan/apply operation and return both a status and an owned JSON result. |
| `sdsync_result_bytes_v1(result, data, length)` | Borrow immutable, non-NUL-terminated JSON bytes until the result is freed. |
| `sdsync_result_free_v1(result)` | Free one result exactly once; `NULL` is accepted. |

Except when `out_result` itself is `NULL`, `sdsync_run_v1` writes an owned result handle on every
return, including request validation and callback failures. Read that result for the structured
diagnostic and free it even when the numeric status is non-zero.

## Request document

The request is valid UTF-8 JSON, at most 16 MiB, with no unknown top-level or deletion fields. The
following fields are accepted:

| Field | Type | Required/default | Meaning |
| --- | --- | --- | --- |
| `schema` | string | required; exactly `sdsync.request.v1` | Input contract version. |
| `endpoint` | string | required | DSM base URL; HTTPS is required unless `allow_http` is true. |
| `username` | string | required | Non-empty DSM account name with no control characters. |
| `source` | string | required | Non-empty local source path. |
| `remote` | string | required | Logical File Station remote root. |
| `allow_http` | boolean | `false` | Permit plain HTTP for an explicitly trusted test/LAN endpoint. |
| `danger_accept_invalid_certificates` | boolean | `false` | Disable certificate validation. |
| `ca_certificate` | string or null | none | Path to an additional PEM CA certificate. |
| `connect_timeout_seconds` | unsigned integer or null | `15` | Positive connection timeout. |
| `request_timeout_seconds` | unsigned integer or null | `7200` | Positive complete-request timeout. |
| `retries` | unsigned integer or null | `2` | Retries after the first request, from `0` through `5`. |
| `max_upload_rate` | unsigned integer or null | unlimited | Shared positive upload limit in bytes per second. |
| `exclusions` | string array | `[]` | Gitignore-style local exclusion patterns. |
| `comparison` | string or null | `content` | `content`, `metadata`, or `size-only`. |
| `deletion` | object or null | disabled | Explicit bounded mirror-deletion policy. |
| `jobs` | unsigned integer or null | `2` | Concurrent mutation-worker count, from `1` through `16`. |

When `deletion.enabled` is `true`, `deletion.max_delete` is required and must be positive.
`deletion.allow_empty_source` defaults to `false`. When deletion is disabled, neither a maximum nor
the empty-source override may be supplied.

```json
{
  "schema": "sdsync.request.v1",
  "endpoint": "https://files.example.com",
  "username": "mirror-bot",
  "source": "./export",
  "remote": "/TeamShare/Project",
  "exclusions": ["*.tmp", ".cache/"],
  "comparison": "content",
  "jobs": 2,
  "deletion": {
    "enabled": false
  }
}
```

## Callback table

Initialize `sdsync_callbacks_v1.struct_size` to `sizeof(sdsync_callbacks_v1)`, set `reserved` to
zero, and set unused callbacks to `NULL`. The implementation rejects a table smaller than the v1
prefix or a non-zero reserved field.

| Callback | Input | Return values |
| --- | --- | --- |
| `secret` | secret kind plus a two-pass output buffer | `SDSYNC_CALLBACK_OK`, `_UNAVAILABLE`, or `_CANCELLED` |
| `plan` | borrowed `sdsync.plan.v1` JSON | `SDSYNC_PLAN_PREVIEW_ONLY`, `_APPLY`, or `_CANCEL` |
| `event` | borrowed `sdsync.event.v1` JSON | `SDSYNC_EVENT_CONTINUE` or `_CANCEL` |

The secret kinds are password, OTP-required, and OTP-rejected. On the first secret call, `buffer` is
`NULL` and `capacity` is zero; write the required UTF-8 byte length to `*written`. On the second,
copy exactly the first-pass queried byte length into `buffer` and write that same length to `*written`.
A secret must be between 1 byte and 64 KiB.

The plan payload is shaped as `{ "schema": "sdsync.plan.v1", "plan": ... }`. Event payloads use
`{ "schema": "sdsync.event.v1", "event": ... }`; their tagged event kinds cover phase start/end,
plan readiness, and completed mutations. All callback byte views are valid only during that callback.

## Status and result documents

Function statuses are deliberately coarse:

| Constant | Value | Meaning |
| --- | ---: | --- |
| `SDSYNC_STATUS_OK` | 0 | The run returned a normal outcome, including preview-only. |
| `SDSYNC_STATUS_INVALID_ARGUMENT` | 1 | A pointer, table, request document, or request value was invalid. |
| `SDSYNC_STATUS_CALLBACK_FAILED` | 2 | A callback returned an unknown/failing response. |
| `SDSYNC_STATUS_CANCELLED` | 3 | Cancellation was requested. |
| `SDSYNC_STATUS_OPERATION_FAILED` | 4 | Authentication, filesystem, network, remote, safety, or reconciliation failed. |
| `SDSYNC_STATUS_PANIC` | 255 | The ABI boundary caught an internal Rust panic. |

A successful result is:

```json
{
  "schema": "sdsync.ffi-result.v1",
  "ok": true,
  "outcome": {
    "plan": { "changes": [], "creates": 0, "copies": 0, "uploads": 0, "deletes": 0, "unchanged_files": 0, "protected_entries": 0, "upload_bytes": 0 },
    "applied": false,
    "reconciled": false,
    "execution": null
  }
}
```

A failure uses the same envelope with `ok: false` and an `error` object:

```json
{
  "schema": "sdsync.ffi-result.v1",
  "ok": false,
  "error": {
    "code": "invalid-argument",
    "message": "request schema must be exactly sdsync.request.v1"
  }
}
```

Application error codes include `invalid-request`, `credential-unavailable`, `otp-required`,
`authentication`, `local-filesystem`, `network`, `remote`, `safety`, `cancelled`, `reconciliation`,
and `internal`. Treat unknown future strings as an extensible error category and use the message for
operator diagnostics, not control flow.
