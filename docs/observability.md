# Observability and machine output

`synology-drive-sync` keeps three output channels separate:

- command results go to standard output and follow `--output`;
- diagnostics and structured log events go to standard error, an optional rotating file, and an optional HTTPS collector;
- live progress goes to standard error only when its terminal policy allows it.

This separation makes stdout safe to pipe into a parser without losing durable diagnostics. The
log, progress, and plan/sync machine contracts are described by
[`observability.schema.json`](observability.schema.json). Doctor and credential results, plus
configuration path and validation results, use their own schema tags; `config show` emits the
non-secret effective-profile object directly.

## Defaults

| Setting | Default | Notes |
| --- | --- | --- |
| Log level | `info` | `debug` with `-v`, `trace` with `-vv` |
| Log format | `human` | `json` is one complete object per line |
| Standard-error log sink | enabled | disabled by `--quiet` or `--log-level off` |
| Log file | disabled | enabled with `--log-file FILE` |
| File rotation | 10 MiB, 3 backups | active file plus `.1`, `.2`, and `.3`; `.1` is newest |
| Remote collector | disabled | enabled with `--remote-log-url URL` |
| Remote queue | 1,024 events | bounded; it cannot grow with an unavailable collector |
| Remote request timeout | 10 seconds | connect and whole-request timeout |
| Remote failure policy | `best-effort` | select `required` when logs are part of the run's success criteria |
| Shutdown/flush deadline | 5 seconds | bounds the explicit remote drain attempt |
| Progress | `auto` | interactive human terminal only |
| Command output | `human` | alternatives are `json` and `ndjson` |

`--quiet` disables the standard-error log sink and progress, but it does not silently disable a
configured file or remote sink. `--log-level off` disables structured event emission to every
sink. Command results on stdout and final error reporting are separate from structured logging.

## Configuration and precedence

All observability options are global, so they may appear before or after a subcommand:

```console
synology-drive-sync sync ./export /team/export \
  --profile production \
  --log-level debug \
  --log-format json \
  --log-file ./logs/sync.log \
  --progress never \
  --output json
```

The general value precedence is command line, `SDSYNC_*` environment variable, selected profile,
then built-in default. Log-level resolution is deliberately more specific:

1. `--log-level LEVEL` (or `SDSYNC_LOG_LEVEL` when the flag is absent);
2. command-line `-vv` (`trace`) or `-v` (`debug`);
3. profile `log-level`;
4. profile `verbose` (`2` means `trace`, `1` means `debug`);
5. `info`.

An explicit log level therefore wins over verbosity. Levels are `trace`, `debug`, `info`, `warn`,
`error`, and `off`. `-v` conflicts with `--quiet` on the command line.

The corresponding non-secret profile fields are:

```toml
[profiles.production]
log-level = "info"
log-format = "json"
log-file = "logs/sync.log"
remote-log-url = "https://logs.example.com/v1/sdsync/events"
remote-log-token-file = "secrets/log-token"
remote-log-mode = "best-effort"
progress = "auto"
output = "human"
```

Relative `log-file` and token-file paths are resolved relative to the configuration file. A
profile may use `remote-log-token-env = "MY_SDSYNC_LOG_TOKEN"` instead of a token file. It may
never contain the token value itself.

| CLI option | Environment setting | Profile field |
| --- | --- | --- |
| `--quiet` | `SDSYNC_QUIET` | `quiet` |
| `--log-level` | `SDSYNC_LOG_LEVEL` | `log-level` |
| `--log-format` | `SDSYNC_LOG_FORMAT` | `log-format` |
| `--log-file` | `SDSYNC_LOG_FILE` | `log-file` |
| `--remote-log-url` | `SDSYNC_REMOTE_LOG_URL` | `remote-log-url` |
| `--remote-log-token-file` | `SDSYNC_REMOTE_LOG_TOKEN_FILE` | `remote-log-token-file` |
| `--remote-log-token-env` | `SDSYNC_REMOTE_LOG_TOKEN_ENV` | `remote-log-token-env` |
| `--remote-log-mode` | `SDSYNC_REMOTE_LOG_MODE` | `remote-log-mode` |
| `--progress` | `SDSYNC_PROGRESS` | `progress` |
| `--output` | `SDSYNC_OUTPUT` | `output` |

`SDSYNC_REMOTE_LOG_TOKEN_ENV` contains an environment-variable *name*, not a bearer token. If a
remote URL is configured without either token-source option, the named source defaults to
`SDSYNC_REMOTE_LOG_TOKEN`; that latter variable contains the actual secret.

## Logs

`--log-format human` produces timestamped, single-line diagnostics. For example:

```text
1785769200123 INFO  sync run completed operations=14 files=10 bytes=5242880 elapsed_ms=8421
```

`--log-format json` emits one `sdsync.log.v1` object per line to each enabled local sink:

```json
{"schema":"sdsync.log.v1","timestamp_ms":1785769200123,"level":"info","event":"run.completed","operation_id":null,"attempt":null,"metrics":{"operations":14,"files":10,"bytes":5242880,"elapsed_ms":8421,"throughput_bytes_per_second":622600,"eta_ms":null}}
```

The timestamp is Unix time in milliseconds. Event names are a closed set:

```text
run.started                    run.completed                  run.failed
local_scan.started             local_scan.completed
api_discovery.started          api_discovery.completed
authentication.started         authentication.completed
remote_scan.started            remote_scan.completed
plan.ready
upload.started                 upload.attempt_started          upload.progress
upload.completed               upload.failed
directory.created              entry.deleted
retry.scheduled                cancellation.requested
```

Every JSON log record has the same fields. `operation_id` and `attempt` are `null` when not
applicable, and unused numeric metrics are zero. This stable shape is preferable to parsing human
messages.

### Rotating file sink

`--log-file FILE` appends records in the selected `--log-format`. Before a write that would take a
non-empty active file past 10 MiB, the logger rotates it:

```text
sync.log      active
sync.log.1    newest backup
sync.log.2
sync.log.3    oldest backup
```

Rotation is local, uncompressed, and limited to three backups. A newly created log file requests
mode `0600` on Unix. An existing file retains its permissions, and Windows files inherit the
directory's ACL, so provision the log directory for the service account rather than relying only
on the file-creation default.

## Command output: human, JSON, and NDJSON

`--output` controls command-result records on stdout independently of `--log-format`:

- `human` prints concise result sentences;
- `json` writes one complete command result object;
- `ndjson` streams a plan summary, each planned action, and—after a sync—a completion record.

For example, `plan --output json` returns the full plan in one `sdsync.plan.v1` object:

```json
{"schema":"sdsync.plan.v1","plan":{"summary":{"uploads":1,"upload_bytes":5242880,"directories":1,"deletions":0,"unchanged_files":41,"protected_entries":0,"changes":true},"actions":{"pre_deletes":[],"creates":[{"relative":"releases","remote_path":"/team/export/releases"}],"uploads":[{"relative":"release.bin","remote_path":"/team/export/release.bin","bytes":5242880,"mtime_ms":1785769200000}],"post_deletes":[]}}}
```

`sync --output json` uses `sdsync.sync.v1`, retains the complete `plan`, and adds `result`. A changed
sync result contains `changed`, `uploaded`, `upload_bytes`, `directories_created`, `deleted`, and
`elapsed_ms`; an already-synchronized run uses `"result":{"changed":false}`.

NDJSON uses a streaming contract rather than splitting the JSON object mechanically. Records are
ordered as follows:

1. one `sdsync.plan.v1` record with `kind: "summary"`;
2. zero or more `sdsync.plan-action.v1` records in execution order—`delete-conflict`,
   `create-directory`, `upload`, then `delete`;
3. for `sync` only, one `sdsync.output.v1` record with `kind: "completion"`.

```ndjson
{"schema":"sdsync.plan.v1","kind":"summary","uploads":1,"upload_bytes":5242880,"directories":1,"deletions":0,"unchanged_files":41,"protected_entries":0,"changes":true}
{"schema":"sdsync.plan-action.v1","action":"create-directory","relative":"releases","remote_path":"/team/export/releases"}
{"schema":"sdsync.plan-action.v1","action":"upload","relative":"release.bin","remote_path":"/team/export/release.bin","bytes":5242880,"mtime_ms":1785769200000}
{"schema":"sdsync.output.v1","kind":"completion","result":{"changed":true,"uploaded":1,"upload_bytes":5242880,"directories_created":1,"deleted":0,"elapsed_ms":8421}}
```

A plan-only NDJSON stream ends after its last action. An unchanged sync emits the summary followed
by `{"schema":"sdsync.output.v1","kind":"completion","changed":false}`. The linked JSON Schema
defines every plan/action/result variant.

Plan results intentionally contain relative local paths and File Station logical remote paths so
automation can review exact work. Treat stdout as path-sensitive operational data even though it
never contains file contents or authentication secrets.

JSON logs are not command output: `--log-format json` never changes stdout, and `--output json`
never changes the log sinks. This makes combinations such as human results plus JSON file logs, or
NDJSON results plus human diagnostics, explicit and predictable.

## Progress

Progress is enabled only for human command output and when `--quiet` is absent:

- `--progress auto` (default) renders a single updating line only when stderr is a terminal and
  logs are human-readable;
- `--progress always` renders progress even when stderr is redirected;
- `--progress never` disables progress.

With `--output human --log-format json --progress always`, progress is emitted as
`sdsync.progress.v1` NDJSON on stderr so the diagnostic stream stays entirely structured. `auto`
remains suppressed with JSON logs.

Machine result modes (`--output json` and `--output ndjson`) suppress progress even if `always` is
configured. This guarantees that automation receives deterministic output. Structured logs remain
available for unattended status reporting.

The tracker is safe for parallel uploads and reports aggregate files, logical bytes, bytes sent on
the wire, throughput, active operations, and ETA. On an upload retry, the current file's logical
position resets before the new attempt so overall completion never exceeds the plan. Wire bytes
remain cumulative across attempts and are used for throughput. Per-operation records contain only
a numeric ID, operation kind, attempt, and byte counters—never a local or remote path.

The library also defines `sdsync.progress.v1` NDJSON records for callers that route progress to a
dedicated stream. The CLI deliberately does not mix those records into stdout or a JSON log stream.

## Remote HTTPS logging

Remote logging sends each `sdsync.log.v1` event as one HTTPS request:

```http
POST /v1/sdsync/events HTTP/1.1
Content-Type: application/json
Authorization: Bearer <token>

{"schema":"sdsync.log.v1", ...}
```

Any 2xx response accepts the event. Other statuses are rejected deliveries. The configured URL
must:

- use `https`;
- contain a host;
- contain no username or password;
- contain no query string or fragment.

Redirect following is disabled. This pins the event and bearer header to the exact configured
endpoint rather than forwarding them to another origin. Configure the collector's final URL and a
certificate trusted by the host running `synology-drive-sync`.

Bearer tokens are loaded at startup from `--remote-log-token-file FILE` or the environment variable
named by `--remote-log-token-env NAME`. The two options conflict. The token is limited to 16 KiB,
must be visible ASCII without quotes, has trailing CR/LF removed, and is held in zeroizing memory.
The token is never accepted directly as a command-line or TOML value. Provision token files with
service-account-only permissions.

### Backpressure and failure policy

The remote sender has a bounded 1,024-event queue and never makes upload workers perform the HTTPS
request themselves.

In `best-effort` mode:

- a full queue drops the new remote event without stopping the sync;
- transport failures and rejected responses are counted;
- local stderr/file logging continues;
- shutdown reports drop/failure counters but remote delivery does not determine command success.

In `required` mode:

- a full queue is an observability error;
- the first transport failure or rejection is surfaced on a subsequent event, flush, or shutdown;
- a missing `--remote-log-url` is a configuration error;
- inability to satisfy the remote logging contract can fail the command.

Each connect and request is bounded to 10 seconds. Normal explicit shutdown stops accepting remote
events, places a barrier after already queued events, and waits up to 5 seconds for delivery and
worker termination. If the deadline expires, shutdown returns a timeout and detaches the unfinished
worker rather than blocking indefinitely. Ordinary object destruction is also non-blocking; callers
that need a delivery result must use explicit shutdown.

### Minimal collector example

The following standard-library Python receiver validates a bearer token and the schema tag. It
listens on loopback HTTP; terminate TLS in a real reverse proxy in front of it. It intentionally
does not log request headers.

```python
import json
import os
import secrets
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

EXPECTED = os.environ["COLLECTOR_TOKEN"].encode("ascii")
MAX_EVENT_BYTES = 64 * 1024


class Handler(BaseHTTPRequestHandler):
    def log_message(self, _format, *_args):
        pass

    def reply(self, status):
        self.send_response(status)
        self.end_headers()

    def do_POST(self):
        if self.path != "/v1/sdsync/events":
            return self.reply(404)

        supplied = self.headers.get("Authorization", "").encode("ascii", "ignore")
        if not secrets.compare_digest(supplied, b"Bearer " + EXPECTED):
            return self.reply(401)

        try:
            size = int(self.headers.get("Content-Length", ""))
        except ValueError:
            return self.reply(411)
        if size < 1 or size > MAX_EVENT_BYTES:
            return self.reply(413)

        try:
            event = json.loads(self.rfile.read(size))
        except (json.JSONDecodeError, UnicodeDecodeError):
            return self.reply(400)
        if not isinstance(event, dict) or event.get("schema") != "sdsync.log.v1":
            return self.reply(422)

        sys.stdout.write(json.dumps(event, separators=(",", ":")) + "\n")
        sys.stdout.flush()
        self.reply(204)


ThreadingHTTPServer(("127.0.0.1", 9080), Handler).serve_forever()
```

An illustrative Caddy front end is:

```caddyfile
logs.example.com {
    reverse_proxy 127.0.0.1:9080
}
```

Run the receiver with `COLLECTOR_TOKEN` supplied by the collector host's secret manager, then point
the client at `https://logs.example.com/v1/sdsync/events`. Do not put the token in the URL, Caddyfile,
process arguments, or access logs.

## Privacy and redaction guarantees

Logging is redacted by construction rather than by searching arbitrary strings after formatting:

- event codes are a closed enum;
- event metadata is numeric only;
- progress uses numeric operation IDs and counters;
- no log or progress record accepts paths, usernames, URLs, headers, bearer tokens, passwords, OTP
  codes, TOTP seeds, free-form messages, or arbitrary key/value fields;
- remote transport errors are reduced to fixed categories and never include response bodies;
- endpoint validation errors and token errors do not echo the offending value;
- redirects are disabled so a bearer header cannot be forwarded by the client.

Logs still disclose operational metadata: timestamps, event types, counts, byte volumes, durations,
throughput, retry attempts, and numeric operation IDs. Treat them according to that metadata's
sensitivity. This redaction-by-construction guarantee applies to log and progress schemas, not all
command output. Plan/sync output deliberately identifies relative and remote paths; the effective
configuration view likewise contains non-secret connection and path settings. None of these
machine-output contracts includes password, OTP, TOTP-seed, bearer-token, session, or file-content
fields. Operating-system errors, reverse-proxy access logs, crash dumps, and third-party collector
software are outside the observability boundary. In particular, configure every proxy and collector
to redact or omit the `Authorization` header.
