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

Rules match case-insensitively, with the pattern and the path both Unicode-normalised (NFC), when the
source resolves names that way. The tool asks the filesystem rather than assuming the platform: it looks
up the source root's own name with its letters' case swapped, and falls back to the platform default
(folding on Windows and macOS) when the root's name has no cased letter. An SMB share mounted on Linux
can therefore fold, and a case-sensitive macOS volume does not. The folding applies to **both** walks. Under
`--exclude report.pdf` on such a source, a NAS-only `Report.pdf` is excluded (never deleted, never
overwritten) and a local `Report.pdf` is never uploaded, whichever spelling either side uses. On a
case-sensitive source the rules stay case-sensitive.

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
majority of the folders listed (and at least three) look like links, the run fails with the named cause
`remote-real-path-unusable` instead of silently syncing nothing there.

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

A file or folder the NAS refuses by name or permission (DSM codes such as 418, 414, 407 or 105 once the
session is proven alive) is named and left out, and its siblings continue; see
[Partial runs, deferrals and skips](../observability.md#partial-runs-deferrals-and-skips). A refusal
that is really about the share is not hidden by that rule: when at least three refusals share one cause
and they are more than half of the folders and files the run attempted, the run fails once and names
the cause and the paths, so a share that refuses everything cannot read as a green run.

A planned mirror deletion re-checks that the local path is still absent immediately before it
deletes the remote copy. If the local file reappeared in the meantime, the remote copy is kept and
the refusal is reported rather than the file being deleted. When a kept deletion is a directory's
child, the directory's own deletion is kept too, because a non-recursive delete of a directory that
still has a child could only fail. Neither fails the run, and the run then **uploads** the files it
kept, in a follow-up phase with the same preflight and verification as any other upload -- so the
mirror converges within the run rather than waiting for a later one. The counts are reported as
`deletes_skipped` and `kept_deletes_uploaded`.

Post-run reconciliation rebuilds the plan from live state and requires it to describe no remaining
work. A file written locally *while the run was working* is the exception: the run was never going to
carry bytes that did not exist when it read the tree, so an entry whose local modification time is
after the run's start instant is reported under `changed_during_run` and the run succeeds. Anything
else -- a remaining deletion, a remaining server copy, or an upload of a file older than the run --
is still a failure, because those mean the run did not do what it planned.

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
