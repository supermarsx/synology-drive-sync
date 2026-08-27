# DSM 7 package

This directory builds the current manually installable Synology DSM 7 package source. Release 26.10
introduced its administrator-only native Vue AppWindow. The package also includes the
`sdsync-dsm` SSH manager. Install it on the NAS that
owns the authoritative source directory; the package reads that directory locally and sends changes
over HTTPS to a destination accepted by the remote NAS File Station WebAPI.

The destination is not fixed or pre-provisioned in package code. Each profile chooses its own File Station **logical** path:

- `/home/Drive/Backups` targets the authenticated remote account's Synology Drive home. The remote NAS must already have the user-home service and that account's home provisioned. The sync engine creates missing descendants such as `Backups`, but it cannot enable the DSM home service or create another user's home.
- `/TeamShare/Backups` targets a shared folder or Drive Team Folder on the remote NAS. The DSM account must have File Station and write permission there.
- Never use a remote physical path such as `/volume1/homes/alice/Drive/Backups`.

The source is independently selectable for every profile and is a physical local path visible on the package NAS, such as `/volume1/Photos`. Grant the actual package identity shown under **Control Panel > Shared Folder > Edit > Permission > System internal user** read/traverse permission; DSM may collision-rename its NSS username. The package never requests root or capabilities. The dashboard covers profiles, protected-secret state, per-profile routines, Doctor, health, activity, bounded logs, direct DSM desktop alerts, and non-secret display settings. See the [complete DSM package and dashboard guide](../../docs/synology-package.md).

> [!WARNING]
> Do not install the immutable 26.5 or 26.6 SPKs. Release 26.5 is setid/privilege-invalid. Release
> 26.6 removed setid but affected DSM installs reject its `conf/resource` `sysnotify` acquisition
> worker with `pkgmgr_worker_violation`. Use 26.7 or later only when that release is published and
> its exact SPK/checksum are verified; a local build is not physical-DSM installation proof.

## Build and validate

Build the pinned native DSM UI first, then supply two fully static, little-endian Linux ELFs matching
the selected release architecture: the core `synology-drive-sync` and compiled `sdsync-dsm-api`
helper. The builder rejects a missing or empty bundle, a mismatched AppWindow/config definition, the wrong ELF
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
cd packaging/synology/ui-src
pnpm install --frozen-lockfile --ignore-scripts
pnpm run build
cd ../../..
git diff --exit-code -- packaging/synology/ui-src/dist

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
header for reproducible output. The builder validates `ui-src/app.config` and `config.define`, then
deterministically renders the DSM toolkit-equivalent module wrapper under `ui/config` and packages
`ui/SynologyDriveSync.js` plus `ui/style.css`. The package deliberately has no standalone
`ui/index.html` or undocumented `launchApp` redirect. DSM launches the application through the
registered `dsmappname` and installed `ui/config` AppWindow module; the Webman third-party mapping
exists for registered assets and `api.cgi`, not as a directory-index application.

Every executable, including `ui/api.cgi`, is ordinary `0755` in both the archive and installed
package. Nothing carries a set-user-ID/set-group-ID bit. `conf/privilege` contains only
`defaults.run-as: package`; it requests no root run-as, joined DSM group, tool privilege, or Linux
capability. The ordinary CGI fails closed unless Webman starts it with real/effective UID equal to
its exact non-root package owner; the `sdsync-dsm-api --serve` daemon uses that package UID through
the package run-as contract. The CGI first validates the fixed root-owned `authenticate.cgi`. It
executes the helper when the kernel permits that package UID to do so; if the trusted helper is
kernel-inaccessible, including the observed DSM `root:system 0750` mode, it performs one bounded
loopback-only `SYNO.Core.Desktop.Initdata` `get_user_service` request using the current DSM cookie.
The response must contain a valid `Session.user` and exact `is_admin=true`, and the CGI then
independently resolves NSS identity and `administrators` membership. It relays one bounded assertion
and request through fixed
`/var/packages/synology-drive-sync/var/run/api.sock`, owned by the package identity and mode `0600`.
Mutable socket state never enters the installed/Webman-exposed `target/ui` tree. Both peers verify
socket metadata and kernel peer identity; the daemon never executes DSM's root-owned authenticator.
The validator rejects any privilege-bearing archive member or broader privilege manifest. The
fallback pins the destination to literal IPv4 loopback and the current CGI server port, disables
proxying and redirects, bounds time and response bytes, and never puts a cookie or token in the URL.
It does not trust `SERVER_NAME`, another host, or a redirected peer.

Synology publicly documents the direct `authenticate.cgi` custom-CGI path. The user-service response
shape above is DSM runtime behavior observed in the supplied physical capture, not a public API
compatibility promise. Physical-NAS acceptance must therefore re-prove it on each supported DSM
branch rather than treating repository fixtures as platform proof.

The corrected package contains no `conf/resource` acquisition worker or sysnotify mail templates.
Optional alerts invoke `/usr/syno/bin/synodsmnotify -c` with only the fixed application ID
`SYNO.SDS.App.SynologyDriveSync.Instance`,
administrator recipient, and preloaded title/message I18N keys. They are DSM-desktop-only: profile,
exit, path, account, log, and secret details remain in Activity and bounded logs, and no Notification
Center email, SMS, mobile, CMS, or rule/channel delivery is registered.

Linux reports a compatible 32-bit ARM NAS as `armv7l`, but `armv7l` is not a valid package-builder
argument or DSM `INFO` family. Select the `armv7` artifact. Its `INFO` includes the unified `armv7`
family used for Alpine platforms and the exact aliases which Synology's DSM 7 toolkit does not
unify. The package has no kernel module, so all aliases use the same validated userspace binary.
ARMv5/88f628x and PowerPC devices are not part of this DSM 7 package: their official toolchains and
supported DSM generations require a separate legacy package design.

The SPK contains the project license, generated Rust dependency notices, DSM AppWindow bundled-code
notices, and musl's upstream `COPYRIGHT` both in the outer package and under the installed
`share/licenses` directory. Vue is externalized to DSM; UI packages used only during the build are
not presented as shipped runtime dependencies.

## Install and initial configuration

1. For the native AppWindow flow below, use a verified 26.10-or-later SPK. Releases 26.7-26.9 retain
   their originally published UI. In DSM Package Center, choose
   **Manual Install** and select the SPK for the NAS architecture. DSM
   normally warns that this is a third-party package; that publisher-trust warning is expected for a
   package not distributed by Synology. A refusal saying root or lower privileges are required is a
   different, unexpected result: preserve the asset and collect its SHA-256, `INFO`, member list,
   `/var/log/synopkg.log`, `/var/log/messages`, and the package log exactly as described in the
   troubleshooting guide. Preserve `pkgmgr_worker_violation`, resource names, phases, and timestamps.
2. Start the package. The package-user API service and controller start safely with scheduling
   disabled.
3. Open the native **Synology Drive Sync** AppWindow from the DSM desktop or Package Center. The
   DSM-launched CGI authenticates the current session cookie through the validated direct helper or,
   only when kernel permissions deny execution of that otherwise trusted helper, through the bounded
   loopback user-service path. The package service then
   independently verifies the package-UID peer, account identity and administrator membership,
   recomputed session binding, policy, and package CSRF. It never executes DSM's protected
   authenticator. The native UI does not inspect or rewrite the DSM shell location and sends no
   `SynoToken`. If session authentication or the bridge fails, use the CLI and record the
   physical-NAS evidence.
4. Enable SSH temporarily for ACL verification and recovery. Resolve the actual package owner as
   shown in [CLI parity](../../docs/dsm/cli-parity.md#discover-the-actual-package-identity); the
   management entry point is:

   ```sh
   PACKAGE_USER=$(stat -L -c '%U' /var/packages/synology-drive-sync/home)
   case "$PACKAGE_USER" in ''|root|UNKNOWN) echo 'unsafe package owner' >&2; exit 1 ;; esac
   MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
   sudo -u "$PACKAGE_USER" -- "$MANAGER" paths
   ```

5. Grant the system-internal package user read access to the desired source shared folder. Configure
   the targets graphically, or use the equivalent CLI:

   ```sh
   sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-profile \
     --name personal \
     --source '/volume1/Photos' \
     --url 'https://files.remote.example/nas/' \
     --username 'mirror-bot' \
     --remote '/home/Drive/Preferred Backup' \
     --default

   sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-profile \
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
sudo -u "$PACKAGE_USER" -- "$MANAGER" set-password personal
sudo -u "$PACKAGE_USER" -- "$MANAGER" set-totp personal

# --from-file is also available when FILE is a protected, non-symlink file
# readable by the package identity; delete that input after the copy succeeds.
```

Use a dedicated, non-administrator account on each remote NAS. A stored TOTP seed enables unattended login but puts both factors under the security boundary of this package account. Secure SignIn push approval and hardware-key interaction are not unattended mechanisms supported by this package.

## Diagnose, review, and run

```sh
sudo -u "$PACKAGE_USER" -- "$MANAGER" list-profiles
sudo -u "$PACKAGE_USER" -- "$MANAGER" show-config personal       # core command redacts secret values
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal            # source hash scan + non-mutating target check
sudo -u "$PACKAGE_USER" -- "$MANAGER" plan personal              # deletion forced off
sudo -u "$PACKAGE_USER" -- "$MANAGER" run personal               # one foreground sync, deletion forced off
sudo -u "$PACKAGE_USER" -- "$MANAGER" plan --all
sudo -u "$PACKAGE_USER" -- "$MANAGER" run --all
sudo -u "$PACKAGE_USER" -- "$MANAGER" run --all --allow-delete --max-total-delete 25
```

When no profile name is supplied, `doctor`, `plan`, and `run` use the selected default profile; only explicit `--all` batches every profile. `doctor --write-test` is an explicit mutating target probe and should be used only against a disposable prepared destination. Exact-mirror profiles require both `configure-profile --delete --max-delete N` and `plan/run --allow-delete`; an all-profile foreground deletion may additionally set a one-off aggregate bound with `--max-total-delete N` (default 100). Scheduled deletion separately requires `enable --allow-delete`, and its aggregate bound comes from `enable --max-total-delete N`. Without every layer, remote-only entries are preserved.

All profile, secret, and scheduler mutations use a package-manager lock and refuse to race an active plan/sync. Foreground and scheduled jobs share a separate PID lock. Stale locks are recovered only when their recorded PID is no longer alive, and lock directories are removed with `rmdir`, never recursive deletion.

## Routines, scheduler, and service management

The dashboard configures independent interval, daily-window, or realtime routines for each profile,
including debounce, polling fallback, retries/backoff, dependencies, and layered deletion approval.
The CLI retains this legacy all-profile interval schedule. DSM Package Center start/stop controls
the long-lived, unprivileged package-user API service and controller, and all automation remains
disabled until explicitly enabled:

```sh
sudo -u "$PACKAGE_USER" -- "$MANAGER" enable --interval 3600
sudo -u "$PACKAGE_USER" -- "$MANAGER" status
sudo -u "$PACKAGE_USER" -- "$MANAGER" logs 200
sudo -u "$PACKAGE_USER" -- "$MANAGER" disable
```

The interval range is 60 seconds through 30 days. Enabling never triggers an immediate mutation; use `run` after reviewing `plan`, or wait one interval. The cadence is delay-after-completion: after a scheduled job finishes, the next job is due one full interval later, so long jobs never overlap. Changing the interval rebases the pending deadline. A failed scheduled job is recorded and is not immediately retried. The controller checks schedule changes within 30 seconds, forwards TERM to the active job, waits for graceful core shutdown, and is never force-killed by the package script. Package Center stop also shuts down the verified API service and waits for a verified foreground run. Logs rotate at 10 MiB with five backups; core logs have their own built-in rotation.

Status and logs are also available over SSH:

```sh
sudo synopkg status synology-drive-sync
sudo synopkg start synology-drive-sync
sudo synopkg stop synology-drive-sync
sudo tail -n 200 /var/log/packages/synology-drive-sync.log
sudo tail -n 200 /var/packages/synology-drive-sync/var/log/api.log
```

## Upgrade and uninstall

DSM stops a started package before upgrade. The manifest declares `auto_upgrade_from="26.7-1"`;
26.7-1 through 26.10-1 share the reviewed markerless lifecycle generation, while 26.5/26.6 are not
direct-upgrade eligible. Read-only pre-upgrade checks reject a live runner or exact orphaned package
core, and the new post-install hook safely adopts only a strictly validated stale legacy run lock.
Configuration and credentials are retained across upgrade and validated against the new binary
before service restart. GitHub Releases are a Manual Install channel, not a private Package Center
feed or self-updater.

Because Synology's `preupgrade` hook is read-only, this markerless compatibility path cannot publish
a new exclusion lock. Do not run `sdsync-dsm` or other package-private commands concurrently with
the Package Center upgrade; the path assumes DSM serializes the package transaction and that the
package service UID remains trusted.

On a real uninstall (`SYNOPKG_PKG_STATUS=UNINSTALL`), the post-uninstall script removes only this package's configuration, credentials, runtime state, and logs under `/var/packages/synology-drive-sync/{home,var}`. It does not touch any source directory or remote NAS data. This purge is permanent.

## Acceptance boundary

Static validation proves archive structure, the root-free manifest, ordinary executable modes,
architecture, static linkage, dashboard/relay contracts, lifecycle behavior, and deterministic
assembly. Before relying on the package, test its exact NAS model and DSM version with a disposable
source and target, including Package Center installation and exact warning, CGI package identity,
package-user API service, package-owned `0600` socket, native AppWindow loading/rendering, browser
`X-SDSYNC-Request: 1` to CGI `HTTP_X_SDSYNC_REQUEST=1` forwarding, the primary protected
`authenticate.cgi` path when executable or the loopback user-service path when a validated
`root:system 0750` helper is kernel-inaccessible, nonempty GET error envelopes with HTTP transport 200
and semantic status/code/stage, administrator/CSRF rejection cases,
reverse-proxy upload limits, TLS trust, TOTP clock synchronization, routines, direct DSM desktop
alerts, large files, Drive indexing, restart during a long transfer, upgrade, and uninstall.
Automated Chrome fixture QA passes against the captured DSM control structure; physical native DSM
rendering and accessibility interaction, browser-header forwarding, package installation, Webman's
package-owner CGI identity, and both DSM authentication branch contracts remain unverified. A
manually built SPK is not automatically a Synology Package Center-approved release.

Official framework references: [package structure](https://help.synology.com/developer-guide/synology_package/introduction.html), [native app launch](https://help.synology.com/developer-guide/synology_package/package_tgz/launch_app.html), [AppWindow framework](https://help.synology.com/developer-guide/appendix/ui_framework/application.html), [architecture mapping](https://help.synology.com/developer-guide/appendix/platarchs.html), [privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html), [FHS paths](https://help.synology.com/developer-guide/integrate_dsm/fhs.html), and [lifecycle status codes](https://help.synology.com/developer-guide/synology_package/scripts.html).
