# DSM 7 package

This directory builds a manually installable, headless Synology DSM 7 package. Install it on the NAS that owns the authoritative source directory; the packaged service reads that directory locally and sends changes over HTTPS to any destination accepted by the remote NAS File Station WebAPI.

The destination is not fixed or pre-provisioned in package code. Each profile chooses its own File Station **logical** path:

- `/home/Drive/Backups` targets the authenticated remote account's Synology Drive home. The remote NAS must already have the user-home service and that account's home provisioned. The sync engine creates missing descendants such as `Backups`, but it cannot enable the DSM home service or create another user's home.
- `/TeamShare/Backups` targets a shared folder or Drive Team Folder on the remote NAS. The DSM account must have File Station and write permission there.
- Never use a remote physical path such as `/volume1/homes/alice/Drive/Backups`.

The source is independently selectable for every profile and is a physical local path visible on the package NAS, such as `/volume1/Photos`. Grant the `synology-drive-sync` system-internal package user read/traverse permission in **Control Panel > Shared Folder > Edit > Permission > System internal user**. The package never requests root or capabilities.

## Build and validate

Supply a fully static, little-endian ELF64 Linux binary. The builder rejects a wrong machine type, a dynamic interpreter, `DT_NEEDED`, malformed program headers, or an ELF without an executable load segment.

```sh
bash packaging/synology/build-spk.sh \
  --binary dist/x86_64-unknown-linux-musl/synology-drive-sync \
  --arch x86_64 --version v0.1.0 --output dist/spk

bash packaging/synology/build-spk.sh \
  --binary dist/aarch64-unknown-linux-musl/synology-drive-sync \
  --arch armv8 --version v0.1.0 --output dist/spk

python packaging/synology/validate_spk.py \
  --arch x86_64 dist/spk/synology-drive-sync-0.1.0-x86_64.spk
python packaging/synology/test_synology_package.py
```

Artifacts are named `synology-drive-sync-VERSION-x86_64.spk` and `synology-drive-sync-VERSION-armv8.spk`; a leading `v` is removed. A semantic version such as `0.1.0` is rendered as DSM version `0.1.0-1` in `INFO`. `SOURCE_DATE_EPOCH` controls every tar member and the inner gzip header for reproducible output.

The SPK contains the project license, generated third-party notices, and musl's upstream `COPYRIGHT` both in the outer package and under the installed `share/licenses` directory.

## Install and initial configuration

1. In DSM Package Center, choose **Manual Install** and select the SPK for the NAS architecture. DSM warns that this is a third-party package; that is expected for a package not distributed by Synology.
2. Start the package. The controller starts safely with scheduling disabled.
3. Enable SSH temporarily and sign in as an administrator. The concrete management entry point is:

   ```sh
   MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
   sudo "$MANAGER" paths
   ```

4. Grant the system-internal package user read access to the desired source shared folder, then configure one or more targets:

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

`silent_install=yes` and `silent_upgrade=yes` are intentional: there is no install wizard carrying credentials or paths. This supports Package Center/CMS installation without placing secrets in wizard environment variables. Configuration remains an explicit, auditable post-install SSH operation. Uninstallation is not silent because it permanently purges package-owned profiles, credentials, state, and logs.

## Credentials

Secret values are never accepted as command-line values or embedded in TOML. The manager copies the first line from a masked prompt, standard input, or a non-symlink file into the private DSM package home using mode `0600` and atomic replacement:

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

## Scheduler and service management

The DSM Package Center start/stop actions control one long-lived, unprivileged controller. Scheduling remains disabled until explicitly enabled:

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

Static validation proves archive structure, lower privilege, modes, architecture, static linkage, lifecycle contracts, and reproducibility. Before relying on the package, test its exact NAS model and DSM version with a disposable source and target, including reverse-proxy upload limits, TLS trust, TOTP clock synchronization, large files, Drive indexing, restart during a long transfer, upgrade, and uninstall. A manually built SPK is not automatically a Synology Package Center-approved release.

Official framework references: [package structure](https://help.synology.com/developer-guide/synology_package/introduction.html), [privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html), [FHS paths](https://help.synology.com/developer-guide/integrate_dsm/fhs.html), and [lifecycle status codes](https://help.synology.com/developer-guide/synology_package/scripts.html).
