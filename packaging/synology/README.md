# DSM 7 package

This directory builds a manually installable Synology DSM 7 package with a native
administrator-only desktop application and the `sdsync-dsm` SSH manager. Install it on the NAS that
owns the authoritative source directory; the package reads that directory locally and sends changes
over HTTPS to a destination accepted by the remote NAS File Station WebAPI.

The destination is not fixed or pre-provisioned in package code. Each profile chooses its own File Station **logical** path:

- `/home/Drive/Backups` targets the authenticated remote account's Synology Drive home. The remote NAS must already have the user-home service and that account's home provisioned. The sync engine creates missing descendants such as `Backups`, but it cannot enable the DSM home service or create another user's home.
- `/TeamShare/Backups` targets a shared folder or Drive Team Folder on the remote NAS. The DSM account must have File Station and write permission there.
- Never use a remote physical path such as `/volume1/homes/alice/Drive/Backups`.

The source is independently selectable for every profile and is a physical local path visible on the package NAS, such as `/volume1/Photos`. Grant the `synology-drive-sync` system-internal package user read/traverse permission in **Control Panel > Shared Folder > Edit > Permission > System internal user**. The package never requests root or capabilities. The dashboard covers profiles, protected-secret state, per-profile routines, Doctor, health, activity, bounded logs, DSM notifications, and non-secret display settings. See the [complete DSM package and dashboard guide](../../docs/synology-package.md).

## Build and validate

Supply two fully static, little-endian Linux ELFs matching the selected release architecture: the
core `synology-drive-sync` and compiled `sdsync-dsm-api` helper. The builder rejects the wrong ELF
class or machine, a dynamic interpreter, `DT_NEEDED`, malformed program headers, or an ELF without
an executable load segment. ARMv7 is additionally required to be EABI5 hard-float; an ARM
soft-float binary cannot be made compatible by changing its filename.

| `--arch` | Rust target | ELF contract | DSM `INFO` arch value |
| --- | --- | --- | --- |
| `x86_64` | `x86_64-unknown-linux-musl` | ELF64, `EM_X86_64` | `x86_64` |
| `i686` | `i686-unknown-linux-musl` | ELF32, `EM_386` | `i686` |
| `armv7` | `armv7-unknown-linux-musleabihf` | ELF32, `EM_ARM`, EABI5 hard-float | `armv7 armada370 armada375 armada38x armadaxp comcerto2k monaco` |
| `armv8` | `aarch64-unknown-linux-musl` | ELF64, `EM_AARCH64` | `armv8` |

```sh
bash packaging/synology/build-spk.sh \
  --binary dist/x86_64-unknown-linux-musl/synology-drive-sync \
  --api-binary dist/x86_64-unknown-linux-musl/sdsync-dsm-api \
  --arch x86_64 --version v0.1.0 --output dist/spk

bash packaging/synology/build-spk.sh \
  --binary dist/aarch64-unknown-linux-musl/synology-drive-sync \
  --api-binary dist/aarch64-unknown-linux-musl/sdsync-dsm-api \
  --arch armv8 --version v0.1.0 --output dist/spk

bash packaging/synology/build-spk.sh \
  --binary dist/armv7-unknown-linux-musleabihf/synology-drive-sync \
  --api-binary dist/armv7-unknown-linux-musleabihf/sdsync-dsm-api \
  --arch armv7 --version v0.1.0 --output dist/spk

python packaging/synology/validate_spk.py \
  --binary dist/x86_64-unknown-linux-musl/synology-drive-sync \
  --api-binary dist/x86_64-unknown-linux-musl/sdsync-dsm-api \
  --arch x86_64 dist/spk/synology-drive-sync-0.1.0-x86_64.spk
python packaging/synology/test_synology_ui.py
python packaging/synology/test_synology_package.py
```

Artifacts are named `synology-drive-sync-VERSION-ARCH.spk`, where `ARCH` is `x86_64`, `i686`,
`armv7`, or `armv8`; a leading `v` is removed. A semantic version such as `0.1.0` is rendered as
DSM version `0.1.0-1` in `INFO`. `SOURCE_DATE_EPOCH` controls every tar member and the inner gzip
header for reproducible output.

The SPK archives every executable, including `ui/api.cgi`, without setuid/setgid bits; the CGI is
stored as ordinary `0755`. `conf/privilege` requests no root run-as or Linux capabilities and tells
DSM to assign that one installed CGI to the non-root package user/group before applying `4755`.
This ordering avoids Package Center rejecting a pre-install root-owned setuid archive entry. The
validator rejects any outer or inner setuid/setgid member and also rejects weakening or broadening
the exact package-owned installed-tool manifest.

Linux reports a compatible 32-bit ARM NAS as `armv7l`, but `armv7l` is not a valid package-builder
argument or DSM `INFO` family. Select the `armv7` artifact. Its `INFO` includes the unified `armv7`
family used for Alpine platforms and the exact aliases which Synology's DSM 7 toolkit does not
unify. The package has no kernel module, so all aliases use the same validated userspace binary.
ARMv5/88f628x and PowerPC devices are not part of this DSM 7 package: their official toolchains and
supported DSM generations require a separate legacy package design.

The SPK contains the project license, generated third-party notices, and musl's upstream `COPYRIGHT` both in the outer package and under the installed `share/licenses` directory.

## Install and initial configuration

1. In DSM Package Center, choose **Manual Install** and select the SPK for the NAS architecture. DSM warns that this is a third-party package; that is expected for a package not distributed by Synology.
2. Start the package. The controller starts safely with scheduling disabled.
3. Open **Synology Drive Sync** from the DSM desktop or Package Center. A fresh administrator launch
   must supply DSM SynoToken; if the dashboard fails closed, use the CLI and record this physical-NAS
   acceptance gap.
4. Enable SSH temporarily for ACL verification and recovery. The management entry point is:

   ```sh
   MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
   sudo -u synology-drive-sync -- "$MANAGER" paths
   ```

5. Grant the system-internal package user read access to the desired source shared folder. Configure
   the targets graphically, or use the equivalent CLI:

   ```sh
   sudo -u synology-drive-sync -- "$MANAGER" configure-profile \
     --name personal \
     --source '/volume1/Photos' \
     --url 'https://files.remote.example/nas/' \
     --username 'mirror-bot' \
     --remote '/home/Drive/Preferred Backup' \
     --default

   sudo -u synology-drive-sync -- "$MANAGER" configure-profile \
     --name archive \
     --source '/volume1/Documents' \
     --url 'https://files.archive.example/' \
     --username 'archive-bot' \
     --remote '/ArchiveTeam/Documents'
   ```

`configure-profile` atomically regenerates a strict non-secret TOML file. Quotes, backslashes, control lines, relative sources, remote `/`, and `.`/`..` remote segments are rejected. Sources are canonicalized and cannot be the filesystem root, overlap package storage, or sit inside DSM-managed trees. DSM-managed components (`@eaDir`, `#recycle`, `#snapshot`, `@tmp`, `@sharebin`, `@apphome`, `@appdata`, `@appstore`, `@apptemp`, `@appconf`, and `.SynologyWorkingDirectory`) are rejected as source/remote roots and excluded while scanning. Remote components also enforce Synology Drive/Windows portability limits: no leading `~`, control or reserved characters, reserved device names, trailing dot/space, or paths longer than 247 characters. Updating a profile retains its protected credentials.

`silent_install=yes` and `silent_upgrade=yes` are intentional: there is no install wizard carrying credentials or paths. This supports Package Center/CMS installation without placing secrets in wizard environment variables. Configuration remains an explicit, auditable post-install dashboard or package-identity CLI operation. Uninstallation is not silent because it permanently purges package-owned profiles, credentials, state, queue, and logs.

## Credentials

Secret values are never returned to the dashboard, accepted as command-line values, or embedded in
TOML. The dashboard exposes only stored/not-stored presence plus explicit keep/replace/clear modes.
The manager copies the first line from a masked prompt, standard input, or a non-symlink file into
the private DSM package home using mode `0600` and atomic replacement:

```sh
sudo -u synology-drive-sync -- "$MANAGER" set-password personal
sudo -u synology-drive-sync -- "$MANAGER" set-totp personal

# --from-file is also available when FILE is a protected, non-symlink file
# readable by the package identity; delete that input after the copy succeeds.
```

Use a dedicated, non-administrator account on each remote NAS. A stored TOTP seed enables unattended login but puts both factors under the security boundary of this package account. Secure SignIn push approval and hardware-key interaction are not unattended mechanisms supported by this package.

## Diagnose, review, and run

```sh
sudo -u synology-drive-sync -- "$MANAGER" list-profiles
sudo -u synology-drive-sync -- "$MANAGER" show-config personal       # core command redacts secret values
sudo -u synology-drive-sync -- "$MANAGER" doctor personal            # source hash scan + non-mutating target check
sudo -u synology-drive-sync -- "$MANAGER" plan personal              # deletion forced off
sudo -u synology-drive-sync -- "$MANAGER" run personal               # one foreground sync, deletion forced off
sudo -u synology-drive-sync -- "$MANAGER" plan --all
sudo -u synology-drive-sync -- "$MANAGER" run --all
sudo -u synology-drive-sync -- "$MANAGER" run --all --allow-delete --max-total-delete 25
```

When no profile name is supplied, `doctor`, `plan`, and `run` use the selected default profile; only explicit `--all` batches every profile. `doctor --write-test` is an explicit mutating target probe and should be used only against a disposable prepared destination. Exact-mirror profiles require both `configure-profile --delete --max-delete N` and `plan/run --allow-delete`; an all-profile foreground deletion may additionally set a one-off aggregate bound with `--max-total-delete N` (default 100). Scheduled deletion separately requires `enable --allow-delete`, and its aggregate bound comes from `enable --max-total-delete N`. Without every layer, remote-only entries are preserved.

All profile, secret, and scheduler mutations use a package-manager lock and refuse to race an active plan/sync. Foreground and scheduled jobs share a separate PID lock. Stale locks are recovered only when their recorded PID is no longer alive, and lock directories are removed with `rmdir`, never recursive deletion.

## Routines, scheduler, and service management

The dashboard configures independent interval, daily-window, or realtime routines for each profile,
including debounce, polling fallback, retries/backoff, dependencies, and layered deletion approval.
The CLI retains this legacy all-profile interval schedule. DSM Package Center start/stop controls one
long-lived, unprivileged controller, and all automation remains disabled until explicitly enabled:

```sh
sudo -u synology-drive-sync -- "$MANAGER" enable --interval 3600
sudo -u synology-drive-sync -- "$MANAGER" status
sudo -u synology-drive-sync -- "$MANAGER" logs 200
sudo -u synology-drive-sync -- "$MANAGER" disable
```

The interval range is 60 seconds through 30 days. Enabling never triggers an immediate mutation; use `run` after reviewing `plan`, or wait one interval. The cadence is delay-after-completion: after a scheduled job finishes, the next job is due one full interval later, so long jobs never overlap. Changing the interval rebases the pending deadline. A failed scheduled job is recorded and is not immediately retried. The controller checks schedule changes within 30 seconds, forwards TERM to the active job, waits for graceful core shutdown, and is never force-killed by the package script. Package Center stop also waits for a verified foreground run. Logs rotate at 10 MiB with five backups; core logs have their own built-in rotation.

Status and logs are also available over SSH:

```sh
sudo synopkg status synology-drive-sync
sudo synopkg start synology-drive-sync
sudo synopkg stop synology-drive-sync
sudo tail -n 200 /var/log/packages/synology-drive-sync.log
```

## Upgrade and uninstall

DSM stops a started package before upgrade. The lifecycle scripts additionally refuse upgrade or uninstall while either the controller or a foreground run PID is live. Configuration and credentials are retained across upgrade and validated against the new binary before service restart.

On a real uninstall (`SYNOPKG_PKG_STATUS=UNINSTALL`), the post-uninstall script removes only this package's configuration, credentials, runtime state, and logs under `/var/packages/synology-drive-sync/{home,var}`. It does not touch any source directory or remote NAS data. This purge is permanent.

## Acceptance boundary

Static validation proves archive structure, lower privilege, modes, architecture, static linkage,
dashboard/bridge contracts, lifecycle behavior, and deterministic assembly. Before relying on the
package, test its exact NAS model and DSM version with a disposable source and target, including
Package Center installation, rendered dashboard behavior, AppLaunch SynoToken forwarding,
authenticate.cgi/admin/CSRF rejection cases, reverse-proxy upload limits, TLS trust, TOTP clock
synchronization, routines, notifications, large files, Drive indexing, restart during a long
transfer, upgrade, and uninstall. Rendered browser QA and physical installation remain unverified. A
manually built SPK is not automatically a Synology Package Center-approved release.

Official framework references: [package structure](https://help.synology.com/developer-guide/synology_package/introduction.html), [architecture mapping](https://help.synology.com/developer-guide/appendix/platarchs.html), [privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html), [FHS paths](https://help.synology.com/developer-guide/integrate_dsm/fhs.html), and [lifecycle status codes](https://help.synology.com/developer-guide/synology_package/scripts.html).
