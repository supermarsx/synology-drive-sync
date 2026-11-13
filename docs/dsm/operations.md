# Health, activity, logs, and notifications

The dashboard separates current snapshot state, diagnostic evidence, structured activity, raw
bounded logs, and attention routing. A green connection badge is not a target health proof; run
Doctor and inspect its refreshed evidence.

## Health and Doctor

Doctor supports one named profile or `all`. SSH examples assume `$PACKAGE_USER` was resolved through
the canonical [package-identity discovery](cli-parity.md#discover-the-actual-package-identity).
Standard is the default; the AppWindow and manager also expose Quick and Extensive explicitly.

| Level | Package-local source check | Independent File Station target check |
| --- | --- | --- |
| Quick | Skipped | Unauthenticated URL policy, TLS/reverse-proxy negotiation, DSM/File Station API discovery, and baseline capabilities |
| Standard | Full source name, type, exclusion, boundary, metadata, and enumeration/readability scan without reading every payload for hashes | Quick plus password/optional TOTP authentication, temporary session, required capabilities, destination permission, bounded inventory, and logout |
| Extensive | Standard source scan plus a complete payload pass computing MD5, CRC32, and SHA-256 | Standard plus the fullest content/download/delete/copy capability evidence; target contents remain unchanged |

The manager runs the target check even when the separate Standard/Extensive source scan fails. This
preserves routing and authentication evidence instead of hiding a target problem behind a local
source problem. The final manager result is still nonzero when either side fails. For an existing
destination, permission is checked at the exact path; for a missing destination, it checks the
nearest existing ancestor's ability to create the first missing component.

Standard and Extensive request one bounded discovery page and retain at most five deterministically
sorted entries. With a configured remote this is a direct-child sample beneath that exact logical
root; without a remote it is a non-descending sample of visible shared-folder roots. Evidence
includes the File Station-reported total, sample count, truncation state/count, and bounded safe
metadata. A reported shared-folder root is discovery evidence, not proof of browse/read/write
permission; permission is checked only when a destination is explicitly selected. It never
recursively walks a destination, reads remote payloads, emits ACLs or secrets, or
replaces the complete inventory used by Plan and Run. Extensive keeps the same bound. See
[Bounded target discovery](../diagnostics-and-batch.md#bounded-target-discovery).

The target result is broken down into routing/TLS, API discovery, authentication, File Station
capabilities, destination permission, destination inventory, disposable write/verify/cleanup, and
logout. Every section has an **OK**, **warning**, **not OK**, or **skipped** verdict and elapsed time;
the whole result has an overall verdict and total duration. Shared routing/discovery latency is
identified as shared rather than counted as two independent timings. A warning alone is not a
failure. A failed operational section produces bounded evidence and a nonzero result; a rejected
request can fail before execution and therefore have no section breakdown.

The dashboard's **Run doctor** action is initially queued, then the page polls the controller's
sanitized result to a terminal verdict without an overall client pending-state deadline. A lost POST
acknowledgement is first recovered with at most two exact replays of the same serialized request and
client request ID, which resolves to the same queued job. An `expired_or_missing` response, invalid
result evidence, or five consecutive result-observation failures makes the accepted job
outcome-unknown. Inspect refreshed health, Activity, and logs before another Doctor request. Only the
Run/Doctor scope is paused; unrelated configuration remains available. Closing the AppWindow aborts
observation but does not cancel the queued job.

### Disposable write test

**Disposable write test** is a mutating diagnostic available only at Extensive depth and disabled
unless the authenticated API service grants `write_test`. It requires a separate checkbox and
confirmation. Enabling it locks the level to Extensive. It can create a unique probe, upload and
verify size, MD5, CRC32, and SHA-256, report whether the destination kept the uploaded modification
time, exercise an optional same-target copy path, and remove the probe.

Run it only against a prepared non-critical existing destination. After a core diagnostic request
timeout, terminal failure, or outcome-unknown result, inspect both target folders for leftovers
rather than assuming cleanup happened.

CLI:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal --level quick
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal --level standard
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal --level extensive
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal --level extensive --write-test
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor --all --level standard
```

For compatibility, a manager write test with no explicit level promotes itself to Extensive;
automation should still pass both `--level extensive` and `--write-test` so the mutation boundary is
visible in the command.

### Cached target-health table

The table has columns for last check, reachability, authentication, writability, latency, last
successful sync, Doctor status, and free space. The current manager persistently proves only an
aggregate Doctor state, selected level, check time, exit code, and whether a write test was
requested, plus routine last-success evidence. The complete section breakdown belongs to the
terminal AppWindow result; more granular cached cells remain **Unavailable** until the snapshot
supplies explicit evidence.

Free space is never guessed from a share, volume, or unrelated API. It is shown only with a backend
`free_space_proven` flag; otherwise it remains **Unavailable**.

## Plan and Run status

Plan is non-mutating. Run performs additive/update-only synchronization unless deletion is approved
at every required layer. Both may target a named/default profile or `all`.

The snapshot reports:

- state: `never`, `running`, `succeeded`, or `failed`;
- operation: `plan`, `sync`, or `none`;
- scope: profile name, `all`, or `none`;
- start/finish epochs; and
- numeric exit code when complete.

Plan and Run remain asynchronous in the dashboard. “Queued” is not completion. Follow the run state,
Activity, and logs.

### Run outcomes, reasons, and where a failure is recorded

The state above is the legacy view. The snapshot also carries an `outcomes` block
(`sdsync.dsm-outcomes.v1`) that interprets the package's state files into a verdict and a cause. The
bridge computes it after either read rung answers, so both carry it, and it never fails the snapshot:
a missing, unsafe, or unrecognised state file degrades that one entry and adds a named record to
`outcomes.errors` instead.

| Outcome | Meaning |
| --- | --- |
| `succeeded` | Finished, exit 0. Counts of skipped or deferred entries turn it into a warning, never plain success. |
| `partial` | The core exited `3`: finished, but named paths were deferred and sync on the next run. Not a failure: no failure alert, no retry consumed, and a dependent routine may run after it. |
| `failed` | Ended non-zero, or was refused in preflight. `reason` says why when known. |
| `busy` | Refused because another run held the run lock, or a save or a Doctor write test held the management lock (exit `75`). A routine retries on its own (30 seconds later, no retry consumed); a run the user started is not repeated, so run it again if it is still needed. Not a failure and not counted by the failure alert. |
| `cancelled` | Stopped by the user or a package stop (`130`/`143`, or `137` when a package stop had to kill a runner that did not finish cancelling within its 60-second grace). No alert, and an Activity `run.cancelled` row. A routine runs again on its own; a run the user started is not repeated. |
| `not_run` | No record, or the last aggregate run did not reach this profile; its previous completion is kept. |
| `stale` | `run.state` says `running` but the runner is gone (see below). Treated as failed. |
| `unknown` | A record exists but its value is not one this reader knows. Shown as “Status unknown”, never as success. |

The snapshot carries one outcome for the last run (`outcomes.run`, with the last whole-estate run kept
in `run.aggregate` so a later single-profile run cannot hide an aggregate failure) and one per
profile (`outcomes.profiles[]`, with `consecutive_failures` and `last_success_epoch`). Reasons are
short tokens, never paths or secrets; the dashboard turns each into a cause and a next step, and shows
any token it does not recognise as `unknown (<token>)` rather than hiding it. `outcomes.alerts` carries
the consecutive-failure count and threshold, so the failure is durable in every poll rather than only
in a notification. The count is per scope: `outcomes.alerts.failure_count` is the consecutive failures
of the scope that ran last (a profile name or `all`) and `threshold_reached` applies to that scope. Each
scope keeps its own count and its own last-notification time in `state/alerts-<scope>.state`, so a
healthy profile's success no longer resets a failing profile's count, and one scope's cooldown does not
silence another's alert.

**A run that is still going.** While `outcomes.run.outcome` is `running`, `running_for_seconds` says
how long, and `long_running` turns true past six hours (adding the warning token `long_running`). A
core waiting on a hung mount or a request with no deadline never fails; it just never finishes, and it
holds the run lock, so every routine waits behind it. To make that visible the runner refreshes
`run.state` every 60 seconds with `heartbeat_epoch`, the last time the core showed progress: its
`rchar + wchar` counters in `/proc/<pid>/io` grew by at least 16 KiB since the previous 60-second tick
(the floor is lowered to 60 times the slowest covered profile's `max-rate` when that is less, so a
deliberately throttled run is not called stuck). Those counters follow reads and writes over NFS and
CIFS too, which block-device counters would miss. `heartbeat_source` is `io`, or `liveness` where the
kernel hides the counters: a `liveness` record is written **without** an epoch, because it only proves
the core is alive. A progress record no longer counts as a signal (a batch run's record restarts per profile, and a refresh
moves its modification time). If the counters become unreadable during a run the runner logs
`run_stall_detection_unavailable` once (`warn`, scheduler; `scope=`, `pid=`, `cause=`). The snapshot reports `outcomes.run.heartbeat_epoch` and `stalled`: `true` when the heartbeat is
more than 30 minutes old (adding the warning token `stalled`), `false` while it is recent, and `null`
when the run published no heartbeat **or when the heartbeat source is `liveness`**. A liveness-only
heartbeat proves the core process exists, not that it is working, so it can never read as healthy: the
snapshot reports `stalled: null` (with or without an epoch) and adds the warning token
`stall_unobservable` while the run is `running`, and the dashboard says progress cannot be observed
rather than "not stalled". Only `io` decides true or false. A run
record whose start or heartbeat is more than 60 seconds ahead of the NAS clock reads as unknown
(`running_for_seconds` and `stalled` are `null`) with `{"source":"run.state","code":"clock_skew"}` in
`outcomes.errors`, instead of a negative duration. The runner logs `run_stalled` (`error`) once, and `run_stall_cleared`
if progress resumes. When the heartbeat has been stale for 30 minutes the controller also logs
`controller_blocked_by_stalled_run` (`error`, once per run; the scope, the last progress time, the number
of queued jobs and what is waiting: `routines`, `polls`, `schedule`) and raises the failure alert (DSM
log and desktop) for it. Nothing here stops the core, and the controller's own loop does not become
responsive: under the shell engine, while a run holds it, polls and serialized dashboard jobs still wait (the daemon's queue does not, see [What may run beside what](#what-may-run-beside-what)). The dashboard lists a long-running or stalled run in
the attention list and shows a routine waiting behind it as “Waiting (busy)” or, once the run is
long-running, “Waiting (run may be stuck)”, never as healthy.

**A refusal before the core starts is recorded where a reader looks.** A scheduled run that is refused
in preflight (invalid configuration, a profile log path that is not the package-managed one, the
security policy, an unsafe package file, a missing password, an unadmitted wrapper) used to exit with
no trace. Every exit of the runner now names a reason and writes four records: `run.state`
(`failed`, with `reason` and a one-line `detail`), the profile's own state, the failure counter (for
sync), and an error-level Activity `run.failed` with the cause. If a run is already in flight, its
`running` record is preserved rather than overwritten. A refusal because the run lock is held writes
only an Activity `run.deferred` and never touches `run.state`, and writes no audit pair either: a run's
mandatory audit opens only once the run is admitted (lifecycle, service, run lock, management lock).
Every other refusal before admission (policy, a missing binary or configuration, a lock or admission
failure) is still audited, requested and then failed.

A runner that never started (the controller got exit `126` or `127`, or found no `run.state` or profile
record newer than its dispatch) is recorded with reason `runner_error`, as an error-level Activity
`run.failed`, and counts as a failure; it no longer inherits the previous run's reason. A runner the
controller could not launch at all is recorded `failed` with `last_reason=admission_failed` and
retried 30 seconds later instead of ending the controller.

**Captured core stderr for scheduled runs.** A scheduled run captures the core's standard error. The
last `error:` line (or else the last non-empty line) is sanitised to printable ASCII, cut at 160
characters, and becomes both the run's `detail` and the Activity message. The last 16 KiB are kept
in `var/log/run-stderr.log` (mode `0600`, with a header naming epoch, scope, operation, and exit;
rotated per run to `.1` through `.3`). It can contain local and remote paths, as a manual job result
already can; collect it as evidence like the other package logs. A manual run keeps its output in
the job result instead.

The same text is readable from the dashboard: the **Logs** source `run-stderr` (shown as *Core error
output*, and not part of `all`) serves `run-stderr.log` and its rotations, redacted like every other
source. When the newest block's header names the run that `outcomes.run` reports, the snapshot sets
`outcomes.run.stderr_log` to `{"source":"run-stderr","finished_epoch":E,"exit_code":X}` and the Overview
shows that run's lines under the attention list, or says the run's output has since rotated away.
`outcomes.profiles[].detail` and `failed_path` (the core's last error line and the percent-encoded path
it failed on) are filled from the profile's own state when the runner recorded them; absent they are
`null`.

**Per-profile outcomes in an aggregate.** An aggregate run no longer stops at the first profile that
fails preflight. Each refused profile gets its own failed state and Activity record, and the profiles
that were admitted run with the same deletion budget (a subset can only delete less). The run still
ends failed, with reason `profiles_skipped`, and still alerts. A scheduled aggregate reads the core's
per-job records, so each profile is recorded `succeeded`, `failed`, or `not_run` on its own evidence;
a manual aggregate falls back to the exit code, and the profile outcomes it cannot read are recorded
`unknown` and counted as failed for the dependency gate. The core continues past a failed target; it
stops only on cancellation or when the aggregate deletion cap refuses a reservation.

**A run that died.** If `run.state` says `running` but `run.lock` has no live owner (pid, start time,
and boot identity all checked, and the record re-read), the runner is gone: killed, out of memory, or
power loss. The controller rewrites the state as `failed` with reason `runner_died`, writes an
Activity `run.failed` and the failure alert, and logs `run_runner_died`; the dashboard shows “A run
died without finishing” with a stale outcome. The bridge applies the same rule at read time, so the
verdict does not wait for the controller (or survives a stopped one). An owner file that fails its
ownership or mode check is *not* declared stale, because death cannot be proven. An owner record that
was torn before the last boot (power loss mid-write) is recovered as dead rather than ending the
controller. The daemon publishes `run.lock/pid` by linking a staging file `run/.run.lock.pid.<pid>.<n>`
and unlinking it; killed between the two, the owner file has two names. That reads as the owner's
record, and recovering the dead owner removes the staging names that are the same inode and says so on
stderr (`removed the staged lock owner record a crashed owner left beside …`); a file of another inode
is left.

A package **stop** is not a died run. The controller waits up to 60 seconds for a running sync to cancel
cooperatively (`SDSYNC_DSM_RUNNER_STOP_GRACE`, 1 to 110 seconds, inside the 120-second lifecycle
budget; dashboard jobs still get 3 seconds). If the runner still has to be killed, the controller logs
`runner_stop_forced` (`warn`) and publishes `run.state` `failed` with outcome and reason `cancelled`
and exit `137`, with an `info` Activity `run.cancelled` row (state `cancelled`), no alert and no
`runner_died`. A cancellation the runner itself observes, before the core starts or before it
completes, writes the same `run.cancelled` row; cancellations no longer appear as `run.failed`.

The shell engine's stop has one budget, `SDSYNC_DSM_STOP_TIMEOUT` (default and maximum 120 seconds),
for the controller's whole shutdown, not only the runner's grace. If it ends with services still
live, the stop does not exit `1` with them running: it says `graceful stop timed out after N seconds;
killing what is left`, logs `service_stop_forced` (`error`, `engine=shell`), kills the runner (found
by its verified identity) with its direct children and the API service, gives the controller 5 more
seconds to record the run and exit, and kills it too, naming each KILL. Each target is killed only after
its start tick is read again immediately before the KILL (the runner by its argv and the start tick the stop
recorded when it first saw it, and its children found only through a runner that passed); `kill`'s status
is checked, so "killed ..." lists only what was killed, and everything not killed is named with its reason
(`not killing ...`: a replaced PID, another owner, an unreadable `/proc` entry; `could not kill ...`, which
fails the stop; `already gone before the KILL: ...`). When the controller has already exited the stop says
which service is still live rather than that the controller did not exit. A service that is still live
10 seconds after KILL fails the stop. A run ended this way is not a clean cancellation.

A run read as stale is never reported as running anywhere in the snapshot: the legacy `run.state`,
`outcomes.run.state` and every covered routine's state read `failed` (`outcomes.run.outcome` stays
`stale`, reason `runner_died`). A profile that run covered, whose own record predates the run, reads
`status_cache: last_run_failed`, and keeps doing so after the controller's reconcile; a record written by
a `plan` (`action=plan`) is reported as `no_run_recorded`, so a plan can never vouch for a profile's
last sync.

**Lasting skips and deferrals** exit `0`. This covers every condition only a person can clear: names the
NAS or Synology Drive refuses, permissions, links, type conflicts, a case rename File Station would not
make. The run is `succeeded` with a warning (`skipped_entries` or `deferred_entries`, never `partial`
and never plain green); the dashboard renders it as an amber warning, for example “Synced, 3 entries
skipped and will not sync until fixed”, gives each cause as “not synced until you fix this: …” followed
by a next step, and the entries are named by `entry.skipped` and `entry.deferred` records in the
sync log. A condition that clears by itself is `partial` instead
(exit `3`). See
[partial runs, deferrals and skips](../observability.md#partial-runs-deferrals-and-skips).

**Degraded echoes.** When a run's own result is sound but some records of it could not be written
(Activity line, alert counter, DSM log), the run carries `degraded` naming the sinks, and the
dashboard says so. An alert that could not be processed is itself recorded: the dashboard shows
“Failure alerts unavailable” and Activity gets `notification.unavailable`.

## Service engine

The package service runs on one of two engines. The default is the **daemon**: `sdsync-dsm-api
--daemon`, started by `--launch-daemon --serve-api`. One process keeps the schedule, the realtime
watchers, the dashboard queue and the dashboard's API service (a supervised child). The shell
controller and runner stay for one release as a fallback. `var/state/engine` selects between them:

| Content of `var/state/engine` | Engine |
| --- | --- |
| absent, or `daemon` | daemon |
| `shell` | shell controller and runner |
| anything else, or unreadable | the daemon runs, and the content is named: `service_engine` in `controller.log` (`file=invalid`, with the path and cause) and an amber item on the Overview |

The change takes effect when the package is stopped and run again in Package Center. The About page
and `outcomes.engine` show which engine is active. The daemon logs `service_engine` once at start.

What differs under the daemon, and what the pages below describe for it:

- Lasting blocks, such as the one after an out-of-memory kill, are applied by the daemon only; the
  shell engine retries as before.
- The API service is not ready until `api.sock` answers. A crash of it is named
  (`api_server_exited`) and it is restarted with a backoff that starts at one second and doubles to
  thirty (`api_server_restarted`).
- A run never starts while `management.lock` is live, so a save or a Doctor write test is never
  read half-written: the run is deferred as `run_deferred` with `reason=lock_busy` and retried.
- Every run publishes `var/run/run.scope` (see [Saves beside a daemon run](#saves-beside-a-daemon-run)).
- Realtime routines use the daemon's in-process inotify watcher, not `sdsync-dsm-api --watch` (see
  [Realtime mode](routines.md#realtime-mode-and-fallback)).
- A run hands its DSM session back to the daemon instead of logging out, and the next run of the
  profile within 10 idle minutes borrows it: no discovery, login or logout (see
  [DSM session reuse between runs](security.md#dsm-session-reuse-between-runs)).

### Gates before a daemon run

A daemon run applies the same checks as the shell runner before it starts the core, and refuses with
the same reasons: `policy_*` (the security policy), `password_missing`, `secret_unsafe` and
`audit_unavailable`. The audit trail is mandatory for a run: the daemon writes the audit record
before the core starts and settles it when the core ends. A daemon pointed at a tree that is not a
package tree has no audit helper; it runs unaudited and says so (`service_audit` in
`controller.log`, once at start).

### Failure alerts under the daemon

A failed run raises its alert through the package's [alert policy](#dsm-desktop-alert-policy): the
same per-scope counter, threshold and cooldown, the same desktop notifier and the same DSM system log
entry. An escalation after exhausted retries follows the policy's cooldown. A delivery that fails is
logged as `alert_unavailable` with the channel and the cause, and the run keeps its own result.

## What may run beside what

Every dashboard request that does real work becomes a queued job, and the service classifies each
one before dispatching it. The class decides what else may run at the same time.

**Daemon.** The queue runs in lanes beside any sync, including a sync of the profile the job names:

| Lane | Jobs | At once | Deadline |
| --- | --- | ---: | --- |
| Serialized | profile save, secrets, routines, alert and security policy, interface event | 1 | 10 minutes |
| Concurrent | Check status, Doctor, resync plan and the other actions | 2 | 6 hours |
| Connection | authentication test, remote browsing | 2 | 3 minutes |

Order is kept within a lane and never across lanes. A save is therefore never queued behind a sync or
a status walk. A job that passes its deadline has its process group stopped and is answered exactly
as a dead worker is: `consumer_failed`, or, for a save, which may have committed part of its change,
no result at all and an outcome-unknown verdict (see
[Named terminal failures](troubleshooting.md#named-terminal-failures)). The dashboard labels the
reason `consumer_timeout`. The daemon logs `control_consumer_failed` and
`control_consumer_outcome_unknown` for the two cases, `control_request_rejected` for a request file
it refused to read (not a private regular file, or over 64 KiB) and
`control_file_expired` for an old request or response it removed.

A worker that outlives its KILL by 10 seconds (a process stuck in the kernel on a dying disk) frees
its lane: it is named `consumer_unkillable` (job, operation, PID, path), handed to a reaper, and its
job is answered as above. What a worker writes on standard error reaches `controller.log` as
`consumer_stderr` lines naming the job. Before admitting jobs the queue reconciles the audit outbox.
A reconcile that fails is named once (`audit_reconcile_failed`, with an Activity `bridge.failed`
row) and retried with a backoff of up to a minute. Its recovery is named too
(`audit_reconcile_recovered`). Jobs wait meanwhile; after 30 seconds of failure they are answered
`audit_unavailable` (never started) instead of waiting forever.

**Shell engine.** The controller has one work slot:

| Class | Jobs | May run beside |
| --- | --- | --- |
| Connection | authentication test, remote browsing | anything except another connection job |
| Concurrent | Plan, Run, Doctor, Check status, resync plan | one connection job **and** one serialized job |
| Serialized | profile save, secrets, routines, alert and security policy, interface event | one concurrent job |

Two jobs of the same class never overlap. A serialized job commits configuration, credential, policy
or scheduler state, so two of them cannot interleave and neither may run beside a scheduled sync.

**Check status** walks the whole local tree and
enumerates the whole remote one, which on a small NAS takes minutes; it used to hold the only work
slot for that entire time, so a profile save started while it ran waited it out. The dashboard bounds
its own wait for a queued result at thirty seconds, so such a save was reported as an outcome the
page could not determine — while the save itself was still queued and landed later. A save now
proceeds beside a status walk instead of behind it.

A scheduled or manual **Run** of the shell engine (`var/state/engine` = `shell`) is still a barrier
for saves, deliberately: it is the one long job that is reading the configuration a save would
rewrite. A save started during such a run is queued and applied when the run finishes, and
`sdsync-dsm` refuses one that reaches it with exit `75` (`cannot change profiles, credentials, or
schedule while PID … is planning/syncing`).

### Saves beside a daemon run

A daemon run names the profiles it covers in `var/run/run.scope`, a package-owned `0600` regular file
beside `run.lock` of exactly four lines: the PID, `/proc` start time and boot id of the `run.lock`
owner, then `profiles=` and a comma-separated list. While that file names the live `run.lock` owner
exactly, the manager narrows its barrier to what would change or remove what the run is using, and
refuses that with exit `75` and a message beginning `profile_running:` that names the profile and the
operation:

- `remove-profile`, `set-default` and `remove-routine` of a profile in the run;
- `remove-password`, `remove-totp` and `remove-remote-log-token` of a profile in the run (the core
  reads its credentials again at each re-authentication);
- `configure-profile` of a profile in the run that changes its `source`, `remote`, `url`, `username`,
  `compare`, `excludes`, `delete`, `allow-empty-source` or `max-delete` (any other setting — jobs,
  retries, timeouts, logging, rate — is saved);
- a confirmed `resync` of any profile (it re-uploads, and never runs beside a sync);
- `doctor --write-test` of a profile in the run, or of `--all`.

Everything else commits during the run, and its stderr ends with
`note: profile <p> is syncing now; this change takes effect on its next run` (a change to a profile
in the run) or `note: a sync is running; this change takes effect on the next run` (schedule,
routines, the default profile, alert, performance and security policy). Through
`sdsync-dsm api <mutation>` the note is the result's `applies_next_run` warning. The run in flight
keeps the settings it started with.

A `run.scope` that is absent, a symlink, not `0600` or not package-owned, over 4096 bytes, malformed,
or naming anything but the live `run.lock` owner is not trusted: the run is treated as a shell run and
the whole-package refusal above applies, preceded — when the file exists — by one
`run.scope ignored: <cause> (<path>)` line.

**Doctor** and a **resync plan** (no `--confirm`) only read, and take no management lock, so a save
does not wait for them and they do not make a run refuse. A read-only Doctor is never refused beside
a run of either engine; a resync plan beside a shell-engine run still is. A **Doctor write test**
writes into the destination and keeps the management lock for its whole duration; beside a run whose
scope cannot be attributed it is refused with exit `75` (`doctor --write-test is refused while PID …
is planning/syncing: it writes into the destination the run is syncing`). An interactive `set-password` or `set-totp` reads the secret before it takes
the lock, so a person typing no longer makes runs refuse.

### Verified-pair index status

The daemon runs every sync with the [verified-pair index](../pair-index.md) on:
`--pair-index var/state/cache/pairs`, one `<profile>.redb` per profile (a name over 64 characters is
shortened with a hash; see [Where it lives](../pair-index.md#where-it-lives)). The profile's `index:` entry in
`sdsync-dsm-api --ctl status`, and the `Checksum index:` note at the end of the profile's **Last run:** line in the dashboard, show what its last run
did with it: `state` (`off`, `cold`, `warm`, `refreshed`, `unusable` or `local-only`), `entries` held,
`hits` (reused) and `misses` (checked live), the canary checked and any canary mismatches, whether it
was `refreshed`, and the most serious issue the run named with its path and cause on one line.

- A damaged index is named `pair-index-corrupt`, set aside as `<profile>.redb.corrupt-<epoch>` and
  rebuilt from the run. It is an amber item.
- An index found damaged three opens in a row is named `pair-index-unkeepable` (amber) and is not
  rebuilt: every file is checked live. It is tried again after a package update or in 7 days.
- An index kept for a profile that was re-created with another source, remote or NAS is retired and
  rebuilt: `pair-index-retired`, shown as information on the line, not as a problem. It explains the
  slow first run.
- A read of an index that a run of the same profile is using reports `index: "busy"`, not an
  error.
- `remove-profile` removes the profile's index file, its quarantined copies (`.redb.corrupt-<epoch>`),
  its panic record (`.redb.panicked`), its count of damaged opens (`.redb.rebuilds`), its lasting block and its alerts state. For a name over 64
  characters the files are the hashed ones, `<first 48>-<16 hex of the SHA-256>.redb` and its
  siblings, found by that exact stem and never by a prefix glob, so another long profile's index is
  left alone. A file it could not remove, or a stem it could not compute (no `sha256sum`), is named
  in the result as a warning.

### Lasting block after an out-of-memory kill

A run whose core was killed with exit `137` is recorded as `core_oom_killed` when this cgroup's
out-of-memory counter moved, and as `core_killed` when it did not, never as `core_failed`. The detail
names the memory a run could use. Under the daemon, `core_oom_killed` also holds the profile as a
lasting block in `var/state/lasting/<profile>.state`: nothing retries it and no scheduled or
realtime trigger starts it, because running it again would kill it again. Each held trigger is
logged at most once an hour (`routine_suppressed`). The Overview shows an amber **not synced until
you fix this** item for the profile.

The block is lifted by **Run now**, or by a change to the profile's settings or to the performance
level. The comparison is by content, so a save that changes nothing does not lift it. It survives a
restart of the package. A record that cannot be read is named (`lasting_state_unreadable`) and still
holds the profile; **Run now** clears it. The shell engine does not apply lasting blocks.

## Structured Activity

Activity is a bounded, package-private event stream with fixed schemas and messages. Accepted event
codes are:

| Code | Meaning |
| --- | --- |
| `run.started` | Package Plan/Sync action started |
| `run.succeeded` | Package action completed successfully |
| `run.failed` | Package action completed with a nonzero result, was refused in preflight, or its runner died; the message carries the cause |
| `run.cancelled` | A run was cancelled by a package stop, a reboot or the user (`info`, state `cancelled`), by the runner or, for a forced stop, the controller. A routine run again on its own; a run the user started is not repeated |
| `run.deferred` | A run was refused only because another operation held the run lock, or a routine wrapper was never admitted; retried, not a failure. For a scheduled routine or the schedule it is written once per busy episode, not on every 30-second re-check; a manual run's deferral is always written |
| `routine.deferred` | Routine was outside its window or dependency evidence was not satisfied; recorded once per episode, not on every controller turn |
| `routine.retry_scheduled` | A whole-action retry was scheduled |
| `routine.retries_exhausted` | A routine used all its retry attempts (`error`, `failed`; the message carries the attempt count and last reason), or a realtime routine's sixth consecutive partial run (`warn`, `deferred`) |
| `routine.watch_abandoned` | The controller stopped retrying a native watcher |
| `routine.unobserved` | Nothing, or not everything, is observing a routine: its backend is blind, its poll floor cannot finish, its poll walk was killed for outliving its budget, the walk cap refused it, a lasting block holds it, scheduling is blocked by a long job, or it is disabled. One row per episode (`warn`; `info` for disabled); the message names the cause and `Logs` the detail. The routine's state record keeps the episode (`observe_state`, `observe_cause`, `observe_since_epoch`), so a restart does not repeat it |
| `routine.observed` | The episode above ended: something observes the routine again, it was enabled again, or scheduling resumed (`info`) |
| `doctor.succeeded` | Doctor completed successfully |
| `doctor.failed` | Doctor completed with a nonzero result |
| `doctor.inventory` | The DSM package retained one bounded private Doctor discovery record |
| `configuration.changed` | Profile, routine, alert, or schedule configuration changed |
| `notification.unavailable` | The DSM desktop notification helper was unavailable or failed; the message ends `(cause=notifier_missing)` or `(cause=notifier_exit_N)`. One row per failure episode: the alert cooldown starts on the attempt, not on a delivery |

Each event contains an epoch, fixed code, validated profile/`all`/`none` scope, fixed state, and
bounded message. A `doctor.inventory` message embeds the corresponding private maximum-five record,
including bounded logical names/relative paths and per-field truncation flags, but no credentials,
tokens, ACLs, file payloads, or absolute local volume paths. Activity rotates at 1 MiB with three
backups. Browser clearing affects only the current view.

## Bounded logs

The dashboard reads `1..1000` lines and can filter:

- API/CGI diagnostics;
- private Doctor discovery evidence;
- controller;
- scheduler;
- sync; or
- mandatory audit history.

Beyond those six, `run-stderr` (the core's own error output of the last runs, described above) can be
selected explicitly; it is never part of `all`.

The service answers a Logs read itself, newest first and bounded: for each source it reads the
retained files backwards and stops once it has the requested number of lines or has scanned 1 MiB of
that source. A read that stops at the scan budget because a strict threshold hid most lines carries
`"scan_limited": true` on that source, and the dashboard says *newest lines only; older history was not
scanned* rather than implying there is nothing older. A whole read also has a four-second deadline,
shared evenly between the sources still to be read; a source the deadline cut carries `scan_limited`
**and** `"deadline_reached": true`, and the service writes one coalesced (every five minutes) `warn`
record `logs_read_deadline` (`sources`, `elapsed_ms`) to `api.log`. The result is byte-for-byte what the
shell manager returns for the same files, including the redaction and the placeholders below, except
for `deadline_reached`; a test compares the two.

Redaction is applied to the **whole line** first and only then is the line shortened, so a secret cut in
two by a length limit cannot survive as a fragment:

- terminal escape sequences are stripped before redaction, so a secret cannot be hidden by an escape
  placed inside it;
- only a line that is a complete JSON record has the string-token rule applied; every other line is
  redacted as plain text, and an unterminated token is redacted to the end of the line;
- a line is then shown cut to 8,192 bytes on a character boundary;
- a line longer than 32 KiB is not shown at all: it reads `[line too long: N bytes, withheld]`, with N
  the raw byte length;
- a line that still contains a secret after every rewrite reads `[line withheld: could not be
  redacted]`.

The level of a long line is read from its first 8,192 bytes, so the threshold still applies to a line
the dashboard shows only as a placeholder.

API, controller, scheduler, and audit logs retain five rotations. The core sync log retains three.
The DSM package's private Doctor discovery history uses a 1 MiB rotation threshold plus three
rotations (`.1` through `.3`), all package-owned `0600` files. One final bounded record can cross
the threshold before the next append rotates it. Structured Activity also retains three
rotations. Reads reject symlinks, hard links, unsafe ownership/modes, or unreadable state, and
read each source's rotations newest first (the active file, then `.1`, `.2`, and so on), stopping once
they have the requested lines or have scanned 1 MiB of that source, on the default lane and on the
forced shell lane alike. The response is the newest requested suffix that fits its byte budget. Doctor discovery is available only through
the local `doctor` Logs source and matching Activity events and is excluded from the core remote-log
sink. Structured API failure lines obey their exact bridge/authentication/security category
threshold, but an `error`-level record is never hidden by one: it is read in Activity and Logs
whatever the category is set to, `off` included, on the default lane as well as on the forced shell
lane, and a line with no level (raw standard error a child process left in `controller.log`) is read
as an error. In the sync log, the path of a per-entry record is stored percent-encoded (bytes outside
`A-Za-z0-9._/:,-` as `%XX`, at most 255 characters); the dashboard shows it decoded. The Activity read reports where its
history starts (`history`), so a failure rotated away is not mistaken for none. API output replaces private package paths with neutral labels and masks secret-file
paths; secret values are never an allowed log field. Selecting one of the six sources reads only
that source; `all` remains globally bounded below the API bridge's 1 MiB response-capture limit.

The Activity page can pause refresh without changing package logging. Log refresh choices are
**Manual only**, 5, 10, or 30 seconds. Manual-only mode clears the background log timer; selecting
Activity or changing its source/line filters still performs an explicit bounded refresh. For SSH
recovery:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" logs 200
sudo tail -n 200 /var/log/packages/synology-drive-sync.log
sudo tail -n 200 /var/packages/synology-drive-sync/var/log/api.log
sudo tail -n 200 /var/packages/synology-drive-sync/var/log/doctor-inventory.log
```

### Controller and scheduler log events that record a failure

Failures the package used to swallow are now written to `controller.log` or `scheduler.log` at
`error`, which no category threshold, `off` included, hides. The ones worth
knowing when reading it:

| Event | Meaning |
| --- | --- |
| `controller_reload_failed` | A saved change that needs a reload could not be signalled to the running controller (`reason=`, `pid=`, `operation=`, `marker_status=`); see exit `76` in [CLI parity](cli-parity.md) |
| `controller_reload_skipped` | The same failure for a save that needs no reload (alert policy, schedule switched off): `warn`, `needed=false`, exit `0`, no marker |
| `routine_state_write_failed` | A routine's state file could not be written; it now carries `cause=` (the operating system's message). A run whose `running` record cannot be written is not started, and an outcome that cannot be written is held in memory and flushed before any routine is evaluated again. A full or unwritable state directory degrades a turn; it no longer ends the controller |
| `control_queue_failed` / `control_queue_recovered` | One dashboard queue turn failed (`error`, once per episode, `status=`) and the next one succeeded (`warn`); queue turns are no longer fatal |
| `clock_step_detected` | A next-run or poll time further ahead than its interval allows, as after a backwards clock step, was clamped to now (`warn`; `field=`, `ahead_seconds=`), so polling and deadlines no longer stall until the old time arrives |
| `routine_partial_backoff` | A realtime routine finished partial again (`warn`); see [Retry and backoff](routines.md#retry-and-backoff) |
| `routine_escalation_cooldown` | A retry-ladder exhaustion escalation (DSM log and desktop alert) was suppressed by the alert cooldown (`info`) |
| `runner_stop_forced` | A package stop had to kill a runner after its 60-second cancellation grace (`warn`; `grace=`, `exit=`) |
| `runner_wait_guard_failed` / `runner_wait_guard_recovered` | The helper that watches a running sync for the controller could not start or stop (`phase=start` or `stop`, `status=`, `pid=`; `error`, once per episode per phase) / worked again (`warn`). The controller keeps scheduling and polls the run once a second until it ends |
| `controller_blocked_by_stalled_run` | A run has shown no progress for 30 minutes and holds everything behind it (`error`, once per run) |
| `poll_scratch_leftover_removed` / `poll_scratch_leftover_unremovable` | A realtime poll was killed outright and left `run/fingerprint.<controller pid>.<poll pid>.<start tick>.<n>`; the controller removed it before the next poll (`warn`) or could not (`error`), with `path=`, `owner_pid=` and the cause |
| `routine_watch_orphan_stopped` / `routine_watch_orphan_unstoppable` / `routine_watch_guard_unavailable` | The controller was killed outright and its `inotifywait` fallback watcher was stopped by its guard job (`warn`, `profile=`, `pid=`), could not be signalled (`error`), or the guard could not be started because the controller's own start time was unreadable (`warn`). The package's own watcher dies with its parent by itself |
| `runtime_leftovers_pruned` | At start the controller removed leftover capture, admission and fingerprint files of processes that are no longer live (`count=`, `unremovable=`; `info`, `warn` when some could not be removed) |
| `run_state_write_failed` | The runner could not write its final `run.state` after three attempts one second apart (`attempts=3 outcome= exit=`); it kept its outcome in `state/run.unrecorded` |
| `run_state_recovered` | The controller found a dead run, matched it to `run.unrecorded` by start time and scope, and published the runner's real outcome (with `degraded=...,run_state`, reason `run_state_write_failed`) instead of `failed/runner_died`; no Activity failure and no alert, because the runner already wrote both |
| `run_outcome_unrecorded` / `run_outcome_marker_unremovable` | Even `run.unrecorded` could not be written (or removed); the controller will report a dead runner |
| `run_stalled` / `run_stall_cleared` | The core's heartbeat stopped for 30 minutes / progress resumed; see [A run that is still going](#run-outcomes-reasons-and-where-a-failure-is-recorded) |
| `run_heartbeat_unwritable` | The runner could not refresh `run.state` while a run was in flight |
| `controller_reload_marker_unremovable` | The reload marker could not be removed after a successful reload |
| `dsm_log_gate_failed` / `dsm_log_policy_invalid` | The optional DSM system-log channel could not read its cooldown ledger (`file=system-log.state status=`), or an enabled policy has an invalid field (`field=`); delivery is skipped rather than sent unbounded |
| `health_cache_write_failed` | Doctor's health record could not be saved; Doctor still writes its Activity line and alerts and exits with its own status |
| `default_profile_invalid` | `default-profile` exists but names no existing profile; scope-less actions are refused |
| `routine_watch_unsafe` / `routine_watch_ladder_unwritable` / `routine_watch_setup_failed` | A native watcher path is a symlink / its failure record could not be written, so the watcher is abandoned instead of retried forever / its setup failed |
| `child_launch_failed` | The controller could not launch a child (`kind=`, `status=`) |
| `controller_exited` | The controller's last line: `status=` and `shutdown=`, `info` after a requested stop and `error` otherwise |
| `run_state_unverifiable` | The controller could not decide whether a run was alive (`phase=liveness` or `acquire`) |
| `controller_found_dead` | `warn`, category controller. The controller is gone while its API service is still running: `phase=status` (written once per dead controller and API instance, which is the record a SIGKILL never wrote) or `phase=start` (written whenever Start repairs it); `pid=` the controller's recorded pid or `none`, `api_pid=` the service. Start then stops the service and starts both again, and writes an Activity `service.restarted` (`warn`) saying so |
| `core_killed` | `error`, scheduler. The sync core was killed (`exit=137`) without reporting an error and the run was not a cancellation. `scope=`, `oom_kill=<before>-><after>`, `proven=true\|false`: `proven=true` when this cgroup's out-of-memory kill counter moved across the run. The profile and run detail then say "killed by the out-of-memory killer" (or, unproven, "killed (SIGKILL, exit 137) without reporting an error; out of memory? check the DSM system log"); the reason is `core_oom_killed` when proven and `core_killed` otherwise, never `core_failed`. The detail also names the memory a run could use. Under the daemon `core_oom_killed` holds the profile ([lasting block](#lasting-block-after-an-out-of-memory-kill)). The package does not read `dmesg`, which is privileged |
| `realtime_watcher_command` | Which watcher realtime routines use, once per controller start: `path=` and `kind=native` (the package's `sdsync-dsm-api --watch`) or `kind=inotifywait` (`info`); `warn` with `path=none kind=none` when neither passed the executable checks and every realtime routine is polled (`backend_reason=watch_unavailable`) |
| `routine_watch_pid_mismatch` | `warn`. A routine's `watch-<profile>.pid` named a live child of the controller that is not its watcher (a wrapped PID: `profile=`, `pid=`, `reason=not_the_watcher`, `cmd=` its argv[0]); it was not signalled or adopted and the stale file was removed |
| `routine_watch_failed` / `routine_watch_limit` | `warn`. A native watcher exited (`profile=`, `consecutive=`, `detail=` its first stderr line: the reason, then the cause, then the path) / a source does not fit the per-user inotify watch budget (`dirs=`, `budget=`, `limit=` from the controller's own count, or `source=watcher detail=watch_limit dirs=… budget=… limit=… held=…` from the watcher's) |
| `routine_watch_limit_unchecked` | `warn`. The inotify watch limit could not be checked for a routine (`profile=`, `cause=absent\|not_a_file\|unreadable\|malformed\|count_failed\|cache_unsafe\|cache_unwritable`, sometimes `path=`); logged once per controller lifetime, and the watcher is started anyway |
| `run_stall_detection_unavailable` | `warn`, scheduler. The core's I/O counters stopped being readable mid-run (`scope=`, `pid=`, `cause=`), so hang detection is unavailable for the rest of that run; the heartbeat becomes `liveness` and `stalled` reads `null`. Once per run |
| `routine_window_dst_gap` | `warn`, scheduler. A daily window fell wholly inside a spring-forward gap and opened after it (`profile=`, `window_day=`, `start=`, `end=`) |
| `routine_clock_unavailable` / `routine_clock_recovered` | The controller could not read the clock (`cause=`; `warn`, once per episode, and nothing is dispatched meanwhile) / it could again (`info`) |
| `routine_fingerprint_format_changed` | `info`. A realtime routine's stored fingerprint was written by an earlier release (`from=`), so one comparison sync runs and the `to=v3` reading becomes the baseline. Replaces `routine_fingerprint_rebased` |
| `service_engine` | The engine in force, once per daemon start: `engine=daemon file=absent\|daemon\|shell\|invalid\|unreadable` (`warn` when the file is `shell`, invalid or unreadable, with the path and cause) |
| `service_audit` | `runs=audited` (`info`), or `runs=unaudited` (`warn`) when the daemon's tree is not the package's `var` |
| `run_deferred` | `warn`. A run was not started because `management.lock` was live (`reason=lock_busy`, `scope=`, `cause=`); it is retried and no retry is consumed |
| `routine_backend` | The change source of a realtime routine changed (`profile=`, `backend=inotify\|polling\|none`, `reason=ok\|watch_limit\|watch_unavailable\|watch_setup_failed`, then the detail; `none` while the process-wide walk cap refuses the arm). `warn` when the reason is not `ok`. One line per cause: later reports of the same backend, reason and cause (the skipped-entries count of a polling tree aside) are counted, and the report that ends the episode carries `suppressed_reports=`, `previous_backend=` and `previous_reason=`; a re-arm that ends one says `episode_ended=rearmed` |
| `service_layout` | `info`, once per daemon start: `home=` and `var=` as named and `resolved=` as followed. DSM's aliases are followed one level only; the target must be a directory owned by the package user and not writable by group or others, and a refusal names both paths |
| `routine_full_reconcile` | `info`. An armed realtime routine syncs its whole tree once every 24 hours (`profile=`, `every_seconds=`) |
| `routine_rearmed` | `info`. A realtime routine's source or exclude list was edited, so its watch was replaced (`profile=`, `source=`) |
| `routine_watch_notice` | A named skip or note while watching (`profile=` and the detail): a vanished entry, an unreadable subdirectory (first three paths, with cause), a walk that hit its deadline, `watch_missed_change dirs=N first=… floor_seconds=…` (`warn`: the stat-only floor found a directory that changed without an inotify event, and synced it), `poll_floor_failed cause=…` (a floor reading failed, once), or `nested_mounts=N first=… watched_by=poll_floor every_seconds=…` (`info` at arm: other devices below the source) |
| `routine_suppressed` | `warn`. A trigger was held by a lasting block (`profile=`, `run=`, `reason=`, `clears_on=run_now_or_settings_change`); at most once an hour per profile |
| `control_consumer_failed` / `control_consumer_outcome_unknown` | A dashboard job's worker died or passed its lane deadline: the operation was answered `consumer_failed` / left outcome-unknown because it may have committed (see [Named terminal failures](troubleshooting.md#named-terminal-failures)) |
| `control_request_rejected` / `control_file_expired` | A queue request file was refused (not a private regular file, or over 64 KiB) / an old request or response was removed (only once it is gone) |
| `control_file_unremovable` | `error`. The queue could not remove one of a job's files (`id=`, `kind=processing\|secret\|request\|response\|progress`, `path=`, `cause=`). A `secret` may hold a credential: remove it by hand. A secret that is a symbolic link is left in place and named |
| `control_dir_unreadable` / `control_dir_recovered` | `error` / `info`. The queue could not read one of its control directories (`requests`, `processing`, `progress` or `responses`; `path=`, `cause=`), which used to look like an empty queue. Named once per episode, and again, with `was=<the earlier cause>`, when the cause moves while it stays unreadable (denied, then not a directory), so `controller.state` never keeps the first failure's words; while it lasts `controller.state` carries `degraded=control_dir_unreadable` (the audit's `audit_unavailable` takes precedence) and the dashboard shows it in amber / the directory reads again (`path=`) |
| `api_server_exited` / `api_server_restarted` | `error` / `warn`. The daemon's API service child exited (`status=`, `served_seconds=`, `cause=`: its last stderr line) / was started again (`attempt=`, `after_seconds=`) |
| `api_socket_stale_removed` / `api_socket_stale_kept` | `warn` / `error`. Before starting its API service, the daemon removed an `api.sock` left by a server that is gone: one killed between bind and activation (`mode=0000`), or one nobody accepts on. Or it kept the socket and named why (another live server, or not a socket of this package) |
| `api_server_unwaitable` | `error`. The API service child could not be waited for (`pid=`, `cause=`, `killed=`, `reaped=`, `gone=`). It is KILLed only while its pid still names it (after `ECHILD` it was reaped elsewhere and is not signalled: `killed=skipped`) |
| `api_server_respawn_held` / `api_server_respawn_released` | `error` / `info`. The previous API service child could not be proven gone, so no new one is started beside it; checked again every 30 s / it is gone and the restart goes ahead |
| `api_server_identity_unavailable` | `warn`. The API service child's start tick could not be read at spawn (`pid=`); it is told apart from a reused pid only while unreaped |
| `service_start_failed` | `error`, with Activity `service.start_failed`. Either the daemon did not start (`step=` `daemon.pid`, `daemon.ready`, `ctl_socket`, `api_server`, `queue`, … `path=`, `cause=`), and its records were removed; or the supervisor gave up (`exit=`, `last_stderr=`, `restarts_exhausted=`) |
| `service_restarted` | `warn`. The supervisor restarted a crashed daemon (`exit=`, `last_stderr=`; `cause=daemon_lock_held` for a daemon that found `daemon.lock` held and no daemon answering) |
| `service_superseded` | `info` when a supervisor left because another daemon already serves; `error` when it left because `supervisor.lock` was still held after the 2 s it waits for a moment's probe (`cause=`: another supervisor, or a holder that never lets go, and this start produced no daemon). The lock case also says so on stderr |
| `service_records_removed` / `service_records_kept` | `warn` / `error`. A supervisor that gave up removed the dead daemon's `daemon.pid`, `daemon.ready` and `ctl.sock` (`paths=`), or could not (`cause=`) |
| `daemon_stderr_unavailable` | `error`. The supervisor could not open `log/daemon-stderr.log` (`cause=`); the daemon's stderr is discarded |
| `config_unreadable` / `config_recovered` | `error` / `info`. A configuration or state file could not be read (`path=`, `cause=`): permissions, an I/O error, or over 1 MiB. It is never read as absent; `kept=` says what stands instead (the last configuration read; a routine's state holds the routine; a dependency's profile state is a dependency not met; a profile state is not rewritten; `run.state` is rewritten without the aggregate it carried), and a routine with no configuration is skipped as `field=unreadable`. Listed in `ctl status` (`config_unreadable`). Once per episode |
| `config_forgotten` | `info`. Nothing reads an unreadable record any more, so it leaves `config_unreadable` (`path=`, `cause=`): its routine was removed (its `.conf` and routine state), its profile was removed (the fragment and the profile state), or no routine depends on the profile any more (a dependency's profile state that was listed only because a routine's `depends_on` read it) |
| `profile_state_unreadable` | `error`, scheduler. A run ended but the profile's state record could not be read (`id=`, `profile=`, `path=`, `cause=`), so it was not rewritten: the failure streak and last success it carries stay as they are. The run says `degraded=profile_state` |
| `audit_reconcile_failed` / `audit_reconcile_recovered` / `audit_unavailable` | `error` / `info` / `error`. The dashboard queue could not reconcile the audit outbox (once per episode, with `retry_seconds=`) / it could again / a waiting job was answered `audit_unavailable` after the 30 s bound (`id=`, `op=`). A job is refused only on an attempt made after it arrived: within 2 s of the last attempt it waits for a fresh one, so a recovered audit is not refused for the rest of the backoff. An open episode is retried on its backoff even with no job waiting |
| `service_degraded` / `service_degraded_cleared` | `warn` / `info`. The service is degraded (`reason=audit_unavailable`, `since=`, `cause=`): `controller.state` carries `degraded=`, `degraded_since=` and `degraded_cause=`, and the dashboard shows it in amber / it recovered and the keys are gone. While it lasts, a change of cause (the reconcile failing with another exit) is a further `service_degraded` line with the same `since=` and the new `cause=`, and the keys follow. A daemon killed outright cannot clear them: the status snapshot serves `controller.degraded` only while the service it reports is `running`, so a dead service is never shown amber from its leftover keys |
| `consumer_unkillable` / `consumer_unwaitable` / `consumer_stderr` | `error` / `error` / `warn` or `error`. A dashboard job's worker outlived KILL and its slot was freed / could not be waited for / wrote this line on stderr (`id=`, `op=`, `exit=`, `path=`, `line=`) |
| `event_log_lock_stale` / `event_log_rotation_deferred` / `event_log_rotation_failed` / `event_log_rotation_recovered` | `warn` / `info` / `error` / `info`. A rotation cleared a stale `event-log.lock` owner (the cause says which: the pid is gone, the pid now belongs to another process because it was reused, the record was written in an earlier boot, or a torn record) / found it held by a live writer (or a torn record not yet provably stale) and is tried again in 5 s (`path=`, `cause=`, `retry_seconds=`) / could not be made, or the lock stayed held for a minute (`path=`, `cause=`, `retry_seconds=`) / the log is under its limit again |
| `ctl_accept_failed` / `ctl_accept_recovered` | `error` / `info`. The control socket could not accept a connection or start a thread for it (`path=`, `cause=`); the listener pauses 200 ms instead of spinning. Once per episode |
| `ctl_peer_gone` / `ctl_peer_unchecked` | `info` / `warn`. A client of a long control request (`wait`, `mutate`, `stop`) closed its connection before the reply, so its slot was freed (`verb=`, `waited_ms=`) / the daemon could not tell whether the client was still there (`verb=`, `cause=`), so that wait cannot notice a hang-up; its deadline still bounds it |
| `ctl_reply_dropped` / `ctl_socket_option_failed` | `info` / `warn`. A reply could not be written because the client had gone (`code=`, `cause=`) / a socket option could not be set on a connection (`option=write_timeout`, `cause=`), so a reply to that client may block until its deadline. A connection that sent no request (`code=empty_request`) is logged once per episode |
| `ctl_empty_requests_ended` / `ctl_unread_evicted` / `ctl_unread_eviction_ended` | `info` / `warn` / `info`. Connections that sent no request were counted instead of logged, and none has come for 30 s (`count=`, `over_ms=`) / the control socket had 128 connections waiting to send a request, so the oldest is dropped for each new one and told `ctl_busy` (`path=`, `cause=`; once per episode, so a client that reconnects without a request cannot starve `ping` or `stop`) / that episode ended (`count=`, `over_ms=`). Both ends are written by the next request the socket reads |
| `session_lent` / `session_kept` / `session_not_kept` | `info`. A run of `profile=` was lent its kept DSM session (`idle_s=`) / handed a session back, now kept (`borrowed=none\|used\|rejected\|ignored`) / handed none back because it ended its own. See [DSM session reuse](security.md#dsm-session-reuse-between-runs) |
| `session_logged_out` / `session_logout_failed` | `info` / `warn`. The daemon ended a kept session (`profile=`, `why=ttl_expired\|replaced\|unused_by_run\|not_handed_back\|service_stop`) / DSM did not take the logout (`cause=`); DSM then ends it on its own idle timer |
| `session_handback_missing` | `warn`. A run that was given the session channel handed nothing usable back (`profile=`, `cause=`); a session it was lent is logged out |
| `session_ttl_shortened` / `session_rejected` | `warn`. DSM refused a lent session that had been idle `idle_s=` seconds; the run logged in once, afresh, and the idle TTL became `ttl_s=` (never under 60) / was already that short |
| `session_channel_failed` / `session_logout_inline` / `session_notes_dropped` | `warn`. No socket pair could be made for a run (`cause=`; the run logs in itself) / no logout thread could be started, so the logout ran on the control thread / more than 64 session notes arrived between two turns (`count=`) |

Re-levelled from `warn` to `error` because each records a failed write or delivery:
`activity_append_failed`, `alert_unavailable`, `observability_write_failed` (which now carries the
helper's `cause=`), `state_unreadable` for `controller.pid`, `dsm_log_emit_failed`,
`run_stderr_unavailable`, and the runner's `runner_message` for every failure (a busy deferral's
stays `warn`). `notification.unavailable` now says why: its message ends `(cause=notifier_missing)` when
the notifier is not installed and `(cause=notifier_exit_N)` when it refused with status N.

A log rotation that fails never drops the line: it is appended to the still-valid active file past the
size limit, and the failure is recorded once per episode on standard error and in
`var/state/log-rotation.failed` (package-owned `0600`: `epoch=`, `path=`, `cause=`), which the next
successful rotation of that log removes. Lines are single `O_APPEND` writes, which never interleave.
The daemon rotates under the same `event-log.lock` as the shell writers, taken by the shell's own
claim-and-link protocol. An empty lock directory is free. An owner that is gone, or a torn record
from before this boot, is cleared and named (`event_log_lock_stale`); the record cleared is checked to be
the one judged (device, inode and contents; an inode number can be reused), and one a live writer linked in meanwhile is put back. A live owner is never waited
for: the rotation is skipped and the line is kept. That is contention, named once at `info`
(`event_log_rotation_deferred`) and tried again in 5 s; only a lock held for a minute, or one that
cannot be taken at all, is the failure (`event_log_rotation_failed`, the marker), and that log is then
not tried again for a minute (a failure that is not contention also ends the count of refusals in a row, so a
writer that appears later is counted from its own first refusal). The episode ends when the log is found under its limit again (rotated
here or by a shell writer), so a later failure is named again. The marker is on-disk
evidence only; the dashboard does not display it. An unsafe rotation set is still refused with exit `73` and nothing is written to it.

### Dashboard requests the service could not serve

A request that reaches the package service and fails there writes one `cgi_failure` record to
`api.log` under the `bridge` category, at stage `service_request`. Earlier releases wrote nothing
at all for this class of failure, so a dashboard showing "Restart Synology Drive Sync" left no
server-side trace of what had actually gone wrong. The record's `code` field now names the cause:
whether the manager was slow (`manager_timeout`), whether too many dashboard windows were competing
for the manager lane (`manager_busy`), whether the package was upgrading or shutting down
(`runtime_upgrading`, `runtime_uninstalling`, `runtime_closed`), or whether a package file is in a
state the service refuses to act on (`manager_unsafe`, `runtime_marker_unsafe`, `policy_unreadable`).
Codes beginning `manager_` describe the shell manager the service ran; the remainder describe the
service's own view of the package.

Two codes belong to reads the service answers itself, without running the manager.
`config_file_unsafe` means a package file failed the ownership, mode, link-count or size contract,
and names a file to repair rather than a service to restart: the envelope carries `file` (its path
under the package root, at most 160 bytes, the tail kept) and `check`, and the same pair is an
`unsafe_file` record in `api.log`. A rename replacement, a read that came back a different length,
and a file removed between listing and reading are looked at again before they count. `package_state_corrupt` means a
package file was readable and holds a record the service cannot parse. Neither is ever a reason to
retry the read through the manager instead: the manager checks less, so falling back would serve
exactly the file the service had just refused.

Two properties of that record are worth knowing before reading one:

- **One record per thirty seconds, across every stage and code.** The window is shared with every
  other `cgi_failure` writer, deliberately, so that a caller able to provoke many distinct codes
  cannot amplify log writes. The consequence is stated plainly rather than hidden: **a coalesced
  record names the most frequent code in its window and counts all of them** in `occurrences`. A
  window that saw two timeouts and one busy lane reports `manager_timeout` with `occurrences` of 3.
  The breakdown is not recoverable from the record; the record is bounded at 512 bytes and a map
  would not fit.
- **These failures appear in `api.log` only, never in the Activity feed.** An operator directed to
  inspect Activity for a `manager_timeout` will find nothing there. This follows the existing
  treatment of `service_saturated` rather than inventing a second convention.

Suppression still obeys `bridge_log_level`. A policy set to `error` writes no record, touches
neither the log nor the coalescing state file, and does not change what the browser is told: the
503 and its code reach the page either way. A quiet log is not a quiet page.

### Forcing dashboard reads through the shell manager

Dashboard reads, the Logs view included, are answered inside the package service, from the package's
own files, instead of by running the shell manager. A private marker forces every read back onto the
manager:

```bash
sudo -u "$PACKAGE_USER" -- sh -c \
  'printf "shell\n" > /var/packages/synology-drive-sync/var/control/read-lane && \
   chmod 0600 /var/packages/synology-drive-sync/var/control/read-lane'
sudo -u "$PACKAGE_USER" -- rm /var/packages/synology-drive-sync/var/control/read-lane
```

The switch is deliberately exact. The file must be owned by the package user, mode `0600`, a single
link, and contain the six bytes `shell` followed by one newline. Absent means the service decides
per read. Present but unreadable, mis-owned, or holding anything else is **refused**, with code
`runtime_marker_unsafe`, rather than ignored: an operator who believes reads are forced to the
manager must not be silently overruled. Create it as the package user, not as root.

Engaging it writes one `read_lane_forced_shell` notice to `api.log` under the `bridge` category, at
`warn`, once per service start rather than once per dashboard poll. That record is a notice and not
a failure, so it carries no HTTP status and no stage.

This is the only escape hatch, and it is deliberately the only one. An in-service read that fails
its own file checks does **not** quietly hand the request to the manager instead. The service
validates owner, mode, link count and size on the descriptor it opened; the manager checks that the
path is a regular file and not a symlink. Falling back from the first to the second on a failed
check would serve, through the laxer reader, exactly the file the stricter one had just refused. A
failed check returns its named code and stops.

## DSM desktop alert policy

The package recognizes three internal alert triggers and maps each one to a fixed title/message pair
from `ui/texts/enu/strings`:

| Trigger | Desktop message |
| --- | --- |
| `sync_succeeded` | Fixed completion title and message |
| `sync_failed` | Fixed sync-failure title and message |
| `doctor_failed` | Fixed Doctor-failure title and message |

The package invokes `/usr/syno/bin/synodsmnotify -c` directly with the fixed application ID
`SYNO.SDS.App.SynologyDriveSync.Instance`, `@administrators`, and full package I18N keys. A profile name, exit code, path, URL, account name,
core/log message, password, TOTP value, cookie, or token never enters notifier arguments. Operation
details remain in Activity and bounded package logs.

This is desktop-only delivery to logged-in DSM administrators. The SPK deliberately has no
`conf/resource` `sysnotify` worker and does not register Notification Center rules or email, SMS,
mobile, or CMS channels. Those channels cannot be enabled through this package alert policy.

Policy fields:

| Field | Dashboard range/meaning |
| --- | --- |
| Enable DSM desktop alerts | Master enable |
| Notify on success | Emit `sync_succeeded`; off by default |
| Notify on failure | Emit sync/Doctor failure events; on by default |
| Failures before alert | `1..100`; default `1` |
| Cooldown | `60..2592000` seconds (up to 30 days); dashboard and manager share the same range |

Failure counts and cooldowns are kept per scope (a profile name, or `all` for an aggregate run), in
`state/alerts-<scope>.state`: a success resets only its own scope's count, the threshold applies to
the scope that failed, and a notification for one scope does not start another scope's cooldown. A
scope with no file of its own starts from the package-wide `alerts.state`, which keeps `failure_count`
(the consecutive failures of the scope that ran last) and the newest `last_notify_epoch` for older
readers. Cooldown rate-limits delivery. If
`/usr/syno/bin/synodsmnotify` is unavailable or fails, synchronization keeps its own result and
records `notification.unavailable` (naming the cause), once per cooldown rather than once per failed
run, because the cooldown starts on the attempt; alert failure is not disguised as successful delivery,
and the DSM system-log entry stays the durable record of every failure.

CLI parity:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-alerts \
  --enabled true \
  --on-success false \
  --on-failure true \
  --failure-threshold 2 \
  --cooldown 3600
```

## DSM system log

A desktop alert is not a log. It is not durable, it is not searchable, and an administrator who was
not signed in when it fired never learns that it happened. The package therefore carries a second,
durable channel: one shell facility, `dsm_log_event`, that every DSM-visible package event travels
through, writing a fixed sentence per event to DSM's own system log through
`/usr/syno/bin/synologset1`.

### What DSM actually offers a third-party package

Synology's package developer guide documents exactly two integration points here, and both are
`conf/resource` resource workers:

| Worker | What the guide says it does | Surface the guide attributes to it |
| --- | --- | --- |
| `sysnotify` | Merges the package's notification strings into DSM's index | Control Panel > Notification > Rules, and from there desktop, email, SMS, mobile, and CMS |
| `syslog-config` | Installs a syslog-ng config fragment and a logrotate config, then reloads syslog-ng | None stated |

Read the second row carefully, because it is easy to assume more than it says. `syslog-config`
installs *configuration* — parsing and rotation rules for a log file the package writes itself, into
`/usr/local/etc/syslog-ng/patterndb.d/` and `/usr/local/etc/logrotate.d/`. It is not an emit path: it
delivers nothing on its own, and the guide page never mentions Log Center. Its timing is
`FROM_STARTUP_TO_HALT`, so acquisition runs before the start script and a failure aborts package
startup — a poor trade for a best-effort logging channel. The only documented route to an
administrator-facing alert surface is `sysnotify`.

This package deliberately acquires **no** `conf/resource` worker, and `validate_spk.py` rejects one,
so neither surface is open to it. That is a reviewed security boundary, not an oversight: acquiring
`sysnotify` would put this package's strings into a DSM-wide index and its events into the rule set
that drives an administrator's mail and mobile push. Changing it is a package-contract decision, not
a logging change — it requires a `conf/resource` member, notification text directories, and matching
validator and negative-test updates.

What remains is `synologset1`, DSM's own system-log writer. The binary is real and DSM's own scripts
use it, but Synology publishes no developer-guide contract for it, and its message identifiers come
from a Synology-owned catalogue that a third-party package cannot extend. **This package therefore
ships no message identifier of its own.** Delivery stays inert until an administrator supplies one
they have verified on their own DSM build. Until then — and this is the important part — every event
still reaches Activity exactly as it did before.

### Configuring it

There is no dashboard field. Configure it over SSH:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-alerts \
  --system-log true \
  --system-log-message-id 0x11100000 \
  --system-log-level warn
```

| Field | Meaning |
| --- | --- |
| `--system-log` | Master enable; default `false`. Refused without a message identifier. |
| `--system-log-level` | Lowest severity delivered: `info`, `warn`, or `err`. Default `warn`. |
| `--system-log-message-id` | `0x` followed by 1..8 hexadecimal digits, from DSM's own catalogue. |

The desktop-alert fields are a full replacement on every save, but these three default to what is
already stored, so a save from the dashboard's alert form does not switch system logging back off.

### Event catalogue

| Event | Severity | Group | Raised when |
| --- | --- | --- | --- |
| `sync_succeeded` | info | sync | A synchronization run completed |
| `sync_failed` | err | sync | Consecutive failures reached the alert failure threshold |
| `doctor_failed` | err | doctor | Doctor failures reached the alert failure threshold |
| `authentication_failed` | warn | authentication | A dashboard authentication attempt was rejected |
| `security_failed` | err | security | A request failed a CGI or runtime identity check |
| `bridge_failed` | warn | bridge | A dashboard request could not be served |
| `bridge_rejected` | warn | bridge | An authenticated dashboard mutation was refused before it was queued; its Activity row names the reason |
| `service_started` | info | lifecycle | The package service started |
| `service_stopped` | info | lifecycle | The package service stopped |
| `service_restarted` | info | lifecycle | The package service restarted |
| `service_start_failed` | err | lifecycle | The package service failed to start |
| `package_installed` | info | lifecycle | A fresh install completed |
| `package_upgraded` | info | lifecycle | An upgrade completed |
| `queue_saturated` | warn | queue | Completed dashboard results were discarded at the queue limit |

DSM receives only the fixed reviewed sentence for the event and the configured message identifier. A
profile name, exit code, path, URL, account name, log or error text, cookie, token, password, or
TOTP value never enters that argv, exactly as none of them enters the desktop notifier's.

### Rate limiting

Sync and Doctor events follow the same `failure_threshold` the desktop alert policy uses. Every event
is then rate-limited by `cooldown_seconds`, tracked **independently per group** in
`var/state/system-log.state`. A routine failing every five minutes produces one system log entry per
cooldown, not 288 a day; a burst of rejected dashboard requests cannot crowd out a sync failure,
because they are different groups. Unlike the desktop policy, system logging is not governed by
`--enabled`, `--on-success`, or `--on-failure`: an administrator who silenced toasts still wants the
durable record.

The cooldown stamp is committed before delivery is attempted, so a DSM without the binary costs one
probe per cooldown rather than one per event. A clock corrected backwards re-opens the gate rather
than silencing the log until real time catches up.

### When it is unavailable

Logging is best effort by contract. An absent or symlinked binary, a permission denial, or a chroot
without DSM tooling leaves the caller's own result untouched and records `notification.unavailable`
in Activity with the message `DSM system log delivery unavailable`. No caller has to guard the call
and no run fails because logging did.

Repository tests cover the policy parsing, the identifier and severity validation, the per-group
cooldown, the fixed argv, and the unavailable fallback. They do **not** prove that `synologset1`
accepts package-user calls, that a given identifier renders on a particular DSM build, or that the
resulting entry appears in Log Center's filters. Establish those during
[live-NAS acceptance](troubleshooting.md#live-nas-acceptance) before relying on this channel for
unattended attention.

## Open-session fallback

The dashboard can request a browser notification and play a short local tone when it observes a new
failure. This is a non-secret browser preference, not a background transport:

- the DSM application must remain open;
- browser notification permission must be granted;
- a failure is anything the snapshot's verdict calls one: a failed run, a run whose runner died, an
  outcome the package could not interpret, and a failed profile of an all-profiles run. Each distinct
  failure notifies once, however often the page refreshes, and a cancelled run never notifies;
- the first status a window reads is its baseline, so a failure already present when the window opens
  is announced by the message in the window and the Overview list, not by a notification;
- if the browser refuses a notification the window says so instead of dropping it; and
- closing the page stops this fallback.

Use an externally monitored log/status path when desktop-only delivery is insufficient for
unattended attention. Validate actual DSM desktop delivery during
[live-NAS acceptance](troubleshooting.md#live-nas-acceptance).
