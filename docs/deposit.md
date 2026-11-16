# Crash-atomic deposit

A file reaches its final NAS name only once it is whole and verified, and a crash, `kill -9`,
power loss, dropped link or DSM restart at any moment leaves the old version or the new one under
that name: never a partial file, and never neither. This is the default (`deposit = "atomic"`).

## How a file is written

| Step | NAS request | What a reader of the final name sees |
| --- | --- | --- |
| 1 | `Upload` to `~sdsync-part-<owner>-<run>-<seq>` in the same folder | the old version (or nothing, for a new file) |
| 2 | `getinfo` and File Station `MD5` of the temporary copy: its length, the modification time sent, and the MD5 of the bytes that went onto the wire | unchanged |
| 3 | `getinfo` of the final name, which must still be what the plan saw | unchanged |
| 4 (new file) | `Rename` the temporary copy to the final name; DSM refuses a name that is taken, so a file someone else created meanwhile is never overwritten | the new version, whole |
| 4 (overwrite) | `Rename` the old version aside to `~sdsync-prev-<owner>-<run>-<seq>` | **absent, for one round trip** |
| 5 (overwrite) | `Rename` the temporary copy to the final name (verified with one `getinfo`) | the new version, whole |
| 6 (overwrite) | `Delete` the previous version | the new version |

The one gap is honest: DSM's File Station `Rename` refuses an existing target, so an overwrite
cannot replace the old version in one step. Between steps 4 and 5 the final name is absent for one
round trip (a few milliseconds on a LAN, about 100 ms through a reverse proxy). The bytes are never
absent: the old version exists under its `~sdsync-prev-` name until the new one holds the final
name, and if the process dies inside the gap the next run puts the old version back.

## What a crash leaves, and what the next run does

Every upload and server copy is recorded in a local journal (one `fsync` for the whole plan)
before its first byte is sent. The next run of the same profile reads it before it lists the NAS:

| Interrupted | Left on the NAS | Next run | Named |
| --- | --- | --- | --- |
| before or during the upload | perhaps a partial `~sdsync-part-` copy | deletes it, then uploads again | `deposit-temp-removed` |
| after verifying, before publishing | a whole `~sdsync-part-` copy | deletes it, then uploads again | `deposit-temp-removed` |
| between the two renames | the old version as `~sdsync-prev-`, the final name absent | renames the old version back, then uploads again | `deposit-rolled-back` |
| after publishing, before deleting the old version | the new version, and `~sdsync-prev-` | checks the final name is the new version (length, time, MD5), deletes the old one | `deposit-completed` |
| as above, but someone else wrote the final name meanwhile | their file, and `~sdsync-prev-` | keeps both | `deposit-backup-kept` (warning, every run until you remove one) |
| a server copy, mid-copy | a partial file at the final name | keeps it (someone may have written the name since), and this run uploads over it whatever its comparison says, through an atomic deposit: it is never adopted | `deposit-copy-kept` |

A run that is cancelled or fails cleans up its own temporary copies as it stops; anything it cannot
remove is named (`deposit-cleanup-failed`) and left in the journal for the next run. A server copy
that fails, times out or does not verify has its partial destination deleted at once
(`deposit-copy-partial-removed`), because the copy only ever runs to a destination that was absent.

Recovery only ever touches names this profile's journal lists. A `~sdsync-` name it does not
account for (a journal that was lost, another installation's) is named
`deposit-orphan-unjournaled` and left alone: check it and remove it by hand. The NAS walk treats
every `~sdsync-part-`, `~sdsync-prev-` and `~sdsync-stage-` name as managed, and the same names with a
leading dot (`.sdsync-part-`, ..., a spelling a development build may have written): it is never
compared, uploaded over or deleted by a mirror run. A local file with such a name is refused by name.

## The journal

One `<owner>.journal` and `<owner>.lock` per profile and destination, where the owner is twelve hex
digits of a SHA-256 over the profile, the NAS destination, the server and the account. The lock
keeps two runs of one profile from interleaving.

| Where | Directory |
| --- | --- |
| `--deposit-journal DIR` | `DIR` |
| DSM package | `var/state/deposit` in the package |
| Linux and other Unix | `$XDG_STATE_HOME/synology-drive-sync/deposit`, or `~/.local/state/synology-drive-sync/deposit` |
| macOS | `~/Library/Application Support/synology-drive-sync/deposit` |
| Windows | `%LOCALAPPDATA%\synology-drive-sync\deposit` |

The directory must be private (no group or other write access). If the journal cannot be used,
nothing is uploaded and the run fails with `deposit-journal-unavailable`, `deposit-journal-locked`,
`deposit-journal-unsafe` or `deposit-journal-corrupt` (event reason `deposit_journal_unavailable`).
A journal whose last record was cut short by a power loss is read without it
(`deposit-journal-torn`): nothing is written before its record is on disk.

## Requirements and the opt-out

Atomic deposit needs `SYNO.FileStation.Rename`. A DSM without it gets nothing written -- no
upload, server copy, folder, deletion or rename of that run -- and every planned path is named
`deposit-not-atomic-unavailable` (a lasting warning, exit 0) until the DSM offers it or the profile
sets `deposit = "direct"`. There is never a silent fall back.

`deposit = "direct"` (or `--deposit direct`) uploads straight to the final name, as before. It
costs fewer requests per file, but a run killed mid-upload can leave a partial or torn file under
the final name, and a `--compare metadata` or `size-only` run may then call a torn file unchanged.
Every run in this mode is named `deposit-not-atomic`. The DSM package's profile editor does not
offer the opt-out: a package profile always deposits atomically (File Station on DSM 7 offers
Rename).

## Cost

| Per file | Direct | Atomic |
| --- | --- | --- |
| new | `Upload`, `getinfo`, `MD5` start and status (4+) | + `getinfo`, `Rename` (verified by one `getinfo`): 7+ |
| overwrite | 4+, plus the precondition `getinfo` | + `getinfo`, 2 `Rename`, `getinfo`, `Delete`: 10+ |
| per run | -- | two journal `fsync`s; recovery after a clean run sends nothing, after a crash one listing per folder involved |

Bytes on the wire are unchanged. An overwrite needs free space for the new copy while the old one
still exists, for each file in flight (at most `--jobs` at once). Small files skip nothing: a power
loss tears a small write as easily as a large one.

## What other software sees

- **File Station and SMB** list `~sdsync-part-` and `~sdsync-prev-` files while they exist.
- **Synology Drive** does not synchronize names that begin with `~` to its clients (the same rule
  that makes this tool refuse such names as sources), but Drive Server's own name filter has no
  prefix rule, so it indexes them. A replaced file is a new file to Drive: its version history
  continues under the new file, not the old one.
- **Recycle bin**: deleting the previous version uses File Station's `Delete`, which honours the
  share's recycle bin. On a share with the recycle bin on, every overwrite leaves the old version in
  `#recycle` until the bin's retention empties it.
- **The temporary names carry no extension**, so DSM's media indexer ignores them.

## What is not atomic

- **A whole run.** Files are published one at a time; a reader during a run sees some files new and
  some old. There is no run-level transaction: File Station cannot publish many files at once into
  an existing tree.
- **A server copy** writes its destination directly (a destination that was absent). A failure is
  cleaned up in the run, and a crash is settled by the next run's comparison, but a reader can see a
  partial copy until then.
- **Type replacement** (a folder replaced by a file, or the reverse) deletes the old entry before
  the new one is published.
