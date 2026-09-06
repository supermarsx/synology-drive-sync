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
the mounted source. See [Local, mapped-drive, and SMB sources](local-and-smb-sources.md) for
per-platform paths, the share-root limitation, and expected SMB performance.

Without `--hash`, the scanner validates names, types, metadata, case portability, link/reparse
boundaries, and readability needed to enumerate the tree. With `--hash`, it additionally reads every
payload file, records MD5, IEEE CRC32, and SHA-256, and verifies that each file still matches the size and mtime
snapshot taken by the scan. Cancellation or any scanner/read/snapshot failure returns no successful
partial report. The diagnostic never writes to the source.

The diagnostic computes all three digests in one read. MD5 is retained for File Station and output
compatibility, while content equality requires CRC32 and SHA-256 as well. Keep an independently
generated and separately retained SHA-256 manifest for release or production acceptance so the
recovery check does not depend on the same live process or endpoint.

Machine output for one source uses `sdsync.source-doctor.v1`. A selected source batch emits one
`sdsync.source-doctor-job.v1` record per profile and an aggregate
`sdsync.source-doctor-batch.v1` summary. JSON nests the job records under the summary; NDJSON emits
the jobs in execution order and the summary last.

## Target diagnostics

`--level` selects target depth and belongs before the `target` subcommand. It does not alter
`doctor source`, which is a separate command and rejects `--level`:

```bash
synology-drive-sync doctor --profile production --level standard target
synology-drive-sync doctor --url https://files.example.com --level quick target
synology-drive-sync doctor \
  --url https://files.example.com \
  --username mirror-bot \
  --level extensive \
  target /team-folder/project
```

| Level | Target checks | Authentication and destination access |
| --- | --- | --- |
| Quick | Endpoint policy, TCP/TLS and reverse-proxy route, DSM/File Station discovery, and the API surface required to establish the diagnostic client | None; it does not resolve credentials, create a session, inspect permissions, or list the destination |
| Standard (default) | Quick plus the effective profile's required APIs | Password and challenge-driven optional TOTP, a temporary session, bounded target discovery, destination permission when a remote is selected, and logout |
| Extensive | Standard plus content-fingerprint/download and delete capabilities, with optional server-side copy reported as a warning when absent | The same bounded target discovery; it still does not mutate the target or read/hash remote file payloads |

Allowed HTTP or disabled certificate verification is visible as a warning, not silently treated as
secure transport. For an existing destination, write permission is checked in that exact directory.
For a missing destination, File Station can check only the first missing component under its
nearest existing ancestor without creating anything; deeper missing components cannot have
independent ACLs yet.

### Bounded target discovery

Standard and Extensive use the same deliberately small discovery check after authentication. Its
scope depends only on whether the resolved invocation selected a remote:

- Without `REMOTE` and without a profile remote, Doctor requests the File Station shared-folder
  roots visible to that account. The result uses scope `visible_shared_folders` and contains at most
  five directory entries. This is discovery evidence only: Doctor never selects a share, descends
  into one, or treats any listed share as a synchronization destination. A root returned by the
  authenticated `list_share` call is retained even when File Station marks child listing disabled;
  its presence does not claim browse, read, or write permission.
- With `REMOTE`, directly or through a profile, Doctor validates that exact logical root and requests
  at most one direct-child page sorted by name. The result uses scope `direct_children` and contains
  at most five folder/file entries. Doctor never follows those children.

Both scopes request at most six entries so Doctor can emit a deterministic sample of no more than
the first five and record whether more entries exist. The result contains the reported total,
`sample_count`, `sample_limit: 5`, `truncated`, and `truncated_count`; `truncated: true` means only
that the displayed evidence was capped. A valid response with no visible roots or no direct
children is preserved as explicit evidence with `sample_count: 0` and an empty `sample`, rather than
being omitted or inferred as a failure. Extensive does not lift these bounds.

Sample entries contain only bounded logical name/relative-path text, kind, file size and mtime when
applicable, mount-boundary state, and explicit text-truncation flags. The sample contains no file
payload, digest, ACL document, absolute local volume path, credential, cookie, token, session
identifier, or unbounded server detail. Doctor never requests a second listing page and never
substitutes this evidence for the full recursive inventory used by Plan or Sync. Discovery has a
five-second deadline; its output records the page, depth, and deadline budget.

The command result retains this bounded `remote_inventory` object. When Doctor runs through the DSM
package manager, the DSM package also writes a separate local/private history record with the same
maximum-five structure for troubleshooting. That DSM-only history is exposed through Activity/Logs
on the NAS and excluded from remote-log forwarding; the core CLI does not generally create this
extra history file.

### Section verdicts and timing

A target report always carries the selected level, overall status, total elapsed time, counts, and
these fixed sections: network reachability and connect timing, routing/TLS, API discovery, DSM
capability enumeration, intermediaries and reverse proxies, DSM session authentication, DSM session
channel ablation, concurrent session fan-out, session cookie permanence, File Station capabilities,
File Station capability diagnosis, destination path resolution, destination permissions,
destination inventory, disposable write/verify/cleanup, and session logout. With no selected
remote, destination permissions and path resolution are explicitly skipped while visible
shared-folder discovery still runs in the destination-inventory section. Each section is `pass`, `warn`, `fail`, or `skip`, with bounded detail, `elapsed_ms`,
`timing_scope`, `step`, `calls`, and `remediation`. Routing and discovery share one connection
measurement and say so instead of presenting the same latency as two independent requests.
Control-plane requests remain capped at ten seconds, connection setup uses the configured connect
timeout, and the inventory has the stricter five-second bound above.

Sections are listed in reading order, which is **not** execution order: File Station capabilities
are settled from the already-fetched discovery response *before* authentication, but belong next to
the other File Station checks, and the two transport summaries run after the last request of the
run yet are displayed beside the checks they explain. Each section therefore carries a `step` giving
its real position, and the human report prints `step N/16` so the printed order never implies the
executed one.

`timing_scope` distinguishes how a section's `elapsed_ms` should be read:

| `timing_scope` | Meaning |
| --- | --- |
| `section` | The section's own requests were timed. |
| `shared_connection` | Routing and discovery shared one connection measurement. |
| `local_only` | No request was made; the verdict came from the cached discovery response. |
| `derived` | No request of its own; the verdict summarises requests other sections made. |

`local_only` exists because File Station capabilities legitimately report near-zero time — they are
a comparison against a response already in hand. The human report says so in words rather than
printing a bare `0 ms`, which reads as a broken measurement.

`calls` lists the DSM requests that section actually made, each with its run-wide `sequence`, API,
method, version, outcome, DSM code, HTTP status, attached session channels, and latency. A section
that made no request reports an empty list, which is itself evidence. The `sequence` is the number
the transport summaries cite, so a cookie the ledger reports as rotated on "call 3" can be found in
whichever section issued call 3. `remediation` is a concrete next step for a
failed section, or `null` when the failure does not imply one.

A section failure that proves the DSM session is unusable — codes 106, 107, and 119 — stops the
diagnostic instead of continuing. Those later requests could not have succeeded, and attempting
them presented one dead session as several independent failures. A permission refusal (105, 407) is
deliberately *not* treated this way: the session is still valid, and "can read but cannot write"
remains a useful diagnosis, so enumeration still runs.

The report and its JSON document both open with the `build` identity — name, version, target
triple, cargo profile, and commit — so a pasted diagnostic always names the binary that produced
it.

Any HTTP or DSM response from either discovery route is evidence that the endpoint route and its
transport negotiation worked, even when the response status or discovery payload is invalid. In
that case routing is pass/warn and API discovery fails. A connection, TLS-handshake, or timeout
failure that yields no response fails routing and leaves discovery skipped.

### Transport diagnostics

Three sections describe the path rather than the session, and all three run at every level
including Quick, because the question they answer matters most when authentication is already
failing.

**Network reachability and connect timing** (step 1) runs before any HTTP client is built.
Reachability is measured by opening TCP connections, **not** by ICMP echo: no ping packet is sent,
and nothing here needs a raw socket or elevated privileges. It records DNS resolution time and how
many addresses the hostname resolved to, then opens N TCP connections spread round-robin across
those addresses and reports min, median, mean, max, and spread. It then makes N unauthenticated
`SYNO.API.Info.query` requests over fresh connections — connection reuse is disabled for the probe,
so each pays a full DNS, TCP, and TLS cost — and reports connect-to-first-byte, body read, and
total. Sample counts are 3 TCP / 2 HTTP at Quick, 5 / 3 at Standard, and 9 / 5 at Extensive, each
under a total time ceiling.

The TLS handshake is **not** separately measurable through the blocking HTTP client, and the report
says so rather than inventing a split. The figure it does publish, "TLS handshake plus DSM service
time", is the median first byte minus the median TCP connect, and is labelled as a derived
remainder rather than a measured phase.

Spread is itself the diagnosis. A hostname resolving to more than one address, or a connect time
that varies by more than 25 ms *and* more than a factor of two across at least three samples, warns
that consecutive connections do not look like they reach one host. The section never fails the run;
`routing_tls` is what gates.

**Intermediaries and reverse proxies** (step 15) fingerprints every response of the run: `Via`,
`Server`, and `X-Powered-By` banners, `X-Forwarded-For`/`X-Real-IP` reflected back into a response,
CDN and cache markers, cookies set under names DSM does not use, and redirects offered (which
transport policy refuses) with their target host. It also classifies the endpoint hostname:
`[alias].[relay_id].quickconnect.to` is the **relay** form, while `[alias].direct.quickconnect.to`
and `[ip].[alias].direct.quickconnect.to` are direct. More than one distinct `Server` banner inside
one run means more than one origin answered, and is reported as such.

**Session cookie permanence** (step 16) follows every cookie the server set across the whole run,
including the logout response. For each name it reports where it first and last appeared, how many
times it was set, how many *distinct* values it carried, whether it is a session or a persistent
cookie or a deletion, and which attributes were present. A cookie re-issued under a changed value on
a call the server also reported as **successful** is called out loudly: this client keeps no cookie
jar, so such a value is observed and discarded while the previous one keeps being sent.

No cookie value ever reaches the report. Values are compared by a salted, per-process 32-bit digest
and are otherwise dropped; `Path` and `Domain` are reported as presence only; resolved IP addresses
are numbered rather than printed.

### Capability enumeration and diagnosis

**DSM capability enumeration** (step 4) asks `SYNO.API.Info.query` with `query=all`, which is
documented, unauthenticated, and available since DSM 4.0. It runs at **every** level including
Quick: an operator who cannot authenticate at all still learns exactly what their DSM offers.

This is deliberately a *separate* read from API discovery, which keeps its ten-name allowlist and
its strict wire type because that map feeds every call the client makes. `query=all` on a NAS with
third-party packages returns hundreds of entries authored by whoever wrote those packages, and
decoding that map strictly would let one malformed entry break connection establishment itself. The
diagnostic read decodes entry by entry into an all-optional type instead, and reports how many
entries it could not read rather than dropping them silently.

The report renders it in three tiers: every `SYNO.FileStation.*` entry with its offered version
range; the requirement matrix, naming for each API this tool uses the version it asks for, the
range DSM advertised, and whether they intersect; and every other namespace as a count. A required
API that is absent, or offered only at versions this tool cannot use, fails the section and names
both ranges -- the case that otherwise ambushes people at runtime. An optional one does not fail
it: the feature that needs it stops working, not the sync.

**File Station capability diagnosis** (step 9) is the other half, because advertised is not
functional. An API can sit in the discovery map and still answer 105 for this account, or 102
because a reverse proxy does not forward its CGI path -- and a `102` is worth having, because
discovery and the call went to the same origin, so only the proxy sits between them.

Only bounded, read-only, side-effect-free methods are probed: `SYNO.FileStation.Info` `get`,
`SYNO.FileStation.List` `list_share` with `limit=1`, and -- when enumeration advertised them --
`SYNO.FileStation.VirtualFolder` `list` and `SYNO.FileStation.BackgroundTask` `list`. Everything
else stays advertised-only and the report says why: mutating methods are proven by `--write-test`,
`DirSize`/`MD5`/`Search` `start` spawn background tasks that can walk a whole share, `Thumb` and
`Download` need a real user file, `Sharing` `list` returns existing share links, and `getinfo` is
already exercised against a real path by the destination walk.

`SYNO.FileStation.Info` `get` is read **twice**, once at the start of the section and once at the
end. It is the only documented non-admin call that names the host serving the request. The same
host name at both ends proves little -- a single NAS behind a relay has one host name -- but two
different ones inside a single run is the only positive evidence in this whole report that
consecutive requests reached different DSM hosts.

A 106, 107, or 119 from any probe means the *session* died, not that the capability is unavailable.
The remaining probes are abandoned, the section warns rather than fails, and every unprobed
capability is reported as `not probed`. Without that rule a dead session would masquerade as a wall
of broken capabilities.

### Session channel diagnostics

DSM accepts a session through several channels at once, and this client presents all of them: the
`_sid` request field, the `SynoToken` request field, an `X-SYNO-TOKEN` header, and a synthesised
`Cookie: id=<sid>`. When DSM rejects a session, the report cannot say which channel it rejected --
unless it varies them.

**DSM session channel ablation** (step 7) makes the same read-only `list_share` request four times,
varying only which channels carry the session: all of them first, so the probe reproduces the run's
own behaviour before changing anything, then the `_sid` field alone, the cookie alone, and the
`X-SYNO-TOKEN` header alone. The last variant carries no session identifier at all and is expected
to be rejected; it is the control that proves the probe can tell acceptance from rejection.

The verdicts are:

| All channels | `_sid` field alone | Verdict |
| --- | --- | --- |
| accepted | accepted | Channel selection is not the fault. |
| **rejected** | **accepted** | **Fail.** The extra channels are what DSM rejects. Logging in with `format=sid` is documented as "cookie will not be set", so the synthesised cookie asks DSM to resolve a session it never issued. |
| accepted | rejected | DSM is resolving the session from the cookie rather than from the field its own guide specifies for a `format=sid` login. |
| rejected | rejected | The session identifier itself is no longer valid server-side; not a channel problem. |

The probe deliberately does **not** stop on 106/107/119. Those codes are the observation it exists
to make, and aborting on the first one would tell the operator nothing. It varies only which
headers and fields are attached, and reports only the four presence booleans the records already
carry -- no session value reaches any surface.

**Concurrent session fan-out** (step 8) runs at Extensive only, because it multiplies request load
against a live NAS. It issues four simultaneous authenticated `list_share` requests and then one
more on its own. The HTTP pool opens additional TCP connections under concurrency, so this is the
closest available test of "a second connection loses the session"; the sequential call afterwards
is what separates "the burst was rejected" from "the burst invalidated the session".

### Destination path resolution

**Destination path resolution** (step 10) renders the component-by-component walk the destination
permission check already performs, so it costs no additional request and its `calls` list is
deliberately empty -- those requests belong to the check that issued them. It reports each
component of the destination in order with whether it exists, whether it is a directory, whether it
is a mounted-filesystem boundary, and the DSM code that stopped the walk.

The distinction it buys is between two faults that used to render identically. A missing *shared
folder* -- the first component -- fails: no amount of creating directories fixes an account that
cannot see the share. A missing component below an existing parent only warns, because sync creates
it on the first run.

Quick explicitly skips authentication, destination, write, and logout sections. Standard and
read-only Extensive explicitly skip the write section. A warning alone still exits successfully;
any failed operational section or cancellation emits the completed/skipped section report and then
returns nonzero. Configuration rejected before execution may have no section report. Human, JSON,
and NDJSON output carry the same verdicts. A single target document remains
`sdsync.doctor.v1`; selected batches use `sdsync.doctor-job.v1` records and an
`sdsync.doctor-batch.v1` summary.

`--routing-only` remains a compatibility spelling for Quick when no source/target subcommand is
present. It cannot be combined with `doctor source`, `doctor target`, Standard, or Extensive; new
automation should use `--level quick target`.

### Disposable write test

`--write-test` is a second, independent opt-in and is valid only at Extensive depth. The logical
target itself must already exist. Use it only inside a prepared, non-critical destination after a
read-only Extensive diagnostic succeeds:

```bash
synology-drive-sync doctor --profile acceptance --level extensive \
  target /team-folder/sdsync-acceptance-UNIQUE --write-test
```

The probe uses a unique disposable name, creates its own folder, uploads known content, verifies
the remote size, mtime, MD5, CRC32, and SHA-256, exercises server-side copy when supported (or the
verified upload fallback), and removes only the probe artifacts it created. It must never use or
replace an existing user path. A process crash, lost connection, DSM failure, or failed cleanup can
leave a disposable artifact; inspect and remove the exact reported probe path manually before
retrying. The nested `write_test` report distinguishes request, preflight, execution, verification,
cleanup, and any leftover path. The write test is not appropriate as a routine production health
check. For compatibility, omitting `--level` with `--write-test` auto-promotes to Extensive, but new
automation should make both opt-ins explicit.

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
  --profiles nas-a,nas-b --level standard target --output json
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

An Extensive target-diagnostic batch with `--write-test` follows the same mutation boundary: it
first checks routing and discovery (including content and delete APIs), authentication, exact target
existence, write permission, and the bounded five-entry direct-child inventory for every selected
profile without mutation. No probe is started if any such preflight fails. Probes then run
sequentially; a failed probe stops later probes. Successful earlier probes have already completed
their own cleanup, while a cleanup failure reports the exact leftover probe path.

A target-diagnostic job is `partial` when its probe progressed far enough that remote mutation may
have occurred before the reported failure; inspect the nested write-test report and leftover path
even when cleanup appears complete. In the doctor aggregate,
`all_targets_preflighted_before_mutation` is true only for a requested write-test batch whose every
target successfully completed preflight; an interrupted or failed preflight reports false.

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
nonzero result and retain aggregate stdout plus stderr. Do not schedule
`--level extensive target --write-test` against a production destination.

The built-in overlap check cannot coordinate another host, container, manual invocation, Drive
client, or File Station user. Keep mirror destinations in a documented single-writer window and use
NAS snapshots/versioning or backup for recovery.
