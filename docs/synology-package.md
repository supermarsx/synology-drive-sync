# Synology DSM package and dashboard

The DSM 7 package installs the one-way sync engine on the NAS that owns the authoritative source
folder. It reads only the local folders granted to its actual DSM system-internal package identity
and sends their contents over HTTPS to a chosen File Station logical path on another NAS. DSM may
collision-rename that identity's NSS username; discover it from package-home ownership instead of
assuming the package name is the account name.

```text
source NAS                                                target NAS
/volume1/Source  ->  Synology Drive Sync SPK  ->  HTTPS File Station WebAPI
                         package identity            /home/Drive/Chosen Folder
                         DSM admin dashboard          /TeamShare/Chosen Folder
```

The SPK includes a dark-first native DSM desktop application and the `sdsync-dsm` SSH manager.
Both surfaces operate the same package-owned profiles, credentials, routines, state, and logs. The
dashboard is administrator-only; it never returns a stored password, TOTP seed, or remote logging
token to the browser. The CLI remains the recovery and automation surface when the dashboard cannot
open.

The package requests no root execution, Linux capabilities, set-user-ID bit, or set-group-ID bit.
Its ordinary `0755` DSM `http` CGI can only relay a bounded request over a fixed package:`http`
`0660` Unix socket. The package-user API service reauthenticates the DSM session, independently
requires administrator membership and package CSRF, and alone reaches private state or the queue.

The package is not a Synology Drive protocol plug-in. It writes through File Station. Synology Drive
can index the result only when the selected destination belongs to the remote account's Drive home
or an enabled Team Folder.

> [!IMPORTANT]
> Static packaging, bridge, manager, lifecycle, and mock File Station tests have passed. They do not
> prove installation on a physical NAS or synchronization between two live NAS devices. DSM 7
> AppLaunch forwarding of `SynoToken` to this third-party application is also not yet proven on
> hardware. Complete the [live-NAS acceptance](dsm/troubleshooting.md#live-nas-acceptance) on
> disposable folders, and keep deletion disabled until its separate destructive test passes.

## Choose the exact DSM topic

| Subject | Guide |
| --- | --- |
| What every dashboard page, status, action, and preference does | [Dashboard and navigation](dsm/dashboard.md) |
| Model, DSM version, CPU, `INFO` architecture, and release asset | [Compatibility and release selection](dsm/compatibility.md) |
| Checksum verification, Package Center install, source ACL, upgrade, and uninstall | [Install, ACLs, and lifecycle](dsm/install-lifecycle.md) |
| Every basic, deletion, TLS, retry, rate, output, and remote-log profile field | [Profiles and destinations](dsm/profiles.md) |
| Password, TOTP, and remote-log-token keep/replace/clear behavior | [Secrets and protected values](dsm/secrets.md) |
| Interval, daily, and realtime routines, dependencies, retries, and deletion approval | [Routines and scheduling](dsm/routines.md) |
| Doctor, cached health, activity, logs, and DSM notifications | [Health, activity, logs, and notifications](dsm/operations.md) |
| `authenticate.cgi`, administrator checks, SynoToken, CSRF, package/`http` socket boundary, and private queue | [Dashboard security model](dsm/security.md) |
| Dashboard-to-`sdsync-dsm` command mapping and private paths | [CLI parity and private paths](dsm/cli-parity.md) |
| Symptoms, recovery, acceptance evidence, and known unverified behavior | [Troubleshooting and live-NAS acceptance](dsm/troubleshooting.md) |
| Reproducible package assembly, ELF contracts, icons, and validation | [Build and validate SPKs](dsm/package-development.md) |

All pages are indexed by the documentation site's instant local search. Search for a graphical label
such as `Routine deletion approval ceiling`, a CLI option such as `--max-total-delete`, an error
status such as `77`, or a protocol term such as `X-SDSYNC-CSRF`.

## First safe run

1. Use the [release selector](release-selector.md) with the exact NAS model, DSM branch/build, and
   `uname -m` value. Do not choose an SPK from a marketing CPU label alone.
2. Verify the selected SPK against the same release's `SHA256SUMS`, then install it through
   **Package Center > Manual Install**.
3. Grant the package's actual **System internal user** read-only access to the intended local source
   share. The package grants itself no share access.
4. Start the package and open **Synology Drive Sync** from the DSM desktop or Package Center. If the
   browser reports that the DSM launch token is missing, stop there and use the CLI; do not copy a
   token into a bookmark, local storage, or log.
5. Create one profile, store its password, run non-writing Doctor, and review Plan. Profile/secret
   saves wait for a terminal controller result; Doctor and Plan remain asynchronous. Run remains
   additive/update-only unless deletion is independently approved at every required layer.

The complete procedure, including remote account preparation and source ACL checks, is in
[Install, ACLs, and lifecycle](dsm/install-lifecycle.md).

## Dashboard and CLI are one control plane

The dashboard provides graphical profile editing, per-profile routines, status, cached health,
Doctor, Plan, Run, bounded logs, structured activity, and DSM notification policy. Mutating requests
are authenticated and queued. Configuration/secret/routine/policy saves poll a sanitized terminal
job result before reporting success. Doctor, Plan, and Run intentionally return `queued`; follow the
snapshot or Activity before treating those operations as complete.

The CLI can perform the same operational work and exposes recovery-oriented commands such as
`paths`, `show-config`, and direct bounded log reads. First resolve `$PACKAGE_USER` with the
canonical [package-identity discovery](dsm/cli-parity.md#discover-the-actual-package-identity), then
run it as that identity:

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo -u "$PACKAGE_USER" -- "$MANAGER" status
```

Profile, credential, routine, schedule, Doctor, Plan, and Run operations reject root and other
identities with exit `77`. This makes source validation test the package user's real ACL rather than
an administrator's broader access. See [CLI parity and private paths](dsm/cli-parity.md).

## Destination and source meanings

The local source is a physical absolute path on the package NAS, such as `/volume1/Photos`. The
remote destination is a File Station **logical path**, never a remote `/volumeN` path:

- `/home/Drive/NAS-A Backup` selects the authenticated target account's own Drive home;
- `/TeamShare/NAS-A Backup` selects a writable shared-folder or Team Folder descendant.

The target NAS must already provide the DSM account, File Station access, the user-home or shared
folder root, and its ACL. Sync can create missing descendants beneath an existing writable parent.
It cannot create a DSM user, enable User Home, create a shared folder, enable a Team Folder, or alter
ACLs. Detailed path validation and every profile field are documented in
[Profiles and destinations](dsm/profiles.md).

## Upgrade, rollback, and uninstall

Upgrade retains package-private profiles, credentials, routines, state, and logs, then validates the
retained configuration against the new executable. A binary rollback does not undo remote writes.
Uninstall requires confirmation and permanently removes the package-owned configuration,
credentials, state, locks, and logs while leaving both NAS data trees untouched. Follow the exact
[upgrade and uninstall procedure](dsm/install-lifecycle.md#upgrade-rollback-and-uninstall).

## Evidence boundary

The native application, package resources, icons, permission contract, request schema, queue, and
manager behavior are covered by repository tests. Rendered browser QA could not be completed because
no browser runtime was available in the test environment. Treat layout on real DSM iframe/window
sizes, DSM 7 AppLaunch `SynoToken` delivery, Package Center install/start/open behavior, Notification
Center delivery, and the complete two-NAS data path as live acceptance work—not as proven behavior.

Official framework references:

- [DSM desktop application integration](https://help.synology.com/developer-guide/integrate_dsm/desktopapp.html)
- [DSM application authentication](https://help.synology.com/developer-guide/integrate_dsm/web_authentication.html)
- [DSM package privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html)
- [DSM package FHS](https://help.synology.com/developer-guide/integrate_dsm/fhs.html)
- [Platform and `arch` mapping](https://help.synology.com/developer-guide/appendix/platarchs.html)
- [Synology File Station API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Package/FileStation/All/enu/Synology_File_Station_API_Guide.pdf)
