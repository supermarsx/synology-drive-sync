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
| `busy` | Refused because another run, Doctor, or management action held the run lock (exit `75`). A deferral that is retried; not a failure and not counted by the failure alert. |
| `cancelled` | Stopped by the user or a package stop (`130`/`143`, or `137` when a package stop had to kill a runner that did not finish cancelling within its 60-second grace). No alert. |
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
`run.state` every 60 seconds with `heartbeat_epoch`, the last time the core showed progress: its I/O
counters in `/proc/<pid>/io` moved, or, where those cannot be read, its progress record changed
(`heartbeat_source` is `io`, `progress` or `liveness`; with `liveness` the epoch only proves the core is
alive). The snapshot reports `outcomes.run.heartbeat_epoch` and `stalled`: `true` when the heartbeat is
more than 30 minutes old (adding the warning token `stalled`), `false` while it is recent, and `null`
when the run published no heartbeat. The runner logs `run_stalled` (`error`) once, and `run_stall_cleared`
if progress resumes. When the heartbeat has been stale for 30 minutes the controller also logs
`controller_blocked_by_stalled_run` (`error`, once per run; the scope, the last progress time, the number
of queued jobs and what is waiting: `routines`, `polls`, `schedule`) and raises the failure alert (DSM
log and desktop) for it. Nothing here stops the core, and the controller's own loop does not become
responsive: while a run holds it, polls and serialized dashboard jobs still wait. The dashboard lists a long-running or stalled run in
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
controller.

A package **stop** is not a died run. The controller waits up to 60 seconds for a running sync to cancel
cooperatively (`SDSYNC_DSM_RUNNER_STOP_GRACE`, 1 to 110 seconds, inside the 120-second lifecycle
budget; dashboard jobs still get 3 seconds). If the runner still has to be killed, the controller logs
`runner_stop_forced` (`warn`) and publishes `run.state` `failed` with outcome and reason `cancelled`
and exit `137`, with an `info` Activity `run.failed` row, no alert and no `runner_died`.

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

## What may run beside what

Every dashboard request that does real work becomes a queued job, and the controller classifies each
one before dispatching it. The class decides what else may run at the same time:

| Class | Jobs | May run beside |
| --- | --- | --- |
| Connection | authentication test, remote browsing | anything except another connection job |
| Concurrent | Plan, Run, Doctor, Check status, resync plan | one connection job **and** one serialized job |
| Serialized | profile save, secrets, routines, alert and security policy, interface event | one concurrent job |

Two jobs of the same class never overlap. A serialized job commits configuration, credential, policy
or scheduler state, so two of them cannot interleave and neither may run beside a scheduled sync.

The row that matters in practice is the second one. **Check status** walks the whole local tree and
enumerates the whole remote one, which on a small NAS takes minutes; it used to hold the only work
slot for that entire time, so a profile save started while it ran waited it out. The dashboard bounds
its own wait for a queued result at thirty seconds, so such a save was reported as an outcome the
page could not determine — while the save itself was still queued and landed later. A save now
proceeds beside a status walk instead of behind it.

A scheduled or manual **Run** is still a barrier for saves, deliberately: it is the one long job that
is reading the configuration a save would rewrite. A save started during a run is queued and applied
when the run finishes.

## Structured Activity

Activity is a bounded, package-private event stream with fixed schemas and messages. Accepted event
codes are:

| Code | Meaning |
| --- | --- |
| `run.started` | Package Plan/Sync action started |
| `run.succeeded` | Package action completed successfully |
| `run.failed` | Package action completed with a nonzero result, was refused in preflight, or its runner died; the message carries the cause |
| `run.deferred` | A run was refused only because another operation held the run lock, or a routine wrapper was never admitted; retried, not a failure. For a scheduled routine or the schedule it is written once per busy episode, not on every 30-second re-check; a manual run's deferral is always written |
| `routine.deferred` | Routine was outside its window or dependency evidence was not satisfied; recorded once per episode, not on every controller turn |
| `routine.retry_scheduled` | A whole-action retry was scheduled |
| `routine.retries_exhausted` | A routine used all its retry attempts (`error`, `failed`; the message carries the attempt count and last reason), or a realtime routine's sixth consecutive partial run (`warn`, `deferred`) |
| `routine.watch_abandoned` | The controller stopped retrying a native watcher |
| `doctor.succeeded` | Doctor completed successfully |
| `doctor.failed` | Doctor completed with a nonzero result |
| `doctor.inventory` | The DSM package retained one bounded private Doctor discovery record |
| `configuration.changed` | Profile, routine, alert, or schedule configuration changed |
| `notification.unavailable` | The DSM desktop notification helper was unavailable or failed |

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
scanned* rather than implying there is nothing older. The result is byte-for-byte what the shell
manager returns for the same files, including the redaction; a test compares the two.

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

Re-levelled from `warn` to `error` because each records a failed write or delivery:
`activity_append_failed`, `alert_unavailable`, `observability_write_failed` (which now carries the
helper's `cause=`), `state_unreadable` for `controller.pid`, `dsm_log_emit_failed`,
`run_stderr_unavailable`, and the runner's `runner_message` for every failure (a busy deferral's
stays `warn`). `notification.unavailable` now says why (`status N`: the notifier is not installed or
refused).

A log rotation that fails never drops the line: it is appended to the still-valid active file past the
size limit, and the failure is recorded once per episode on standard error and in
`var/state/log-rotation.failed` (package-owned `0600`: `epoch=`, `path=`, `cause=`), which the next
successful rotation of that log removes. The marker is on-disk evidence only; the dashboard does not
display it. An unsafe rotation set is still refused with exit `73` and nothing is written to it.

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
and names a file to repair rather than a service to restart. `package_state_corrupt` means a
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
records `notification.unavailable`; alert failure is not disguised as successful delivery.

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
| `bridge_rejected` | warn | bridge | An unauthorized dashboard mutation was refused |
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
