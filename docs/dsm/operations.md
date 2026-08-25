# Health, activity, logs, and notifications

The dashboard separates current snapshot state, diagnostic evidence, structured activity, raw
bounded logs, and attention routing. A green connection badge is not a target health proof; run
Doctor and inspect its refreshed evidence.

## Health and Doctor

Doctor supports one named profile or `all`. SSH examples assume `$PACKAGE_USER` was resolved through
the canonical [package-identity discovery](cli-parity.md#discover-the-actual-package-identity).

The default diagnostic:

1. resolves the selected profile(s) under the package identity;
2. walks and hashes the complete local source;
3. performs File Station API discovery through the configured URL/prefix;
4. authenticates with the protected credential and optional TOTP flow;
5. inventories the exact destination when it exists, or checks the nearest existing ancestor when
   the selected descendant is missing; and
6. reports success or a nonzero exit without changing target contents.

The dashboard's **Run doctor** action is asynchronous. Its immediate HTTP result means queued; the
final verdict appears through run/activity/health state after the controller executes it.

### Disposable write test

**Disposable write test** is a mutating diagnostic and is disabled unless the authenticated API
service grants `write_test`. It requires a separate checkbox and confirmation. It can create a
unique probe, upload and verify it, exercise an optional same-target copy path, and remove the probe.

Run it only against a prepared non-critical existing destination. After any timeout or failure,
inspect both target folders for leftovers rather than assuming cleanup happened.

CLI:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal --write-test
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor --all
```

### Cached target-health table

The table has columns for last check, reachability, authentication, writability, latency, last
successful sync, Doctor status, and free space. The current manager persistently proves only an
aggregate Doctor state, check time, exit code, and whether a write test was requested, plus routine
last-success evidence. More granular cells remain **Unavailable** until a backend supplies explicit
evidence.

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

Operational actions remain asynchronous in the dashboard. “Queued” is not completion. Follow the
run state, Activity, and logs.

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
| `configuration.changed` | Profile, routine, alert, or schedule configuration changed |
| `notification.unavailable` | The DSM desktop notification helper was unavailable or failed |

Each event contains an epoch, fixed code, validated profile/`all`/`none` scope, fixed state, and
bounded non-secret message. Activity rotates at 1 MiB with three backups. Browser clearing affects
only the current view.

## Bounded logs

The dashboard reads `1..1000` lines and can filter:

- controller;
- scheduler; or
- sync.

Controller and scheduler logs rotate at 10 MiB with five backups. The core sync log uses its own
10 MiB, three-backup policy. Rotation rejects symlinks. API output replaces private package paths
with neutral labels and masks secret-file paths; secret values are never an allowed log field.

The Activity page can pause refresh without changing package logging. Log refresh choices are 5,
10, or 30 seconds. For SSH recovery:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" logs 200
sudo tail -n 200 /var/log/packages/synology-drive-sync.log
sudo tail -n 200 /var/packages/synology-drive-sync/var/log/api.log
```

## DSM desktop alert policy

The package recognizes three internal alert triggers and maps each one to a fixed title/message pair
from `ui/texts/enu/strings`:

| Trigger | Desktop message |
| --- | --- |
| `sync_succeeded` | Fixed completion title and message |
| `sync_failed` | Fixed sync-failure title and message |
| `doctor_failed` | Fixed Doctor-failure title and message |

The package invokes `/usr/syno/bin/synodsmnotify -c` directly with the fixed application ID,
`@administrators`, and full package I18N keys. A profile name, exit code, path, URL, account name,
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
