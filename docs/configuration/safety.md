# Comparison, exclusions, and deletion

The local source is authoritative. A run creates missing directories, uploads missing or changed
files, and preserves remote-only entries by default. Mirror behavior is opt-in.

## Comparison modes

| Mode | Same-path file is unchanged when | Trade-off |
| --- | --- | --- |
| `content` | size and File Station's server-side MD5 match; the modification time is not consulted, so equal bytes whose time moved (a `touch`, a restore, a daylight-saving shift, a DSM that did not keep the time an upload sent) are not re-sent | Safest and default; one server-side MD5 per same-size file, with no transfer. |
| `metadata` | size and whole-second mtime match (floor or rounded) | Avoids remote content hashes but cannot detect equal-size changes within the same second. |
| `size-only` | size matches | Fastest and weakest; use only when that correspondence is explicitly acceptable. |

Content mode compares with File Station's server-side MD5 (`SYNO.FileStation.MD5`), which DSM computes
without sending the file to the client. The deletion and server-copy guards are stricter: they act on
remote data, so they require the complete MD5, IEEE CRC32 and SHA-256 fingerprint, which DSM does not
compute and the client obtains by streaming the file. That needs download permission, but only for
the remote-only files a deletion would remove and the files a server-side copy would duplicate, a set
that is empty when nothing is being deleted or copied. Include that read traffic in bandwidth,
reverse-proxy timeout, and scheduling estimates for a first mirror run against a populated destination.

Content correspondence is stateless. Every run rebuilds it from current local and remote state; no
persistent path/hash database can become stale.

## Exclusions

Profile `excludes`, the source-root `.sdsyncignore`, and repeated `--exclude` rules use gitignore-style
matching. Rules are ordered, and `!` may re-include something excluded earlier:

```toml
excludes = ["target/", "*.tmp", ".cache/"]
```

```bash
synology-drive-sync plan --profile production \
  --exclude '*' \
  --exclude '!*.pdf'
```

Built-in default excludes come first, ahead of every rule above: `desktop.ini`, `Thumbs.db` and
`ehthumbs.db` (any case), `.DS_Store`, a macOS `Icon\r` file, and Office lock files (`~$*`). They are
never uploaded, and any copy already on the NAS is protected from `--delete`. Because they are ordinary
rules evaluated first, a rule of your own such as `!desktop.ini` re-includes one.

Command-line rules append to profile rules. Excluded local entries are outside the desired sync set;
review mirror plans carefully because a formerly included remote path can become remote-only.

Exclusion is decided **before** a name is checked for portability, so a rule can exclude a name the
tool would otherwise refuse to carry — a name such as `~scratch.txt`, a Windows device
stem such as `aux.h`, a trailing dot, a component over the length limit, or even a name that is not
valid UTF-8. Without a matching rule such a name no longer fails the run: it is set aside by name as an
`unsupported-entry` deferral (see [Partial runs, deferrals and skips](../observability.md#partial-runs-deferrals-and-skips)).
It is never uploaded, the NAS copy of it is never deleted, the rest of the tree syncs, and the run
exits `0` with a warning that names the path and the rule, on every run until the name is changed or
excluded. The same applies to a path that is too long once the destination prefix is prepended, a name
over 255 bytes, a FIFO, socket or device, and a modification time before 1970.

Rules always compare the pattern and the path Unicode-normalised (NFC), on every source: a rule typed
with a composed `é` excludes a decomposed `é` folder on a Linux or case-sensitive source, and the reverse.
Letter case is folded only when the source resolves names that way. The tool asks the filesystem rather
than assuming the platform: it first asks inside the root, looking up one of the root's own entries with
its letters' case swapped, because the root may be a mount point (a USB or CIFS volume) whose parent is
another filesystem. Only an empty root is answered by its own name in its parent directory, and only a
root neither can ask falls back to the platform default (folding on Windows and macOS). The first answer
decides; answers are never combined. An SMB share mounted on Linux can therefore fold, and a
case-sensitive macOS volume does not. The
matching applies to **both** walks. Under `--exclude report.pdf` on a folding source, a NAS-only
`Report.pdf` is excluded (never deleted, never overwritten) and a local `Report.pdf` is never uploaded,
whichever spelling either side uses. On a case-sensitive source the rules stay case-sensitive.

A rule also covers **both spellings of a pair** the run has matched: when a local folder was renamed
(`Reports` to `reports`) or differs from its NAS folder only by normalisation, a rule that excludes the
local spelling protects the NAS spelling from deletion and replacement, and a rule written against the
NAS spelling excludes the local content under the new one.

**What protects a NAS-only name from `--delete`.** On every source, including case- and
normalisation-folding ones (Windows NTFS, macOS APFS, most SMB mounts), a NAS-only name is protected when
either of two readings of your rules excludes it: the rules matched Unicode-normalised, with case folded
where the source folds (the reading the walks use), **or** the patterns exactly as typed, case-sensitive and
as spelled, against the NAS path. Protection only widens, so a negation (`!rule`) can never make deletable
a NAS-only name that the typed patterns exclude. For example, with `*.txt` and `!Report.txt`, a NAS-only
`report.txt` is kept on Windows even though the folding reading re-includes it, and with `*.txt` and
`!café.txt` (composed) a NAS-only decomposed `café.txt` is kept on macOS. Such names are reported as
protected, not deleted. Before this release those two cases were deleted on a folding source, because only
the folded reading protected there. Uploads, comparison and the two walks still use the folded reading
only: the negation still re-includes the local `Report.txt` for upload.
Content an exclude newly covers is left on the NAS exactly as it is. A file that an earlier release
uploaded before the rule matched it is **not** removed by this release or by a later `--delete`; delete it
by hand in File Station.

Both walks apply the same rules. On the destination side an excluded directory, and every DSM-managed
directory (`@eaDir`, `#recycle`, `#snapshot`, `@tmp`), is recorded but not descended into: nothing
under it can be compared, copied or deleted, so listing its contents would cost round trips and scan
budget for entries the planner discards. Case-portability checking skips them for the same reason — a
recycle bin holding both `Report.pdf` and `report.pdf` is not a reason to refuse a run.

The destination walk also sets aside, by name, what it cannot safely carry. A NAS name this tool
cannot address (a control character such as a stray `Icon
`, or a backslash) is never descended into,
compared, overwritten or deleted, and its folder and ancestors are kept: it is a lasting `remote-skipped`
warning rather than a failed run. A folder whose DSM `real_path` is somewhere else (a symbolic link
out of the destination) or one already walked is treated like a mount point: never descended, never
deleted through. The walk asks DSM for `real_path` on every listing and compares each child's real path
with its parent's relatively, so a volume prefix or an encrypted share's prefix does not make ordinary
folders look like links. If DSM reports no `real_path` at all for the destination, the link guard cannot
run; sync and plan print a warning and carry on, and the guard stays unverified until a real NAS answers
the [link questions](../production-acceptance.md#questions-only-a-real-nas-can-answer). When a
majority of the folders listed (and at least three) look like links, or the same holds for the files
checked, the run fails with the named cause `remote-real-path-unusable` instead of silently syncing
nothing there. Folders DSM listed without any real path are counted as `link_guard_unchecked` in the
result.

## Enabling mirror deletion

Deletion requires all applicable layers:

1. profile, environment, or CLI selects `delete`;
2. a per-profile `max-delete` permits the planned count;
3. a batch aggregate `max-total-delete` permits the total;
4. the execution path explicitly authorizes destructive operation;
5. fresh inventory/replan checks still match the reviewed safety conditions.

`--no-delete` can disable a profile or environment setting for one invocation. It also disarms
`allow-empty-source`, which exists only to authorise deletions: with deletion off there is nothing
for it to guard, so `--no-delete` alongside `allow-empty-source` from any source is valid and simply
deletes nothing.

The planner refuses deletion when the source contains no payload files unless
`allow-empty-source` is explicitly enabled alongside deletion. `/` can never be a destination, and
every removed path must remain a strict child of the configured root. Protected DSM-managed paths
remain out of scope.

## Failure ordering

Uploads, copies, directory creation, source stability checks, and required log delivery must succeed
before the later remote-only deletion phase. A changed source file, missing remote digest, stale
destination snapshot, cancellation request, or failed verification stops the run before unsafe
cleanup can continue.

Type conflicts that require replacing a remote entry (a local file where the NAS holds a folder, or
the reverse) are destructive and therefore follow the same deletion authorization and caps. A
conflict the run cannot carry no longer fails it. Without `--delete`, or when the NAS folder holds an
excluded, DSM-managed or mounted entry, only that path is set aside as a lasting `type-conflict`
skip: both sides are left as they are, nothing beneath it is written, and the run exits `0` with a
warning. A local entry whose NAS counterpart is a link (a symlink or a folder DSM resolves
elsewhere) is the lasting `remote-link` skip: nothing is written, deleted or descended at or beneath it.
A `--scope` that is, or lies beneath, a NAS link is not skipped but **refused**, exit `1`, before
anything is listed: a scoped run never writes or deletes through a link (see
[Partial runs, deferrals and skips](../observability.md#partial-runs-deferrals-and-skips)).

A file or folder the NAS refuses by name or permission (DSM codes such as 418, 414, 407 or 105 once the
session is proven alive) is named and left out, and its siblings continue; see
[Partial runs, deferrals and skips](../observability.md#partial-runs-deferrals-and-skips). A refusal
that is really about the share is not hidden by that rule: when at least three refusals share one class
(permission, name, size, clash, folder-limit or create-failed) and they are more than half of what the
run considered (every planned folder, copy and upload plus the files already in sync), the run fails
once and names the class, DSM's answers and the paths, so a share that refuses everything cannot read
as a green run. On a large share that is already in sync, a new share-wide refusal can read amber with
every path named instead; that is the price of not blaming the share for three bad names.

A planned mirror deletion re-checks that the local path is still absent immediately before it
deletes the remote copy. If the local file reappeared in the meantime, the remote copy is kept and
the refusal is reported rather than the file being deleted. When a kept deletion is a directory's
child, the directory's own deletion is kept too, because a non-recursive delete of a directory that
still has a child could only fail. Neither fails the run, and the run then **uploads** the files it
kept, in a follow-up phase with the same preflight and verification as any other upload -- so the
mirror converges within the run rather than waiting for a later one. The counts are reported as
`deletes_skipped` and `kept_deletes_uploaded`.

Post-run reconciliation rebuilds the plan from live state and requires it to describe no remaining
work. It re-lists both sides in full, which is what proves nothing else moved, and verifies live every
pair the run touched (uploaded, copied, created, renamed, deleted or deferred) plus a canary of 25
untouched pairs. Every other pair whose local size, times, device and inode and whose NAS size and
times are exactly what the plan pass saw keeps the digests the plan pass proved. A canary that
disagrees makes the same pass verify every pair live and is named (`reconcile-carry-canary-mismatch`).
What this gives up is a NAS content change during the run that leaves size, mtime and change time all
unchanged, which a write through the kernel cannot do; when the NAS reports no change time, a
same-size, same-mtime rewrite by another writer during the run is seen by the next run instead.
`--reconcile full` (command line only) restores the exhaustive pass.

Reconciliation judges what is left against the run's **first scan**, never against a clock. A path
the first walk did not hold (written, or moved or copied in while the run worked, which keeps an old
modification time), a path whose local kind, size, modification time or file identity moved since
the first walk, a path deleted locally while the run worked, and a NAS entry that appeared, vanished
or changed since the first listing where this run did not write, delete or rename it (and both
listings were whole there) changed while the run worked. Each is counted in `changed_during_run`
and named as a transient deferral (`source-changed`, `source-vanished` or `remote-changed`): the run
is partial (exit `3`), no alert is raised, and the next run carries it. Anything else is still a
failure (exit `1`), because it means the run did not do what it planned: a deletion this run made
that did not take, a write this run made that the NAS still disagrees with, a path in the first scan,
unchanged since, whose NAS copy still differs, a type conflict or case-only rename left unresolved,
and a missing destination root. The failure names the first five operations with their action, path
and cause (`upload docs/a.pdf (in the first scan, unchanged since, and the NAS still differs)`),
counts only those that nothing excused, and puts the first path in the result record's `error.path`;
each named path is also logged as an `entry.skipped` warning with reason `reconciliation_pending`.

A run that fails after performing some of its work still reports that work. The plan and the counts
of what completed are printed before the failure is returned, and the machine-readable result carries
`failed: true` alongside them.

## Safe rollout

Keep `delete = false` until you have:

- externally verified uploaded bytes and Synology Drive indexing;
- enabled and manually tested an independent restore layer;
- exercised retry and interruption behavior;
- confirmed alerts and logs reach their owner;
- completed the disposable deletion-containment step in the
  [live-NAS acceptance runbook](../production-acceptance.md).

Start with a deliberately tiny deletion cap and inspect every plan. A mirror sync is not an
independent backup.
