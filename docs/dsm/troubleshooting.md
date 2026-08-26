# Troubleshooting and live-NAS acceptance

Start with evidence from the exact NAS, package identity, and profile. Do not fix a dashboard error
by broadening ACLs, disabling TLS, editing private files, or repeatedly resubmitting a mutation.
Resolve `$PACKAGE_USER` with the canonical
[package-identity discovery](cli-parity.md#discover-the-actual-package-identity) before using these
SSH commands.

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo synopkg status synology-drive-sync
sudo -u "$PACKAGE_USER" -- "$MANAGER" status
sudo -u "$PACKAGE_USER" -- "$MANAGER" logs 200
sudo -u "$PACKAGE_USER" -- "$MANAGER" paths
```

## “Package is not supported” or “incompatible DSM version”

Common causes are:

- wrong architecture asset;
- using Linux `armv7l` as an asset suffix instead of selecting `armv7`;
- an Evansport `i686` model running outside DSM 7.0/7.1;
- DSM older than `7.0-40759`;
- DSM newer than the reviewed `7.4-99999` ceiling;
- ARMv5/88f628x, PowerPC, DSM 6, DSM Enterprise, or another unsupported platform; or
- a model/DSM branch conflict even though its broad CPU family looks compatible.

Re-run the [release selector](../release-selector.md) with exact model, DSM minor/build, and
`uname -m`. Do not modify `INFO` or rename an SPK/binary to force installation.

## Normal third-party warning versus a DSM install-policy rejection

DSM normally warns before installing a package that is not signed and distributed by Synology.
That publisher-trust warning is expected for this project; accept it only after verifying the
repository, exact release asset, checksum, and optional attestation. It is distinct from Package
Center refusing installation because a package violates its lower-privilege or resource-worker
policy.

> [!WARNING]
> Do not install the immutable 26.5 or 26.6 SPK assets. Release 26.5 requested package-owned mode
> `4755` for `ui/api.cgi`; although it did not select UID 0, DSM correctly treated the set-user-ID
> permission as identity-changing/root-privilege-invalid. Release 26.6 removed setid, but affected DSM
> installations then rejected its `conf/resource` `sysnotify` acquisition worker and recorded
> `pkgmgr_worker_violation`. Published assets are not repaired or replaced in place. Use 26.7 or
> later only when that release is published and its exact SPK/checksum are verified; repository
> source or a draft artifact is not physical-DSM installation proof.

The corrected 26.7-or-later source contract requests no root run-as, Linux capabilities, set-user-ID
bit, or set-group-ID bit. Every executable, including `ui/api.cgi`, is ordinary `0755`, and
`conf/privilege` contains exactly:

```json
{
  "defaults": {
    "run-as": "package"
  },
  "join-groupname": "http"
}
```

If Package Center reports a privilege/resource rejection, do not accept it as the normal trust
warning and do not repair the SPK with `chmod`, by deleting a manifest, or by changing anything to
root. Preserve the exact artifact and DSM build, validate the SPK on a workstation, and collect the
install logs below. An old, locally modified, corrupted, or differently signed artifact is not
evidence about the current source contract. Synology documents the messages separately in its
[DSM 7 system requirements](https://help.synology.com/developer-guide/getting_started/system_requirement.html)
and [breaking changes](https://help.synology.com/developer-guide/breaking_changes.html).

## Collect installation and dashboard evidence safely

Record the exact Package Center failure time. On the affected NAS, collect the immutable device and
artifact identity first, then bounded tails rather than whole system logs:

```bash
date '+%Y-%m-%dT%H:%M:%S%z'
uname -m
cat /proc/sys/kernel/syno_hw_version
cat /etc.defaults/VERSION
sha256sum /path/to/synology-drive-sync-YY.N-ARCH.spk
tar -xOf /path/to/synology-drive-sync-YY.N-ARCH.spk INFO
tar -tf /path/to/synology-drive-sync-YY.N-ARCH.spk | grep -E '^(conf/|scripts/|INFO$)'
sudo tail -n 200 /var/log/synopkg.log
sudo tail -n 200 /var/log/messages
sudo tail -n 200 /var/log/packages/synology-drive-sync.log
sudo tail -n 200 /var/packages/synology-drive-sync/var/log/controller.log
sudo tail -n 200 /var/packages/synology-drive-sync/var/log/api.log
```

Replace the SPK path with the preserved file that Package Center actually received; do not extract,
edit, or repack it. Keep any `pkgmgr_worker_violation`, resource name, package-manager phase, exit
code, and nearby timestamp intact. The final two logs are package-private service logs and may not
exist if installation or first start did not reach that stage. Inspect all output locally before
sharing it. Never paste a DSM cookie,
`SynoToken`, `X-SDSYNC-CSRF`, password, TOTP seed/current code, remote-log token, secret queue file,
or token-bearing URL into an issue, chat, screenshot, or support archive. Redact sensitive host,
account, and path values without removing timestamps, exit codes, DSM build, or package version.

## Release 26.11 controller startup failure on a physical volume

Release 26.11 can fail to start when DSM supplies physical volume paths such as
`/volume3/@appstore/synology-drive-sync` while the controller supervisor uses the equivalent
`/var/packages/synology-drive-sync/target` alias. The package then reports `controller failed before
startup commit`, and `controller.log` contains `controller lifecycle parent did not commit startup`.
This is an exact-process path-alias mismatch, not a request for root privileges or broader ACLs.

Collect only the fixed symptom and framework-owned link destinations with this read-only command:

```bash
sudo sh -c '
  ls -ld /var/packages/synology-drive-sync/target /var/packages/synology-drive-sync/var
  grep -F "controller lifecycle parent did not commit startup" \
    /var/packages/synology-drive-sync/var/log/controller.log | tail -n 5
'
```

Do not `chmod`, `chown`, remove locks, or run the service as root. Download the first release newer
than 26.11 whose notes include the DSM controller path-alias fix, verify its SPK and checksum using
the [release instructions](../releases.md#synology-dsm-packages), then install it through
**Package Center > Manual Install**. GitHub Releases are the update channel for this package; the
26.11 asset remains immutable.

## DSM says the page is not found when opening the app

The current source uses the native `type=app` AppWindow introduced in release 26.10. It does not
navigate to a packaged `index.html`. On a 26.10-or-later native artifact, DSM's generic “Sorry, the
page you are looking for is not found” response during Open therefore usually identifies a stale
URL-style registration/shortcut or a failed native asset/API request; it is not evidence that a
`SynoToken` is required.

First click **Open** in Package Center or launch **Synology Drive Sync** from the DSM desktop. In the
browser Network panel, record only each failed request path, status, and response type. Do not copy,
save, or share any query string because it can contain session material. A corrected launch must not
request `/webman/3rdparty/synology-drive-sync/index.html`. The dashboard's API requests use the exact
path `/webman/3rdparty/synology-drive-sync/api.cgi`.

Then inspect the installed registration and mapping without changing them:

```bash
PACKAGE=synology-drive-sync
PACKAGE_BASE=/var/packages/$PACKAGE
WEBMAN_LINK=/usr/syno/synoman/webman/3rdparty/$PACKAGE
UI_ROOT=$PACKAGE_BASE/target/ui

grep -E '^(package|dsmuidir|dsmappname)=' "$PACKAGE_BASE/INFO"
ls -ld "$WEBMAN_LINK" "$UI_ROOT"
readlink "$WEBMAN_LINK"
readlink -f "$WEBMAN_LINK"
stat -Lc '%F %a %U:%G %n' \
  "$WEBMAN_LINK" "$UI_ROOT" "$UI_ROOT/config" \
  "$UI_ROOT/SynologyDriveSync.js" "$UI_ROOT/style.css" "$UI_ROOT/api.cgi"
grep -F 'SYNO.SDS.App.SynologyDriveSync.Instance' "$UI_ROOT/config"
grep -F '"type": "app"' "$UI_ROOT/config"
```

The installed fields must be `package="synology-drive-sync"`, `dsmuidir="ui"`, and
`dsmappname="SYNO.SDS.App.SynologyDriveSync.Instance"`. `config`, `SynologyDriveSync.js`, and
`style.css` must be regular `0644` files; `config` must wrap that class beneath the
`SynologyDriveSync.js` module and declare `type="app"` with the matching `appWindow`. `api.cgi` must
be a regular `0755` file. The Webman link must resolve to that installation's `target/ui` for the
CGI endpoint. Interpret the evidence as follows:

- Any Open request for `index.html`, or an installed `.url`/`type=url` config, identifies an old
  package or stale DSM registration. Verify and reinstall a published 26.10-or-later native artifact;
  do not edit the installed config in place.
- A missing class/module wrapper, bundle, or stylesheet is an invalid/stale SPK payload. Preserve its
  checksum and member listing, then install a verified corrected artifact.
- A missing or broken Webman link with correct installed metadata is a DSM package-framework
  CGI-registration failure. Preserve package-manager logs and reinstall the verified corrected SPK;
  do not create the link manually.
- A correct link and readable target with an `/api.cgi` 404 requires the matching
  timestamp from `/var/log/nginx/error.log`, `/var/log/synopkg.log`, and `/var/log/messages` for DSM
  routing diagnosis.
- If the native bundle/style load but the AppWindow never instantiates, preserve the first browser
  console error and matching DSM/package log timestamps. If only `api.cgi` fails, continue with the
  read-only API-service checks below.

These commands are diagnostic only. Never `ln`, `rm`, `chmod`, or `chown` anything under
`/usr/syno/synoman/webman`, and never add root execution to the package to work around the route.

## Dashboard reports DSM session authentication or control bridge unavailable

Open the application from the DSM desktop or Package Center and confirm the user is a non-root DSM
administrator with a current login session. The browser sends that same-origin cookie to the
packaged CGI; that cookie is the active native authentication input. The AppWindow does not inspect
or rewrite the DSM shell location and sends no `SynoToken`. If the exact message still says a launch
token is required, the installed UI predates the 26.10 native contract. Do not paste anything into
the URL; install a verified 26.10-or-later artifact once published, or use the CLI meanwhile.

The application intentionally has no undocumented DSM-global or login-API fallback. If a fresh
launch cannot authenticate, record the model, DSM build, package version, launch path, `api.cgi`
status, and bounded package logs, then use [CLI parity](cli-parity.md) while diagnosing the service.
Never paste a cookie or package CSRF token into an issue, screenshot, terminal, browser storage, or
bookmark.

## Dashboard is read-only

Read-only means the authenticated API service snapshot did not grant the required capability and
independent CSRF. Possible causes include cookie authentication/admin rejection, CSRF bootstrap
failure, a stopped API service, wrong CGI/socket ownership or mode, a stopped controller/private
state, or an unsafe package path.

Use `status`, `paths`, the bounded logs above, and Package Center restart. Do not chmod or chown the
CGI/socket, add any identity-changing permission or capability, expose the socket, or hand-create
the CSRF key.

## A queued action remains pending or becomes outcome-unknown

Configuration, secret, routine, and alert-policy saves plus Doctor observe a server job result with
no client pending-state deadline. A healthy pending response continues to be observed until the
controller returns terminal or `expired_or_missing` evidence. Five consecutive result-observation
failures or an invalid result document instead produce a typed outcome-unknown result. Closing the
AppWindow aborts browser observation but does not cancel a job already accepted by the server.

1. Refresh the dashboard snapshot.
2. Inspect structured Activity and bounded logs.
3. Check controller status and the host-local run/management lock.
4. Confirm whether the intended profile/routine/policy is now visible.
5. For a profile save, inspect configuration and every credential-presence marker: configuration and
   earlier secret stages may have applied before a later stage failed or became outcome-unknown.
6. Resubmit only after deciding the first job did not apply.

The bridge caps the queue at 256 outstanding safe entries. Request/secret artifacts retain for up to
24 hours; completed responses and unrecoverable processing-orphan artifacts retain for one hour,
also capped at 256. An `expired_or_missing` terminal result means the response is no longer available;
it does not prove success or failure.

After abrupt power loss, a job already claimed for processing is never replayed automatically. Its
outcome is deliberately indeterminate. Compare snapshot, Activity, health, target inventory, and the
requested change before explicitly re-running it.

Doctor is initially queued, but the page polls its sanitized controller result to a terminal state.
Plan and Run remain asynchronous and are shown as queued until normal run/activity evidence changes.

## Profile save is rejected

Check the package identity can read and traverse the local source. Then review:

- profile-name alphabet and immutable existing name;
- physical absolute source versus File Station logical remote path;
- HTTPS URL/prefix and **Allow plain HTTP** state;
- remote `/`, repeated separators, trailing slash, dot segments, managed/reserved components, and
  portability length;
- `jobs`, retry, timeout, connect-timeout, rate, and exclude bounds;
- quiet combined with nonzero verbosity;
- remote-log required mode without an HTTPS collector URL; and
- invalid-certificate selection without the explicit interception-risk confirmation.

The browser can allow a syntactically numeric value that the strict bridge still rejects. Effective
connect timeout is at most 600 seconds and notification cooldown is at least 60 seconds.

## Password or TOTP authentication fails

Confirm the target account can use File Station and can see the selected first path component. Test
the exact reverse-proxy base URL, not merely an HTML File Station page. Re-enter the password through
the masked editor or CLI prompt.

For TOTP, store the Base32 seed or original `otpauth://` URI, not a six-digit current code. Synchronize
both NAS clocks. Secure SignIn push, hardware keys, and other interactive approval challenges are not
supported for unattended File Station login.

## Doctor receives HTML, 404, or API discovery failure

The configured public origin/prefix must route its `/webapi/*` path to File Station. A browser alias
or login page alone is not enough. Verify certificate hostname/trust, request-body limit, upstream
send/read timeout, and prefix rewriting through the exact public URL.

Do not enable invalid-certificate acceptance until the actual CA/hostname problem has been diagnosed.
For a private CA, grant the package user read access to an absolute non-symlink certificate file and
configure that path.

## Destination is not writable

Use `/home/Drive/...` only for the authenticated account's already-provisioned home. Use
`/<share>/...` only when that shared-folder root already exists and is visible to the account. The
engine can create missing descendants beneath the nearest existing writable parent; it cannot create
a DSM share, user home, Team Folder, or ACL.

Run normal Doctor first. Use the disposable write test only on a prepared non-critical destination
and inspect for leftovers after a failure.

## Realtime reports polling

`polling` is an explicit safe fallback when `inotifywait` is unavailable or a watcher recently
failed. Confirm source ACLs and controller logs. Polling uses path/size/mtime fingerprints at the
configured interval; reducing the interval increases source traversal work. Do not claim native
realtime operation unless the snapshot reports `inotify`.

## Routine is deferred

Check active weekday/time window, NAS clock/time zone, dependency latest-success states, and deletion
approval. If a profile's deletion cap is greater than its routine approval ceiling, the controller
defers instead of weakening the bound. Cyclic, missing, duplicate, or self dependencies are rejected
at configuration time.

## DSM notification does not arrive

Confirm the package alert policy, failure threshold, cooldown, and that a DSM administrator is logged
in to the desktop. Look for `notification.unavailable` in Activity, then inspect the bounded package
logs. The package sends only fixed preloaded I18N title/message keys through
`/usr/syno/bin/synodsmnotify -c`; it does not include a profile, exit code, path, URL, account, secret,
or arbitrary log text in notifier arguments. Details remain in Activity and logs.

This path is desktop-only. The package does not acquire `conf/resource` `sysnotify` and does not
register Notification Center email, SMS, mobile, CMS, or rule/channel delivery. Checking those
channels cannot repair a missing desktop alert.

The browser fallback is not an unattended transport. It requires the dashboard to remain open and
browser permission to be granted.

## Exit `75`, active lock, or package will not stop

Status and logs should identify the active operation/PID. Wait for a legitimate operation to finish.
Stop uses cooperative termination and can wait for the core to unwind; it deliberately refuses an
untrusted PID and does not force-kill after timeout.

Stale locks are recovered only when their recorded PID is no longer live. Do not recursively delete
package run/control directories. If identity/path validation reports `73`, preserve evidence and
repair through a verified reinstall/upgrade rather than manual chmod/chown edits.

## No remote-to-remote shortcut

The SPK must run where the source is locally readable. File Station server-side copy/move works only
inside one authenticated target NAS. A unique same-basename matching file may be reused by the core's
guarded same-target cross-parent copy optimization, but NAS A cannot directly instruct NAS B to copy
bytes from NAS A.

The package stores no persistent hash/path correspondence database. Every run rebuilds evidence from
a fresh local scan and target inventory, verifies uploads, and performs final reconciliation.

## Live-NAS acceptance

Repository tests and static SPK validation are not live acceptance. Before production use, execute
the general [production acceptance and recovery runbook](../production-acceptance.md) with the SPK on
the exact source NAS and record all of the following:

### Hardware and installation

- model, Package Arch, `uname -m`, DSM product/version/build;
- selected SPK filename, version, SHA-256, optional GitHub attestation;
- Package Center install, third-party warning, start, stop, restart, upgrade, rollback constraints,
  and disposable uninstall; record a normal unsigned-publisher warning separately from any
  lower-privilege/root rejection and retain the bounded log tails above;
- dashboard icon/Open behavior and actual native DSM AppWindow layout at narrow and wide sizes;
- the installed module-keyed config registers `SYNO.SDS.App.SynologyDriveSync.Instance` as
  `type=app`, Package Center and desktop Open instantiate that class without any `index.html`
  navigation, the bundle/style load, DSM's Webman link resolves to `target/ui`, and
  `/webman/3rdparty/synology-drive-sync/api.cgi` is reached without a routing 404;
- on every claimed DSM branch—and specifically DSM 7.0/7.1 where supported—the same AppWindow
  launch, rendering, assets, and API path succeed on physical NAS hardware; and
- a fresh native launch authenticates with the same-origin DSM cookie, does not inspect/rewrite the
  DSM shell location, and sends no `X-SYNO-TOKEN` header.

Rendered browser QA was unavailable in the development environment. Do not mark layout,
accessibility interaction, or browser-header-to-CGI forwarding as already proven.

### Identity, ACL, and dashboard security

- the actual DSM package identity has read/traverse permission only to intended source shares;
- `conf/privilege` is the exact package/`http` document above, all installed executables are `0755`,
  and no package file carries an identity-changing permission bit or Linux capability;
- `ui/api.cgi` is package-owned `0755` but executes with real/effective DSM `http` UID; the API
  `--serve` process executes as the package user; and `ui/api.sock` is package:`http` `0660`;
- the CGI and service reject a substituted socket, wrong owner/group/mode, wrong peer UID, symlink,
  extra hard link, unsafe parent, oversized frame, or malformed relay schema;
- another internal user/root cannot use manager mutations in place of the package identity;
- non-administrator DSM users cannot launch or call the API;
- DSM maps a browser request containing exactly `X-SDSYNC-Request: 1` into the CGI environment as
  `HTTP_X_SDSYNC_REQUEST=1`, the bounded relay preserves that marker to the package-user service,
  and an omitted or wrong-value marker is rejected;
- a stale cookie, any malformed/mismatched optional compatibility token supplied directly to the
  API parser, missing/expired CSRF, wrong methods/fields, and direct CGI calls fail closed, while the
  token-free native UI succeeds through cookie authentication; and
- no secret appears in URL history, Referer, browser storage, Activity, logs, DSM desktop alerts,
  queue result, or support evidence.

### Profile, target, and Doctor

- one `/home/Drive/...` target when home sync is intended and one Team Folder/shared-folder target
  where applicable;
- a missing nested destination is created beneath an existing writable parent while an out-of-scope
  sibling canary remains unchanged;
- source hash scan, API discovery, authentication, target inventory, and prepared disposable write
  test succeed;
- reverse-proxy prefix, certificate/private CA, upload limit, timeouts, largest file, password, TOTP,
  and NAS clock are proven; and
- Synology Drive visibly indexes the File Station result when that behavior is required.

### Automation and observability

- interval, daily, and realtime routines, weekdays/windows, debounce, native watcher or polling
  fallback, retries/backoff, and dependencies behave as documented;
- manual and scheduled actions do not overlap on the source NAS;
- queued configuration and Doctor terminal results, plus asynchronous Plan/Run status, are
  observable;
- Activity/log bounds and rotation work through restart;
- direct DSM success/failure/Doctor desktop alerts obey threshold/cooldown, use only fixed I18N keys,
  expose details only through Activity/logs, and do not register Notification Center channels;
  and
- service restart after source-NAS reboot preserves configuration without triggering an unreviewed
  immediate mutation.

### Safety, upgrade, and recovery

- additive Plan/Run preserves target-only canaries;
- deletion is tested separately with small profile and action bounds, snapshots/version history, an
  empty-source test, mount-boundary test, changed-snapshot test, and failure-before-delete evidence;
- safe upgrade retains profiles, secrets, routines, logs, and validates the configuration;
- rollback documentation acknowledges that completed remote writes are not reversed; and
- disposable uninstall removes package-private profiles/secrets/state/logs while preserving both
  NAS data trees.

TOTP challenge behavior, DSM cookie authentication, direct `synodsmnotify`
desktop delivery, File Station versions, reverse proxies, and Drive indexing vary across deployed
systems. In
particular, Synology documents direct `authenticate.cgi` use by a custom CGI, but this root-free
design invokes it from the package-user API service after an authenticated local socket relay. That
execution behavior remains a live-DSM acceptance requirement. A complete record from the exact
environment is the acceptance evidence.
