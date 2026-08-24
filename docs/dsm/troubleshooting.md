# Troubleshooting and live-NAS acceptance

Start with evidence from the exact NAS, package identity, and profile. Do not fix a dashboard error
by broadening ACLs, disabling TLS, editing private files, or repeatedly resubmitting a mutation.

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo synopkg status synology-drive-sync
sudo -u synology-drive-sync -- "$MANAGER" status
sudo -u synology-drive-sync -- "$MANAGER" logs 200
sudo -u synology-drive-sync -- "$MANAGER" paths
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

## Package Center says the package uses root privileges

Use a current release artifact. The SPK must contain no setuid/setgid archive member:
`ui/api.cgi` is stored in `package.tgz` as ordinary `0755`, and `conf/privilege` separately tells DSM
to install it as package/package `4755`. That installed setuid bit targets only the non-root package
identity; lifecycle actions remain `run-as: package`, and no Linux capabilities are requested.

Do not repair an older or locally modified SPK with `chmod`, by deleting the privilege manifest, or
by changing it to root. Rebuild it with the current assembler and run `validate_spk.py` against the
exact output. The validator rejects both an archived `4755` entry and a manifest that fails to apply
the reviewed package-owned CGI mode.

## Dashboard does not open or says the launch token is missing

Open the application from the DSM desktop or Package Center, not a saved direct URL. Confirm the
user is a non-root DSM administrator with a valid login session. Reloading an old token-bearing URL
is not a recovery method.

The application intentionally has no undocumented DSM-global or login API fallback. Physical DSM 7
AppLaunch delivery of SynoToken has not yet been proven. If the fresh launch still omits it, record
the model/build/launch path and use [CLI parity](cli-parity.md). Never paste session tokens into an
issue, screenshot, terminal, browser storage, or bookmark.

## Dashboard is read-only

Read-only means the bridge snapshot did not grant the required capability and independent CSRF.
Possible causes include authentication/admin rejection, missing/expired SynoToken, CSRF bootstrap
failure, helper ownership/mode drift, stopped controller/private state, or an unsafe package path.

Use `status`, `paths`, the DSM package-control log, and Package Center restart. Do not chmod
`ui/api.cgi`, make the general CLI setuid, add capabilities, or hand-create the CSRF key.

## A configuration action remains pending or times out

Configuration, secret, routine, and alert-policy saves poll a server job result for up to two
minutes. A timeout means completion is unknown, not that the job was cancelled.

1. Refresh the dashboard snapshot.
2. Inspect structured Activity and bounded logs.
3. Check controller status and the host-local run/management lock.
4. Confirm whether the intended profile/routine/policy is now visible.
5. Resubmit only after deciding the first job did not apply.

The bridge caps the queue at 256 outstanding safe entries. Request/secret artifacts retain for up to
24 hours; completed responses and unrecoverable processing-orphan artifacts retain for one hour,
also capped at 256. An `expired_or_missing` terminal result means the response is no longer available;
it does not prove success or failure.

After abrupt power loss, a job already claimed for processing is never replayed automatically. Its
outcome is deliberately indeterminate. Compare snapshot, Activity, health, target inventory, and the
requested change before explicitly re-running it.

Doctor, Plan, and Run are intentionally asynchronous and remain shown as queued until normal
run/activity evidence changes.

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

Confirm the package alert policy, failure threshold, cooldown, and the administrator's DSM
Notification Center channels. Look for `notification.unavailable` in Activity. The package registers
only `sync_succeeded`, `sync_failed`, and `doctor_failed`; it does not send arbitrary log text.

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
  and disposable uninstall;
- dashboard icon/Open behavior and actual DSM iframe/window layout at narrow and wide sizes; and
- whether DSM AppLaunch supplies SynoToken to a fresh administrator launch.

Rendered browser QA was unavailable in the development environment. Do not mark layout,
accessibility interaction, or AppLaunch token forwarding as already proven.

### Identity, ACL, and dashboard security

- `synology-drive-sync` has read/traverse permission only to intended source shares;
- another internal user/root cannot use manager mutations in place of the package identity;
- non-administrator DSM users cannot launch or call the API;
- stale cookie, absent/mismatched SynoToken, missing/expired CSRF, wrong methods/fields, and direct CGI
  calls fail closed; and
- no secret appears in URL history, Referer, browser storage, Activity, logs, Notification Center,
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
- queued configuration terminal results and asynchronous Doctor/Plan/Run status are observable;
- Activity/log bounds and rotation work through restart;
- DSM success/failure/Doctor notifications obey threshold/cooldown and contain only fixed safe data;
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

TOTP challenge behavior, DSM authentication, AppLaunch token delivery, Notification Center,
File Station versions, reverse proxies, and Drive indexing vary across deployed systems. A complete
record from the exact environment is the acceptance evidence.
