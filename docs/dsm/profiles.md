# Profiles and destinations

A DSM profile is the complete unit for one sync target. It binds one physical local source to the
target NAS URL, DSM account, File Station logical destination, protected credential set, safety
policy, network behavior, and observability/output policy. Profiles are independent: they may use
different sources, NAS URLs, accounts, credentials, target paths, and runtime settings.

Selecting an existing profile in the dashboard makes **Name** read-only. There is no silent rename.
Create a distinct profile, validate it, and remove the old one when a name must change.

## Basic fields

| Dashboard field | Accepted value and behavior |
| --- | --- |
| Name | 1–64 ASCII letters, digits, `_`, or `-`; `all` is reserved |
| Local source | Existing canonical folder below `/volumeN`, `/volumeUSBN`, or `/volumeSATAN`, for example `/volume1/Photos`; selected interactively or entered manually, then validated as the package identity immediately before save |
| File Station URL | HTTPS origin or HTTPS origin plus reverse-proxy prefix; up to 2,048 bytes |
| DSM username | Target account name; up to 256 bytes; use a dedicated non-administrator account |
| Remote logical path | Absolute File Station path such as `/home/Drive/Backup` or `/TeamShare/Backup`; never `/volumeN/...`; portability ceiling 247 characters |
| Comparison | `content`, `metadata`, or `size-only` |
| Concurrent uploads | `1..16`; default `2` |
| Allow plain HTTP | Off by default; permits `http://` only for a controlled LAN test |
| Use as default profile | Makes commands/actions without an explicit named scope select this profile |

Comparison behavior:

- **Content** requires size, MD5, IEEE CRC32, SHA-256, and mtime evidence and is the strongest normal correspondence mode.
- **Metadata** uses size and mtime and can miss same-size/same-time content changes.
- **Size only** is the weakest and should be reserved for a deliberately accepted workflow.

The destination's first component must be an existing share visible to the target account. The sync
can create a missing selected directory and its descendants beneath an existing writable parent. It
cannot manufacture a missing shared-folder root.

## Interactive source and destination selection

**Browse NAS** asks the package bridge for directories the package service can actually read and
traverse. The root view exposes only canonical, non-symlink DSM storage roots named `/volumeN`,
`/volumeUSBN`, or `/volumeSATAN`; it never exposes `/etc`, `/usr`, package-private state, or an
arbitrary filesystem root. Unreadable, vanished, non-UTF-8, DSM-managed, and symlinked children are
omitted. Selecting a folder does not bypass save validation: the exact selected or manually entered
path is canonicalized and checked again under the package service identity immediately before the
configuration mutation. If browsing is unavailable, the text field remains an explicit manual-entry
fallback within the same DSM storage-root boundary.

**Browse target** remains locked until **Test authentication** succeeds for the exact URL, account,
TLS settings, and stored or transient credential draft. The package discovers `SYNO.API.Auth` and
`SYNO.FileStation.List` through `SYNO.API.Info`, opens a temporary `FileStation` session, uses
`list_share` for the root and `list` for descendants, returns only directories File Station exposes
with usable list/read permissions, and always attempts logout. It does not guess share names and does
not use the browser File API. A successful test issues a session-and-draft-bound proof for at most
five minutes; changing the connection draft or reaching expiry closes the chooser and requires a new
test. The test can use stored credentials while secret writes are disabled, provided operational
actions remain allowed. Transient values are neither returned nor persisted by testing; creating a
new profile persists its password only in the later protected-secret save stage.

These selectors use documented File Station APIs and the package's authenticated bridge; they do
not depend on an undocumented DSM JavaScript namespace. Manual entry remains available when the
native chooser cannot load.

## Source and destination path rejection

The local source cannot be `/`, a symlink, unreadable/untraversable, package-owned storage, inside a
DSM-managed directory, or outside the recognized internal/USB/SATA DSM storage roots. Arbitrary
mount roots such as `/mnt/...` are intentionally not exposed or accepted by the DSM profile editor;
mount them through a recognized DSM volume when they must be used as package sources. The remote
destination cannot be `/`, end in `/`, contain `//`, `.` or `..` segments, or contain a DSM-managed
component.

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
| Upload timeout | DSM manager and dashboard range `1..86400` seconds; default `7200` |
| Connect timeout | DSM manager and dashboard range `1..600` seconds; default `15` |
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
| Quiet terminal sink | Suppresses terminal-style output while durable logs remain active; nonzero verbosity may still raise durable log detail |
| Log level | `trace`, `debug`, `info`, `warn`, `error`, or `off` |
| Log format | `human` or `json`; default `json` |
| Log file | Displayed read-only as the private package sync log |
| Progress | `auto`, `always`, or `never`; default `never` |
| Output | `human`, `json`, or newline-delimited `ndjson`; default `human` |

The log file path remains a DSM invariant: the dashboard cannot redirect output to an arbitrary
filesystem path. Log format, progress rendering, and command-result output are safe enumerations and
are configurable per profile. `json` logs preserve structured severity/category data; human logs are
shown as opaque informational lines by threshold filtering. Scheduled runs suppress terminal output,
so changing progress/output mainly affects foreground Doctor, Plan, and Run actions.

DSM constrains `max-delete` to `0..2147483647` so the same saved profile is representable on armv7
and 64-bit NAS builds. `max-rate` is either unlimited or `1..9007199254740991` bytes per second so
its exact integer value round-trips through the JavaScript dashboard without precision loss.

## Remote logging fields

| Field | Accepted value and behavior |
| --- | --- |
| Remote log URL | Empty/disabled or an HTTPS URL up to 2,048 bytes |
| Remote log mode | `best-effort` or `required`; required mode needs a URL |
| Remote log token | Separate protected value with keep/replace/clear semantics |

**Best effort** records local logging even when remote delivery fails. **Required** makes remote
logging failure affect operation success; use it only when the collector is itself reliable and the
failure semantics are desired. The token is never returned with the URL or profile snapshot.

The token can be staged while remote logging is disabled. In that state its protected file and
`has_remote_log_token` presence flag are retained, but the generated core profile omits the token
locator. Enabling a remote-log URL restores the fixed package-owned locator; disabling the URL omits
it again. This keeps the core configuration valid without disclosing or deleting a credential that
the administrator intentionally retained.

## Complete DSM profile contract

The dashboard and manager expose every safe, profile-scoped core setting. A few core TOML fields are
represented by safer DSM-specific controls instead of raw paths:

| Core profile concern | DSM representation |
| --- | --- |
| Source and target | `source`, `url`, `username`, and `remote` editable fields |
| Password and TOTP locators | Fixed package-owned files; values are write-only keep/replace/clear operations |
| Remote-log token locator | Fixed package-owned file, referenced only while a remote-log URL is configured |
| OS credential vault | Always disabled because the DSM service has package-private credential files |
| Sync/safety/network | Comparison, jobs, excludes, deletion bounds, empty-source guard, retries, timeouts, rate, CA, and TLS exceptions |
| Local output | Verbosity, quiet, log level, log format, progress, and result output |
| Local log path | Fixed private `sync.log`; visible read-only in the snapshot |
| Remote observability | HTTPS collector URL, delivery mode, and write-only token |

Snapshots return all non-secret values plus only `has_password`, `has_totp`, and
`has_remote_log_token`. They never return a credential value or a credential-file locator.

## Saving, defaults, and removal

Profile configuration is validated, rendered into a strict non-secret fragment, combined into the
generated TOML, validated by the core, and atomically replaced. Existing protected credentials are
retained unless their separate secret selector says replace or clear.

Dashboard profile and credential saves are published through the private queue and poll a sanitized
terminal result before reporting success. Their request and result-observation phases are bounded so
the editor cannot remain stuck on **Saving** indefinitely. If a bound is reached after acceptance,
the result is outcome-unknown and the editor preserves unapplied credential drafts for Activity and
Logs inspection; it never silently retries. Each protected value is cleared only after its own
terminal success. Status polling, including manual refresh, is suspended while the editor owns a
draft or save, and late status responses are generation-fenced so they cannot overwrite it. Closing
the editor performs one fresh status read even when periodic refresh is set to **Manual only**.
Removing a profile also removes its package-owned password, TOTP,
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
