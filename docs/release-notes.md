# Release notes: behaviour changes

This page lists changes you can see or must act on. Releases use calendar tags (`YY.N`); the
changes below are in the first release built after the audit fix round documented in
[Comparison, exclusions, and deletion](configuration/safety.md) and the
[observability reference](observability.md). Older notes are not kept here: each release's GitHub page
lists its own commits.

## Read this before upgrading

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
  only a fallback, used if the package binary fails its executable checks. The setup walk has a
  120-second deadline that names the directory it was stuck on, and a kernel event overflow triggers a
  full re-walk. A realtime routine that is not watched natively is now an amber item on the Overview.
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

## Questions only a real NAS can answer

Five probes were added (15 to 19) and the torn-overwrite probe (2) now describes the one-more-attempt
repair: see [the list](production-acceptance.md#questions-only-a-real-nas-can-answer).
