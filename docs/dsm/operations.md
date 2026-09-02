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
