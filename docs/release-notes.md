# Release notes: behaviour changes

This page lists changes you can see or must act on. Releases use calendar tags (`YY.N`); the
changes below are in the first release built after the audit fix round documented in
[Comparison, exclusions, and deletion](configuration/safety.md) and the
[observability reference](observability.md). Older notes are not kept here: each release's GitHub page
lists its own commits.

## Unreleased: the daemon is the DSM service engine

- **A moved file under 8 MiB is uploaded instead of copied on the NAS.** A server-side copy first
  downloads the old copy to prove it, then runs a copy task and checks the result, which for small
  files took longer than sending them: a 10-file folder move went from about 40 s to about 18 s at
  the NAS's measured 0.19 s per request. Larger moved files are still copied.
- **`sync` and `plan` take a scope set.** `--scope` may be repeated and `--scope-shallow FOLDER`
  names a folder whose direct files only are in scope, so a realtime run can cover the folders that
  changed and nothing between them. Edits in `2026/03` and `2026/11` used to run over all of `2026`.
  A shallow folder's subfolders are never listed, written, deleted or pruned from the index. See
  [Comparison, exclusions, and deletion](configuration/safety.md).
- **A run scoped below the source root re-verifies 2 pairs as its canary, not 25.** That covers a
  realtime routine's runs over the folders that changed and `--scope`. Whole-tree runs, including
  the daily full pass, still draw 25, and `--pair-index-canary` and `--reconcile-canary` still win.
  A one-file edit in a 10-file folder no longer re-reads every file in it twice.
- **Reconciliation no longer re-verifies every pair the index served.** The carry compared the NAS
  clock of the run that had proved each pair with this run's start, so on a warm index every pair
  proved more than 20 s earlier was verified live, and the run named
  `pair-index-plan-clock-unbounded` with advice to check the NAS's time. Only a NAS clock ahead of
  this host's is now refused.
- **A shell-engine start no longer fails at random with `committed services failed to publish final
  readiness`.** The API service and the controller each read the start lease (`controller.starting`)
  while the lifecycle script commits it by renaming a new file over it. A read that landed in that
  instant saw the old file with no links left and refused it as unsafe, so the service exited and the
  start failed. The next attempt started normally. It was rare (a few hundred starts apart in the
  tests, more often on a slow or loaded machine). The read now looks again. A start that does fail after the commit names the service that stopped, how it
  ended and which record it never wrote (`startup diagnostic: stage=final_readiness ...`), and the
  API service's own refusal is in `api.log`. See
  [Start failures](dsm/troubleshooting.md).

- **The Rust daemon replaces the shell controller and runner.** `sdsync-dsm-api --daemon`
  (launched by `--launch-daemon --serve-api`) keeps the schedule, the realtime watchers, the dashboard
  queue and the dashboard's API service in one process. The shell engine stays for this release as a
  fallback: write `shell` to `var/state/engine`, then stop the package and run it again. Any other
  content is named (`service_engine`, and an amber item on the Overview) and the daemon runs. The About
  page and the dashboard's outcomes show the engine. Lasting blocks (below) are not applied by the shell
  engine. See [Service engine](dsm/operations.md#service-engine).
- **Realtime routines use the native inotify watcher inside the daemon.** Polling is the
  fallback, named `watch_limit`, `watch_unavailable` or `watch_setup_failed`, and the daemon tries
  the native watch again after 10 minutes, doubling up to every 6 hours. Beside a healthy watch a
  stat-only floor reading (every min(the routine's poll interval, 1 hour), at most once a minute)
  syncs and names any directory that changed without an event (`watch_missed_change`, `warn`; a failed
  reading is `poll_floor_failed`), nested mounts are listed at arm time (`nested_mounts`, `info`), and
  every armed routine syncs its whole tree once every 24 hours (`routine_full_reconcile`). The polling fallback reports
  from one differing reading instead of two identical ones. A vanished entry or an unreadable
  subdirectory is a named skip and every walk has a deadline. Editing a realtime profile's source or
  excludes re-arms its watch. A start-up catch-up syncs the whole tree when it changed after the last
  success. New `controller.log` events: `routine_rearmed`, `routine_watch_notice`, `routine_backend`.
  See [Routines](dsm/routines.md#realtime-mode-and-fallback).
- **Saves and dashboard jobs no longer wait for a sync.** The dashboard queue runs in lanes beside any
  sync: one serialized job (saves), two concurrent (status walks, Doctor, actions) and two connection
  probes, with deadlines of 10 minutes, 6 hours and 3 minutes. A job past its deadline is answered
  `consumer_failed`, or, for a save, left outcome-unknown, as a dead worker is. New events:
  `control_consumer_failed`, `control_consumer_outcome_unknown`, `control_request_rejected`,
  `control_file_expired`.
- **A save during a sync commits and says it takes effect on the next run** (`applies_next_run`). Only
  a change that would touch a running profile is refused, with exit `75` and a message starting
  `profile_running:`: remove, set-default, remove-routine, a change of URL, username, source, remote,
  compare mode, deletion settings or excludes, removing its secrets, and a confirmed resync. Doctor no
  longer holds the management lock (a Doctor `--write-test` against a running profile is still
  refused), a plan-only resync takes no lock, and an interactive `set-password` reads the secret
  before it takes the lock. A daemon run publishes `var/run/run.scope` naming its profiles, and never
  starts while `management.lock` is live (`run_deferred`, `reason=lock_busy`). The shell engine keeps
  the whole-package refusal. See [Saves beside a daemon run](dsm/operations.md#saves-beside-a-daemon-run).
- **A daemon run is gated and audited like a shell run.** The mandatory audit, the security-policy
  gates and the secret checks apply, with the same reasons (`policy_*`, `password_missing`,
  `secret_unsafe`, `audit_unavailable`). A daemon pointed at a tree that is not a package runs
  unaudited and says so (`service_audit`).
- **`sdsync-dsm plan` and `run` hand the run to the daemon and wait for it.** Under the daemon engine
  the command queues the run, waits for it, and exits with the sync's own exit status (`0`, `3`
  partial, `1`, ...), so scripts read the same status as before. `--wait-seconds S` (default 86400)
  bounds the wait; when it runs out the run goes on and the command exits `75`, naming
  `sdsync-dsm status`. `--no-wait` returns once the run is queued. The dashboard's Plan and Run never
  wait. A daemon that does not answer is refused (`the sync service is not running`, exit `75`), never
  run by the old runner. See [CLI parity](dsm/cli-parity.md).
- **Failure alerts** go through the package's alert policy, counter, notifier and Log Center as
  before, and an escalation follows the policy's cooldown. A delivery that fails is logged as
  `alert_unavailable` with its channel and cause.
- **The verified-pair index is on by default for daemon runs** (`--pair-index var/state/cache/pairs`,
  one `<profile>.redb`). `index:` appears in `ctl status` and in the dashboard: state, entries, hits,
  misses, canary, refreshed and the worst issue with its path and cause. A damaged index is named
  (`pair-index-corrupt`), set aside and rebuilt; one retired by re-creating the profile with another
  source, remote or NAS is named as information. `remove-profile` deletes the `.redb`, its quarantined
  copies, the lasting block and the alerts state. The result's `pair_index` has a new `entries` field.
  See [Verified-pair index](pair-index.md#on-a-dsm-package).
- **An out-of-memory kill is a lasting block.** An exit `137` is `core_oom_killed` (the cgroup's
  counter moved) or `core_killed`, never `core_failed`, and the detail names the memory a run could
  use. After `core_oom_killed` the daemon holds the profile (`var/state/lasting/<profile>.state`): no
  retries and no automatic runs, and each held trigger is logged at most once an hour
  (`routine_suppressed`). **Run now**, or a change to the profile's settings or the performance level,
  lifts the hold; a save that changes nothing does not. It survives restarts, an unreadable record is
  named and still holds, and the dashboard shows an amber **not synced until you fix this**. See
  [Lasting block](dsm/operations.md#lasting-block-after-an-out-of-memory-kill).
- **An HTTP request that timed out is `timeout`, not `network`**, and its message leads with the
  innermost cause. See [Observability](observability.md).
- **The API service is a supervised child of the daemon.** The package is not ready until `api.sock`
  answers; a crash is named (`api_server_exited`) and restarted with a backoff (`api_server_restarted`).
- **New dashboard labels** for the reasons `run_stalled`, `launch_failed`, `state_unwritable`,
  `consumer_timeout` and `lasting_state_unreadable`. The daemon ends a run with no progress for 45
  minutes as `run_stalled`.
- **`service_layout`** logs the home and var roots as named and as resolved. DSM's aliases are followed
  one level only; the target must be a directory owned by the package user and not writable by group or
  others, and a refusal names both paths.
- **A password, TOTP seed or remote-log token saved while that profile syncs** says it takes effect on
  the next run (`applies_next_run`).
- **Log rotation by the daemon takes the shell's `event-log.lock`.** A rotation that cannot be made
  keeps the line and writes `var/state/log-rotation.failed`.
- **The checksum index works on armv7 NAS models.** On armv7 (for example the DS416) every index
  the package built was found damaged at its next open (`pair-index-corrupt: Database page size 0
  does not match expected 4096`), so it was set aside and rebuilt on every run and never saved a
  checksum. The cause was a compiler fault in the armv7 release build of the database library,
  which wrote back a header with everything but its first ten bytes zeroed. That build is now made
  so the fault does not occur. A file left from an earlier release is set aside once more, and the
  index is warm from the run after that.
- **An index that cannot be kept is turned off by name (`pair-index-unkeepable`).** When the index is
  found damaged three opens in a row with the same cause, it is set aside as before but not built
  again. Each run names the condition as a warning and checks every file live, and no more copies
  pile up. It is tried again after a package update, after 7 days, or when
  `<profile>.redb.rebuilds` is removed from the index folder.

### Daemon fixes from the integration review

- **A crash can no longer wedge start or stop.**
  - `run.lock/` never holds anything but `pid`, not even for an instant.
  - `daemon.pid` names the daemon by PID, start time and boot id.
  - A start that fails once it holds `daemon.lock` (including a `daemon.ready` it cannot write) is
    named `service_start_failed` and removes its records.
  - The supervisor publishes `supervisor.pid` and, when it gives up, removes the dead daemon's
    records (`service_records_removed`).
  - `--launch-daemon` stops the supervisor it started when the daemon is not ready, and shows the
    daemon's last stderr line, which `log/daemon-stderr.log` keeps (1 MiB, shown in the controller
    logs).
  - A daemon waits out the lifecycle's own `daemon.lock` probe instead of exiting.
- **No log line waits on `event-log.lock`.** An empty lock directory is free; a dead or torn owner
  is cleared; a live one skips the rotation, which is tried again in 5 s and is a failure only after
  a minute. Each case is named (`event_log_rotation_deferred`, `event_log_rotation_failed`,
  `event_log_lock_stale`).
- **The dashboard queue never waits forever.**
  - A failing audit reconcile is named once and backed off. Waiting jobs are answered
    `audit_unavailable` after 30 s, a new named failure with its own dashboard guidance.
  - A worker that outlives KILL frees its lane (`consumer_unkillable`).
  - Workers' stderr reaches `controller.log` (`consumer_stderr`).
- **An unreadable configuration file is named, not read as absent.**
  - It is named `config_unreadable` with its path and cause, and the last good configuration stays
    in force.
  - An unreadable routine conf no longer disarms realtime, and an unreadable `routines.d` no longer
    reads as no routines.
  - An unreadable routine state is held, not reset.
- **A realtime source too large to walk within 2 minutes is still synced.**
  - The walk and arm deadlines grow, named (`walk_deadline_exceeded`), up to 30 minutes.
  - Meanwhile the routine stays amber (`watch_setup_failed`) and the whole tree is synced at most
    every 15 minutes (every poll interval if that is longer, at most hourly), not once a minute.
    The grown deadline and that cadence survive the native-watch retries.
  - Every backend report is logged (`routine_backend`), so `walk_deadline_exceeded` and skipped
    entries reach `controller.log`.
  - A disarmed routine's walks end at once, and all walks share one cap of 16; a walk the cap
    refuses is named as the cap (`walk_cap`) and retried within 15 s.
- **The control socket keeps room for `ping` and `status`.**
  - One deadline covers a whole request.
  - `ping`, `status` and `stop` have reserved slots; `wait` and `mutate` are limited to 16 of 32.
  - A connection counts only once its verb is read, and a waiter whose client hung up frees its
    slot (`ctl_peer_gone`).
  - A client's connect is bounded.
  - Accept failures are named and backed off.
- **Runs:**
  - An all-profiles run passes the core the profiles its `run.scope` lists, not `--all-profiles`.
  - The watchdog reads the core's progress record, which socket I/O does not bypass.
  - A cancel or stop that arrives before the core starts keeps it from starting.
  - `run.lock` is published and removed on the bounded state writer.
- **The API service:**
  - A server killed before it activated `api.sock` no longer blocks every restart
    (`api_socket_stale_removed`).
  - Readiness no longer connects (that connect was logged as a failed request).
  - An API child that cannot be waited for is KILLed and reaped before a restart.

### Daemon fixes from the fix5 review

- **`run.lock/pid` is renamed into place**, so a crash can no longer leave it with a second link.
- **The audit log's state is on the Overview.** While the dashboard queue cannot reconcile the audit
  log for more than 30 s, `controller.state` carries `degraded=audit_unavailable`, `degraded_since=`
  and `degraded_cause=`, the snapshot's `controller.degraded`, and an amber item says queued operations
  are refused until it recovers (`service_degraded` / `service_degraded_cleared`). A recovered audit
  log is no longer refused for the rest of its backoff: a job is refused only on an attempt made after
  it arrived, and an open episode is retried even with nothing waiting.
- **A profile state that cannot be read is not rewritten.** Its failure streak and last success stay
  as they are (`profile_state_unreadable`, `degraded=profile_state`). `config_unreadable` says what
  stands instead (`kept=`), a dependency's unreadable state is named, a removed routine's unreadable
  records are forgotten (`config_forgotten`), and `ctl status` names an unreadable `profiles.d`
  (`index_unavailable`).
- **A scheduled all-profiles run publishes its progress record**, so the watchdog no longer takes a
  long network phase of a batch for a stall.
- **A file the dashboard queue cannot remove is named** (`control_file_unremovable`), a secret
  sidecar included; `control_file_expired` is logged only for a file that is gone.
- **Log rotation:** an episode ends when the log is found rotated, so a later failure is named again;
  a torn owner record is reported as torn; a record cleared as stale is checked to be the one judged.
- **Bounded waits:** a configuration save whose manager outlives KILL, or whose grandchild holds its
  stderr, no longer holds the save's reply. A full control-socket backlog is named, not reported as
  "Transport endpoint is not connected".
- **The API service child** is signalled only while its pid still names it, and is not respawned beside
  one that could not be proven gone (`api_server_respawn_held`).
- **`sdsync-dsm-api --daemon-lock-probe <path>`** answers what `flock -n` does on `daemon.lock` (0 free,
  3 held, 1 cannot probe, named on stderr) for a NAS whose BusyBox has no `flock`.

### Lifecycle script fixes from the second review

- **The shell engine no longer starts beside a daemon it cannot see.** When `daemon.lock` cannot be
  probed, start and status ask the identity check on the daemon's records, as stop does: a live
  daemon, or one the check cannot rule out, blocks, and the refusal says why.
- **A unit without `flock(1)` still sees the daemon.** The lock is probed with
  `sdsync-dsm-api --daemon-lock-probe <path>`, and the probe in use is named once per change in
  `controller.log` (`daemon_lock_probe`). Status names why a lock could not be probed.
- **A malformed or unreadable `supervisor.pid` no longer reads as "no supervisor".**
  `supervisor.lock`, which the supervisor holds for its life, is the second witness; start, the
  hooks and stop refuse naming it, and a stale record's removal names why it could not be trusted.
- **The controller signals only its own watcher.** A pid file naming another child of the controller
  (a wrapped PID) is left alone and named (`routine_watch_pid_mismatch`, or `pid_reused` in the
  failure detail).
- **A stop without a readable clock has one deadline.** Each phase is charged what it can take
  against `SDSYNC_DSM_STOP_TIMEOUT`, so the worst case is the same as with a clock.
- **A shell-engine stop that outlasts `SDSYNC_DSM_STOP_TIMEOUT` kills what is left** (the runner and
  its children, the API service, then the controller), logs `service_stop_forced` and names each KILL,
  instead of exiting `1` with the services running.
- **A stop no longer leaves a realtime poll's scratch directory** (`run/fingerprint.<pid>.<n>`).
- **Recovering a dead `run.lock` owner removes the daemon's staged owner file** (`run/.run.lock.pid.<pid>.<n>`)
  and names it.
- **The shell snapshot carries `controller.degraded`** (`reason`, `since_epoch`, `cause`) from the
  daemon's three optional `controller.state` lines, and refuses a malformed set as `corrupt_state`.

### Follow-ups from the integration

- **An unreadable request queue is named, not read as empty.** A control directory the daemon cannot
  read is logged once per episode with its path and cause (`control_dir_unreadable`, then
  `control_dir_recovered`), raised as `controller.degraded` (`control_dir_unreadable`) and shown in
  amber.
- **`pair-index-plan-clock-unbounded`** (information) names how many pairs the run's carry did not
  serve because the NAS clock of the plan pass was more than 20 s from this host's; they were checked
  live. The count is also in the result's `reconciliation.plan_clock_unbounded`.
- **A supervisor waits out a `supervisor.lock` held for a moment** (2 s, like `daemon.lock`) instead of
  leaving with exit 0 and no daemon; if the lock is still held it says so on stderr and as
  `service_superseded`.
- **`--launch-daemon` returns when `daemon.ready` is published**, not on the socket's first ping.
- A control-socket connect failure names the socket path once.

### Shell lifecycle follow-ups (fix7)

- **A hung lock probe is cut off.** `flock -n` and the bridge's `--daemon-lock-probe` run under `timeout 5`
  when `timeout(1)` works; on expiry the lock is "cannot be told" with the cause `timed out after 5 s`.
- **Two bad supervisor witnesses no longer refuse for good.** A malformed `supervisor.pid` beside an
  unusable `supervisor.lock` (a directory, symlink, fifo, mode `000`) is settled by looking for a
  `--supervise` process of the package: none, and stop removes the record and repairs the lock (or prints
  the exact command); one, and it is named. A stop in the respawn window with a malformed
  `supervisor.pid` waits for the respawn and succeeds in one call. The `ctl stop` wait is computed from the
  budget that is left.
- **The forced stop of the shell engine checks and names.** Each target's start tick is read again
  immediately before its KILL, the runner is verified by its start tick, `kill`'s status is checked ("killed"
  lists only what was killed), every skip is named, and a controller that already exited is not reported as
  one that did not.
- **The shell snapshot refuses what the in-process one refuses** (`degraded_since` with leading zeros, a
  `degraded_cause` that is not UTF-8) and serves `controller.degraded` only while the service runs; it also
  reads `daemon.ready` for `service.state`, as the in-process rung does.
- **A poll killed outright no longer leaks for the controller's life** (its scratch directory is named for it
  and removed by the next poll), and **an `inotifywait` fallback watcher is stopped when the controller is
  killed**.

### Verified-pair index: correctness fixes

- **A write through a shared memory mapping is no longer missed.** Linux moves a file's times only
  when a mapped write first dirties a page, so for up to about 35 s after a change (the kernel's
  writeback interval) the content can change again with every fact unchanged. The index took such a
  file as settled after 2 s and could skip its upload indefinitely. The racy window is now
  `dirty_expire_centisecs` + `dirty_writeback_centisecs` from `/proc/sys/vm` + 25 s, at least 2 s:
  60 s with the defaults, and 60 s on macOS and the NAS side. An unreadable setting is
  `pair-index-writeback-unknown` (60 s); periodic writeback switched off is
  `pair-index-writeback-unbounded`, and nothing local is served. **The first run after upgrading
  verifies every pair live**: indexes written before are retired by name (`pair-index-retired`,
  semantics epoch 1, the two-second window). A file changed within a minute of a run is read live
  once more on the next run.
- **A NAS clock that disagrees with this host's is not trusted.** The `Date` header of `MD5.start`
  more than 20 s outside this host's clock during the pass (a proxy, a NAS with the wrong time) makes
  the pair racy and is named once per run, `pair-index-nas-clock-skewed`.
- **Reconciliation carries fewer digests on trust.** A pair is carried from the plan pass only when
  the NAS listing has a change time for it and neither side changed within the racy window of the
  read that produced its digest.
- **A damaged index heals itself.** A file redb repaired, one that fails a check on any open, and
  one with a record that does not decode are set aside and rebuilt on the spot, and nothing read from
  them is served (`pair-index-corrupt`); before, a repair found mid-run kept serving, and one
  undecodable record disabled the index until the file was deleted by hand. redb 4.4 can also crash
  while opening a file with a damaged allocator page: the crash is recorded beside the file
  (`<profile>.redb.panicked`) and the next run sets the file aside, so one crashed run heals it.
- **The index file is verified once per run**, not on every reopen: a reopen within the run trusts a
  file that is exactly as the run left it (identity, length, times and redb's header). On an armv7 NAS
  this removes three or four full reads of the file per run.
- **Quarantined copies that cannot be removed are named** (`pair-index-cleanup-failed`), and past
  three such copies a damaged file is deleted instead of kept.
- **A profile name over 64 characters gets an index** (`<first 48 characters>-<16 hex of its
  SHA-256>.redb`) instead of `pair-index-unsafe` on every run.
- **A panic from this program no longer retires a healthy index.** Only a panic raised in redb's own
  sources is recorded beside the file. A panic record or a quarantined copy that cannot be examined
  is now named with its path and cause (`pair-index-unsafe`, `pair-index-cleanup-failed`) instead of
  being passed over.
- **Reconciliation applies the 20 s NAS-clock bound too**: a pair whose plan-time `MD5.start` clock
  stood further outside this host's clock is verified live rather than carried.
- **Index size and the racy window are documented as measured.** A dense pair costs its path plus
  about 125 bytes; the file on disk is 1.1 to 4 times that. The window's assumptions (steady
  clocks, a flusher that drains within the margin) and the re-hash cost of a realtime routine are
  in [Verified-pair index](pair-index.md#what-it-can-and-cannot-do).
- New `pair_index.issues` codes: `pair-index-writeback-unknown`, `pair-index-writeback-unbounded`,
  `pair-index-nas-clock-skewed`, `pair-index-cleanup-failed`. See
  [Verified-pair index](pair-index.md#when-it-cannot-be-used).
- **The About page shows which package build is installed.** A new **Installed package build** row
  under **Build identity** names the CPU architecture and the target the binary was compiled for, for
  example `armv8 (aarch64-unknown-linux-musl)` or `armv7 (armv7-unknown-linux-musleabihf)`. The
  snapshot's `package` member gains `target` and `arch` beside `version`; all three are compiled in
  and none is read from `INFO`. A build whose target has no Synology architecture reads `Not a
  Synology package build (<target>)`, and a package API that reports neither reads `Not reported by
  package API`. See [Compatibility](dsm/compatibility.md#four-release-spks).

### Reconciliation, the index on a failed run, and Doctor (fix8-core)

- **A file moved into the source while a run works no longer fails the run.** A move or a copy keeps
  the file's old modification time, so reconciliation could not tell it from a file the run had
  failed to upload, and the run failed with `final reconciliation found 1 pending in-scope
  operations`, retried, and alerted. Reconciliation now judges what is left against the run's first
  scan: a path the first walk did not hold, one whose local facts moved, one deleted locally, and a
  NAS entry another hand changed are named as transient deferrals (`source-changed`,
  `source-vanished`, `remote-changed`). The run is partial (exit `3`), raises no alert, and the next
  run carries them. A modification time after the run began no longer excuses anything by itself.
  See [Comparison, exclusions, and deletion](configuration/safety.md).
- **A reconciliation failure names its paths.** It used to give only a count, which also included
  entries it had excused, and advised inspecting a fresh plan that no longer showed them. The
  message now counts only the unconverged operations and names the first five with action, path and
  cause, for example `final reconciliation did not converge on 1 in-scope path(s): upload
  docs/a.pdf (in the first scan, unchanged since, and the NAS still differs)`. The first path is the
  result record's `error.path` and the `run.failed` event's `path`, and each named path is logged as
  an `entry.skipped` warning with reason `reconciliation_pending`. Exit `1` is kept for a deletion
  the run made that did not take, a write the NAS still disagrees with, a path in the first scan
  that still differs, an unresolved type conflict or rename, and a missing destination root.
- **A failed run keeps the index evidence it proved.** Before, any failure abandoned the run's index
  session, so the retry after a reconciliation failure started cold. A failed run now stores every
  proof except those for paths it planned to change on the NAS and paths reconciliation found
  unconverged. Those are dropped, their old records are removed, and the count is named in the new
  information issue `pair-index-failed-run`.
- **A run with nothing to do reports what the index did.** The result record of a no-change run now
  carries `pair_index`, so the daemon's profile state (`index_state`, `index_hits`, ...) shows the
  warm run the index exists for. Before, it showed nothing for that run.
- **A file that is always being written no longer makes every run partial.** A log, an open mailbox
  or database, or an Office lock file changes while every run works, so every run deferred it and
  exited `3`, and a realtime routine re-ran for it forever. With `--pair-index` (the package always
  passes it), a path deferred for a change during the run in three consecutive runs is named
  `file-keeps-changing` (`file_keeps_changing path=<path> runs=<n>` and the last cause): lasting, so
  the run exits `0`, still tried on every run, and cleared the first run it syncs. The count lives in
  `<profile>.redb.unsettled` beside the index; a record that cannot be used is
  `pair-index-unsettled-unusable`, and its paths stay transient.
- **No other transient cause can make every run partial forever either.** A file locked for good
  (`source-unreadable`), a checksum the NAS keeps failing (`remote-digest-unavailable`) and a deletion
  DSM keeps refusing for a reason other than permission (`delete-failed`) become, after three
  consecutive runs of the same cause, the lasting `source-stays-unreadable`,
  `remote-digest-keeps-failing` and `delete-keeps-failing` (`keeps_failing path=<path> runs=<n>
  cause=<reason>`). The run exits `0`, the path is tried every run, and the name clears when it
  succeeds.
- **Office lock and temporary files are excluded by default.** LibreOffice's `.~lock.*#`, Office's
  `~*.tmp` save files, and Access's `*.laccdb` and `*.ldb` join Office's `~$*` among the built-in
  excludes. They exist only while a document is open and change while it is. A copy already on the
  NAS is left there, and a mirror run does not delete it. A rule such as `!*.ldb` re-includes one.
- **Doctor keeps its report when its log cannot be written.** A log that could not be opened, or a failed log write (a full volume) at the start, before
  discovery, before authentication, at the end, or at shutdown, used to replace the whole report with
  one line on standard error, which the package's UI showed as every step skipped. The sections
  already evaluated keep their verdicts. Every other section says `not run: the diagnostic log failed
  (<cause>)`. The run still fails, by name.
- **Doctor explains every section a failure stopped.** A section listed before the failed one but
  run after it (for example the session checks after a failed File Station capability check) now
  says `not run because <section> failed`, not `not reached`.

### Scheduler observability (fix8)

A NAS went 54 hours without a sync and without one Activity row. Every state in which nothing was
observing a routine wrote controller.log at most, so the Activity page read as healthy.

- **Nothing observing a routine is now one Activity row per episode, in both engines.** `routine.unobserved`
  (`warn`; `info` for a disabled routine) names the cause: a non-ok backend, `poll_floor_failed`, the walk
  cap, a lasting block, a watcher that never reported, a poll that cannot read its tree or was killed, or a
  disabled routine. A reading must hold 30 seconds before it is announced (`SDSYNC_DSM_OBSERVE_GRACE` in the
  shell engine), so a watcher that is still starting says nothing; a disabled routine and a recovery are said
  at once. `routine.observed` closes the episode. The episode is kept in the routine's state
  record, so a restart does not repeat it. Both codes are in the shell and daemon Activity vocabularies.
- **The routine state record can tell idle from blind.** Six keys follow `escalated_epoch`:
  `last_trigger_epoch`, `last_run_epoch`, `watching_via`, `observe_state`, `observe_cause` and
  `observe_since_epoch`. `backend` and `backend_reason` are unchanged. A record without them reads as
  observed, and `ctl status` carries the same fields.
- **The shell poll walk is bounded** (900 seconds, `SDSYNC_DSM_POLL_WALK_BUDGET` overrides it). A walk that
  outlives it is killed with its `find`, `poll_walk_timeout` is logged with the profile and seconds, its slot
  is freed and the episode is one Activity row. Walks whose `find` survives SIGKILL on a dead mount are
  counted, and no new walk starts while three are still alive.
- **The shell stall check covers dashboard jobs.** A primary job running longer than 30 minutes
  (`SDSYNC_DSM_CONSUMER_STALL_SECONDS`) is named once with `controller_blocked_by_stalled_consumer` and an
  Activity row for `all` profiles, as a run with no heartbeat is; the end of either is a `routine.observed` row.
- **A native event is marked again when its run is refused at admission** (exit 75), so the retry no longer
  depends on the poll finding the change. A marker that cannot be written is named
  (`routine_event_remark_failed`) and makes the poll due at once.
- **`notification.unavailable` says why and is one row per failure episode.** Its message ends
  `(cause=notifier_missing)` or `(cause=notifier_exit_N)`, and the alert cooldown starts on the attempt, not on a
  delivery, so a notifier that always fails no longer adds a row to every failed run.
- **`module.succeeded` is `debug`.** It repeated `audit.succeeded` for every run; `module.failed` and the
  `audit.*` rows are unchanged.

### Dashboard bridge fixes from the NAS field report

- **Doctor over many profiles reports again.** The package keeps up to 320 KiB of Doctor output, but
  the bridge refused any result output over 64 KiB, so a Doctor over three or four profiles showed
  every step as skipped and "The operation could not be started". The bridge now accepts what the
  package carries; past that the result is `response_too_large`, never "could not be started". A
  result the bridge refuses names the check (`result_shape_invalid`, `result_output_too_large`, ...)
  in its message and in a `consume_failed` record in `api.log`.
- **A read-only Doctor runs beside a sync.** It was refused while a shell-runner run held the run
  lock, with a message about changing profiles. Only `doctor --write-test` is still refused there,
  in its own words.
- **A Doctor with no report says why on every step**: `Not run: Doctor produced no report. Cause:
  <code, exit code>: <first line>`, closed by a **Why Doctor produced no report** row.
- **A refused dashboard change names its reason.** Its Activity row ends `reason=<code>` and, when the
  request named one, `request_id=<id>` (the ID the error message shows), and reads the reason in
  words; the error message is worded by the reason instead of "Request could not be completed". See
  [A dashboard change was refused](dsm/troubleshooting.md#a-dashboard-change-was-refused-rejected-post).
  Conflicting or queue-full refusals (409) are now also in `api.log`.
- **An open page renews its mutation token before it expires**, and when its tab becomes visible
  again, so the first change after five idle minutes is no longer refused. A replay after a refusal
  the page could not read carries a fresh token instead of repeating the refused one.
- **A page left open across an upgrade is told to reload.** Every request carries `X-SDSYNC-Build`;
  a change from another build is refused as `ui_build_stale` with that advice. Pages loaded before
  this release send no build and are refused the same way: reload them once after upgrading.
- **Activity folds each run into one row** with its outcome and duration; warnings and errors are
  always drawn, each fold opens, and **Show every event** draws every row as recorded.
- **Routines say what observes them**: idle and watching via native events, polling or the schedule,
  partly observed, or blind since a time with the cause. Blind and partly observed routines are amber
  Overview items.
- **A dashboard request can no longer hang the NAS's web server for most of an hour.** On DSM 7 each
  request's CGI built its own TLS client for DSM's session check, and on a 3.10 kernel with a drained
  entropy pool that client waited forever for randomness. The session check now runs in the package's
  long-lived service with one client, and every CGI request is answered within 12 seconds (40 for a
  change), at worst with a named `cgi_deadline_exceeded` that is recorded in `api.log`. See
  [A dashboard request timed out](dsm/troubleshooting.md#a-dashboard-request-timed-out-cgi_deadline_exceeded).
- **`config_file_unsafe` names the file and the check** (owner, mode, links, ...) on the page and in
  `api.log`, a read that raced a writer is looked at again, and the page stops refreshing only after
  three in a row.
- **A slow read says the NAS is slow** ("did not answer within 10 seconds") instead of "confirm the
  package is running", and no longer locks edits.
- **The "Every second" status refresh is gone**; a saved one becomes 5 seconds.
- **A damaged checksum index reads in plain words** (files were still compared in full; nothing to do),
  with the library's message under **Technical detail**; a long detail keeps its file name. An index
  this system cannot keep (`pair-index-unkeepable`) says so and when it is tried again.
- **A file that keeps failing or changing names the file** and how many runs in a row.

### A drained entropy pool no longer hangs a run

On DSM 7 kernels (3.10) there is no `getrandom(2)`: the first random draw in a process waits for the
kernel's entropy pool, with no end on a drained one, inside the first TLS client construction that
no request deadline covers. A run, or a daemon job, could hang with no record. The first draw is now
made on a helper thread and waited for at most 15 seconds; on expiry the run fails by name,
`entropy_unavailable`, with the bound and the cause in the run record, the log (`run.failed` reason
`entropy_unavailable`), the run state and the dashboard. The next run asks again. The random source
itself is unchanged.

## Read this before upgrading

- **Rust 1.90 or newer is needed to build from source** (was 1.88). The verified-pair index uses redb
  4.4, which reports a damaged file as an error instead of crashing, and needs 1.90.
- **Post-run reconciliation no longer re-reads the whole tree.** It still re-lists both sides in full,
  but verifies live only the pairs the run touched plus a canary of 25; untouched pairs whose facts
  did not move keep the plan pass's digests. A run that changed two files in 1,000 now asks the NAS for
  27 checksums during reconciliation instead of 1,000. The result's new `reconciliation` object counts
  both; `--reconcile full` restores the old pass. See
  [Comparison, exclusions, and deletion](configuration/safety.md).
- **A file whose permissions or attributes change while it is being read is deferred.** The checks
  that bracket a read now include the status-change time, so a `chmod`, `chown` or extended-attribute
  change during the read defers that file to the next run (`source-changed`) instead of uploading a
  digest taken across the change.
- **A tree too large for the memory a run may use is refused by name.** The run estimates its peak
  memory after the local walk and again after the listing, against the cgroup limit and
  `MemAvailable`. Over budget, it syncs nothing, exits `0` and reports one lasting deferral of `.`
  with reason `tree-too-large-for-memory`, naming the entry counts, the estimate and the fix (split
  the profile). On a DSM package a core killed with exit `137` is now recorded as `core_oom_killed`
  (the cgroup's OOM counter moved) or `core_killed`, instead of `core_failed`.
- **`--pair-index DIR` is new** (opt-in on the command line, Unix only; on by default for DSM daemon
  runs). See [Verified-pair index](pair-index.md).

- **Excluded files that already leaked to the NAS stay there; delete them by hand.** Exclude matching
  now covers cases an earlier release missed (a composed `é` rule against a decomposed `é` folder, a
  renamed folder's other spelling). Content that an exclude newly covers is left on the NAS exactly as
  it is. For example, a `passwords.txt` that an earlier release uploaded into a folder your rule was
  meant to exclude is **not** removed by this release or by a later `--delete`, because an excluded
  path is never deleted. Find such files in File Station and delete them yourself.
- **Every realtime routine on a DSM package syncs once after the upgrade.** The polling fingerprint
  changed format (`v3`), so a baseline written by an earlier release is unknown. The controller logs
  `routine_fingerprint_format_changed` for each routine and runs one ordinary comparison sync.
- **Two profiles with overlapping NAS destinations, where either deletes, now stop their own runs.** The
  runner validates the configuration before each run, scoped to the profile being run, and such a pair
  is a configuration error (exit `2`). A pair that already exists makes the runs of those two profiles
  fail as `config_invalid`, with both profile names in the detail (other profiles are unaffected), until you give the profiles separate destinations or turn deletion off. The
  dashboard and `configure-profile` refuse to create the pair (exit `64`).
- **A scoped run through a NAS link now fails.** A `--scope` (also a profile scope or a `resync` scope)
  that is, or lies beneath, a NAS symbolic link used to follow the link; it now exits `1` before
  anything is listed. Run without the scope, or replace the link with a folder.
- **The `--status-cache` directory must be dedicated to the cache.** A directory holding other files is
  refused with the warning `cache-directory-unusable`, and the query runs uncached. A directory that
  held only cache files is adopted and marked automatically.
- **Rust library users:** `plan::DeleteAction` has a new public field, `local_relative`. Code that
  constructs one must fill it. The SDK and FFI results gain `link_guard_unchecked`.
- **DSM realtime routines watch natively without inotify-tools.** DSM ships no `inotifywait`, so
  until now every realtime routine on a stock NAS was only polled (`backend_reason=watch_unavailable`).
  The package now brings its own watcher, `sdsync-dsm-api --watch`; routines report `backend=inotify`
  after the upgrade. It skips the DSM-managed folders and whatever the profile excludes, and it also
  reports permission changes and bare modification-time changes. A source with more directories than
  80% of the per-user `fs.inotify.max_user_watches` is refused by name (`watch_limit`, with the
  numbers) and polled; only root can raise that limit, which is optional (see
  [Raising the inotify watch limit](dsm/troubleshooting.md#raising-the-inotify-watch-limit)). The
  watch budget is 80% of that limit minus what the package user already holds. An `inotifywait` is now
  only a fallback, used if the package binary fails its executable checks. (Under the daemon engine
  the watcher runs inside the daemon and `inotifywait` is not used.) The setup walk has a
  120-second deadline that names the directory it was stuck on, and a kernel event overflow triggers a
  full re-walk. Under the shell engine, a watcher the controller has just spawned is no longer
  mistaken for a dead one before it has started: it is not counted as a failure, a package stop stops
  it, and one that has not started 30 seconds after its spawn is stopped and named
  (`routine_watch_failed`, `start_timeout`). A realtime routine that is not watched natively is now an amber item on the Overview.
  The watcher uses about 6 MiB at 20,000 directories and about 16 MiB at 100,000.
  `sdsync-dsm-api --watch-probe <folder>`, run as the package user, shows what inotify sees on a share;
  see [Troubleshooting](dsm/troubleshooting.md#probe-what-inotify-sees-on-a-share) and probe 20 in the
  [acceptance runbook](production-acceptance.md#questions-only-a-real-nas-can-answer). CI gained a
  `native-watcher` job.
- **DSM:** Package Center offers Stop and Run, not Restart. Every message that said "Restart" now says
  to stop the package and run it again.

## Safety and correctness

- **Excludes** match Unicode-normalised (NFC) on every source, with case folding only where the source
  folds, and apply to both spellings of a renamed or normalisation-paired folder. A negation can never
  make a protected NAS entry deletable. The probe that decides whether a source folds case now asks
  inside the root first, so a USB exFAT or CIFS mount beneath a case-sensitive parent is detected as
  folding.
- **Negated excludes under `--delete` on Windows and macOS (any case- or normalisation-folding source).**
  A NAS-only name is now protected from deletion if the patterns as typed (case-sensitive, as spelled)
  exclude it, as well as when the folded rules do. With `*.txt` and `!Report.txt` a NAS-only `report.txt`
  used to be deleted on Windows, and with `*.txt` and `!café.txt` (composed) a NAS-only decomposed
  `café.txt` on macOS; both are now kept and reported as protected. Uploads and walks are unchanged.
  If you relied on a negation to delete such a name, delete it by hand in File Station.
- **A deletion under a renamed or normalisation-paired folder** is checked against the spelling the
  plan recorded, and a file recreated under a decomposed paired folder keeps its NAS copy. A kept
  deletion under a case-renamed folder is uploaded in the same run.
- **A skipped spelling names its case-variant sibling** as a `local-case-collision` instead of
  silently shielding it.
- **Overlapping profiles** are checked by single-profile runs too: exit `2` (scoped to the pair) when
  either deletes; a warning, and a `warnings` array in `config validate`, when neither does.
- **NAS folders this account may not list** (DSM `105` on a live session, or `407`) are a lasting
  `remote-skipped` warning with exit `0`; local content beneath them is `remote-unreadable` and never
  uploaded. Three or more such folders that are more than half of those found fail the run by name.
- **A NAS folder deleted during the walk** is the transient `remote-changed` deferral (exit `3`); a
  folder whose listing changes on every read is an incomplete listing (exit `0`, amber, nothing deleted
  in it). Neither fails the run.
- **Comma paths:** a deletion or rename of a path whose text before the first comma names an existing
  entry is withheld (`remote-skipped`), and a server copy of one falls back to an upload, until
  [probe 16](production-acceptance.md#questions-only-a-real-nas-can-answer) shows how DSM parses
  commas.
- **Server-side copy:** a copy that never started, or that left the destination empty, is uploaded in
  the same run (`recovered.server_copy_fallbacks`). Only a copy that left an entry behind is the
  transient `server-copy-failed`, and the next run replaces that entry.
- **Breakers.** The refusal breaker counts by class (permission, name, size, clash, folder-limit,
  create-failed), counts the files already in sync, settles each write once, can trip before any upload
  and never runs in `plan`. On a large share that is already in sync a new share-wide refusal can read
  amber with every path named rather than red. The link-shape breaker now judges files and folders
  separately.
- **`link_guard_unchecked`** reports the folders DSM listed without a real path, so symbolic links in
  them could not be checked. It is a lasting warning in the result, the plan, the NDJSON plan summary,
  the `remote_scan.link_guard_unchecked` event, the SDK/FFI and the DSM dashboard.
- **A real path that differs from its listing only in letter case** is treated as a link.
- Status rows beneath a NAS link or mount point read `excluded`, not `missing-remote`.
- A share-wide refusal failure names at most ten paths and then "and N more"; every refused path
  stays in `result.deferred`.
- With `--jobs` above 1, a tripped refusal breaker stops new uploads on every worker at once: the run
  sends at most the refusals that trip it plus one request per other worker, however slow the
  progress output is. Before, a stalled reader of that output could let workers send most of the queue
  to a share already known to refuse it.

## Transfers and the network

- **`--timeout` has a new meaning for uploads.** It is the no-progress window (at most 300 seconds) and
  the wait for DSM's answer after the last byte (plus a drain allowance of at most 1024 seconds), not a
  total deadline; a transfer that keeps moving is
  not cut by it, but one attempt must average at least 16 KiB/s over the file's size. See
  [Timeouts](configuration/network.md#what-timeout-means-for-an-upload). The control-plane cap and the
  background-operation timeout are unchanged.
- **Retries.** Login is retried on connection failures and HTTP 502/503/504; reads, and uploads, are
  also retried when the connection closes after the request; the reads that verify or guard a mutation
  are retried. A `Delete`, `Rename` or copy start is still sent once. A torn overwrite gets one more
  full attempt in the same run.
- **Re-login is capped** at 4 per rolling 10 minutes plus one per 15 minutes of run time; when spent,
  the run fails with the original session error plus "DSM ended N sessions in M minutes, so
  re-authentication stopped". A re-login mid-upload no longer breaks the attempt numbering.
- Proxy answers 301-308, 413, 502 and 504 are named with a hint for every API; the copy poll ramps from
  50 ms to 2 s; DSM codes 599 and 1000-1007 are described.
- `HttpBody` errors read "HTTP transfer failed during ..." instead of "HTTP response body failed
  during ...".
- New direct dependencies `bytes` and `http-body` (already in the lock through `reqwest`); the About
  table and notices list them.

## Status cache

- The cache directory is claimed with a `.sdsync-status-cache` marker; eviction touches only
  cache-named files. See [Status digest cache](status-cache.md#size-and-where-it-lives).

## Observability

- New event `remote_scan.link_guard_unchecked`; new reasons `remote_refused_share_wide` and
  `remote_real_path_unusable`; `link_guard_unchecked` is a required field of the result, the plan and
  the NDJSON plan summary (see `observability.schema.json`).
- `remote_events_dropped` now also counts the events a delivery worker discarded when it exited.

## DSM package

- **Saving the performance level works again.** `sdsync-dsm configure-performance` (and the dashboard's
  performance setting) failed every time with exit `73`, `mandatory audit outbox could not be
  prepared`, before it changed anything: the bridge did not accept the `performance-policy` audit
  operation the manager records. It now does, filed under the configuration category, and a test checks
  that the bridge accepts every operation the manager can audit.
- **The realtime watcher honours a profile's excludes.** The watcher read `excludes` from the top of the
  profile fragment, but the manager writes the profile as a `[profiles.<name>]` table, so it found none
  and a change to an excluded path raised an event and started a run that had nothing to do. It reads
  the profile's own table now (a fragment holding two profiles is refused by name, not guessed at).
- **`sdsync-dsm resync` runs again.** The previous release shipped it unable to start: every resync,
  a plan or a `--confirm`, from the command line or from the dashboard, stopped with `mandatory audit
  execution phase could not be recorded` (exit `64`) before the engine ran, and nothing was uploaded.
  A resync now writes the mandatory audit record like every other mutating command: operation
  `resync`, the profile it addressed (`all` only when neither a name nor a default could be resolved,
  which the command then refuses), and a terminal state of `succeeded` or `failed`. A test now fails the build
  if a command calls `begin_mutation` without being on the audited list, or the reverse.
- **Dashboard.** A top-bar chip counts the current problems on every page; failure toasts stay until
  dismissed; a stopped, untrusted or silent controller is an attention item (and a wedged controller no
  longer reads "All profiles healthy" in the widget); before the first read the cards say "Not read";
  actions that change something are disabled while the status shown is stale; a second full sync warns
  that one is already running (**Queue another**); the AppWindow never polls slower than every 60
  seconds, and a successful Activity or Logs read refreshes a stale status at once; Activity draws the
  newest 200 events with **Show older**; evidence ages use the NAS clock and absolute times add the NAS
  time when the offsets differ; a render error clears its own banner; a window detached from the page
  stops polling.
- **Logs.** Lines are redacted whole before they are shortened; a line over 32 KiB reads `[line too
  long: N bytes, withheld]` and one that cannot be redacted reads `[line withheld: could not be
  redacted]`. A read has a four-second deadline (`deadline_reached`, `logs_read_deadline`).
- **Named 503 codes:** `shell_snapshot_building` and `control_lock_busy` (see the
  [code table](dsm/troubleshooting.md#dashboard-status-is-stale-or-a-read-reports-a-named-in-service-failure)).
  The source browser is bounded to six seconds (`time_limited`). The status snapshot carries `nas_clock`.
- **Runs.** The stall heartbeat now follows `rchar + wchar` growth of at least 16 KiB a minute (lowered
  for a throttled `max-rate`); a liveness-only heartbeat (hidden `/proc/<pid>/io`) has no epoch and reads
  `stalled: null` plus the warning `stall_unobservable`, not healthy, and logs
  `run_stall_detection_unavailable` if the counters vanish mid-run; a run record dated in the future reads as unknown (`clock_skew`); a cancellation is an
  Activity `run.cancelled` row, not `run.failed`; a core killed with exit `137` is named (and the OOM
  killer when its counter moved: `core_killed`); a refused `config validate` records its reason.
- **Controller.** A controller that died under a live API service is detected by `status`
  (`controller_found_dead`) and repaired by Start. Torn lock-owner recovery can no longer delete a
  record published while it was judged. The inotify watch limit is read whole and the directory count
  is bounded, cached and off the controller turn. Realtime polls no longer hold full turns while a walk
  runs. Reading a busy process's identity from `/proc` is a single snapshot, so a loaded NAS cannot make
  a live controller look unprovable (exit `73` now names its cause).
- **Routine windows** belong to the day they start on (the weekday filter applies to that day), run once
  across a daylight-saving change, and a window inside a spring-forward gap opens after it and logs
  `routine_window_dst_gap`.
- **Realtime fingerprint (`v3`)** also sees an empty folder created, removed or renamed in the source.
- The API socket is bound without changing the process umask, which removes a rare mode-`000` file in
  another thread.
- **CI** runs the package suite under dash and under bash as `/bin/sh`, each in three weight-balanced shards
  (the `packaging-suite` job).
- **A killed daemon no longer leaves the dashboard amber.** After `kill -9` the daemon could not clear the
  `degraded=` block it had written to `controller.state`, so the status snapshot kept serving
  `controller.degraded` for a service that was not there. It is served only while the service the same
  snapshot reports is `running` (a malformed block is still refused, live or not).
- **`config_unreadable` forgets what nothing reads.** A dependency's profile state, and a removed
  profile's fragment and state, used to stay listed until the daemon restarted; each now leaves the list
  with a `config_forgotten` line (path and reason). A changed cause of an unreadable control directory
  is named again and published (`was=` the earlier one).
- **The event-log lock says why it was cleared**: the pid is gone, the pid was reused by another process,
  or the record is from an earlier boot (it said "gone" for all three). A stale record is removed only
  when its contents, not just its inode number, are the ones judged. Contention past a minute retries
  once a minute, as the failure line says, and a failure that is not contention ends the count.
- **`--daemon-lock-probe` writes one line**: control characters in the path it names are escaped.
- **Checksum-index messages fit their cases.** `pair-index-unsafe` and `pair-index-cleanup-failed` no
  longer describe only their commonest cause, a failed weekly sweep no longer calls an unreadable age
  "older than 7 days", and a failed compaction or an index not served this run (`pair-index-compaction-failed`,
  `pair-index-link-guard-unchecked`) are now said on the profile's index line instead of being dropped.
- **The control socket cannot be starved by a client that never sends a request.** When 128 connections are
  waiting to send their request, the oldest is dropped (told `ctl_busy`, logged once per episode as
  `ctl_unread_evicted`) instead of the new one being refused, so `ping` and `stop` are served during a
  reconnect flood. `stop` has two slots of its own; waiting stops no longer fill the slots `ping` and `status`
  use. Connections that send nothing are logged once per episode (`ctl_empty_requests_ended` carries the count).
- **Backend reports are logged once per cause.** `routine_backend` no longer writes a line per reading for a
  polling tree whose skipped entries change every time; the report that ends the episode carries
  `suppressed_reports=N`. `backend_detail` keeps the end of a long detail (`next_walk_deadline_seconds=N`).
- **A busy walk cap is not an unwalkable tree.** While the process-wide walk cap refuses a routine's arm it
  reports `backend=none` (nothing watches or polls it), a second episode is reported, and the start-up
  catch-up waits its turn (`catch_up=deferred`, staggered by routine) instead of syncing the whole tree.

## Questions only a real NAS can answer

Five probes were added (15 to 19) and the torn-overwrite probe (2) now describes the one-more-attempt
repair: see [the list](production-acceptance.md#questions-only-a-real-nas-can-answer).
