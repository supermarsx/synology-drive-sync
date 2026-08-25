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

The current SPK source, planned to first ship in release 26.10, includes a dark-first
administrator-only native DSM Vue `type=app` AppWindow and the `sdsync-dsm` SSH manager. Both surfaces operate the
same package-owned profiles, credentials, routines, state, and logs. The dashboard is
administrator-only; it never returns a stored password, TOTP seed, or remote logging token to the
browser. The CLI remains the recovery and automation surface when the dashboard cannot open.

The package requests no root execution, Linux capabilities, set-user-ID bit, or set-group-ID bit.
Its ordinary `0755` DSM `http` CGI can only relay a bounded request over a fixed package:`http`
`0660` Unix socket. The package-user API service reauthenticates the DSM session, independently
requires administrator membership and package CSRF, and alone reaches private state or the queue.

The package is not a Synology Drive protocol plug-in. It writes through File Station. Synology Drive
can index the result only when the selected destination belongs to the remote account's Drive home
or an enabled Team Folder.

> [!IMPORTANT]
> Do not install the immutable 26.5 or 26.6 SPKs. Release 26.5 is setid/privilege-invalid; release
> 26.6 is rejected on affected DSM by its `conf/resource` `sysnotify` acquisition worker with
> `pkgmgr_worker_violation`. Use 26.7 or later only when that release is published and its exact
> SPK/checksum are verified. Published assets are not repaired in place.

> [!NOTE]
> Static packaging, bridge, manager, lifecycle, and mock File Station tests have passed. They do not
> prove installation on a physical NAS, execution of DSM's cookie authenticator from the
> package-user service, DSM forwarding of `X-SDSYNC-Request: 1` as `HTTP_X_SDSYNC_REQUEST=1`, or
> synchronization between two live NAS devices. The native AppWindow uses cookie authentication
> and does not inspect the DSM shell location for a token. Complete the
> [live-NAS acceptance](dsm/troubleshooting.md#live-nas-acceptance) on disposable folders, and keep
> deletion disabled until its separate destructive test passes.

## Choose the exact DSM topic

| Subject | Guide |
| --- | --- |
| What every dashboard page, status, action, and preference does | [Dashboard and navigation](dsm/dashboard.md) |
| Model, DSM version, CPU, `INFO` architecture, and release asset | [Compatibility and release selection](dsm/compatibility.md) |
| Checksum verification, Package Center install, source ACL, upgrade, and uninstall | [Install, ACLs, and lifecycle](dsm/install-lifecycle.md) |
| Every basic, deletion, TLS, retry, rate, output, and remote-log profile field | [Profiles and destinations](dsm/profiles.md) |
| Password, TOTP, and remote-log-token keep/replace/clear behavior | [Secrets and protected values](dsm/secrets.md) |
| Interval, daily, and realtime routines, dependencies, retries, and deletion approval | [Routines and scheduling](dsm/routines.md) |
| Doctor, cached health, activity, logs, and DSM desktop alerts | [Health, activity, logs, and notifications](dsm/operations.md) |
| `authenticate.cgi`, administrator checks, cookie authentication, CSRF, package/`http` socket boundary, and private queue | [Dashboard security model](dsm/security.md) |
| Dashboard-to-`sdsync-dsm` command mapping and private paths | [CLI parity and private paths](dsm/cli-parity.md) |
| Symptoms, recovery, acceptance evidence, and known unverified behavior | [Troubleshooting and live-NAS acceptance](dsm/troubleshooting.md) |
| Reproducible package assembly, ELF contracts, icons, and validation | [Build and validate SPKs](dsm/package-development.md) |

All pages are indexed by the documentation site's instant local search. Search for a graphical label
such as `Routine deletion approval ceiling`, a CLI option such as `--max-total-delete`, an error
status such as `77`, or a protocol term such as `X-SDSYNC-CSRF`.

## First safe run

1. Use the [release selector](release-selector.md) with the exact NAS model, DSM branch/build, and
   `uname -m` value. Do not choose an SPK from a marketing CPU label alone.
2. For this native AppWindow flow, select a published 26.10-or-later SPK. Verify it against the same
   release's `SHA256SUMS`, then install it through **Package Center > Manual Install**.
3. Grant the package's actual **System internal user** read-only access to the intended local source
   share. The package grants itself no share access.
4. Start the package and open **Synology Drive Sync** from the DSM desktop or Package Center. The
   current DSM session cookie is authenticated server-side. The native UI does not inspect or
   rewrite the DSM shell location and sends no `SynoToken`. If session authentication or the control
   bridge fails, use the troubleshooting checks and CLI; do not copy cookies or CSRF material into
   a bookmark, local storage, log, or support transcript.
5. Create one profile, store its password, run non-writing Doctor, and review Plan. Profile/secret
   saves and Doctor wait for a sanitized terminal controller result; Plan and Run remain
   asynchronous. Run remains additive/update-only unless deletion is independently approved at every
   required layer.

The complete procedure, including remote account preparation and source ACL checks, is in
[Install, ACLs, and lifecycle](dsm/install-lifecycle.md).

## Dashboard and CLI are one control plane

The dashboard provides graphical profile editing, per-profile routines, status, cached health,
Doctor, Plan, Run, bounded logs, structured activity, and a direct DSM desktop-alert policy.
Mutating requests are authenticated and queued. Configuration/secret/routine/policy saves and Doctor
observe a sanitized terminal job result before reporting success, with no client pending-state
deadline. Expired/missing or invalid evidence and five consecutive observation failures produce an
outcome-unknown result; multi-stage profile saves can be partially applied. Plan and Run intentionally
remain asynchronous; follow the snapshot or Activity before treating either operation as complete.

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

The native AppWindow bundle, fixed desktop-alert I18N text, icons, permission contract, request
schema, queue, and manager behavior are covered by repository tests. Rendered browser QA could not
be completed because no browser runtime was available in the test environment. Treat layout in real
DSM AppWindow sizes and supported DSM versions, package-user `authenticate.cgi` execution,
`X-SDSYNC-Request: 1` to `HTTP_X_SDSYNC_REQUEST=1` forwarding,
Package Center install/start/open behavior, DSM desktop delivery through `synodsmnotify`, and the
complete two-NAS data path as live acceptance work—not as proven behavior. The SPK does not acquire
`sysnotify` or register Notification Center email, SMS, mobile, CMS, or rule/channel delivery.

Official framework references:

- [Native package app launch](https://help.synology.com/developer-guide/synology_package/package_tgz/launch_app.html)
- [AppWindow UI framework](https://help.synology.com/developer-guide/appendix/ui_framework/application.html)
- [DSM application authentication](https://help.synology.com/developer-guide/integrate_dsm/web_authentication.html)
- [DSM package privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html)
- [DSM desktop notification command](https://help.synology.com/developer-guide/synology_package/show_massage.html)
- [DSM package FHS](https://help.synology.com/developer-guide/integrate_dsm/fhs.html)
- [Platform and `arch` mapping](https://help.synology.com/developer-guide/appendix/platarchs.html)
- [Synology File Station API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Package/FileStation/All/enu/Synology_File_Station_API_Guide.pdf)
