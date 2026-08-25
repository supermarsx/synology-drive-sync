# Routines and scheduling

The dashboard's primary automation model is one routine per profile. A routine can retain its policy
while disabled, run a non-mutating Plan, run a Sync, wait for other profile routines, and expose its
requested mode separately from the effective backend.

All manual and automatic Plan/Run actions share one host-local run lock. A long operation cannot
pile up another routine on the same NAS. This lock does not coordinate another NAS, container,
File Station user, Synology Drive client, or other process; use an external single-writer window
where those actors can touch the same target.

## Routine fields

| Field | Values and effect |
| --- | --- |
| Profile | One existing profile; removing the profile also removes its routine |
| Enable routine | Executes when due; disabling retains the policy |
| Action | `plan` for non-mutating review or `sync` for an actual finite sync |
| Mode | `interval`, `daily`, or `realtime` |
| Interval | `60..2592000` seconds; used by interval mode and retained for the other modes |
| Active weekdays | Non-empty unique subset of Monday `1` through Sunday `7` |
| Window starts / ends | 24-hour `HH:MM`; applies before a due action is admitted |
| Realtime debounce | Dashboard `5..3600` seconds; the manager/bridge contract permits `1..3600` |
| Fallback poll | `5..3600` seconds |
| Retry attempts | `0..5` after a failed whole routine action |
| Retry backoff | `10..86400` seconds before the first retry |
| Wait for routines | Existing profile names whose latest routine state must be `succeeded` |
| Permit profile deletion rules | Action-level approval for that routine; off by default |
| Routine deletion approval ceiling | Approval ceiling compared with the profile's per-run deletion cap |

Dependencies cannot repeat, name the current profile, reference a missing profile, or form a cycle.
The controller defers rather than runs when a dependency has not succeeded.

## Time window and weekdays

The window is inclusive and uses the NAS local clock. When start is earlier than or equal to end,
the allowed time is within that same-day interval. When start is later than end, the window crosses
midnight. The selected weekday is evaluated when the controller checks the routine.

A due routine outside its weekday/window is marked `deferred`. Correct the NAS time zone and clock
before relying on daily or TOTP behavior.

## Interval mode

The first deadline is scheduled one interval after configuration/controller observation. After an
action finishes, the next interval is measured from completion, so a long-running sync does not
compress or overlap the next run. Changing routine configuration signals the controller to reload.

Interval state normally moves through `never`, `scheduled`, `running`, and `succeeded` or `failed`.
A future `next_run_epoch` is shown in Routines and contributes to the Overview next-routine card.

## Daily mode

Daily mode becomes due once per NAS calendar day when the selected weekday and time window allow it.
The controller records the last processed day. It does not mean “exactly at Window starts”; the
controller admits the action during the window when it observes the routine and the host-local lock
is available.

## Realtime mode and fallback

Realtime mode is still a sequence of finite sync actions, not a permanently connected sync engine.
The controller tries a recursive `inotifywait` watcher when that command exists and the watcher stays
healthy. Changes are debounced before a run.

When a native watcher is unavailable or recently failed, the controller falls back to a periodic
source fingerprint built from file path, size, and mtime. **Fallback poll** controls that check. This
is not a kernel event stream and very short-lived changes between identical fingerprints may not be
observable; final sync planning still performs the engine's normal complete source/target work.

The snapshot reports the effective backend as `inotify`, `polling`, `interval`, `daily`, or `none`.
Overview and Routines surface polling fallback explicitly. Do not infer native realtime support from
the requested `mode=realtime` alone.

## Retry and backoff

Profile **Retries** handle eligible network operations inside one engine run. Routine **Retry
attempts** retry the whole failed Plan/Sync action. The first retry uses the configured backoff; each
later retry doubles it, capped at 30 days by the controller. Activity records
`routine.retry_scheduled`.

Retries do not make a multi-profile or deletion operation transactional. Earlier successful target
changes remain when a later action fails. Investigate authentication, routing, source, or safety
errors instead of using a large retry count to hide them.

## Dependencies

**Wait for routines** is a success-state gate, not a distributed workflow engine. Every selected
profile routine must have a latest state of `succeeded`; otherwise the dependent routine is marked
`deferred` and reconsidered later. Changing a dependency's data does not automatically invalidate
its prior success state.

Keep dependency graphs small and explicit. Use Activity and each routine's last-success time to
decide whether the evidence is recent enough for the intended workflow.

## Deletion approval is layered

Remote-only entries are preserved unless every applicable layer agrees:

1. the profile enables **Mirror remote deletions** and has an explicit **Maximum deletions per run**;
2. the routine enables **Permit profile deletion rules**; and
3. the routine's **deletion approval ceiling** is at least the profile's configured cap.

The routine ceiling is not a second aggregate counter passed to the single-profile core action. It
is an approval boundary: if a profile is later changed so its cap exceeds the routine ceiling, the
controller defers the routine. Actual deletion remains bounded by the profile limit and all core
safety checks.

Manual named Plan/Run needs `--allow-delete`. An all-profile action also needs an aggregate
`--max-total-delete`. The legacy global schedule has its own `allow_delete` and aggregate ceiling.
No layer borrows permission from another.

The core additionally enforces destination containment, empty-source protection unless explicitly
overridden, DSM-managed-path protection, remote mount boundaries, fresh snapshots, caps, and
failure-before-delete ordering. File Station operations are still not atomic; retain snapshots or
version history and test deletion separately.

## Remove versus disable

Disable retains the routine configuration and state while preventing execution. Remove deletes the
routine and its routine-state file but keeps the profile and credentials. Both dashboard operations
wait for a terminal private-queue result before reporting success.

## CLI parity

Resolve `$PACKAGE_USER` through the canonical
[package-identity discovery](cli-parity.md#discover-the-actual-package-identity) first.

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-routine \
  --profile personal \
  --enabled true \
  --action sync \
  --mode realtime \
  --weekdays 1,2,3,4,5 \
  --window-start 07:00 \
  --window-end 23:00 \
  --debounce-seconds 30 \
  --poll-seconds 60 \
  --retry-attempts 2 \
  --retry-backoff 60 \
  --allow-delete false \
  --max-total-delete 100

sudo -u "$PACKAGE_USER" -- "$MANAGER" remove-routine personal
```

Multiple `--depends-on NAME` options build a dependency list. See [CLI parity](cli-parity.md) for
status and recovery commands.

## Legacy global interval schedule

The manager retains a package-wide interval schedule for CLI compatibility:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" enable --interval 3600
sudo -u "$PACKAGE_USER" -- "$MANAGER" disable
```

It runs all profiles sequentially after one interval, uses delay-after-completion, and has separate
scheduled deletion approval. It can coexist in configuration with per-profile routines, but both
share the same run lock. Prefer per-profile routines for new dashboard-managed deployments because
their mode, window, dependency, fallback, and state are individually visible.
