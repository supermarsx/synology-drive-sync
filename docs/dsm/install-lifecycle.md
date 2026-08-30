# Install, ACLs, and lifecycle

Install the SPK on the NAS that physically owns the authoritative local source. The package is not a
remote-to-remote relay: it reads a local filesystem and uploads to another NAS through File Station.

## Before installation

Prepare these facts and dependencies:

- an exact model/DSM/CPU match accepted by the [release selector](compatibility.md);
- the verified architecture-specific SPK and same-release checksum manifest;
- an existing local DSM shared folder for each source;
- a dedicated non-administrator account on the target NAS with File Station application access and
  read/write permission only to the intended destination subtree;
- an HTTPS File Station base URL whose `/webapi/*` route, upload-body limit, and read/send timeouts
  accommodate the largest file;
- an existing target user home or shared-folder root; and
- temporary administrator SSH access for ACL verification and recovery, even when normal operation
  will use the dashboard.

For `/home/Drive/...`, enable User Home and initialize the target account's Drive home first. For
`/<share>/...`, create the target shared folder first and enable it as a Team Folder when Drive
indexing is required. The package creates descendants under an existing writable parent; it does
not create DSM users, homes, shared folders, Team Folders, or ACLs.

## Verify and install

The native AppWindow procedure below applies to a verified, non-blocked 26.10-or-later SPK. Never
install the blocked 26.20 DSM package. Releases 26.7-26.9 may satisfy the corrected rootless package
contract, but they retain their originally published UI and do not gain the native AppWindow
retroactively.

1. Verify the asset as described in [Compatibility and release selection](compatibility.md#download-and-verify-one-exact-release).
2. In DSM, open **Package Center > Manual Install**.
3. Select the verified `.spk`, check its displayed name/version, and review the package information.
4. Accept DSM's normal third-party-package warning only after confirming the repository, asset,
   checksum, and optional attestation. The project does not bypass that publisher-trust warning. A
   refusal reporting a lower-privilege or resource-worker policy violation is a different condition
   and is not expected from a corrected release; preserve the artifact and collect the logs in
   [Troubleshooting](troubleshooting.md#normal-third-party-warning-versus-a-dsm-install-policy-rejection).
5. Finish installation, then start **Synology Drive Sync** in Package Center.

Installation creates an unprivileged system-internal package identity, private FHS storage, a
disabled global schedule, the package controller, a package-user API service, the desktop
application, fixed preloaded desktop-alert I18N text, and deterministic icons. Its privilege manifest
contains only `defaults.run-as=package`; it requests no joined web group, root execution, capability,
or identity-changing file mode. It does not grant access to a user share, create a profile, store a
credential, contact a target, or start a sync.

DSM may collision-rename the package's NSS username. Use the account shown under **System internal
user**, or resolve `$PACKAGE_USER` with the canonical
[package-identity discovery](cli-parity.md#discover-the-actual-package-identity); do not assume a
literal username.

`silent_install=yes` and `silent_upgrade=yes` intentionally avoid a Package Center wizard that
could place secrets or paths in installer variables. `silent_uninstall=no` is equally intentional:
uninstall permanently purges package-private operational state and requires confirmation.

## Open the dashboard

Use the DSM desktop application menu or Package Center's **Open** action. The application is
registered for administrators only. A healthy open sequence requires the DSM session cookie,
the official same-origin `SYNO.API.Auth` version 6 `method=token` bootstrap, administrator
membership, and a package CSRF token for mutation. The AppWindow exactly-once-encodes a valid token,
keeps it only in module memory, and sends it only as the package `X-SYNO-TOKEN` header. The ordinary
package-owned `0755` CGI fails closed unless Webman starts it with the exact non-root package UID. It
first probes `X_OK` on DSM's fixed `authenticate.cgi`, before path or metadata validation. When that
probe succeeds, it resolves the helper through a root-owned, non-group/world-writable ancestor and
symlink chain, accepts DSM's standard exact `system:system` (`1:1`) canonical executable or the
legacy root-owned form, and revalidates its identity immediately before direct execution. When the
probe returns `EACCES`—as with a protected `root:system 0750`
layout—the CGI skips the validator and uses a bounded loopback-only DSM user-service request with the
current cookie and optional token as headers. Every other probe error fails closed. The fallback
requires valid `Session.user` plus exact `is_admin=true`. It independently
resolves the resulting NSS identity and administrator membership without changing the CGI identity,
package privileges, or DSM helper metadata. It then
relays a bounded assertion and request through the fixed package-owned Unix socket, which remains
`0000` before startup commit and activates on the same inode as `0600`. The package-user service
never executes that DSM authentication helper; it verifies the package-UID peer, strict relay/request,
independently resolved UID/name/administrator membership, recomputed session binding, policy, and
package CSRF. The native AppWindow does not parse or rewrite the DSM shell location and never places
SynoToken in a launch URL, history, request body, persistent storage, or logs. Never copy a DSM
cookie, SynoToken, or package CSRF token into a URL, bookmark, browser storage, or support transcript.

## Grant read-only access to each local source

On the source NAS:

1. Open **Control Panel > Shared Folder**.
2. Edit the intended source share and open **Permissions**.
3. Change the user selector to **System internal user**.
4. Grant the actual Synology Drive Sync package identity shown by DSM read-only access. DSM may
   collision-rename its NSS account, so do not rely on a guessed literal username.
5. Apply it only to the required descendants.
6. If Windows ACLs are enabled, verify inherited list, traverse, and read permission.

Do not grant write access merely to make a scan pass. The source is authoritative and the package
does not need to modify it. Do not grant access to unrelated shares.

Test the real package identity from an administrator SSH shell:

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo -u "$PACKAGE_USER" -- "$MANAGER" paths
sudo -u "$PACKAGE_USER" -- test -r /volume1/Source
sudo -u "$PACKAGE_USER" -- test -x /volume1/Source
```

The manager canonicalizes sources and rejects `/`, symlink roots, unreadable/untraversable roots,
package storage, and DSM-managed locations. During scanning it prunes `#recycle`, `#snapshot`,
`@eaDir`, `@tmp`, `@sharebin`, `@apphome`, `@appdata`, `@appstore`, `@apptemp`, `@appconf`, and
`.SynologyWorkingDirectory`.

## Create the first profile

In the dashboard, open **Profiles > New profile**. Supply a physical local source, HTTPS File Station
URL, target DSM username, and File Station logical destination. Keep **Mirror remote deletions**,
**Allow an empty source**, **Allow plain HTTP**, and **Accept invalid TLS certificates** off.

Choose **Replace securely** for Password and enter the value. Save observes the queued configuration
and each secret job to a sanitized terminal result within an explicit overall observation limit;
then confirm the profile and password-presence marker in a refreshed snapshot. If a later stage
fails or becomes outcome-unknown after an earlier stage completed, the UI reports the profile as
partially applied—inspect all configuration and credential-presence markers before retrying. Run
non-writing Doctor and Plan only after that evidence appears. See
[Profiles and destinations](profiles.md) and
[Secrets and protected values](secrets.md).

Equivalent SSH setup:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-profile \
  --name personal \
  --source '/volume1/Photos' \
  --url 'https://files.remote.example' \
  --username 'mirror-bot' \
  --remote '/home/Drive/Preferred Backup' \
  --compare content \
  --jobs 2 \
  --default

sudo -u "$PACKAGE_USER" -- "$MANAGER" set-password personal
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal
sudo -u "$PACKAGE_USER" -- "$MANAGER" plan personal
```

## Start, stop, and scheduling defaults

Package Center start/stop controls two long-lived, unprivileged package-user processes: the local API
service for the DSM dashboard and the controller for routines and queued work. Starting them does
not enable a routine or contact a target. The legacy global interval schedule and every per-profile
routine are disabled until explicitly configured and enabled.

```bash
sudo synopkg status synology-drive-sync
sudo synopkg start synology-drive-sync
sudo synopkg stop synology-drive-sync
```

Stop requests cooperative termination of the verified API service, controller, and any verified
active runner. The lifecycle script validates recorded PIDs and the fixed socket before cleanup,
waits for shutdown, and refuses to signal an untrusted PID or force-kill after a timeout. Profile,
secret, routine, and schedule mutations are serialized and refuse to race an active Plan or Run.

## Upgrade, rollback, and uninstall

Before an upgrade:

1. use the selector and verify the new architecture-specific SPK;
2. pause new automation and let any active operation finish;
3. retain the previous verified SPK, non-secret configuration evidence, and required logs;
4. install the update through Package Center;
5. start the package, confirm dashboard/CLI status, run Doctor, and review a fresh additive Plan;
6. re-enable automation only after those checks pass.

The package declares a direct upgrade floor of `26.7-1`. The markerless 26.7-26.10 lifecycle
generation is accepted only after a read-only stopped-topology check proves that neither its old
runner nor an exact package core remains alive; the new post-install hook then adopts any strictly
validated stale one-line run lock. Releases 26.5 and 26.6 are not eligible for direct upgrade. Move
such an installation to a verified eligible intermediate SPK through Package Center Manual Install
before installing the current release.

Because Synology's `preupgrade` hook is read-only, this markerless compatibility path cannot publish
a new exclusion lock. Do not run `sdsync-dsm` or other package-private commands concurrently with
the Package Center upgrade; the path assumes DSM serializes the package transaction and that the
package service UID remains trusted.

DSM stops a running package before upgrade. Lifecycle scripts also refuse upgrade while a verified
controller or foreground Plan/Run PID remains live. Profiles, credentials, routines, global schedule,
state, and logs are retained and validated with the new binary. If retained configuration is invalid,
investigate it rather than replacing package-private files manually.

An older SPK can be reinstalled only if DSM permits the version transition. Rollback changes package
code; it cannot undo uploads, directory creation, copies, or deletions already completed on the target.

Before uninstalling:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" disable
sudo synopkg stop synology-drive-sync
```

Export any non-secret configuration and retain required audit logs. A real uninstall permanently
removes generated configuration, password/TOTP/remote-log-token files, routines, state, queue,
locks, and package logs under `/var/packages/synology-drive-sync/{home,var}`. It leaves the local
source, remote target data, DSM users, shares, ACLs, snapshots, and Drive configuration untouched.
Deleted package credentials are not recoverable.

## Post-install acceptance evidence

Record the model, DSM build, `uname -m`, exact SPK filename/version, checksum, optional attestation,
the exact Package Center warning/result, bounded install/package logs, CGI/API-service/socket
identities and modes, install/start/open result, package-user ACL test, and first non-writing
Doctor/Plan. Explicitly prove Webman's package-owner CGI identity and record which DSM authentication
branch is selected: full validation/revalidation and direct protected-helper execution after a
successful `X_OK` probe, or successful bounded loopback user-service authentication when that probe
returns `EACCES` without invoking the path validator. Also record the official token bootstrap and
`X-SYNO-TOKEN` header forwarding without recording the token itself. Repository tests do not prove
those physical DSM behaviors. Do not
describe the installation as production-ready until the complete
[live-NAS acceptance](troubleshooting.md#live-nas-acceptance) passes.
