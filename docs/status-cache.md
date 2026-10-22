# Status digest cache

`status` in content-comparison mode spends nearly all of its time obtaining two digests per file:
the local MD5, which costs reading every byte, and File Station's server-side MD5, which costs a
task start plus at least one status poll — two serial round trips per file. In the steady state both
answers are the same as last time.

`--status-cache DIR` stores those digests so an unchanged file need not be re-read and re-hashed.

```bash
synology-drive-sync status --profile photos --status-cache /var/packages/synology-drive-sync/var/state/cache/status
```

The DSM package passes this automatically; there is nothing to enable in the dashboard.

## It is a cache, not a record

**Deleting the directory is always safe and only ever costs time.** Every guarantee the package
makes holds with the cache absent, stale, truncated, or corrupt, because all of those degrade to
exactly one behaviour: recompute from live evidence.

If you ever want to be certain of an answer, delete the directory or pass `--status-cache-refresh`.

## What it stores

One record per unchanged file, holding the two MD5s and the metadata they were observed under:

| | |
| --- | --- |
| Local | size, modification time in milliseconds, device and inode, MD5 |
| Remote | modification time in seconds — all File Station reports — and MD5 |
| Both | when the pair was observed |

It stores **evidence, not verdicts**. There is no "this file is in sync" anywhere in it; the verdict
is derived on every query from the digests, by the same comparison the uncached path uses.

## What it deliberately does not store

- **CRC32 and SHA-256.** The format has no field that could hold one. This is what keeps deletion
  and server-copy safe: both require a strong content match, so a cache entry is structurally
  incapable of satisfying their guard. Wiring one in by mistake produces a *refused* mutation, not
  an unverified one.
- **Secrets**, of any kind. No status query takes secret material as an input to a digest.
- **The local scan or the File Station listing.** Both trees are walked live on every single query,
  cached or not.
- **Anything about a pair whose digests disagreed.** An entry means "these two MD5s were observed
  equal under this metadata", and nothing else.

## When an entry stops being used

Any of these makes it as if the entry were not there:

- The local file's size, modification time, or filesystem identity moved.
- The remote file's size or modification time moved.
- The evidence is older than `--status-cache-max-age` (14 days by default). The date on an entry is
  when its evidence was **first** observed, not when it was last served: a pass that re-observes
  evidence identical in every field keeps the original date, so the ceiling measures the age of the
  reading rather than the age of the poll.
- The file was modified within a second of the cache being written, **or is dated after it** — such
  an entry cannot be distinguished from one rewritten during that same tick, so it is never trusted.
  A tree restored from a backup, read over SMB from a host whose clock is ahead, or extracted from an
  archive with skewed timestamps is therefore re-hashed on every pass, which shows up as a low
  `digests_reused`.
- The package was upgraded, the profile was repointed at another source or destination, or the
  comparison mode changed. Each retires the whole file.
- The file is unreadable, truncated, out of order, missing its end marker, holds a record with a
  malformed digest, or is not a private file owned by the package. Each abandons the whole file and
  runs uncached, and each is **named**: see [Degraded states are named](#degraded-states-are-named).

## Degraded states are named

A cache that cannot be opened, read or written never gives up quietly. Every condition met while
serving or writing is a `cache.issues` entry in the `status` result (always present, empty when
healthy), and the human output prints one line per issue. Each carries a stable `code`, a `severity`
(`warning` for a fault someone may need to fix, `info` for an expected retirement), the `path`
concerned, and a `cause` that includes the operating system's error where there was one. Warning-severity
issues are also logged as warnings.

| Code | Meaning |
| --- | --- |
| `cache-profile-name-unsafe`, `cache-directory-unusable` | The cache directory or the profile's file name is refused (missing, group- or world-writable, not owned by the package); the pass runs uncached and `state` is `unusable`. |
| `cache-file-unsafe`, `cache-file-unreadable`, `cache-file-corrupt` | The profile's file is not private, cannot be read, or is damaged (the cause names the bad header field, record, or "truncated"). It is served cold. |
| `cache-file-retired` | Written by another build, query, or comparison mode. Expected after an upgrade or a repoint. |
| `cache-records-rejected` | Records were refused during a rewrite. |
| `cache-store-failed`, `cache-rollup-store-failed`, `cache-cleanup-failed`, `cache-eviction-failed`, `cache-discard-failed` | The pass computed its answer but could not write, clean up, evict, or discard cache files. |
| `cache-canary-mismatch` | The canary disagreed with live evidence; the whole cache was discarded. |
| `cache-clock-unreadable` | The system clock could not be read, so no entry's age can be judged. |
| `rollup-directory-unreadable`, `rollup-unreadable`, `rollup-corrupt`, `rollup-foreign` | A status rollup the aggregate could not use. `rollup-foreign` (info) was written by another build and refreshes on the next status pass. |

On the DSM package the dashboard reads the rollup issues from the snapshot's
`outcomes.status_cache_health` and shows "Status figures could not be read: <cause>" beside the
observed age. The rollup row also carries the profile's last-run verdict ("Last run failed" or
"Last run partial") next to the tree comparison, because a rollup is a comparison taken when someone
asked, never a verdict on the last run.

## The one thing it can get wrong

A file whose **bytes change while its size, modification time and inode all stay identical** will be
served a stale digest and shown as in sync when it is not. That is reachable, not theoretical: an
in-place `cp -p` from a same-length revision, a `touch -r` after an edit, or a filesystem rollback
will all do it.

This is a real regression in *display* accuracy against the uncached path, and it is worth stating
plainly rather than reassuring you about.

**Synchronization correctness is not affected.** No path that uploads, deletes, or server-copies
reads this cache — they are untouched code that never opens it — so the next run hashes the file
live, sees the difference, and uploads it. The failure is "the listing was late in telling you",
never "the backup missed a file".

The window is bounded four ways: the next sync run moves the remote modification time and
invalidates the entry, the age ceiling expires it, a package upgrade retires the whole file, and a
refresh discards it. **If you sync only on demand rather than on a schedule, the first of those does
not apply and the age ceiling is your only bound** — worth knowing when choosing it.

## How stale an answer is, is always reported

Every status answer carries a `cache` object, whether or not a cache was used:

```json
"cache": {
  "state": "warm",
  "entries_reused": 1843,
  "entries_verified_live": 57,
  "entries_canary_checked": 25,
  "oldest_evidence_epoch": 1757400000
}
```

`state` is `off`, `cold`, `warm`, `refreshed`, or `unusable`.

`unusable` covers both a cache that was found to be wrong and discarded, and a cache directory
that was configured but could not be opened; the second is also warned about on standard error,
because `off` means no directory was configured at all.

`oldest_evidence_epoch` is the **oldest** observation any part of the answer rests on, not the
newest, and it dates each entry from when its evidence was first seen rather than from the last pass
that served it. The newest would describe the freshest fact in the answer and imply it of all of them; the
oldest is the only claim true of every row — and it is exactly the width of the window in which an
undetected change could be hiding. Human output renders it as an age rather than a timestamp,
because how old it is *is* the point.

## The canary

An audit withholds a small sample of otherwise-usable entries (25 by default,
`--status-cache-canary`), recomputes them live, and compares. A single disagreement discards the
**entire** cache, recomputes the whole answer, and reports `state: "unusable"`.

It is not a sweep — 25 entries against a large tree would take hundreds of runs to cover it. It is a
detector for *systematic* error, which it catches almost immediately. Repairing just the offending
entry would hide exactly the case worth finding. Set `0` to disable, at the cost of a cache that can
be broadly wrong with nothing noticing.

**It runs at most once a day per profile**, and the cache header records when it last ran. A newly
built or previously distrusted cache is audited on its first warm pass, whichever pass that is.

The cadence exists because the sample is not free: each probe costs a full local read and a File
Station MD5 task, which on a warm tree is the *entire* remote digest cost of the pass. Charging that
to every pass charged it to whoever pressed Refresh — the one person waiting on the answer, and the
one least likely to be the systematically-wrong-cache case the sample exists to find. The cost of
the cadence, stated plainly: a cache that turns systematically wrong can be believed for up to a day
before the sample says so. `entries_canary_checked` is `0` on a pass that did not audit, so the
answer says which kind of pass it was.

## Size, and where it lives

Under DSM: `$SYNOPKG_PKGVAR/state/cache/status/`, one file per profile, mode `0600` inside a `0700`
directory. The cache is trusted input, so it is refused if the directory grants write access to
group or other.

Measured at the 20,000-entry ceiling with deep paths: **5.4 MB, 269 bytes per entry**. Reading
the whole of it holds about **9 KB** of memory, and writing it about **9 KB** — 17 KB when the
merge reads an existing full file back as it writes — because records are streamed one line at a
time against the inventories in both directions rather than loaded into a structure of their
own. Past the ceiling, entries observed this run are always kept and older carried-over ones are
dropped; past 32 MB across all profiles, whole profile files are removed oldest-first.

## The aggregate rollup

Alongside each cache file, `<profile>.rollup.json` holds the totals from the last unscoped pass, so
a dashboard can render the whole picture without walking anything. It is written by the pass that
computed it — it is not a sum over the cache, and nothing ever reads it back as an input.

`status-rollup` prints them, per profile and combined:

```bash
synology-drive-sync status-rollup --status-cache DIR --profiles photos,documents
```

```text
documents: 3120 in sync, 4 pending upload (3.9 KiB), 4 needing attention, observed 2 hours ago.
photos: 12481 in sync, 37 pending upload (36.1 KiB), 37 needing attention, observed 2 hours ago.
All profiles: 15601 in sync, 41 pending upload (40.0 KiB), 41 needing attention.
```

**It scans nothing.** No local walk, no File Station call, no content digest — one small file read
per profile, however large the trees are, so the work is flat in the number of profiles rather than
in what they contain. That is what makes it affordable for a dashboard or a widget polling on a short
interval.

Pass `--profiles` with the profiles you expect. A named profile with no stored totals is reported as
never observed and makes the combined total incomplete, rather than contributing zero and
understating what is pending. Without it, the result is explicitly incomplete: a total cannot be
complete over a set nobody has stated.

**When two profiles cover overlapping trees the combined total is withheld**, because the same files
would be counted twice, and `total_unavailable_reason` says so. Render that sentence rather than
omitting the row — a missing number invites the reader to add the per-profile figures themselves and
reach the same wrong answer by hand.

Two things carrying similar names are easy to confuse, so they are named precisely:

| Field | Meaning |
| --- | --- |
| `state.in_sync` | Files currently in sync. A **state**, from this observation. |
| `state.would_transfer` | Files still to transfer, with the bytes a run would actually move. A **state**. |

There is no `last_run` field here: this document holds only the two rollup **states** above, from
the last unscoped pass. The nearest **event** — files an actual run moved — lives in a different
document with a different schema: the Rust engine's `run.completed` structured-log event carries a
`files` metric (`src/observability.rs`, documented at `docs/observability.md:121`), dated to when
that run finished rather than to when this rollup was last computed. `status-rollup` does not read
it, and it is not reachable through `<profile>.rollup.json`.

The word *synced* appears in neither document, because it is the word that means both.

`observation.complete` is false when a scan budget stopped either walk. **Every count is then a
floor and must be rendered as `4,000+`, never `4,000`** — understating what is still pending is the
direction that tells someone they are caught up when they are not.

A scoped query never writes a rollup: its totals describe a subtree, and storing them as a profile
total would be a confidently wrong number.
