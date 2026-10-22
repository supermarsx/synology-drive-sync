# Comparison, exclusions, and deletion

The local source is authoritative. A run creates missing directories, uploads missing or changed
files, and preserves remote-only entries by default. Mirror behavior is opt-in.

## Comparison modes

| Mode | Same-path file is unchanged when | Trade-off |
| --- | --- | --- |
| `content` | size, MD5, IEEE CRC32, SHA-256, and one-second mtime all match | Safest and default; streams selected remote files to compute the complete fingerprint. |
| `metadata` | size and one-second mtime match | Avoids remote content hashes but cannot detect equal-size/equal-time byte changes. |
| `size-only` | size matches | Fastest and weakest; use only when that correspondence is explicitly acceptable. |

Content mode requires the File Station account to have download permission for every selected
remote file. Each proof streams those bytes through the client, so include remote read traffic in
bandwidth, reverse-proxy timeout, and scheduling estimates.

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

Command-line rules append to profile rules. Excluded local entries are outside the desired sync set;
review mirror plans carefully because a formerly included remote path can become remote-only.

Exclusion is decided **before** a name is checked for portability, so a rule can exclude a name the
tool would otherwise refuse to carry — an Office lock file such as `~$report.docx`, a Windows device
stem such as `aux.h`, a trailing dot, a component over the length limit, or even a name that is not
valid UTF-8. Without a matching rule those names still fail the whole run, which is the right answer
for a name that cannot be mapped to a remote path; with one, they are simply skipped.

Both walks apply the same rules. On the destination side an excluded directory, and every DSM-managed
directory (`@eaDir`, `#recycle`, `#snapshot`, `@tmp`), is recorded but not descended into: nothing
under it can be compared, copied or deleted, so listing its contents would cost round trips and scan
budget for entries the planner discards. Case-portability checking skips them for the same reason — a
recycle bin holding both `Report.pdf` and `report.pdf` is not a reason to refuse a run.

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

Type conflicts that require replacing a remote entry are destructive and therefore follow the same
deletion authorization and caps.

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
