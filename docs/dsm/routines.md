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
| Interval | `60..2592000` seconds; present and used only in interval mode |
| Active weekdays | Non-empty unique subset of Monday `1` through Sunday `7`; daily mode only |
| Window starts / ends | 24-hour `HH:MM`; daily mode only |
| Realtime debounce | `1..3600` seconds; defaults to `45`; realtime mode only |
| Fallback poll | `5..3600` seconds; realtime mode only. Under the daemon it is how often the polling fallback reads the tree while the native watch is unavailable (60 seconds if unset) |
| Retry attempts | `0..5` after a failed whole routine action; defaults to `5` |
| Retry backoff | `10..300` seconds before the first retry |
| Exponential backoff | On by default for compatibility; when off, every retry uses the fixed base delay |
| Wait for routines | Existing profile names whose latest routine state must be `succeeded` |
| Permit profile deletion rules | Action-level approval for that routine; off by default |
| Routine deletion approval ceiling | `0..2147483647`; approval ceiling compared with the profile's per-run deletion cap |

Dependencies cannot repeat, name the current profile, reference a missing profile, or form a cycle.
The controller defers rather than runs when a dependency has not succeeded.

## Daily time window and weekdays

The window is inclusive and uses the NAS local clock. When start is earlier than or equal to end,
the allowed time is within that same-day interval. When start is later than end, the window crosses
midnight. The selected weekday is the weekday of the **window day**, the day on which the window
opens, not the weekday at the moment the controller checks.

A daily routine runs once per **window day**, the calendar day on which its window opened. In the part
of a wrapping window after midnight (23:00 to 01:00, at 00:30) the window day is still yesterday, so a
Friday-only routine with that window runs at Saturday 00:30 (Friday's window) and a Saturday 00:30 is
not Saturday's. A later window that has already run closes the tail of an earlier one. The
day is recorded when the run is dispatched, not when it finishes, so a run that crosses midnight no
longer consumes the next night and a wrapping window no longer runs twice in one night. A run cancelled
(`130`/`143`) is held for 30 seconds before the routine is considered again, so a stop or reboot does
not turn into an immediate re-run. If the NAS clock steps backwards, a next-run or poll time that is
further ahead than its interval allows is clamped to now and recorded as `clock_step_detected` in
`controller.log`.

**Daylight saving.** The window is judged on the NAS wall clock, once per turn, from one reading of the
clock. When the clocks go back, a window runs once, not twice, and a window nearly a whole day long runs
once on the long night. When the clocks go forward, a window that falls wholly inside the skipped hour
opens right after the gap and the controller logs `routine_window_dst_gap` (`warn`, scheduler;
`profile=`, `window_day=`, `start=`, `end=`) so the shifted run is explained. A clock the controller cannot read never dispatches: it logs
`routine_clock_unavailable cause=` once per episode and `routine_clock_recovered` when the clock
returns.

A due daily routine outside its weekday/window is marked `deferred`, including a daily retry whose
deadline has passed. Interval and realtime routines never consult weekdays or clock windows.
Correct the NAS time zone and clock before relying on daily or TOTP behavior.

## Interval mode

The first deadline is scheduled one interval after configuration/controller observation. After an
action finishes, the next interval is measured from completion, so a long-running sync does not
compress or overlap the next run. Changing routine configuration signals the controller to reload; if the signal cannot be delivered
the change is still saved and the dashboard says it is not yet applied (see
[a save that is not applied](troubleshooting.md#a-run-failed-died-or-was-refused)).
Interval mode has no weekday or time-window control.

Interval state normally moves through `never`, `scheduled`, `running`, and `succeeded` or `failed`.
A future `next_run_epoch` is shown in Routines and contributes to the Overview next-routine card.

## Daily mode

Daily mode becomes due once per NAS calendar day when the selected weekday and time window allow it.
The controller records the last processed day. It does not mean “exactly at Window starts”; the
controller admits the action during the window when it observes the routine and the host-local lock
is available.

## Realtime mode and fallback

Realtime mode is still a sequence of finite sync actions, not a permanently connected sync engine.

**Under the daemon (the default engine)** the daemon watches each realtime source itself with inotify,
in process (`src/daemon/watch_source.rs`), one watcher thread per routine, using the same watcher
library, skip list and exclude matcher as `sdsync-dsm-api --watch` below, so the watch budget and the
setup deadline are the same. Polling is the fallback: it replaces the native watch when the
watch cannot run, for the named reason `watch_limit`, `watch_unavailable` or `watch_setup_failed`,
and the daemon tries the native watch again after 10 minutes, doubling up to every 6 hours. The
fallback reports a change from a single differing reading, because the debounce below is what waits
for a copy to settle. A vanished entry or an unreadable subdirectory is a named skip, not a stall,
and every walk has a deadline. Editing a realtime profile's source or excludes replaces the watch
(`routine_rearmed`). When the daemon starts, and when the native watch is armed again, it syncs the
whole tree once if the source changed after the last success. The controller log carries
`routine_backend` (a change of backend or cause; a run of reports with one cause is one line, and the report that ends it counts the others as `suppressed_reports=N`), `routine_watch_notice` (named skips)
and `routine_rearmed`. A healthy native watch has a safety-net floor: a stat-only reading of the tree every
min(the routine's poll interval, 1 hour), and never more often than once a minute. A directory that changed without an
inotify event is synced and named (`watch_missed_change dirs=N first=… floor_seconds=…`, `warn`, under
`routine_watch_notice`); a floor reading that fails is named once (`poll_floor_failed cause=…`). When
the watch is armed, nested mounts (a different device below the source: USB, bind, CIFS or NFS) are
listed as an `info` note (`nested_mounts=N first=… watched_by=poll_floor every_seconds=…`), because
inotify does not see changes made through them. Separately, an armed realtime routine syncs its whole
tree once every 24 hours (`routine_full_reconcile`, `info`). A profile held by a [lasting
block](operations.md#lasting-block-after-an-out-of-memory-kill) is not run by its routine, and each
held trigger is logged at most once an hour (`routine_suppressed`). See
[Realtime reports polling](troubleshooting.md#realtime-reports-polling).

**Under the shell engine** DSM ships no
`inotifywait`, so the package brings its own recursive watcher: the controller runs
`sdsync-dsm-api --watch`, which watches every directory of the source except the DSM-managed ones
(`@eaDir`, `#recycle`, `#snapshot`, `@tmp`, ...) and whatever the profile's excludes and the source's
`.sdsyncignore` exclude, using the same matcher as the sync. It reports file writes when they are
closed, creations, deletions, moves, permission changes and bare modification-time changes, at most
one marker line per 100 ms however many files change. An `inotifywait` from inotify-tools is used
only if the package binary fails its executable checks; nothing needs to be installed from SynoCommunity
or Entware, and the controller logs which watcher it chose once per start (`realtime_watcher_command`).

**Watch budget.** inotify needs one watch per directory. The watcher may use 80% of
`fs.inotify.max_user_watches`, minus the watches this user (the package user) already holds for other
watchers. A source with more directories than that is not watched at all: the routine reports
`backend_reason=watch_limit` with the numbers (`dirs=`, `budget=`, `limit=`, `held=`), appears on the
Overview as an amber item, and is polled meanwhile. Raising the limit needs root and is optional; see
[Raising the inotify watch limit](troubleshooting.md#raising-the-inotify-watch-limit). The setup walk that arms the
watches has a 120-second deadline; if a directory (a hibernating disk, a hung mount) holds it up the
watcher gives up and names that directory (`watch_setup_failed`, `deadline=120s`). Under the daemon
engine a tree that is simply too large to walk in time is not left unobserved. The arm deadline and
the polling walk's deadline double on each attempt, up to 30 minutes, and the backend detail names
the growth (`walk_deadline_exceeded … next_walk_deadline_seconds=N`). While no walk finishes, the
routine stays amber (`watch_setup_failed`) and the whole tree is synced at most every 15 minutes
(every poll interval if that is longer, and at most hourly); a tree that cannot be walked is
expensive to sync, so it is not synced once a minute, and what the poller has learned (the grown
deadline, when the tree was last synced whole) survives the daemon's native-watch retries. Every
new backend account is logged as a `routine_backend` line (a polling tree whose skipped entries change on every reading is one episode, not a line per reading), so the growth is in `controller.log`.
Disarming a routine ends its walk at once (the floor's walks too), and the process runs at most 16
directory walks at a time. A walk refused by that cap is named as the cap (`walk_cap`: other
routines are arming, or walks abandoned on a stalled disk have not returned) and is tried again
within 15 seconds, not after the 10-minute polling backoff. While the cap refuses the arm nothing
watches or polls the tree, so the routine reports `backend=none` with `reason=watch_setup_failed`
(not `polling`), and a second cap episode after a successful arm is reported again. The start-up
catch-up walk is refused the same way: it is named (`catch_up=deferred cause=walk_cap`), asked for
again after about 15 to 30 seconds (staggered by routine name, so routines refused together do not
return together) and does not sync the whole tree for it; `catch_up=resumed` says when it went
through. Only a cap that refuses for 10 minutes ends in a whole-tree sync, named
(`catch_up=whole_tree cause=walk_cap`). If the kernel drops
events (`IN_Q_OVERFLOW`), the watcher re-walks the tree to restore its watches and reports that the whole
tree must be treated as changed, so a run is triggered. The watcher keeps about 6 MiB of
memory at 20,000 directories and about 16 MiB at 100,000. A directory created while watching is added to
the watch set as it appears. The quiet period is measured from the event file's last write, so each newly observed
change restarts the debounce and a run cannot begin until the full period has elapsed. The default
debounce is 45 seconds. Realtime mode has no interval, weekday, window-start, or window-end setting.
The controller revalidates and consumes one exact native-event generation only after dependency and
shutdown admission. A concurrent event remains queued for its own complete debounce, and a deferred
dependency therefore does not discard the pending marker.

A source that never has a quiet moment is not left pending forever: past `debounce_seconds × 4`
(clamped between `debounce_seconds + 60` and 3600 seconds) the controller dispatches anyway, so
worst-case staleness under continuous writes is bounded rather than open-ended. At the 45-second
default that ceiling is 180 seconds. The ceiling is not configurable; the operator already chose the
debounce, and it is derived from that one field rather than adding a second number nothing else
explains.

**Fallback poll is a floor, not a fallback (shell engine).** It runs on every install and alongside every healthy
native watcher, checking a periodic source fingerprint (file path, size and mtime, plus the names of directories) whenever no
native event is pending for that turn. A native event inside its debounce only suppresses the poll
for that one turn; it does not turn polling off. Reducing **Fallback poll** therefore increases
source traversal work even on a routine whose watcher has never failed.

While a poll walk is in flight, the controller splits its five-second turn into one-second ticks; with
no walk in flight it naps until the soonest realtime poll is due (at least one second, at most the
five-second turn). A tick costs no evaluation until the walk ends; then a poll turn evaluates only the
routine whose walk finished and the routine whose poll is due next, so a source tree that takes seconds
to walk no longer delays the rest of the controller. Everything else (native events, windows, the queue
and the schedule) keeps the five-second turn.

Under the shell engine the poll requires one unchanged reading before it dispatches: a fingerprint that differs from the
last one is held rather than acted on immediately, and is only promoted once the next poll sees that
same reading again. This adds at most one poll interval of latency, and is the price of not
dispatching against a copy still in progress. `@eaDir`, `#recycle`, `#snapshot`, and `@tmp` are
pruned from the walk, since they can never be synced and are the most common source of a poll that
fails on a permissions error the operator never set. A subtree that is genuinely unreadable is
reported rather than silently retried forever: the readable part of the source stays observed and
polled, and the routine's `backend_reason` (below) names which condition applies.

**Known limitation: the poll fingerprint is path, size and mtime for files, and the name of each
directory.** The creation, removal or rename of an empty directory is seen (format `v3`; releases before
it could not see one). A rewrite that keeps both the size and the mtime is not seen by polling. Where
that matters, use an interval or daily routine, whose runs compare the whole tree. No change time is
used, because its noise on a real DSM is unmeasured.

**Upgrading changes the fingerprint format, so every realtime routine syncs once.** A baseline written
by an earlier release (untagged or `v2`) cannot be compared with a `v3` reading, so it is treated as
unknown: the controller logs `routine_fingerprint_format_changed` (`info`; `profile=`, `from=`,
`to=v3`, `action=one comparison sync`) once, lets the reading settle over one more poll, runs one
ordinary sync, and takes the `v3` reading as the new baseline. The old silent rebase
(`routine_fingerprint_rebased`) no longer exists. Nothing outside the controller reads the fingerprint.

This is not a kernel event stream and very short-lived changes between identical fingerprints may not
be observable; final sync planning still performs the engine's normal complete source/target work. A
healthy native event that is still inside its debounce suppresses fingerprint fallback for that turn,
so polling cannot dispatch the same change before the quiet period ends.

The fingerprint baseline moves only after a sync of the change **succeeds** (exit 0). A reading that
becomes due is held as pending; a deferred dependency, a busy refusal, a failure, or an exhausted
retry ladder leaves the old baseline and the pending reading in place, so the change is due again on
the next poll rather than being forgotten. After a successful native dispatch the controller
re-baselines in the background, so fallback polling cannot dispatch that same change a second time;
if that fails the baseline is left alone and the backstop poll re-dispatches. The poll walk itself is
a single background job (one at a time): the controller turn collects its result on a later turn
instead of blocking on a slow tree. A not-before time written by a busy refusal, a dependency
re-check, or exhaustion is honoured by realtime routines too.

The snapshot reports the effective backend as `inotify`, `polling`, `interval`, `daily`, or `none`,
and, for a realtime routine, a `backend_reason` naming why: `ok`, `source_missing`,
`source_symlink`, `watch_unavailable`, `watch_setup_failed`, `watch_limit`, `watch_stopped`,
`poll_unreadable`, or `poll_unavailable` (see
[Realtime routine never runs](troubleshooting.md#realtime-routine-never-runs) for what each means and
what to do about it). A state file written before this field existed carries none, and that is read
as `ok`. `backend` itself is only as current as the last poll that actually completed; a stalled or
failing poll leaves it holding whatever it last reported rather than updating to reflect the present.
Overview and Routines surface both the backend and, when it is not `ok`, the reason explicitly. Do
not infer native realtime support from the requested `mode=realtime` alone.

Under the daemon engine, `sdsync-dsm-api --ctl status` carries the cause behind the reason: each entry of
`routines` has `backend_detail`, the detail of the routine's latest backend report (at most 240 bytes,
with the end kept and the middle cut: `next_walk_deadline_seconds=N` survives; `null` when there is
none, cleared when the routine is armed or disarmed), and each new cause is also a
`routine_backend` line in `controller.log`.

### Idle or blind

A realtime routine that has not run for a day is either idle (nothing changed) or blind (nothing is
looking). The routine's state record says which, under both engines, in keys appended after
`escalated_epoch`: `watching_via` (`native`, `poll`, `schedule` or `none`), `observe_state` (`ok`,
`degraded`, `blind` or `disabled`), `observe_cause` (a token, `none` while observed),
`observe_since_epoch`, `last_trigger_epoch` (the last change seen, or the last time the routine was
due) and `last_run_epoch` (the last run that finished; a refusal is not a run). A reader that predates
them ignores them, and a record that lacks them reads as observed.

`degraded` still has an observer: the native watch is down but the poll runs, or the poll floor under a
healthy native watch cannot finish its walk. `blind` has none: the poll cannot read its tree or was
killed for taking longer than its budget (900 seconds under the shell engine, bounded by
`SDSYNC_DSM_POLL_WALK_BUDGET`), the process-wide walk cap refused the arm (daemon), a lasting block holds
the routine (daemon), the watcher never reported (daemon), or the source is missing. Each change of
state or cause is one `routine.unobserved` Activity row, and the end of it a `routine.observed` row;
`ctl status` carries the same fields. Under the shell engine, a run with no heartbeat for 30 minutes,
or a dashboard job that has run longer than 30 minutes, also writes one row for `all` profiles (cause
`controller_blocked`), since the controller loop waits on it.

## Retry and backoff

Profile **Retries** handle eligible network operations inside one engine run. Routine **Retry
attempts** retry the whole failed Plan/Sync action and default to five attempts. The first retry uses
the configured backoff. With **Exponential backoff** enabled, each later retry doubles the preceding
delay; with it disabled, every retry uses the fixed base delay. Both paths are capped at 300 seconds
(five minutes). Activity records `routine.retry_scheduled` with the selected delay.

Once a whole-action retry is pending, its deadline owns admission. Daily cadence and realtime native
events or fingerprint changes cannot start another attempt before that deadline. Daily retries still
wait for their configured weekday/window after the deadline; realtime changes remain available for
a later finite run rather than consuming an additional retry early.

Routine files created before the exponential toggle are interpreted as enabled, preserving their
former doubling behavior. A legacy base above five minutes is read and executed as 300 seconds; the
next save writes the supported bounded value.

When a routine exhausts its retry attempts (the daemon's escalation follows the same alert
policy and cooldown), Activity records `routine.retries_exhausted` (error)
with the attempt count and the last reason, a sync routine raises a DSM system-log `sync_failed` and,
when failure alerts are enabled, a desktop one, and a realtime routine then cools down for 900
seconds before it may try again. The desktop and system-log escalation honours the alert cooldown
per routine, so a routine stuck on its ladder does not alert on every pass; the Activity row is still
written each time. Exit `3` (partial) consumes no retry and no failure count: the deferred paths are
picked up by the next run. A realtime routine whose runs keep finishing partial does not re-run at the
speed of its events: consecutive partials back off on the routine's own retry ladder (at most 300
seconds, five steps), then every 15 minutes, and the sixth in a row writes one `warn`
`routine.retries_exhausted` row (state `deferred`) so the repetition is visible.

Deferrals are recorded once per episode, not every controller turn. A routine outside its daily
window, or waiting on a dependency, writes one Activity line and one log line when it enters that
state, and an earlier succeeded or failed outcome is not overwritten by the deferral. The dependency
is still re-checked every turn, so the routine runs as soon as it is satisfied. A run refused because
the run lock was busy (exit `75`) is also a deferral: the routine is retried 30 seconds later and no
retry is consumed. A routine whose configuration is skipped as invalid is reported once per field per
controller lifetime, in `controller.log` and Activity, instead of silently.

Retries do not make a multi-profile or deletion operation transactional. Earlier successful target
changes remain when a later action fails. Investigate authentication, routing, source, or safety
errors instead of using a large retry count to hide them.

## Dependencies

**Wait for routines** is a success-state gate, not a distributed workflow engine. Every selected
profile routine must have a latest state of `succeeded`, and that state comes from a **sync**: a Plan
never writes a profile's last-run record, its failure count, or the aggregate result, so a manual Plan
cannot satisfy a dependency and cannot hide a failed sync. Otherwise the dependent routine is marked
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
controller defers the routine with the reason `delete_bound`. That deferral can never clear by waiting,
so the dashboard calls the routine *blocked* (a failure-level attention item and a red *Blocked* widget
row) rather than an ordinary deferral, and tells you to raise the routine's deletion limit or lower the
profile's maximum deletions on the Routines page. Actual deletion remains bounded by the profile limit
and all core safety checks.

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
  --debounce-seconds 45 \
  --poll-seconds 60 \
  --retry-attempts 5 \
  --retry-backoff 60 \
  --retry-exponential true \
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
scheduled deletion approval. Its next deadline is kept in `var/state/schedule.state`, so a restart or
reboot no longer postpones a run by a whole interval: on start the next run is the earlier of the saved
deadline and one interval from now, and a deadline that passed while the controller was stopped runs
one minute after it starts. An aggregate run cancelled by a package stop or a reboot (`130`/`143`, or
`137` after the stop had to kill it) is logged at `info` with reason `cancelled`, not as an error, and
runs again 60 seconds later rather than after a whole interval. It can coexist in configuration with per-profile routines, but both
share the same run lock. Prefer per-profile routines for new dashboard-managed deployments because
their mode-specific timing, dependency, fallback, and state are individually visible.
