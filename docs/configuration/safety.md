# Comparison, exclusions, and deletion

The local source is authoritative. A run creates missing directories, uploads missing or changed
files, and preserves remote-only entries by default. Mirror behavior is opt-in.

## Comparison modes

| Mode | Same-path file is unchanged when | Trade-off |
| --- | --- | --- |
| `content` | size, File Station MD5, and one-second mtime match | Safest and default; requires remote hashes. |
| `metadata` | size and one-second mtime match | Avoids remote content hashes but cannot detect equal-size/equal-time byte changes. |
| `size-only` | size matches | Fastest and weakest; use only when that correspondence is explicitly acceptable. |

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

## Enabling mirror deletion

Deletion requires all applicable layers:

1. profile, environment, or CLI selects `delete`;
2. a per-profile `max-delete` permits the planned count;
3. a batch aggregate `max-total-delete` permits the total;
4. the execution path explicitly authorizes destructive operation;
5. fresh inventory/replan checks still match the reviewed safety conditions.

`--no-delete` can disable a profile or environment setting for one invocation.

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
