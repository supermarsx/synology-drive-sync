# CLI and environment variables

Environment variables belong to two different contracts:

1. **Core CLI variables** are declared by the Rust command parser and behave like the matching
   option. They override a selected profile.
2. **Deployment-wrapper variables** are interpreted by the supplied systemd, cron, Docker, or other
   launcher and translated into positional arguments or core options.

Do not assume that a wrapper-only variable is accepted when invoking the binary directly.

## Core selection and connection

| Environment variable | CLI equivalent | Notes |
| --- | --- | --- |
| `SDSYNC_CONFIG` | `--config FILE` | Selects a non-secret TOML file. |
| `SDSYNC_PROFILE` | `--profile NAME` | Overrides `default-profile`. |
| `SDSYNC_URL` | `--url URL` | Public File Station reverse-proxy base URL. |
| `SDSYNC_USERNAME` | `--username USER` | Dedicated DSM account. |
| `SDSYNC_REMOTE` | target diagnostic `REMOTE` | Core environment input for the target diagnostic destination. Sync/plan source and remote remain positional or profile values. |

## Core authentication

| Environment variable | CLI equivalent | Secret value? |
| --- | --- | --- |
| `SDSYNC_PASSWORD_STDIN` | `--password-stdin` | No; boolean selector. |
| `SDSYNC_PASSWORD_FILE` | `--password-file FILE` | No; protected file path. |
| `SDSYNC_TOTP_SECRET_FILE` | `--totp-secret-file FILE` | No; protected file path. |
| `SDSYNC_NO_VAULT` | `--no-vault` | No; boolean selector. |
| `SDSYNC_PASSWORD` | no CLI value option | **Yes.** Password fallback; avoid long-lived service environments. |
| `SDSYNC_OTP` | no CLI value option | **Yes.** One current six-digit OTP, not a reusable seed. |

`--vault` re-enables vault lookup over `SDSYNC_NO_VAULT=true`. `--password-stdin` suppresses a
lower-layer password-file locator. Batch plan/sync and batch target diagnostics reject password
stdin because one stream cannot safely represent independent profile credentials.

## Core synchronization and deletion

| Environment variable | CLI equivalent | Accepted values |
| --- | --- | --- |
| `SDSYNC_COMPARE` | `--compare` | `content`, `metadata`, `size-only` |
| `SDSYNC_JOBS` | `--jobs` | `1` through `16` |
| `SDSYNC_DELETE` | `--delete` | boolean; defeated by `--no-delete` |
| `SDSYNC_ALLOW_EMPTY_SOURCE` | `--allow-empty-source` | boolean; requires deletion |
| `SDSYNC_MAX_DELETE` | `--max-delete` | non-negative count |
| `SDSYNC_MAX_TOTAL_DELETE` | `--max-total-delete` | aggregate batch count; default `100` |

There is no core `SDSYNC_EXCLUDE`: use profile `excludes`, a root `.sdsyncignore`, or repeated
`--exclude` arguments.

## Core network and TLS

| Environment variable | CLI equivalent | Accepted values |
| --- | --- | --- |
| `SDSYNC_RETRIES` | `--retries` | `0` through `5` |
| `SDSYNC_TIMEOUT` | `--timeout` | seconds, at least `1` |
| `SDSYNC_CONNECT_TIMEOUT` | `--connect-timeout` | seconds, at least `1` |
| `SDSYNC_MAX_RATE` | `--max-rate` | bytes/second, at least `1` |
| `SDSYNC_CA_CERTIFICATE` | `--ca-certificate` | PEM file path |
| `SDSYNC_ALLOW_HTTP` | `--allow-http` | boolean |
| `SDSYNC_DANGER_ACCEPT_INVALID_CERTS` | `--danger-accept-invalid-certs` | boolean |

## Core output and observability

| Environment variable | CLI equivalent | Accepted values |
| --- | --- | --- |
| `SDSYNC_QUIET` | `--quiet` | boolean; defeated by `--no-quiet` |
| `SDSYNC_LOG_LEVEL` | `--log-level` | `trace`, `debug`, `info`, `warn`, `error`, `off` |
| `SDSYNC_LOG_FORMAT` | `--log-format` | `human`, `json` |
| `SDSYNC_LOG_FILE` | `--log-file` | writable file path |
| `SDSYNC_REMOTE_LOG_URL` | `--remote-log-url` | absolute HTTPS URL |
| `SDSYNC_REMOTE_LOG_TOKEN_FILE` | `--remote-log-token-file` | protected token-file path |
| `SDSYNC_REMOTE_LOG_TOKEN_ENV` | `--remote-log-token-env` | **name** of a token-bearing variable |
| `SDSYNC_REMOTE_LOG_TOKEN` | implicit fallback | **Secret bearer-token value** |
| `SDSYNC_REMOTE_LOG_MODE` | `--remote-log-mode` | `best-effort`, `required` |
| `SDSYNC_PROGRESS` | `--progress` | `auto`, `always`, `never` |
| `SDSYNC_OUTPUT` | `--output` | `human`, `json`, `ndjson` |

There is no core verbosity environment variable; use profile `verbose` or repeat `-v`/`--verbose`.

Credential enrollment uses command-specific stdin flags rather than secret-valued options:
`credentials set-password --password-stdin` reads the first password line, and `credentials
set-totp --secret-stdin` reads the first Base32-seed or `otpauth://` line. These flags are distinct
from sync authentication's `--password-stdin` and should receive protected piped input.

## Wrapper-only orchestration variables

The supplied service wrappers add a small orchestration layer around the binary:

| Variable | Wrapper scope | Meaning |
| --- | --- | --- |
| `SDSYNC_SOURCE` | systemd, cron, Docker/Compose | Local path translated into positional `SOURCE`; it is not a direct core CLI environment input. |
| `SDSYNC_REMOTE` | systemd, cron, Docker/Compose | Destination translated into positional `REMOTE` for sync/plan. |
| `SDSYNC_PROFILES` | systemd, cron | Comma-separated profile batch translated into `--profiles`. |
| `SDSYNC_ALL_PROFILES` | systemd, cron | Boolean translated into `--all-profiles`. |
| `SDSYNC_BATCH_SHARED_CREDENTIALS` | systemd, cron | Explicit confirmation that the wrapper's common password/TOTP files are correct for every selected profile. |
| `SDSYNC_LOCK_FILE` | systemd, cron | Shared local non-overlap lock. Related jobs must use the same writable path. |
| `SDSYNC_EXECUTABLE` | cron | Absolute binary selected by the wrapper. |
| `SDSYNC_USE_TOTP_CREDENTIAL` | systemd | Maps the optional systemd TOTP credential into the invocation. |
| `SDSYNC_USE_REMOTE_LOG_CREDENTIAL` | systemd | Maps the optional systemd bearer-token credential. |
| `SDSYNC_RUNTIME_UID`, `SDSYNC_RUNTIME_GID` | Docker/Compose helper | Container runtime identity used for source/secret access. |
| `SDSYNC_COMPOSE_DIR`, `SDSYNC_COMPOSE_CONTAINER_NAME` | Docker/Compose helper | Helper state and container naming. |

Each wrapper deliberately rejects dangerous secret-value environment variables or unsupported
combinations. Read its [operation page](../operations/scheduling.md) and tracked source documentation
before changing the example environment file.

## Boolean environment spelling

Use the values accepted by Clap and by the wrapper in scope, normally `true` or `false`. A boolean
variable being present with `false` still counts as an environment-sourced parser value; the core
resolver therefore handles the documented negation options instead of relying on parser conflicts.
