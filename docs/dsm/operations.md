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
verify size, mtime, MD5, CRC32, and SHA-256, exercise an optional same-target copy path, and remove
the probe.

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
| `run.failed` | Package action completed with a nonzero result |
| `routine.deferred` | Routine was outside its window or dependency evidence was not satisfied |
| `routine.retry_scheduled` | A whole-action retry was scheduled |
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

API, controller, scheduler, and audit logs retain five rotations. The core sync log retains three.
The DSM package's private Doctor discovery history uses a 1 MiB rotation threshold plus three
rotations (`.1` through `.3`), all package-owned `0600` files. One final bounded record can cross
the threshold before the next append rotates it. Structured Activity also retains three
rotations. Reads inspect every retained file, reject symlinks, hard links, unsafe ownership/modes,
or unreadable state, and traverse from the oldest rotation through the active file. The response is
the newest requested suffix that fits its byte budget. Doctor discovery is available only through
the local `doctor` Logs source and matching Activity events and is excluded from the core remote-log
sink. Structured API failure lines obey their exact bridge/authentication/security category
threshold. API output replaces private package paths with neutral labels and masks secret-file
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

Some dashboard reads are answered inside the package service, from the package's own files, instead
of by running the shell manager. A private marker forces every one of them back onto the manager:

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
| Cooldown | Dashboard `60..604800` seconds; manager supports up to 30 days |

Successful sync resets the tracked sync failure count. Cooldown rate-limits delivery. If
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
failed run. This is a non-secret browser preference, not a background transport:

- the DSM application must remain open;
- browser notification permission must be granted;
- the first already-visible failure does not generate a duplicate alert; and
- closing the page stops this fallback.

Use an externally monitored log/status path when desktop-only delivery is insufficient for
unattended attention. Validate actual DSM desktop delivery during
[live-NAS acceptance](troubleshooting.md#live-nas-acceptance).
