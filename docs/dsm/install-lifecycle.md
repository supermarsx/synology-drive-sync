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

1. Verify the asset as described in [Compatibility and release selection](compatibility.md#download-and-verify-one-exact-release).
2. In DSM, open **Package Center > Manual Install**.
3. Select the verified `.spk`, check its displayed name/version, and review the package information.
4. Accept DSM's normal third-party-package warning only after confirming the repository, asset,
   checksum, and optional attestation. The project does not bypass that warning.
5. Finish installation, then start **Synology Drive Sync** in Package Center.

Installation creates the unprivileged system-internal identity `synology-drive-sync`, private FHS
storage, a disabled global schedule, the package controller, the desktop application, fixed DSM
notification resources, and deterministic icons. It does not grant access to a user share, create a
profile, store a credential, contact a target, or start a sync.

`silent_install=yes` and `silent_upgrade=yes` intentionally avoid a Package Center wizard that
could place secrets or paths in installer variables. `silent_uninstall=no` is equally intentional:
uninstall permanently purges package-private operational state and requires confirmation.

## Open the dashboard

Use the DSM desktop application menu or Package Center's **Open** action. The application is
registered for administrators only. A healthy open sequence requires the DSM session cookie,
administrator membership, and a `SynoToken` delivered with the launch URL. The package then issues
its own short-lived, session-bound CSRF token.

DSM 7 AppLaunch forwarding of that SynoToken has not yet been proven on physical hardware. If the
page reports a missing launch token, do not paste a token into the URL, a bookmark, browser storage,
or a support transcript. Continue through the [CLI path](cli-parity.md) and record the behavior as a
live acceptance failure.

## Grant read-only access to each local source

On the source NAS:

1. Open **Control Panel > Shared Folder**.
2. Edit the intended source share and open **Permissions**.
3. Change the user selector to **System internal user**.
4. Grant `synology-drive-sync` read-only access.
5. Apply it only to the required descendants.
6. If Windows ACLs are enabled, verify inherited list, traverse, and read permission.

Do not grant write access merely to make a scan pass. The source is authoritative and the package
does not need to modify it. Do not grant access to unrelated shares.

Test the real package identity from an administrator SSH shell:

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo -u synology-drive-sync -- "$MANAGER" paths
sudo -u synology-drive-sync -- test -r /volume1/Source
sudo -u synology-drive-sync -- test -x /volume1/Source
```

The manager canonicalizes sources and rejects `/`, symlink roots, unreadable/untraversable roots,
package storage, and DSM-managed locations. During scanning it prunes `#recycle`, `#snapshot`,
`@eaDir`, `@tmp`, `@sharebin`, `@apphome`, `@appdata`, `@appstore`, `@apptemp`, `@appconf`, and
`.SynologyWorkingDirectory`.

## Create the first profile

In the dashboard, open **Profiles > New profile**. Supply a physical local source, HTTPS File Station
URL, target DSM username, and File Station logical destination. Keep **Mirror remote deletions**,
**Allow an empty source**, **Allow plain HTTP**, and **Accept invalid TLS certificates** off.

Choose **Replace securely** for Password and enter the value. Save polls the queued configuration
and secret jobs to sanitized terminal results; then confirm the profile and password-presence marker
in a refreshed snapshot. Run non-writing Doctor and Plan only after that evidence appears. See
[Profiles and destinations](profiles.md) and
[Secrets and protected values](secrets.md).

Equivalent SSH setup:

```bash
sudo -u synology-drive-sync -- "$MANAGER" configure-profile \
  --name personal \
  --source '/volume1/Photos' \
  --url 'https://files.remote.example' \
  --username 'mirror-bot' \
  --remote '/home/Drive/Preferred Backup' \
  --compare content \
  --jobs 2 \
  --default

sudo -u synology-drive-sync -- "$MANAGER" set-password personal
sudo -u synology-drive-sync -- "$MANAGER" doctor personal
sudo -u synology-drive-sync -- "$MANAGER" plan personal
```

## Start, stop, and scheduling defaults

Package Center start/stop controls one long-lived, unprivileged controller. Starting it does not
enable a routine or contact a target. The legacy global interval schedule and every per-profile
routine are disabled until explicitly configured and enabled.

```bash
sudo synopkg status synology-drive-sync
sudo synopkg start synology-drive-sync
sudo synopkg stop synology-drive-sync
```

Stop requests cooperative termination of the verified controller and any verified active runner.
The lifecycle script waits for shutdown and refuses to signal an untrusted PID or force-kill after a
timeout. Profile, secret, routine, and schedule mutations are serialized and refuse to race an active
Plan or Run.

## Upgrade, rollback, and uninstall

Before an upgrade:

1. use the selector and verify the new architecture-specific SPK;
2. pause new automation and let any active operation finish;
3. retain the previous verified SPK, non-secret configuration evidence, and required logs;
4. install the update through Package Center;
5. start the package, confirm dashboard/CLI status, run Doctor, and review a fresh additive Plan;
6. re-enable automation only after those checks pass.

DSM stops a running package before upgrade. Lifecycle scripts also refuse upgrade while a verified
controller or foreground Plan/Run PID remains live. Profiles, credentials, routines, global schedule,
state, and logs are retained and validated with the new binary. If retained configuration is invalid,
investigate it rather than replacing package-private files manually.

An older SPK can be reinstalled only if DSM permits the version transition. Rollback changes package
code; it cannot undo uploads, directory creation, copies, or deletions already completed on the target.

Before uninstalling:

```bash
sudo -u synology-drive-sync -- "$MANAGER" disable
sudo synopkg stop synology-drive-sync
```

Export any non-secret configuration and retain required audit logs. A real uninstall permanently
removes generated configuration, password/TOTP/remote-log-token files, routines, state, queue,
locks, and package logs under `/var/packages/synology-drive-sync/{home,var}`. It leaves the local
source, remote target data, DSM users, shares, ACLs, snapshots, and Drive configuration untouched.
Deleted package credentials are not recoverable.

## Post-install acceptance evidence

Record the model, DSM build, `uname -m`, exact SPK filename/version, checksum, optional attestation,
Package Center install/start/open result, package-user ACL test, and first non-writing Doctor/Plan.
Do not describe the installation as production-ready until the complete
[live-NAS acceptance](troubleshooting.md#live-nas-acceptance) passes.
