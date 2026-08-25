# Profiles and destinations

A DSM profile binds one physical local source to one File Station logical destination, target
account, protected credential set, safety policy, and observability policy. Profiles are independent:
they may use different sources, NAS URLs, accounts, credentials, and target paths.

Selecting an existing profile in the dashboard makes **Name** read-only. There is no silent rename.
Create a distinct profile, validate it, and remove the old one when a name must change.

## Basic fields

| Dashboard field | Accepted value and behavior |
| --- | --- |
| Name | 1–64 ASCII letters, digits, `_`, or `-`; `all` is reserved |
| Local source | Existing absolute physical path on this NAS, for example `/volume1/Photos`; canonicalized and validated immediately |
| File Station URL | HTTPS origin or HTTPS origin plus reverse-proxy prefix; up to 2,048 bytes |
| DSM username | Target account name; up to 256 bytes; use a dedicated non-administrator account |
| Remote logical path | Absolute File Station path such as `/home/Drive/Backup` or `/TeamShare/Backup`; never `/volumeN/...`; portability ceiling 247 characters |
| Comparison | `content`, `metadata`, or `size-only` |
| Concurrent uploads | `1..16`; default `2` |
| Allow plain HTTP | Off by default; permits `http://` only for a controlled LAN test |
| Use as default profile | Makes commands/actions without an explicit named scope select this profile |

Comparison behavior:

- **Content** uses size, MD5, and mtime evidence and is the strongest normal correspondence mode.
- **Metadata** uses size and mtime and can miss same-size/same-time content changes.
- **Size only** is the weakest and should be reserved for a deliberately accepted workflow.

The destination's first component must be an existing share visible to the target account. The sync
can create a missing selected directory and its descendants beneath an existing writable parent. It
cannot manufacture a missing shared-folder root.

## Source and destination path rejection

The local source cannot be `/`, a symlink, unreadable/untraversable, package-owned storage, or inside
a DSM-managed directory. The remote destination cannot be `/`, end in `/`, contain `//`, `.` or
`..` segments, or contain a DSM-managed component.

Remote components also reject leading `~`, Windows-reserved device names, unsupported characters
such as `* : ? " < > |`, trailing dots/spaces, and values that exceed Synology Drive portability
limits. This protects mixed DSM/Windows Drive environments; it does not synchronize ACLs, owners,
modes, xattrs, or directory mtimes.

## Deletion fields

| Field | Meaning |
| --- | --- |
| Mirror remote deletions | Allows exact-mirror logic in this profile; off by default |
| Maximum deletions per run | Per-profile hard cap; enabling deletion requires an explicit bound |

These fields do not arm a dashboard Run or routine by themselves. Action-level approval is also
required, and all-profile actions can require an aggregate bound. See
[Deletion approval is layered](routines.md#deletion-approval-is-layered).

## Advanced source and retry fields

| Field | Accepted value and behavior |
| --- | --- |
| Excludes | One non-empty glob per line; at most 64 entries, each at most 512 bytes |
| Allow an empty source | Dangerous opt-out from the empty-source deletion guard; off by default |
| Retries | `0..5`; default `2` |
| Upload timeout | `1..86400` seconds; default `7200` |
| Connect timeout | Effective dashboard bridge range `1..600` seconds; default `15` |
| Maximum rate | Positive bytes per second, or `0` in the form for unlimited |

Default excludes include DSM metadata such as `@eaDir/`, nested `**/@eaDir/`, `#recycle/`, and
`#snapshot/`. An empty exclude list is possible through CLI parity, but do not re-include DSM
administrative trees as payload.

Retries apply to eligible transport operations within one finite sync. They are not the same as a
routine retry after the entire action fails; [routines](routines.md#retry-and-backoff) configure that
outer policy separately.

## TLS fields

| Field | Accepted value and behavior |
| --- | --- |
| CA certificate path | Absolute, package-readable, non-symlink certificate file on the source NAS; contents never enter the browser |
| Accept invalid TLS certificates | Dangerous bypass of certificate trust and identity checks |
| Interception-risk confirmation | Required each time the invalid-certificate option is enabled |

Prefer a valid public certificate or an explicit private CA file. **Allow plain HTTP** and
**Accept invalid TLS certificates** solve different problems and both weaken transport security.
Neither bypass should be a normal production setting.

## Output and local logging fields

| Dashboard field | DSM package behavior |
| --- | --- |
| Verbosity | `0` normal, `1` verbose, `2` very verbose |
| Quiet terminal sink | Suppresses terminal-style output while durable logs remain active; cannot be combined with nonzero verbosity |
| Log level | `trace`, `debug`, `info`, `warn`, `error`, or `off` |
| Log format | Displayed as package-managed `json` |
| Log file | Displayed read-only as the private package sync log |
| Progress | Displayed as package-managed `never` |
| Output | Displayed as package-managed `human` action output |

The four package-managed values are deliberate DSM invariants. They keep unattended logs
deterministic, bounded, and private and prevent the browser from selecting arbitrary filesystem
paths. Workstation TOML/CLI deployments can choose other supported log format, log path, progress,
and output values; the DSM dashboard intentionally cannot. The SSH manager can render a profile with
different valid values only inside the same private log boundary, but normal dashboard saves retain
the package-managed contract.

## Remote logging fields

| Field | Accepted value and behavior |
| --- | --- |
| Remote log URL | Empty/disabled or an HTTPS URL up to 2,048 bytes |
| Remote log mode | `best-effort` or `required`; required mode needs a URL |
| Remote log token | Separate protected value with keep/replace/clear semantics |

**Best effort** records local logging even when remote delivery fails. **Required** makes remote
logging failure affect operation success; use it only when the collector is itself reliable and the
failure semantics are desired. The token is never returned with the URL or profile snapshot.

## Saving, defaults, and removal

Profile configuration is validated, rendered into a strict non-secret fragment, combined into the
generated TOML, validated by the core, and atomically replaced. Existing protected credentials are
retained unless their separate secret selector says replace or clear.

Dashboard saves are published through the private queue and poll a sanitized terminal result before
reporting success. Treat the subsequent snapshot as the authoritative displayed configuration; a
timeout leaves the outcome unknown. Removing a profile also removes its package-owned password, TOTP,
remote-log-token, routine, and cached state, then rebuilds the generated configuration. It does not
touch local or remote data. Removing the final profile disables the legacy global schedule.

## CLI example

Resolve `$PACKAGE_USER` through the canonical
[package-identity discovery](cli-parity.md#discover-the-actual-package-identity) first.

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-profile \
  --name archive \
  --source '/volume1/Documents' \
  --url 'https://files.archive.example/nas/' \
  --username 'archive-bot' \
  --remote '/ArchiveTeam/Documents' \
  --compare content \
  --jobs 2 \
  --retries 2 \
  --timeout 7200 \
  --connect-timeout 15
```

Continue with [Secrets and protected values](secrets.md), then run
[Doctor and Plan](operations.md#health-and-doctor) before the first sync.
