# Synology DSM package

The DSM 7 package runs the existing one-way sync engine on the source NAS. It reads an explicitly
permitted local shared folder and sends the bytes over HTTPS to File Station on another NAS. The
remote folder may be the target account's Synology Drive home or any writable subfolder of a Team
Folder/shared folder.

```text
source NAS                                           target NAS
/volume1/Source  ->  synology-drive-sync SPK  ->  HTTPS File Station WebAPI
                            package user              /home/Drive/Chosen Folder
                                                      /team-folder/Chosen Folder
```

This is a headless, manually installed package. Configuration and diagnostics are performed over
SSH with `sdsync-dsm`; there is currently no DSM desktop UI. The package is not a Synology Drive
protocol plug-in. Synology Drive sees the result after it indexes File Station writes in a user's
Drive home or an enabled Team Folder.

> [!IMPORTANT]
> CI validates the SPK layout, lifecycle scripts, static ELF architecture, manager behavior, and
> mock File Station interactions. It does not install on or synchronize between live NAS devices.
> Complete the [two-NAS acceptance](#live-two-nas-acceptance) on disposable folders before relying
> on it, and leave deletion disabled until its separate destructive test passes.

## What the destination setting means

`--remote` is not hard-coded to one home directory. It accepts a File Station **logical path**
beginning with an existing shared-folder name. These are valid examples:

```text
/home/Drive/NAS-A Backup
/home/Drive/Projects/Archive
/team-folder/NAS-A Backup
/photos/Incoming/NAS-A
```

The manager rejects `/`, a trailing slash, repeated `/`, every `.` or `..` segment, and any
case-insensitive component named `#recycle`, `#snapshot`, `@eaDir`, `@tmp`, `@sharebin`,
`@apphome`, `@appdata`, `@appstore`, `@apptemp`, `@appconf`, or
`.SynologyWorkingDirectory`. It also rejects Drive-incompatible components (including leading `~`,
Windows-reserved names, unsupported characters, and trailing dots/spaces) and applies the
247-character portability limit to the complete selected prefix plus each source-relative path.
A DSM-managed location cannot be selected as the sync root even when the remote account could
otherwise see it.

Use `/home/Drive/...` to write as the configured remote DSM account into that account's own Drive
home. User Home service and that user's `/home` logical root must already be provisioned on the
target NAS. Once `/home` exists and is writable, sync can create missing `Drive/...` descendants,
but it does not install or initialize Synology Drive; initialize the account's Drive home first when
Drive indexing is required. Use `/<share>/...` for a shared folder or Team Folder; create the shared
folder in DSM first and, when Drive visibility is wanted, enable it as a Team Folder in Synology
Drive Admin Console.

The package does **not** create a DSM user, enable User Home service, create a shared folder, enable
a Team Folder, or change a remote ACL. Those are DSM administrative operations. The remote account
must already be able to see the first path component and create content below the nearest existing
parent.

The sync engine does create the selected destination directory when it is a missing subdirectory of
an existing writable share, and creates all missing descendants shallowest-first. File Station is
also asked to create missing parents. The engine refuses to manufacture a missing shared-folder
root, rejects `/`, and confines all generated paths to the configured destination. Empty local
directories and the complete source hierarchy are therefore reproduced beneath the selected root;
ACLs, owners, modes, xattrs, and directory mtimes are not synchronized.

Do not use physical target paths such as `/volume1/homes/alice/Drive/Backup`. The target is always a
File Station logical path. Conversely, the package's local `--source` is a physical absolute path
on the source NAS, such as `/volume1/Photos`.

## Requirements and package variants

- DSM `7.0-40759` or newer on the source NAS.
- A source shared folder that the package's system-internal user can read and traverse.
- A dedicated, non-administrator account on the target NAS with File Station application access
  and read/write permission only to the selected remote subtree.
- An HTTPS File Station reverse-proxy URL whose `/webapi/*` route, body-size limit, and timeouts
  accommodate the largest file.
- SSH access for initial headless configuration.

One release SPK must match the source NAS architecture:

| DSM architecture | Release asset | Embedded executable |
| --- | --- | --- |
| `x86_64` | `synology-drive-sync-YY.N-x86_64.spk` | static `x86_64-unknown-linux-musl` ELF64 |
| `armv8` | `synology-drive-sync-YY.N-armv8.spk` | static `aarch64-unknown-linux-musl` ELF64 |

The package builder and validator require the requested ELF machine and reject a dynamic
interpreter or `DT_NEEDED` library. The executable does not depend on the source NAS's glibc or a
desktop D-Bus Secret Service. `x86_64` and `armv8` are separate SPKs; neither is `noarch`, and an
SPK for one architecture will not be accepted as the other.

Models using Synology architecture labels other than `x86_64` or `armv8` are not currently in the
release matrix. Confirm the model's architecture in Synology's platform table before installation.

## Download, verify, and install

Download the correct `.spk` and `SHA256SUMS` from the same calendar release. For an ARMv8 NAS, for
example:

```bash
asset=synology-drive-sync-26.1-armv8.spk
expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print tolower($1) }' SHA256SUMS)
test "$(printf '%s\n' "$expected" | awk 'NF { n++ } END { print n + 0 }')" -eq 1
actual=$(sha256sum "$asset" | awk '{ print tolower($1) }')
test "$actual" = "$expected"
```

Optionally verify GitHub provenance too:

```bash
gh attestation verify synology-drive-sync-26.1-armv8.spk \
  --repo supermarsx/synology-drive-sync
```

In DSM, open **Package Center > Manual Install**, select the verified SPK, review the requested
package information, and install it. DSM 7 warns when installing any non-Synology package. This
project does not bypass that warning: proceed only when the asset name, checksum, provenance, and
source repository are the ones you intended to trust.

The SPK deliberately has no credential/path installation wizard: `silent_install=yes` and
`silent_upgrade=yes` keep secrets out of Package Center wizard variables, and configuration remains
an explicit post-install SSH step. `silent_uninstall=no` is deliberate because uninstall permanently
purges package-owned profiles, credentials, state, and logs. These settings do not suppress DSM's
third-party package warning.

Installation creates the unprivileged system-internal user `synology-drive-sync`, private package
storage, and a disabled schedule. It does not grant itself access to user shares and does not begin
synchronizing. The service controller may run while scheduling is disabled; no target request is
made until an operator runs a diagnostic/plan/run or enables a configured schedule.

## Grant the package access to the local source

On the **source** NAS, open **Control Panel > Shared Folder**, edit the selected source share, open
**Permissions**, switch the selector to **System internal user**, and grant
`synology-drive-sync` read-only access. Apply the permission to the required descendants. If the
share uses Windows ACLs, verify that inherited ACLs also allow the package user to list directories,
traverse them, and read files.

Do not grant write access merely to make a failing scan pass. The source is authoritative and the
package never needs to modify it. Do not grant access to unrelated shares.

The package rejects `/`, symlink source roots, unreadable/untraversable roots, and roots inside DSM
managed directories. While scanning a normal DSM share, administrative entries named `#recycle`,
`#snapshot`, `@eaDir`, `@tmp`, `@sharebin`, `@apphome`, `@appdata`, `@appstore`, `@apptemp`,
`@appconf`, and `.SynologyWorkingDirectory` are pruned and never treated as payload.

Run management commands as the package identity so this ACL is tested accurately. From an
administrator SSH shell:

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo -u synology-drive-sync -- "$MANAGER" paths
```

The package name is also its default DSM package username because `conf/privilege` deliberately
uses `run-as: package` and does not override `username`. Profile/secret/schedule mutations and all
doctor/plan/run operations fail with exit `77` when invoked as root or another identity. Do not use
plain `sudo "$MANAGER" ...` for those operations: it would test the administrator's filesystem
access instead of the package ACL if it were allowed.

## Configure one chosen destination

This example reads `/volume1/Source` on NAS A and writes to a freely chosen directory in the remote
account's Drive home on NAS B:

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm

sudo -u synology-drive-sync -- "$MANAGER" configure-profile \
  --name nas-b-home \
  --source '/volume1/Source' \
  --url 'https://files-b.example.com' \
  --username 'mirror-bot' \
  --remote '/home/Drive/NAS-A Backup' \
  --compare content \
  --jobs 2 \
  --default
```

For a Team Folder or ordinary writable shared-folder subdirectory, choose that logical path instead:

```bash
sudo -u synology-drive-sync -- "$MANAGER" configure-profile \
  --name nas-b-team \
  --source '/volume1/Projects' \
  --url 'https://files-b.example.com/nas/' \
  --username 'project-mirror' \
  --remote '/team-folder/Chosen Folder/Projects' \
  --compare content \
  --jobs 2
```

`configure-profile` validates the local source immediately and writes a strict generated profile.
HTTPS is required. `--allow-http` exists for controlled LAN testing only. Profile names may contain
letters, digits, `_`, and `-`; `jobs` is limited to `1..16`.

The target directory may already exist or may be missing below a writable existing share. Normal
target diagnostics check the exact destination when it exists; otherwise they check permission to
create the first missing component beneath its nearest existing ancestor. A later sync creates that
component and the remaining hierarchy. If the first shared-folder component is missing or hidden
from the account, configuration can still be written, but target diagnosis/planning fails closed
and the sync does not create a new DSM share.

## Store the password and optional TOTP seed

Set secrets through a masked prompt as the package identity:

```bash
sudo -u synology-drive-sync -- "$MANAGER" set-password nas-b-home
sudo -u synology-drive-sync -- "$MANAGER" set-totp nas-b-home
```

`set-totp` accepts the existing DSM Base32 manual seed or original `otpauth://` URI, not a current
six-digit code. It is needed only for a target account using authenticator-app TOTP. Secure SignIn
approval and hardware/security-key challenges are not supported by the File Station login flow.
Synchronize both NAS clocks before testing TOTP.

For automation during provisioning, `set-password NAME --from-file FILE` and
`set-totp NAME --from-file FILE` accept a readable, non-symlink file whose first line is the secret.
The input file is not retained by the manager and should be securely removed by the operator. Do
not put password or TOTP values in command arguments, the generated TOML, environment files, or
logs.

Package directories are mode `0700` and generated configuration/secret files are mode `0600` under
the package identity. The installed profiles use protected files and `no-vault = true`, avoiding a
headless Secret Service dependency.

## Diagnose, plan, run, and schedule

Start with non-mutating checks and a reviewed plan:

```bash
sudo -u synology-drive-sync -- "$MANAGER" doctor nas-b-home
sudo -u synology-drive-sync -- "$MANAGER" plan nas-b-home
sudo -u synology-drive-sync -- "$MANAGER" run nas-b-home
```

`doctor` hashes the entire local source, then authenticates and inventories the target. Add
`--write-test` only for a prepared disposable destination: it creates, verifies, and removes a
uniquely named probe. `plan` does not mutate the target. `run` performs an additive/update-only sync
by default.

Create as many independent profiles as needed. Each profile can use a different local source,
remote URL, account, remote folder, password, and TOTP seed:

```bash
sudo -u synology-drive-sync -- "$MANAGER" list-profiles
sudo -u synology-drive-sync -- "$MANAGER" doctor --all
sudo -u synology-drive-sync -- "$MANAGER" plan --all
sudo -u synology-drive-sync -- "$MANAGER" run --all
sudo -u synology-drive-sync -- "$MANAGER" run --all \
  --allow-delete --max-total-delete 25
```

With no profile argument, `doctor`, `plan`, and `run` use the selected default profile; only an
explicit `--all` selects every profile. `--write-test` is accepted only by `doctor`, while
`--allow-delete` is accepted only by `plan`/`run`; command-inapplicable options and trailing
arguments fail with usage status instead of being ignored.

All-profile operations use the core engine's deterministic profile-name order, all-target
preflight, overlap checks, sequential job execution, per-profile deletion limits, and aggregate
deletion cap. A foreground `--all` deletion accepts an independent one-off
`--max-total-delete N` (default `100`); it does not borrow the scheduler's cap. They are not a
transaction: if a later job fails after an earlier job succeeded, the earlier target remains
changed. Equal or nested roots on the same normalized URL are rejected;
different hostnames or proxy prefixes that route to the same NAS cannot be recognized as aliases,
so use one canonical File Station URL per target NAS and keep those roots disjoint manually.

Profile maintenance is also explicit:

```bash
sudo -u synology-drive-sync -- "$MANAGER" set-default nas-b-team
sudo -u synology-drive-sync -- "$MANAGER" show-config nas-b-team
sudo -u synology-drive-sync -- "$MANAGER" remove-totp nas-b-team
sudo -u synology-drive-sync -- "$MANAGER" remove-profile nas-b-team
```

`show-config` is non-secret. `remove-totp` removes only that profile's stored seed. Removing a
profile also removes its package-owned password/TOTP files and rebuilds the generated config; it
does not touch either NAS data tree. Removing the final profile first disables scheduling so the
controller cannot wake repeatedly with no runnable configuration. Profile, credential, and
schedule mutations are serialized and refuse to run while a plan/sync holds the package run lock.

Enable the package's persistent interval scheduler only after the diagnostics and plan succeed:

```bash
sudo -u synology-drive-sync -- "$MANAGER" enable --interval 3600
sudo synopkg start synology-drive-sync
sudo -u synology-drive-sync -- "$MANAGER" status
```

The interval is `60..2592000` seconds. Enabling does not trigger an immediate run; the first run is
due after one interval. The cadence is delay-after-completion: the next deadline is one full
interval after the preceding job finishes, so long jobs do not overlap. Changing an enabled
interval rebases the pending deadline. One package-owned lock rejects overlapping manual, planned,
and scheduled runs. This is a host-local lock only and cannot coordinate another NAS, container,
process, Drive client, or File Station user.

To stop new scheduled runs without interrupting an active one:

```bash
sudo -u synology-drive-sync -- "$MANAGER" disable
```

Package Center Stop, or `sudo synopkg stop synology-drive-sync`, requests cooperative termination of
the verified controller and any verified scheduled or manual plan/sync process. The lifecycle
script retains the run lock and waits up to 120 seconds by default for the core to exit; it refuses
to signal an untrusted PID or force-kill a process after a timeout.

## Deletion safety

Profiles are additive by default. Scheduled deletion requires two independent opt-ins:

1. configure the profile with `--delete --max-delete N` using an intentionally small per-profile
   cap; and
2. run `enable --allow-delete --max-total-delete N` for the scheduler, or add `--allow-delete` to a
   specific reviewed manual `run`.

For example, after disposable acceptance:

```bash
sudo -u synology-drive-sync -- "$MANAGER" configure-profile \
  --name nas-b-team \
  --source '/volume1/Projects' \
  --url 'https://files-b.example.com/nas/' \
  --username 'project-mirror' \
  --remote '/team-folder/Chosen Folder/Projects' \
  --delete --max-delete 5

sudo -u synology-drive-sync -- "$MANAGER" plan nas-b-team --allow-delete
sudo -u synology-drive-sync -- "$MANAGER" plan --all \
  --allow-delete --max-total-delete 5
sudo -u synology-drive-sync -- "$MANAGER" enable \
  --interval 3600 --allow-delete --max-total-delete 5
```

Without the manager-level opt-in, the package runner passes `--no-delete` even when a profile has
`delete = true`. The core additionally enforces destination containment, empty-source protection,
DSM-managed-path protection, remote mount boundaries, per-profile and aggregate caps, fresh remote
snapshot checks, and failure-before-delete ordering. These checks reduce risk but do not make File
Station operations atomic; retain snapshots/version history and enforce a single-writer window.

## Status, logs, and private paths

```bash
sudo -u synology-drive-sync -- "$MANAGER" status
sudo -u synology-drive-sync -- "$MANAGER" logs 200
sudo -u synology-drive-sync -- "$MANAGER" show-config nas-b-home
sudo -u synology-drive-sync -- "$MANAGER" paths
```

The principal paths are:

| Purpose | Path |
| --- | --- |
| Manager | `/var/packages/synology-drive-sync/target/bin/sdsync-dsm` |
| Core binary | `/var/packages/synology-drive-sync/target/bin/synology-drive-sync` |
| Generated config | `/var/packages/synology-drive-sync/home/config/config.toml` |
| Profile fragments | `/var/packages/synology-drive-sync/home/config/profiles.d/` |
| Password/TOTP files | `/var/packages/synology-drive-sync/home/secrets/` |
| Schedule | `/var/packages/synology-drive-sync/home/config/schedule.conf` |
| State and locks | `/var/packages/synology-drive-sync/var/state/`, `/var/packages/synology-drive-sync/var/run/` |
| Package logs | `/var/packages/synology-drive-sync/var/log/` |
| DSM package-control log | `/var/log/packages/synology-drive-sync.log` |

`logs` returns at most 1,000 lines from controller, scheduler, and sync logs. Controller and
scheduler logs rotate at 10 MiB with five backups; the core sync log uses its documented 10 MiB,
three-backup policy. Rotation refuses symlinked log paths. Status records the controller PID,
scheduler state, next run, active run, last scope, exit code, and timestamps. Connect nonzero
scheduled results to an external alert; the package records failures but does not provide an alert
transport by itself.

For automation, the manager's package-wrapper statuses are:

| Exit | Meaning |
| --- | --- |
| `0` | wrapper command succeeded |
| `64` | invalid command, option, argument, or validated input |
| `66` | required profile, configuration, or protected credential is absent |
| `69` | installed core executable is missing, symlinked, or not executable |
| `73` | unsafe package path, state, lock, log, or untrusted PID |
| `75` | another management or plan/sync operation is active |
| `77` | command was not run as the DSM package identity |
| `130`/`143` | interrupted management or terminated plan/sync operation |

Core planning, transport, authentication, and File Station failures propagate their own nonzero
status and remain operator failures rather than retryable wrapper contention. Treat `75` as a
bounded retry only after confirming the recorded operation is expected; investigate every other
nonzero status and inspect a fresh plan before rerunning a mutating command.

## Upgrade, rollback, and uninstall

Verify the new architecture-specific SPK and stop the package before a manual upgrade. Package
Center upgrades retain private configuration, credentials, schedule, state, and logs, then validate
the existing config with the new binary. Restart it, run `doctor`, and review a fresh additive plan
before allowing the schedule to continue.

An older SPK can be used only when DSM permits the version transition. Binary/package rollback does
not undo any remote writes already completed. Preserve the previous verified SPK and recovery
evidence before upgrading.

Before uninstalling, disable scheduling and stop the package:

```bash
sudo -u synology-drive-sync -- "$MANAGER" disable
sudo synopkg stop synology-drive-sync
```

The uninstaller and upgrader refuse to proceed while the controller or a foreground plan/sync PID
is live. A completed uninstall permanently removes this package's generated configuration,
credentials, state, locks, and logs from its exact private FHS directories. It does **not** delete
or modify the local source share, target NAS data, remote account, snapshots, or shared-folder ACLs.
Export any non-secret configuration and retain required audit logs before uninstalling; deleted
package credentials are not recoverable.

## Build and validate an SPK locally

Build on matching Linux architecture with Rust 1.88 or newer and a musl compiler. For x86-64:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
bash packaging/synology/build-spk.sh \
  --binary target/x86_64-unknown-linux-musl/release/synology-drive-sync \
  --arch x86_64 \
  --version 26.1 \
  --output dist
python3 packaging/synology/validate_spk.py \
  --binary target/x86_64-unknown-linux-musl/release/synology-drive-sync \
  --arch x86_64 \
  dist/synology-drive-sync-26.1-x86_64.spk
```

For ARMv8, use a matching ARM64 Linux builder and replace the Rust target with
`aarch64-unknown-linux-musl` and `--arch` with `armv8`. The builder accepts one regular, non-symlink
static ELF, rejects an architecture mismatch or dynamic dependency, assembles deterministic archive
metadata under `SOURCE_DATE_EPOCH`, and emits one architecture-specific SPK. The validator checks
the INFO, icons, licenses, privilege policy, lifecycle scripts, safe archive members, executable
modes, and embedded ELF contract. This static validation is not an installation test.

## No remote-to-remote shortcut

The package is useful when it is installed on the NAS that physically holds the source. It reads
that local filesystem and uploads to the other NAS. File Station exposes server-side copy/move only
inside one authenticated target NAS; there is no direct File Station operation that transfers a
file from NAS A to NAS B.

Consequently, a safe same-target rename optimization may avoid retransmission only when matching
content already exists on NAS B. It cannot copy bytes directly from NAS A's File Station service to
NAS B. Installing the package on a third NAS would require the source to be mounted locally and
managed separately, or a future remote-source download adapter.

The SPK does not add a persistent hash/path database. Every run rebuilds correspondence from a
fresh local scan, the current remote File Station inventory, and the content hashes needed by its
comparison/safety policy; it verifies uploaded bytes and finishes with a fresh reconciliation. A
unique matching remote file can be reused only through the core's guarded same-target, cross-parent,
same-basename server-copy case; ambiguous duplicates, basename changes, and cross-NAS content fall
back to a verified upload.

## Live two-NAS acceptance

Before production use, complete the general [live-NAS acceptance and recovery runbook](production-acceptance.md)
with the package installed on the exact source NAS. Additionally record and prove:

- source model, architecture, DSM build, installed SPK name/checksum/attestation, and Package Center
  install/upgrade/start/stop status;
- that `synology-drive-sync` has read-only access to only the intended source share and
  `doctor --all` succeeds as that package identity;
- each chosen logical destination, including a missing nested directory that sync creates beneath
  an existing writable parent, while an out-of-scope sibling canary remains unchanged;
- one `/home/Drive/...` destination when user-home sync is intended, plus visible Drive indexing;
- every remote URL, account, password file, optional TOTP seed, and reverse-proxy prefix under a
  scheduled non-interactive run;
- service restart after a source-NAS reboot, interval scheduling, non-overlap, log rotation, failure
  state, and alert delivery;
- safe upgrade with configuration retained and config validation passing;
- uninstall behavior on a disposable installation, including package-private secret removal and
  preservation of both source and target data.

TOTP challenge behavior and reverse-proxy path/timeout behavior vary with the deployed DSM,
File Station, identity provider, and proxy configuration. Mock tests cannot establish those
combinations. A successful File Station sync also does not establish that Synology Drive indexed or
replicated the result; verify that separately through Drive.

## Official DSM package references

- [DSM Package Developer Guide](https://help.synology.com/developer-guide/)
- [DSM 7 package privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html)
- [DSM package FHS](https://help.synology.com/developer-guide/integrate_dsm/fhs.html)
- [DSM 7 package-framework changes](https://help.synology.com/developer-guide/breaking_changes.html)
- [Synology File Station API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Package/FileStation/All/enu/Synology_File_Station_API_Guide.pdf)
