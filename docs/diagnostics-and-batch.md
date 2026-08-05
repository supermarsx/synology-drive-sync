# Diagnostics and multi-profile batches

The diagnostic commands reuse the same source scanner, File Station discovery, authentication,
and path rules as synchronization. Multi-profile selection turns complete named profiles into one
deterministically ordered batch. This page defines where those checks stop, when remote mutation
can begin, and what an operator must still provide around the process.

> [!IMPORTANT]
> The automated suite uses local and mock-HTTP tests; it does not log in to a live NAS. A successful
> diagnostic is evidence about the endpoint and account that were actually tested, not proof for
> every DSM/File Station version, reverse-proxy product, DNS alias, or production scheduler. Finish
> the [disposable live-NAS acceptance](production-acceptance.md) before production use.

## Local source diagnostics

`doctor source` is local-only. It neither resolves DSM credentials nor contacts or modifies a NAS:

```bash
synology-drive-sync doctor source ./export
synology-drive-sync doctor source ./export --hash --output json
```

`SOURCE` may instead come from a selected profile. The command canonicalizes the source root and
uses the production scanner, the root `.sdsyncignore`, profile exclusions, and repeated diagnostic
`--exclude` values. It reports deterministic counts for entries, files, directories, payload bytes,
and hashed files. The root itself is not included in the entry or directory count.

A source may be a mounted or mapped NAS folder only when the operating system exposes it to the
running identity as an ordinary readable directory with stable file metadata. This client does not
log in to or mount that source NAS: SMB/NFS credentials and mount lifecycle remain the operator's
responsibility. In particular, a Windows drive mapping created in an interactive session may not
exist for a scheduled task; validate the exact path with `doctor source` under the scheduler
identity before relying on it. DSM password/TOTP options authenticate the File Station target, not
the mounted source.

Without `--hash`, the scanner validates names, types, metadata, case portability, link/reparse
boundaries, and readability needed to enumerate the tree. With `--hash`, it additionally reads every
payload file, records an MD5 digest, and verifies that each file still matches the size and mtime
snapshot taken by the scan. Cancellation or any scanner/read/snapshot failure returns no successful
partial report. The diagnostic never writes to the source.

MD5 here is a compatibility and accidental-corruption check because it is the digest exposed by
File Station; it is not collision-resistant. Keep an independently generated SHA-256 manifest for
release or production acceptance.

Machine output for one source uses `sdsync.source-doctor.v1`. A selected source batch emits one
`sdsync.source-doctor-job.v1` record per profile and an aggregate
`sdsync.source-doctor-batch.v1` summary. JSON nests the job records under the summary; NDJSON emits
the jobs in execution order and the summary last.

## Target diagnostics

The normal target diagnostic is authenticated but non-mutating:

```bash
synology-drive-sync doctor --profile production target
synology-drive-sync doctor \
  --url https://files.example.com \
  --username mirror-bot \
  target /team-folder/project
```

It checks TLS and reverse-proxy routing, required API versions, password and optional TOTP
authentication, a File Station write-permission query, and recursive inventory of the logical
destination. For an existing destination, permission is checked in that exact directory. For a
missing destination, File Station can check only the first missing component under its nearest
existing ancestor without creating anything; deeper missing components cannot have independent
ACLs yet.

`--routing-only` remains the unauthenticated TLS/proxy/API-discovery check and cannot be combined
with `doctor source` or `doctor target`.

### Disposable write test

`doctor target --write-test` is deliberately different: it is an explicit remote mutation test.
The logical target itself must already exist. Use it only inside a prepared, non-critical
destination after the normal target diagnostic succeeds:

```bash
synology-drive-sync doctor --profile acceptance \
  target /team-folder/sdsync-acceptance-UNIQUE --write-test
```

The probe uses a unique disposable name, creates its own folder, uploads known content, verifies
the remote bytes, exercises a server-side copy when the NAS advertises support, and removes only
the probe artifacts it created. It must never use or replace an existing user path. A process crash,
lost connection, DSM failure, or failed cleanup can leave a disposable artifact; inspect and remove
that exact probe path manually before retrying. The write test is not appropriate as a routine
production health check.

A single target result uses `sdsync.doctor.v1`. The nested `write_test` object records whether the
probe was requested, its success/failure status, each completed stage, cleanup state, and any
leftover probe path. A selected target batch uses `sdsync.doctor-job.v1` records and an
`sdsync.doctor-batch.v1` summary.

## Selecting complete profile jobs

`--profiles` selects comma-separated names and may be repeated. `--all-profiles` selects every
named profile. Both require an existing configuration file, conflict with single `--profile`, and
reject duplicate or unknown names:

```bash
synology-drive-sync plan --config ./config.toml \
  --profiles photos,documents --max-total-delete 20 --output json

synology-drive-sync sync --config ./config.toml \
  --all-profiles --max-total-delete 20 --output ndjson

synology-drive-sync doctor --config ./config.toml \
  --profiles photos,documents source --hash --output ndjson

synology-drive-sync doctor --config ./config.toml \
  --profiles nas-a,nas-b target --output json
```

Each selected sync/plan profile must resolve a complete `source`, `remote`, reverse-proxy `url`,
DSM `username`, authentication source, and safety policy. Batch sync/plan rejects positional SOURCE
or REMOTE overrides; source and target diagnostic batches likewise reject one positional path being
applied to every profile. Common non-positional CLI/environment options still override each selected
profile. If profiles choose different result formats, pass one explicit `--output` for the batch.

`--password-stdin` is rejected for plan/sync and target-diagnostic batches because one stream cannot
safely represent distinct job credentials. Use the OS vault entries keyed to each profile's
URL/username or a protected `password-file` in each profile; add a protected
`totp-secret-file` per profile when its DSM account uses authenticator-app TOTP. Secret values are
never valid TOML fields.

Selection and execution are deterministic by profile name; the order written after `--profiles`
does not create a priority mechanism. Jobs execute sequentially. Uploads inside one job may still
use that profile's `jobs` concurrency.

## Overlap and endpoint identity

Before any batch planning or execution, the client normalizes each configured URL and rejects equal
or nested File Station roots on the same normalized endpoint. DSM username is intentionally not part
of that overlap key: two accounts do not make concurrent writers to the same remote tree safe.
Sibling component names such as `/team/root` and `/team/rooted` are not overlaps.

This protection cannot determine that different public URLs reach the same NAS. Different DNS
names, ports, or reverse-proxy prefixes can be aliases while appearing to be different endpoints.
Operators must use one canonical public base URL per NAS or manually prove that selected roots do
not overlap across aliases. The same limitation applies between separate running processes.

## All-job preflight and deletion budgets

A mutating batch has two stages:

1. resolve and validate every selected profile, reject overlaps, scan every source, authenticate to
   every target, check destination permission, inventory/hash the target as required, and build
   every plan;
2. only after every plan succeeds and every deletion budget passes, execute jobs sequentially in
   deterministic order. Each job is freshly replanned immediately before it can mutate, and its
   fresh deletion count must still fit the remaining aggregate budget.

If any stage-one job fails, no selected job is mutated. This all-target preflight is a safety gate,
not a remote transaction: another writer can still change a target after its plan and before that
job executes.

Every job retains its own `delete` and `max-delete` guard. A batch adds
`--max-total-delete N` (or `SDSYNC_MAX_TOTAL_DELETE`) across all selected plans; its default is 100.
The client requires a deletion count for every job, requires zero when deletion is disabled, checks
each per-job cap, checked-adds the counts, and then checks the aggregate cap before mutation. Review
both the per-job plans and aggregate count; do not use the aggregate limit to compensate for an
overly broad individual profile.

An initial aggregate-cap breach stops the batch before any selected mutation. During execution,
each fresh plan reserves its deletion count against the same cap before that profile may mutate. A
fresh-plan breach denies mutation for that profile and stops later jobs, although earlier completed
jobs remain committed. Both cases are operational safety failures with exit `1`, not configuration
errors with exit `2`.

A target-diagnostic batch with `--write-test` follows the same mutation boundary: it first checks
routing and discovery (including the required content and delete APIs), authentication, exact target
existence, write permission, and inventory for every selected profile without mutation. No probe is
started if any such preflight fails. Probes then run sequentially; a failed probe stops later
probes. Successful earlier probes have already completed their own cleanup, while a cleanup failure
reports the exact leftover probe path.

A target-diagnostic job is `partial` when its probe progressed far enough that remote mutation may
have occurred before the reported failure; inspect the nested write-test report and leftover path
even when cleanup appears complete. In the doctor aggregate,
`all_targets_preflighted_before_mutation` is true only for a requested write-test batch whose every
target produced preflight evidence; an interrupted or failed preflight reports false.

## Aggregate results and partial failure

Human output identifies each profile and finishes with aggregate counts.
JSON returns one aggregate object containing ordered per-job results. NDJSON writes ordered per-job
records followed by one aggregate summary record. Depending on the command phase, each job status
is `preflighted`, `success`, `partial`, `failed`, or `not-run`; aggregate status is `success`,
`partial`, or `failed`.

For `sdsync.batch-job.v1`, `preflight_plan` is the first non-mutating all-target plan and
`execution_plan` is the fresh plan obtained when that job reaches execution. `mutation_authorized`
becomes true only after a non-empty fresh plan passes its aggregate deletion reservation and remote
operations are allowed to begin. `failed` therefore means the job failed before that boundary;
`partial` conservatively means mutation was authorized and the target may have changed before the
failure. A successful already-converged job can have `mutation_authorized: false` and an empty
`execution_plan`.

The `sdsync.batch.v1` aggregate keeps `preflight_deletions` distinct from
`execution_reserved_deletions`. The former is the accepted initial all-job total; the latter is the
sum successfully reserved from fresh plans for jobs reached during sync. Either can be `null` when
its phase was not completed or was not applicable. The `all_targets_preflighted_before_mutation`
boolean is observed evidence derived from the job records, not a declaration of intended policy: it
is false when preflight failed or was interrupted. Each plan still retains its own deletion count.

If execution of a mutating job fails, already completed earlier jobs are not rolled back and later
jobs are not run. The command returns nonzero after writing the aggregate result. The aggregate is
`partial` when a job itself is potentially partial or when an earlier job succeeded before another
failed; otherwise a no-success failure is `failed`. Preserve stdout and stderr together with the
profile order, repair the failed job, obtain a fresh plan for every affected target, and do not
infer that `not-run` means already synchronized.

For machine parsing, pass `--output json` or `--output ndjson` explicitly rather than relying on
different per-profile defaults. Command results remain on stdout; diagnostic logs and progress
remain on stderr. Paths and error messages in aggregate output are operationally sensitive even
though secret values are redacted.

## Scheduling and shared locks

Prefer one scheduled batch command over independent per-profile tasks when the jobs share an
operational window. The process itself is sequential, but it does not provide a host-wide or
cluster-wide lock. Use one shared scheduler mutex for the batch and every single-profile sync that
could reach the same NAS or source. On Linux this can be one `flock` path; on systemd, macOS, and
Windows use the documented no-overlap mechanism plus a shared wrapper lock when multiple units,
agents, or tasks exist.

Size the outer scheduler timeout for the complete batch: every source scan/hash, every target
preflight, all sequential uploads/copies/deletions and retries, final reconciliation, logging flush,
and cleanup. A per-job timeout multiplied by the job count is only a starting estimate. Alert on any
nonzero result and retain aggregate stdout plus stderr. Do not schedule `target --write-test` against
a production destination.

The built-in overlap check cannot coordinate another host, container, manual invocation, Drive
client, or File Station user. Keep mirror destinations in a documented single-writer window and use
NAS snapshots/versioning or backup for recovery.
