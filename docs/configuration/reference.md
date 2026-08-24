# Complete TOML reference

The configuration file contains one optional top-level selector and one or more independent profile
tables. Every accepted key is listed below. Keys use kebab case and unknown keys are rejected.

## Top-level document

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default-profile` | string | none | Profile selected when neither `--profile` nor `SDSYNC_PROFILE` is present. The name must exist under `profiles`. |
| `profiles.<name>` | table | empty | One independent profile. Names are used by `--profile`, `--profiles`, and batch output. |

A profile may intentionally contain only connection defaults for single-profile invocations that
provide source and destination positionally. Batch plan/sync requires every selected profile to
resolve `source`, `remote`, `url`, `username`, and authentication independently.

## Source and connection

| TOML key | CLI / core environment | Type and default | Rules |
| --- | --- | --- | --- |
| `source` | positional `SOURCE`; no core CLI environment variable | path; required for a complete job | Relative paths are anchored to the config directory. Must resolve to an ordinary readable directory; filesystem roots, links/reparse points, and unsafe special paths fail closed. Service wrappers may translate `SDSYNC_SOURCE` into this positional argument. |
| `remote` | positional `REMOTE`; target diagnostics also accept `SDSYNC_REMOTE` | string; required for a complete job | File Station logical absolute path, never `/volume*`. `/` is forbidden. Empty, dot, traversal, non-portable, or DSM-managed components are rejected. |
| `url` | `--url`, `SDSYNC_URL` | URL; required | Absolute HTTPS reverse-proxy URL with a host and no embedded credentials, query, or fragment. A path prefix is allowed. |
| `username` | `--username`, `SDSYNC_USERNAME` | string; required | Dedicated DSM account name used for File Station authentication. |

## Authentication locators

| TOML key | CLI / core environment | Type and default | Rules |
| --- | --- | --- | --- |
| `password-file` | `--password-file`, `SDSYNC_PASSWORD_FILE` | path; unset | Protected regular non-symlink file; the first line is the password. A higher-layer `--password-stdin` takes precedence. |
| `totp-secret-file` | `--totp-secret-file`, `SDSYNC_TOTP_SECRET_FILE` | path; unset | Protected regular non-symlink file containing a Base32 seed or `otpauth://` URI on its first line. |
| `no-vault` | `--no-vault`, `SDSYNC_NO_VAULT`; reversed by `--vault` | boolean; `false` | Disables both password and TOTP seed lookup in the current user's OS vault. |

Secret values are not valid TOML fields. See [passwords, TOTP, and secret sources](credentials.md)
for the complete resolution order and scheduler guidance.

## Selection and comparison

| TOML key | CLI / core environment | Type and default | Rules |
| --- | --- | --- | --- |
| `compare` | `--compare`, `SDSYNC_COMPARE` | `content`; enum | `content`, `metadata`, or `size-only`. Content is the safest default. |
| `jobs` | `--jobs`, `SDSYNC_JOBS` | integer; `2` | Concurrent uploads, from `1` through `16`. The configured rate limit is shared rather than multiplied by this value. |
| `excludes` | repeated `--exclude`; no core environment variable | array of strings; `[]` | Gitignore-style rules evaluated in order. CLI rules append. A leading `!` negates an earlier match. The root `.sdsyncignore` also participates. |

## Deletion safety

| TOML key | CLI / core environment | Type and default | Rules |
| --- | --- | --- | --- |
| `delete` | `--delete`, `SDSYNC_DELETE`; reversed by `--no-delete` | boolean; `false` | Plans removal of remote-only entries and permits remote type replacement. Still subject to caps, fresh state, protected paths, and execution authorization. |
| `allow-empty-source` | `--allow-empty-source`, `SDSYNC_ALLOW_EMPTY_SOURCE` | boolean; `false` | Valid only with `delete = true`. Removes the empty-source deletion guard; it is not a general permission bypass. |
| `max-delete` | `--max-delete`, `SDSYNC_MAX_DELETE` | non-negative integer; `100` | Per-profile maximum planned remote deletions. Use a deliberately small value for mirror jobs. |

Multi-profile invocations also accept the CLI/environment guard `--max-total-delete` /
`SDSYNC_MAX_TOTAL_DELETE`, default `100`. It is not a TOML key.

## Network and trust

| TOML key | CLI / core environment | Type and default | Rules |
| --- | --- | --- | --- |
| `retries` | `--retries`, `SDSYNC_RETRIES` | integer; `2` | From `0` through `5`; applies to bounded retryable transport/API failures. |
| `timeout` | `--timeout`, `SDSYNC_TIMEOUT` | seconds; `7200` | At least `1`. Covers one upload or background operation, not the entire job. Account for the largest file and any rate cap. |
| `connect-timeout` | `--connect-timeout`, `SDSYNC_CONNECT_TIMEOUT` | seconds; `15` | At least `1`; bounds TCP/TLS connection setup. |
| `max-rate` | `--max-rate`, `SDSYNC_MAX_RATE` | bytes/second; unlimited | At least `1` when set. One shared upload budget across all jobs. |
| `ca-certificate` | `--ca-certificate`, `SDSYNC_CA_CERTIFICATE` | path; unset | PEM certificate added to the TLS trust store for a private reverse-proxy CA. |
| `allow-http` | `--allow-http`, `SDSYNC_ALLOW_HTTP` | boolean; `false` | Permits HTTP only for controlled LAN testing. It does not weaken certificate checks for HTTPS. |
| `danger-accept-invalid-certs` | `--danger-accept-invalid-certs`, `SDSYNC_DANGER_ACCEPT_INVALID_CERTS` | boolean; `false` | Disables TLS certificate verification. Prefer a private CA file; do not use for production. |

Control-plane discovery, login, inventory, hash, and similar requests remain capped at 10 seconds.
See [network, reverse proxy, and TLS](network.md).

## Diagnostics, results, and progress

| TOML key | CLI / core environment | Type and default | Rules |
| --- | --- | --- | --- |
| `verbose` | repeated `-v` or `--verbose`; no core environment variable | integer; `0` | `1` implies debug logging and `2` or more implies trace unless `log-level` explicitly overrides it. |
| `quiet` | `--quiet`, `SDSYNC_QUIET`; reversed by `--no-quiet` | boolean; `false` | Suppresses non-error terminal diagnostics and progress. It does not disable configured file or remote logs. |
| `log-level` | `--log-level`, `SDSYNC_LOG_LEVEL` | `info`; enum | `trace`, `debug`, `info`, `warn`, `error`, or `off`. `off` disables structured event emission to every sink. |
| `log-format` | `--log-format`, `SDSYNC_LOG_FORMAT` | `human`; enum | `human` or newline-delimited `json` diagnostic events. |
| `log-file` | `--log-file`, `SDSYNC_LOG_FILE` | path; unset | Enables a rotating file sink. Provision its parent directory for the run identity first. |
| `progress` | `--progress`, `SDSYNC_PROGRESS` | `auto`; enum | `auto`, `always`, or `never`. Machine result formats suppress terminal progress. |
| `output` | `--output`, `SDSYNC_OUTPUT` | `human`; enum | Command result format: `human`, `json`, or `ndjson`; independent from diagnostic log format. |

The file sink rotates at 10 MiB and retains three backups. See
[output, logs, and monitoring](../observability.md) for schemas and redaction rules.

## Remote HTTPS logging

| TOML key | CLI / core environment | Type and default | Rules |
| --- | --- | --- | --- |
| `remote-log-url` | `--remote-log-url`, `SDSYNC_REMOTE_LOG_URL` | URL; unset | Absolute HTTPS collector URL with a host and no credentials, query, or fragment. |
| `remote-log-token-file` | `--remote-log-token-file`, `SDSYNC_REMOTE_LOG_TOKEN_FILE` | path; unset | Protected bearer-token file. Mutually exclusive with `remote-log-token-env` in one profile. |
| `remote-log-token-env` | `--remote-log-token-env`, `SDSYNC_REMOTE_LOG_TOKEN_ENV` | variable name; unset | Names the variable holding the token; this field never contains the token. |
| `remote-log-mode` | `--remote-log-mode`, `SDSYNC_REMOTE_LOG_MODE` | `best-effort`; enum | `best-effort` or `required`. `required` needs `remote-log-url` and makes delivery part of run success. |

When a remote URL is configured without an explicit token locator, the token value is read from
`SDSYNC_REMOTE_LOG_TOKEN`. Token locators are invalid without a remote URL.

## Cross-field validation summary

- `allow-empty-source = true` requires `delete = true` in the same profile.
- `remote-log-mode = "required"` requires `remote-log-url`.
- A remote-log token source requires `remote-log-url`.
- `remote-log-token-file` and `remote-log-token-env` cannot coexist in one profile.
- `url` requires HTTPS unless `allow-http = true`; `remote-log-url` always requires HTTPS.
- Unknown keys and invalid enum spellings fail validation.

Render the repository's [full configuration example](example.md), then use `config validate` and
`config show` to prove the exact effective profile before scheduling it.
