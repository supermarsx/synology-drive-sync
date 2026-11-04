# Verified-pair index

A content comparison (`--compare content`, the default) proves each same-size pair equal with two
MD5s: one read of the local file, and one File Station `MD5` task that makes the NAS read its copy.
In the steady state that is the whole share on both sides, on every run. On a NAS each `MD5` task is
a start plus at least one status poll, so a share of 100,000 unchanged files costs about 200,000
requests and two whole-share reads before anything is known to be unchanged.

`--pair-index DIR` keeps what earlier runs proved, so a run where nothing changed costs the listing.
On the command line it is opt-in. On a DSM package it is on by default: the daemon engine passes
`--pair-index var/state/cache/pairs` to every run (see [On a DSM package](#on-a-dsm-package)).

```bash
synology-drive-sync sync --profile photos --pair-index /var/packages/synology-drive-sync/var/state/cache/pairs
```

Measured on the in-tree File Station mock with 3,000 files in 173 folders (`tests/scale_requests.rs`):

| Run | Requests | `MD5.start` |
| --- | ---: | ---: |
| first run, nothing changed (fills the index) | 6,182 | 3,000 |
| second run, nothing changed | 232 | 25 (the canary) |
| two files missing on the NAS | 472 | 54 |

The 100,000-file measurement on a patched mock went from 205,725 requests and 40.6 s to 5,725
requests and 7.4 s. On a real NAS the remaining cost is the listing.

## What is stored, and when it is used

After a pair is proven equal with two live digests, the index stores the facts both sides had at
that moment with the digest:

- locally: size, modification time and status-change time (`ctime`) in nanoseconds, device and
  inode;
- on the NAS: size, modification time, change time and creation time from the listing;
- the MD5, the local clock when the read began, and the NAS clock (`Date` of the `MD5.start`).

A later run takes the stored digest for a pair only when **every** fact is unchanged on both sides,
the pair is still paired under the same spellings, the index is bound to this profile, source,
remote root and NAS, the evidence is not racy and not expired, and the run's link guard ran
everywhere. Anything else, and the pair is verified live as before.

The local status-change time is what makes this safe. A rewrite in place that restores size and
modification time afterwards (`touch -r`, `cp -p`, `rsync --inplace -t`) still moves `ctime`, because
`write(2)` and `utimes(2)` both set it to the current time and nothing unprivileged can set it back.
On the NAS side, the change time does the same for a second writer (a Drive client, `rsync -t`, a
restore) that keeps size and mtime.

## What it can and cannot do

- **It can only suppress an upload.** A stored digest is MD5-only, so no deletion guard and no
  server-side copy can use it (both need a full fingerprint read from the bytes), and a pair that
  differed is never stored. The worst a wrong record can do is skip an upload of a pair whose every
  fact held.
- **The canary.** Every run re-verifies `--pair-index-canary` stored pairs live (default 25). If one
  disagrees, the whole index is discarded, every pair is verified live in the same run, and the
  disagreement is named (`pair-index-canary-mismatch`, with the path and digest prefixes).
- **Rolling expiry.** Evidence older than `--pair-index-max-age` days (default 30) is re-verified,
  spread across the second half of that window rather than all at once.
- **Racy evidence is never served.** A pair observed within the racy window of its last change, on
  either side, is stored but verified live next time. The window is what keeps a write through a
  shared memory mapping (a database, a VM image) safe: Linux moves a file's mtime and ctime only
  when such a write first dirties a clean page, not on later writes into the page before
  writeback cleans it, so for a while the content can change with every fact unchanged. Locally
  the window is `dirty_expire_centisecs` plus `dirty_writeback_centisecs` from `/proc/sys/vm`
  plus 25 s, never under 2 s: 60 s with the kernel defaults (30 s + 5 s). If those cannot be read
  it is 60 s, named `pair-index-writeback-unknown`; with periodic writeback switched off
  (`dirty_writeback_centisecs` = 0) nothing bounds it, so no local evidence is served
  (`pair-index-writeback-unbounded`). On macOS, which has no such setting to read, it is 60 s.
  The NAS side uses 60 s too: DSM runs the kernel defaults, which cannot be read from the client.
  Its clock is the `Date` header of the `MD5.start` that read the copy; one more than 20 s outside
  this host's clock during the pass (a proxy's clock, or a NAS with the wrong time) cannot judge
  the window, so the pair is stored racy and the run names `pair-index-nas-clock-skewed` once.
  The windows are applied again when a record is about to be served, so evidence settled under a
  shorter window is not served on its word. A file changed within the window of a run is read
  live once more on the next run, after which it is served as before.
- **Local-only mode.** If the NAS listing carries no change time, or one of the run's own uploads is
  listed with a change time that does not show the write, File Station's MD5 always runs and only the
  local read is skipped (`pair-index-remote-ctime-unavailable`, `pair-index-remote-ctime-static`).

What remains is the class no metadata scheme can see: raw block writes, a clock set back to an exact
nanosecond, local bit rot. The canary and rolling expiry bound it.

## When it cannot be used

Every case is a named warning in the result's `pair_index.issues`, a `pair_index.*` event in the log,
and a fully live run. None fails a run or makes it partial.

| Code | Meaning |
| --- | --- |
| `pair-index-locked` | Another process held the file for more than 30 s. |
| `pair-index-readonly` | The file or its volume does not let this account write. |
| `pair-index-corrupt` | The file failed its integrity check (on the run's first open, or on a reopen that found it changed), redb repaired damage in it, a stored record, path or `meta` value does not decode, or a previous run stopped inside redb on it (`<profile>.redb.panicked`). The file is set aside as `<profile>.redb.corrupt-<epoch>` (one copy, swept after 7 days; earlier copies that cannot be removed are named `pair-index-cleanup-failed`, and with three of them the damaged file is deleted instead of kept), nothing from it is served, and the index is rebuilt from this run. |
| `pair-index-format-unsupported` | The file is in a format this build's redb does not read; set aside and rebuilt. |
| `pair-index-store-failed` | The end-of-run write failed (a full volume); the previous evidence stays valid. |
| `pair-index-unsafe` | The file is a link, not a regular file, owned by someone else, or shared with group or other. |
| `pair-index-directory-unusable` | The directory is not private, or holds files the index did not write. |
| `pair-index-unbound` | File Station did not name the host serving the run, which the index binds to. |
| `pair-index-writeback-unknown` | `/proc/sys/vm/dirty_expire_centisecs` or `dirty_writeback_centisecs` could not be read (path and cause named); the local racy window is 60 s. |
| `pair-index-writeback-unbounded` | Periodic writeback is switched off (`dirty_writeback_centisecs` = 0), so no window bounds a mapped write: nothing local is served and every pair is verified live. |
| `pair-index-cleanup-failed` | A file the index set aside or no longer needs (a quarantined copy, a panic record) could not be removed; the path and cause are named. |
| `pair-index-nas-clock-skewed` | The NAS clock (`Date` of an `MD5.start`) stood more than 20 s outside this host's clock while the pass read; evidence it proved is stored racy and verified live next run. Fix the NAS's (or a proxy's) time. |
| `pair-index-retired` (info) | The index was bound to another profile, source, remote root or NAS, or an older comparison semantics; it was cleared. Semantics epoch 1 (the two-second racy window) is retired by name. |
| `pair-index-not-applicable` (info) | The run does not compare content, so it neither reads nor writes the index. |
| `pair-index-unsupported-platform` (info) | Windows: a directory walk exposes no status-change time or file identity, so the index is never used there. |

The index is a cache: deleting the directory is always safe and only costs one full run.

## Where it lives

One file per profile, `DIR/<profile>.redb` (redb 4.4), mode 0600, in a 0700 directory claimed by a
`.sdsync-pair-index` marker. A profile name longer than 64 characters cannot be a file name here,
so its files are named by the first 48 characters, a hyphen and the first 16 hex digits of the
name's SHA-256 (`printf '%s' "$name" | sha256sum | cut -c1-16`): `DIR/<first 48>-<hash>.redb`.
Below, `<profile>` stands for that name where it applies. A directory holding anything else is refused. The file is held only in
two short windows: after the listing (one read pass) and at the end of the run (one write
transaction), so a dashboard read is never queued behind a sync. The first open of every run runs
redb's `check_integrity()` before any read. A reopen later in the same run skips it only when the
file is exactly as this process closed it: same device, inode, length, modification and change
time, and the same redb header, which every commit by any writer rewrites. Anything else is
verified again. This keeps one full read of the file per run instead of four or five. An
interrupted write leaves the last complete commit.

redb 4.4 can panic while it opens a file with a damaged allocator-state page, before any check
can run; a release build stops there. A panic inside redb on an index file is recorded beside it
as `<profile>.redb.panicked`, and the next run sets the file aside instead of opening it, so one
crashed run heals the index.

Size is about 340 bytes per pair at 100,000 pairs and 270 at a million; redb's page cache is held at
8 MiB, so memory does not grow with the share. When the file grows past twice its contents plus
64 MiB, it is compacted (`pair-index-compacted`).

## On a DSM package

The daemon engine (the default; see [Service engine](dsm/operations.md#service-engine)) runs every
sync with `--pair-index /var/packages/synology-drive-sync/var/state/cache/pairs`, one
`<profile>.redb` per profile. The shell engine does not pass it.

- **Status.** The profile's `index:` entry in `sdsync-dsm-api --ctl status`, and the dashboard's
  `Checksum index:` note on the profile's **Last run:** line, show the state, `entries`, `hits`,
  `misses`, the canary checked and mismatched, whether it was refreshed, and the worst issue of the
  run with its path and cause. `entries` is the result's `pair_index.entries`. A read of the index
  while a run of that profile uses it says `busy` instead of opening it.
- **A damaged file** is named `pair-index-corrupt` (an amber item on the dashboard), set aside as
  `<profile>.redb.corrupt-<epoch>` and rebuilt from the run.
- **A retired index** (the profile was re-created with another source, remote or NAS) is named
  `pair-index-retired` and shown as information on the profile's line, because it explains why the
  first run afterwards is slow.
- **Removing the profile** deletes its `.redb`, the quarantined copies, a panic record, its lasting
  block and its alerts state. A file that cannot be removed is named as a warning.

## Reconciliation

Independently of the index, post-run reconciliation re-lists both sides in full and verifies live
only the pairs the run touched, plus a canary of 25 untouched pairs. Every other pair whose facts
did not move keeps the digests the plan pass proved, provided the NAS listing carries a change
time for it and neither side's change time was within the racy window of the read that produced
the digest (the run's start locally, the `MD5.start` clock on the NAS); otherwise it is verified
live. The result's `reconciliation` object says how
many pairs were carried and verified live. `--reconcile full` (hidden, command line only) restores
the exhaustive pass.
