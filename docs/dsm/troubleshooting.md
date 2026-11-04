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

> [!WARNING]
> Release 26.20 has a DSM authentication bug: it rejects DSM's standard `system:system` (`1:1`)
> `authenticate.cgi`. Use 26.21 or later after verifying its exact SPK and checksum. Do not change
> the helper link, ownership, mode, package groups, or package privilege policy.

The current corrected source contract requests no root run-as, joined web group, Linux capability,
set-user-ID bit, or set-group-ID bit. Every executable, including `ui/api.cgi`, is ordinary `0755`,
and `conf/privilege` contains exactly:

```json
{
  "defaults": {
    "run-as": "package"
  }
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
exist if installation or first start did not reach that stage. Collect them before using the
dashboard's per-log **Clear** control in Activity: that control empties the durable log and removes
its retained rotations, and the evidence it discards cannot be recovered. Inspect all output locally before
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

### The same mismatch inside scheduled runs

A related field failure hit scheduled routines rather than startup: DSM handed the runner the
physical `SYNOPKG_PKGDEST`, `SYNOPKG_PKGHOME` and `SYNOPKG_PKGVAR` (`/volumeN/@appstore/...`) while the
manager had written the profile's log path under the `/var/packages/synology-drive-sync/...` alias. The
runner's log-file preflight compared the two as strings and refused every profile (exit `66`), and the
supervisor refused its own binary because `argv[0]` spelled a different path. Nothing in Activity said
so. The package now pins `package_root`, `package_home` and `package_var` to the alias whenever the
alias and the physical path resolve to the same device and inode, in the runner, the controller, and
the lifecycle scripts, so every process derives byte-identical paths. The bridge's supervisor check
likewise accepts a path that is the same file as the package binary, while still executing the alias.
A supervisor refusal is no longer silent: it prints one `sdsync-dsm-api: supervised core refused:
<code>` line and exits `78`, recorded as reason `supervisor_refused`.

## `api.cgi?action=csrf` reports semantic status 503

Package-generated GET failures use HTTP transport 200 because Webman can discard or replace non-2xx
CGI bodies. Failure remains explicit in the nonempty trusted JSON envelope:
`schema="sdsync.dsm-error.v1"`, `ok=false`, semantic `status`, stable `code`, bounded `stage`, and
generic `message`. The AppWindow uses those semantic fields, not the transport status, and never
treats `ok=false` as success. POST failures retain their real HTTP status and mutation-acceptance
semantics.

Semantic status 503 with code `service_unavailable` and stage `bridge_connect` means the fixed
package-private API socket was not ready for a verified connection. A request that overlaps normal
startup retries only the missing, connection-refused, or private pre-commit socket state for a short
bounded window. Retry once after Package Center reports the package running. Repeated semantic 503
responses mean the service did not become ready; **Stop** **Synology Drive Sync** in Package Center,
**Run** it again (DSM offers no Restart action for a package), and inspect the package startup result rather than repeatedly refreshing the dashboard.

Semantic stage `dsm_authentication` identifies the server-side DSM identity boundary. Its fixed
codes are:

- `dsm_authentication_helper_unsafe` with status 503: `X_OK` succeeded, but the fixed helper entry, a
  root-owned symlink boundary/ancestor, the final DSM `system:system` (`1:1`) or legacy root-owned
  executable, or immediate pre-execution identity failed owner/non-writability/type/stability
  validation;
- `dsm_authentication_helper_unavailable` with status 503: the fixed-path `X_OK` probe failed with
  anything other than `EACCES`, or a validated executable helper could not start or complete within
  its bound;
- `dsm_authentication_rejected` with status 401: the executable direct helper did not authenticate
  the native cookie;
- `dsm_authentication_quickconnect_unsupported` with status 401: the same direct-helper rejection
  arrived with a strictly parsed `*.quickconnect.to` authority. Synology excludes third-party
  applications from QuickConnect support, so retry through LAN, DDNS, VPN, or a separately tested
  DSM custom reverse proxy. This code is diagnostic only and never changes authentication;
- `dsm_authentication_forbidden` with status 403: the direct-helper identity did not satisfy the
  independent account/administrator requirement;
- `dsm_authentication_webapi_unavailable` with status 503: the kernel-inaccessible-helper fallback
  could not obtain one bounded HTTP 200 response from the loopback-pinned DSM user service, or the
  body was malformed/oversized or did not match the required typed session schema;
- `dsm_authentication_webapi_rejected` with status 401: that user service returned `success=false`,
  omitted session data, or returned a semantically invalid username; and
- `dsm_authentication_webapi_forbidden` with status 403: the typed response contained
  `is_admin=false`, or the returned account failed the independent NSS/administrator check.

Collect the helper chain and package-UID execute result with read-only checks:

```bash
AUTH=/usr/syno/synoman/webman/modules/authenticate.cgi
PACKAGE_USER=$(stat -L -c '%U' /var/packages/synology-drive-sync/home)
TARGET=$(readlink -f "$AUTH")

case $PACKAGE_USER in
  ''|root|UNKNOWN) echo 'unsafe or unresolved package identity' >&2; exit 1 ;;
esac

stat -c '%F %a %u:%g %h %n' "$AUTH"
stat -Lc '%F %a %u:%g %h %n' "$AUTH"
readlink "$AUTH" || true
printf 'canonical target: %s\n' "$TARGET"
command -v namei >/dev/null 2>&1 && namei -l "$AUTH"
sudo -u "$PACKAGE_USER" test -x "$AUTH" \
  && echo 'package UID can execute fixed helper entry' \
  || echo 'package UID cannot execute fixed helper entry'
```

These commands expose no DSM cookie or package CSRF token. Preserve the output with the exact
installed package version and semantic response code.

Do not replace the helper link, broaden directory or target modes, grant set-id/capabilities, join the
package to DSM's `system` group, add a `conf/resource` acquisition, run the package as root, or call a
DSM endpoint manually with the cookie. The package probes the exact fixed helper entry before
inspecting its metadata. If fixed-entry `test -x` succeeds, it validates every ancestor/link boundary
and the canonical target, then rechecks the final device/inode/UID/GID/mode immediately before direct
execution. Root-owned ancestors and links remain mandatory. The final canonical executable accepts
DSM's exact built-in `system:system` identity (`1:1`) or the legacy root-owned form; no package group
membership or permission change is required.
If that probe fails with `EACCES`, it deliberately skips path validation and selects the bounded
loopback user-service path—not a request to change permissions. Any other probe error fails closed.

The fallback connects only to literal `127.0.0.1` on the current CGI server port, derives HTTP versus
HTTPS from the current request, disables proxying and redirects, bounds time and response bytes, and
uses the cookie and optional AppWindow-bootstrapped SynoToken only as sensitive headers. It never
trusts `SERVER_NAME`, a remote address, or a token-bearing URL. It requires valid `Session.user` and
exact `is_admin=true`, then repeats NSS and administrator membership checks before relay. Synology
publicly documents direct `authenticate.cgi` use by custom CGI and `SYNO.API.Auth` version 6
`method=token`. The private fallback response shape is corroborated by saved first-party DSM runtime
sources, was not present in the supplied HTTP capture, and is not a public API promise. Preserve the
actual branch and result as physical-NAS acceptance evidence.

When policy permits the stage-derived warning category, a pre-relay failure is coalesced globally to
at most one emission per 30 seconds and recorded as fixed, secret-free JSON in
`/var/packages/synology-drive-sync/var/log/api.log` (10 MiB, five backups). The matching fixed Activity
event is in `/var/packages/synology-drive-sync/var/log/activity.log` and is visible through the
dashboard or `sdsync-dsm api activity --lines 100`. Authentication warnings obey
`authentication_log_level`; `off` suppresses both persistence and Activity before coalescing state is
touched. Records contain only epoch, level/category, fixed event/service/stage/code, and status—not
request environment, query, cookie, token, username, or path. An unsafe/corrupt policy or unsafe log
path fails closed, so preserve the semantic response itself when no diagnostic was persisted.

Synology's
[QuickConnect support list](https://kb.synology.com/en-eu/DSM/tutorial/Which_services_support_QuickConnect)
marks every third-party service and application unsupported. Its
[QuickConnect white paper](https://global.download.synology.com/download/Document/Software/WhitePaper/Os/DSM/All/enu/Synology_QuickConnect_White_Paper_enu.pdf)
also distinguishes direct, relay, and WebRTC paths. Do not copy proxy-origin headers into the helper
or replace `REMOTE_ADDR` to work around a rejection: DSM's native source/session validation remains
authoritative. The package recognizes a valid QuickConnect hostname only after rejection to improve
the error code; it neither authorizes the request nor invokes the private loopback user service on
that basis.

A raw empty or HTML HTTP 503 is not this package envelope. It indicates Webman, its proxy, the route,
CGI launch, or another pre-response layer failed or replaced the output. Record its response headers,
duration, exact package version, and matching bounded Webman/package log lines. A later Synology
`synowebrtc` message such as `Peer connection closed` is downstream noise, not the root cause.

Failed startup now emits one fixed-value line beginning `startup diagnostic:`. Collect that complete
line from the Package Center start details or package log, together with the bounded
`controller.log` and `api.log` tails listed above. Its fields report only states such as
`exact`, `missing`, `absent`, `mismatch`, `prepared`, `unverified`, or `unsafe` for the controller
PID/child/lock and API PID/bound/socket checks; they contain no request query, cookie, token, secret,
or filesystem path.
Do not hand-create readiness files, remove locks, broaden socket permissions, or run either service
as root. Those changes erase the evidence and bypass the package's fail-closed identity checks.

## `api.cgi?action=csrf` reports semantic status 400

First inspect only the response type and bounded error code in the browser Network panel. Do not
copy cookies, CSRF values, Synology tokens, or the complete request headers.

- Package JSON with schema `sdsync.dsm-error.v1`, semantic status 400, and code `invalid_request`
  means Webman reached the package CGI but its request metadata was rejected. Release 26.14 treated DSM/FastCGI variables
  such as an empty `CONTENT_LENGTH`, `CONTENT_TYPE`, transfer encoding, or mutation-token header as
  present request data. Install the first release after 26.14; the bridge now treats only exact empty
  CGI metadata as absent while continuing to reject non-empty GET bodies, content types, transfer
  encodings, and mutation tokens.
- Synology HTML rather than the package JSON schema means DSM, its reverse proxy, or QuickConnect
  rejected the request before the package CGI. Reopen the app from the DSM desktop on the same
  origin used to sign in. If the response remains HTML, troubleshoot the DSM route rather than the
  package socket.
- Package JSON with code `unauthorized` carries semantic status 401 over GET transport 200. Sign in
  to DSM again and reopen the AppWindow so the browser can attach the DSM session cookie. A missing
  cookie is deliberately never converted into package authority.

The compatibility handling does not relax authentication: the browser request marker, DSM cookie,
one validated native-CGI identity path, independent CGI and daemon UID/name/administrator checks,
HTTPS policy, recomputed session binding, and POST CSRF verification remain mandatory. A trusted
kernel-executable helper cannot silently fall back after a direct-path failure.

## DSM says the page is not found when opening the app

The current source uses a native `type=app` AppWindow. DSM desktop and Package Center instantiate
the class registered by `dsmappname` and installed `ui/config`; a third-party package directory is
not a documented AppWindow URL. The package therefore ships no `ui/index.html` or undocumented
`/webman/index.cgi?launchApp=...` redirect. Opening
`/webman/3rdparty/synology-drive-sync/` in a separate tab may legitimately show DSM's generic
“Sorry, the page you are looking for is not found” response and does not by itself prove that the
native registration or CGI route is broken.

First click **Open** in Package Center or launch **Synology Drive Sync** from the DSM desktop. In the
browser Network panel, record only each failed request path, status, and response type. Do not copy,
save, or share arbitrary query strings because they can contain session material. A correct native
launch loads the registered content-addressed `SynologyDriveSync.<bundle-sha256-prefix>.js` module.
The dashboard's API requests use the exact
independent path `/webman/3rdparty/synology-drive-sync/api.cgi`.

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
  "$WEBMAN_LINK" "$UI_ROOT" "$UI_ROOT/config" "$UI_ROOT/style.css" "$UI_ROOT/api.cgi"
find "$UI_ROOT" -maxdepth 1 -type f -name 'SynologyDriveSync.*.js' -exec \
  stat -Lc '%F %a %U:%G %n' {} \;
grep -F 'SYNO.SDS.App.SynologyDriveSync.Instance' "$UI_ROOT/config"
grep -F '"type": "app"' "$UI_ROOT/config"
```

The installed fields must be `package="synology-drive-sync"`,
`dsmuidir="synology-drive-sync:ui"`, and
`dsmappname="SYNO.SDS.App.SynologyDriveSync.Instance"`. `config`, `style.css`, and exactly one
content-addressed `SynologyDriveSync.<bundle-sha256-prefix>.js` must be regular `0644` files;
`config` must wrap that class beneath the same module filename and declare `type="app"` with the
matching `appWindow`. `api.cgi` must
be a regular `0755` file. The Webman link must resolve to that installation's `target/ui` for the
CGI endpoint. Interpret the evidence as follows:

- A 404 for the package directory or `index.html` in an ordinary browser tab is not an AppWindow
  failure. Diagnose the DSM desktop/Package Center application-class launch and the exact `api.cgi`
  route separately. An installed `.url`/`type=url` config still identifies an invalid or stale
  package.
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
administrator with a current login session. The AppWindow must first issue the official same-origin
`GET /webapi/entry.cgi?api=SYNO.API.Auth&version=6&method=token` request. A valid bounded
`data.synotoken` is encoded exactly once, retained only in module memory, and sent only as the package
`X-SYNO-TOKEN` header; the browser separately attaches the DSM cookie. The AppWindow does not inspect
or rewrite the DSM shell location. If the exact message still says a launch token is required, the
installed UI predates this contract. Never paste a token into the URL or manually replay the private
endpoint; install the corrected verified artifact or use the CLI meanwhile.

The application has no launch-location, DSM-browser-global, persistent-storage, or client-side login
fallback. Server-side, it probes fixed-path `X_OK` first. Success triggers full helper
validation/revalidation and direct execution; `EACCES` skips the validator and selects the bounded
loopback user-service path; other probe errors fail closed. If a fresh launch cannot authenticate,
record the model, DSM build, package version, launch path, whether the official token request
succeeded without recording its value, whether the package request carried `X-SYNO-TOKEN`, helper
owner/mode and package-UID fixed-entry `test -x` result, `api.cgi` semantic status/code/stage, and
bounded package logs. Then use [CLI parity](cli-parity.md) while diagnosing the service. Never paste a
cookie, SynoToken, or package CSRF token into an issue, screenshot, terminal, request body, browser
storage, history, or bookmark.

## Dashboard status is stale, or a read reports a named in-service failure

A dashboard read that the package accepted, authenticated and admitted can still fail while the
service is assembling the answer. Those failures all arrive as one semantic 503, and each of them now
carries a code naming which one it was. Before the codes existed every one of them rendered "Package
service unavailable · Restart Synology Drive Sync", which is wrong advice for a package that is
mid-upgrade, wrong for one that is merely busy, and actively harmful for one whose files have the
wrong ownership, where a restart hides the finding without fixing it.

The code appears in the failure envelope the AppWindow receives and in the coalesced `api.log`
record described below. Codes marked **not retried** stand the automatic refresh down: the header
says so, and **Retry** re-arms it once the package is repaired.

| Code | What it means | What to do |
| --- | --- | --- |
| `runtime_upgrading` | The package transition marker says an upgrade is in progress | Wait; the page retries on its own |
| `runtime_uninstalling` | The transition marker says the package is being removed | Nothing. **Not retried** |
| `runtime_closed` | Admission is closed because the package is stopping or upgrading | Wait; the page retries on its own |
| `runtime_marker_unsafe` | A transition or admission marker is unreadable or corrupt | Inspect `api.log`. **Not retried** |
| `policy_unreadable` | The service could not read its own `security.conf` | Repair or reinstall one complete release. **Not retried** |
| `manager_busy` | The five-second wait for a manager lane expired; at most three reads run at once | Close AppWindows you are not using. Retried on a bounded ramp |
| `manager_lane_poisoned` | The manager lane cannot serve further reads in this process | Stop the package in Package Center, then Run it again. **Not retried** |
| `manager_unsafe` | The manager is missing, wrongly owned, writable by others, setuid, or not executable | Repair or reinstall; do not change ownership or permissions by hand. **Not retried** |
| `manager_spawn_failed` | The service could not fork or exec the manager | Inspect `api.log`. Retried on a bounded ramp |
| `manager_timeout` | The manager exceeded its twenty-second read ceiling | Check for a concurrent sync or an unusually large log set. Retried on a bounded ramp |
| `shell_snapshot_building` | The forced shell lane is building the status snapshot in the background (a package with hundreds of profiles on a small CPU) and the read gave up waiting after eight seconds; the build is not killed, and the read after it finishes is served | Wait; the page retries on its own with backoff, and the title reads "Package is building its status view". If it never clears, the build failed and the read reports that failure's own code (`manager_exit_status`, `manager_timeout`, `manager_busy`) |
| `control_lock_busy` | A dashboard request waited eight seconds for the control lock (the queue or audit-outbox lock) held by another job; the consumer and CLI wait up to 120 seconds | Retry. It is safe: an enqueue is idempotent by client request id. Persistent occurrences mean a job is holding the lock for a long time; inspect `controller.log` |
| `manager_output_too_large` | The manager produced more than the one-mebibyte output bound | Inspect `api.log`. Retried on a bounded ramp |
| `manager_exit_status` | The manager exited non-zero | Inspect `api.log`. Retried on a bounded ramp |
| `manager_output_invalid` | Manager output was not JSON, not an object, or missing a required key | Inspect `api.log`. Retried on a bounded ramp |
| `manager_output_schema` | The document's schema string is not the one this bundle expects | Repair or reinstall one complete release. Retried on a bounded ramp |
| `config_file_unsafe` | A configuration or state file failed the ownership and mode contract | Repair the package rather than editing the file. **Not retried** |
| `package_state_corrupt` | A stored package record could not be parsed; the Activity feed is where this surfaces today | Inspect Logs and Activity for the affected file. A restart does not repair it; the record clears when the file rotates or is cleared. **Not retried** |
| `clock_unavailable` | The system clock reads before the epoch | Correct the NAS system time. **Not retried** |

A code this bundle does not recognise still renders the original "Package service unavailable"
guidance, so a newer package cannot leave the window silent.

**In-service failures appear in `api.log`, not in the Activity feed.** This is deliberate and it is
the one place the design will surprise someone: an operator told to inspect Activity for a
`manager_timeout` will find nothing there. The Activity half of a pre-relay diagnostic is written by
the manager against an allowlist of exact stage/code/status triples, and these codes are not in it —
the same is already true of `service_saturated`. Use
`/var/packages/synology-drive-sync/var/log/api.log`, or the **API** source in the dashboard's Logs
view, and grep for `cgi_failure`.

Two properties of that record matter when you read it. It is coalesced globally to at most one
emission every thirty seconds across every stage and code, so **a coalesced record names the most
frequent code in its window and counts all of them** in `occurrences`; an outage producing three
distinct codes emits a record naming one. And the record obeys the configured log level exactly as
the authentication warnings above do: a policy that suppresses the warning category writes no record
and touches no coalescing state, while the 503 and its code still reach the browser.

While a read is failing, the dashboard keeps the last document it did retrieve and labels it with
both its age and the cause; see [the dashboard's connection states](dashboard.md). Nothing is
cached on the service side, so a read either answers with current state or fails with one of the
codes above.

If a named code proves to be a parity problem rather than a fault — a reported failure the previous
release did not produce for the same package state — the read lane has an operator kill switch that
returns every read to the manager for the whole service. It is a private control file, not a
setting, and engaging it writes one log line. [Operations](operations.md) documents the file, its
exact contents, and how to confirm it took effect.

## Dashboard is read-only

Read-only means the authenticated API service snapshot did not grant the required capability and
independent CSRF. Possible causes include cookie authentication/admin rejection, CSRF bootstrap
failure, a stopped API service, wrong CGI/socket ownership or mode, a stopped controller/private
state, or an unsafe package path.

Use `status`, `paths`, the bounded logs above, and a Package Center Stop followed by Run. Do not chmod or chown the
CGI/socket, add any identity-changing permission or capability, expose the socket, or hand-create
the CSRF key.

## A queued action remains pending or becomes outcome-unknown

Each mutation uses one serialized body and client request ID. When POST delivery or acknowledgement
is ambiguous, the AppWindow first performs an authenticated `request-status` lookup for that client
request ID. The ownership binding uses the unique DSM `id` cookie, authenticated username/UID, and the
exact optional launch token; mutable ancillary QuickConnect cookies are ignored. An owned positive
mapping supplies the original server job ID, so the browser resumes its normal result poll without
submitting a duplicate. Missing and foreign-session mappings are deliberately indistinguishable. Only
an exact schema-validated `unresolved` response authorizes a bounded replay of the byte-identical
serialized request; a malformed, unavailable, or operation-mismatched response never authorizes one.
Private-queue idempotency resolves an already accepted replay to the same job.

Profile saves, authentication/remote-browser probes, and autosave mutations retry transient result
reads within explicit overall limits. Other terminal observers continue while pending but produce a
typed outcome-unknown result after five consecutive result-observation failures, invalid or
`expired_or_missing` evidence, or AppWindow shutdown. Closing the AppWindow aborts browser observation
but does not cancel a job already accepted by the server.

1. Refresh the dashboard snapshot.
2. Inspect structured Activity and bounded logs.
3. Check controller status and the host-local run/management lock.
4. Confirm whether the intended profile/routine/policy is now visible.
5. For a profile save, inspect configuration and every credential-presence marker: configuration and
   earlier secret stages may have applied before a later stage failed or became outcome-unknown.
6. Note the request and job correlation shown by the UI, when available.
7. Only the affected UI scope and dependent operations are paused; use independent controls normally.
   An unresolved connection-request incident freezes the affected profile and credential fields and
   blocks profile submission, authentication, and File Station requests so the submitted draft cannot
   drift and two temporary sessions cannot overlap. Use **Reconcile connection request** to resume the
   exact queued result. A request DSM already settled — including a failure reporting that the
   temporary File Station session could not be closed — is reported with its correlation but does not
   raise that barrier; correct the draft and try again.
8. When the banner offers **Reconcile profile request**, use it before another save. It performs no
   mutation. A success requires exact request/job/operation ownership; configuration success also
   requires an exact fresh non-secret snapshot, while secret success requires trustworthy presence
   state for the named profile. An exactly correlated terminal failure unlocks only the failed stage
   for correction. Any mismatch leaves the draft locked.
9. Keep the AppWindow open while evidence is unresolved. Close and Cancel are intentionally disabled
   so the profile and credential draft cannot be discarded accidentally. Do not blindly create a new
   request.

The bridge caps the queue at 256 outstanding safe entries. Request/secret artifacts retain for up to
24 hours; completed responses and unrecoverable processing-orphan artifacts retain for one hour,
also capped at 256. An `expired_or_missing` terminal result means the response is no longer available;
it does not prove success or failure.

After abrupt power loss, a job already claimed for processing is never replayed automatically. Its
outcome is deliberately indeterminate. Compare snapshot, Activity, health, target inventory, and the
requested change before explicitly re-running it.

Doctor is initially queued, but the page polls its sanitized controller result to a terminal state.
Plan and Run remain asynchronous and are shown as queued until normal run/activity evidence changes.

### Named terminal failures

A job that ends without its operation having run reports a named code rather than disappearing into
`unresolved`. Each is a case where the controller previously deleted the processing request, wrote
its reason to `controller.log`, and left the dashboard unable to say anything except that the outcome
could not be established — a verdict indistinguishable from “this request ID was never accepted”,
which calls for the opposite action.

| Code | What happened | What to do |
| --- | --- | --- |
| `classification_failed` | The controller could not determine what kind of operation the request was, so it never started it. | Nothing ran and nothing changed. Read the classification exit code from `controller.log`, then submit again. |
| `secret_claim_failed` | The stored credential the request needed could not be claimed by the controller, so the operation was never started. | Nothing ran and nothing changed. Check the profile's stored credential under Security and `controller.log`, then submit again. |
| `consumer_failed` | The worker running the operation exited before finishing, or (daemon) passed its lane deadline of 3 minutes, 10 minutes or 6 hours and was stopped. The exit code is reported with the failure. | On a memory-constrained unit this is most often the kernel OOM killer — check `dmesg` as well as Logs. Retry with a narrower scope. |
| `consumer_wrote_no_result` | The worker exited cleanly but wrote no result, so how far it got cannot be established from the queue. | Treat the operation as **unconfirmed**, not as failed. This is the one named code that still raises a reconciliation barrier and retains its request authentication, because naming the stage that dropped a job is not the same as knowing what it left behind. Inspect Activity and current state before submitting again. |

Each code is published by the bridge itself, from the same result vocabulary as a failure inside the
worker, and every code the dashboard names has a writer in this package: a code with a help entry and
nothing that produces it would be documentation of a feature that does not exist.

None of these is an outcome-unknown verdict: the stage that dropped the job is known, so all but
`consumer_wrote_no_result` release their scope the way any other terminal failure does. An
outcome-unknown result still outranks a named code — naming a stage can never convert an unknown
outcome into a claimed-known one, and the reconciliation barrier keeps its exact current meaning.

That ordering is why `consumer_failed` and `consumer_wrote_no_result` are reported for some
operations and not for others. They describe a worker that died without saying how far it got, and
whether that is a knowable failure or a genuinely unknown outcome depends on what the operation was
allowed to commit:

- **Sync status, Doctor, Plan, Run, Resync, Clear logs, and the connection and remote-browser
  probes** commit no profile, credential, policy, or scheduler state. A worker that dies part-way
  through one of them has changed none of that, so “it failed” is the complete truth and the named
  code is reported. These are also every operation slow enough to be worth watching, so this is the
  case an operator actually meets.
- **Profile, credential, policy, schedule, and routine changes** may have committed part of the
  change before the worker died. There the outcome is genuinely unknown, so no result is published
  at all and the job stays outcome-unknown rather than being reported as a failure that may not have
  happened. Publishing a failure would not only claim the outcome, it would settle the audit record
  as `failed` — a response is the evidence the audit reconciliation adopts, and finding none beside
  a dead worker is precisely what makes it record `outcome_unknown` instead.

Both cases write `controller.log`, so the operation name and the worker's exit status are recorded
either way: `control_consumer_failed` for the first, `control_consumer_outcome_unknown` for the
second. Only the first reaches the dashboard as a named result.

A job that is still genuinely queued is reported with the phase it published or, when it has
published none, with the reason it has not started — see
[Dashboard](dashboard.md#what-a-queued-job-reports-while-it-waits). If the dashboard reports the
controller as stopped or as possibly wedged while a job is pending, that is the fault to fix; the
queued request itself is preserved and needs no resubmission.

### A finished status walk, resync or log clear was reported as still queued

Packages built between the introduction of the queued status walk and this fix (26.45 through
26.47) could complete one of those three operations and still show it as queued, and Reconcile then
reported it as outcome-unknown. The bridge wrote each queued response with the job's operation name
on its envelope and its own reader refused those three names, so the finished result answered every
poll with an error the dashboard treated as transient and kept polling. The work itself completed
and was recorded in Activity; only its report was lost. After upgrading, a job that was stuck this
way resolves on its next poll, and no request needs to be resubmitted.

## Profile save is rejected

Check the package identity can read and traverse the local source. Then review:

- profile-name alphabet and immutable existing name;
- physical absolute source versus File Station logical remote path;
- HTTPS URL/prefix and **Allow plain HTTP** state;
- remote `/`, repeated separators, trailing slash, dot segments, managed/reserved components, and
  portability length;
- `jobs`, retry, timeout, connect-timeout, rate, and exclude bounds;
- verbosity, quiet terminal suppression, log/progress formats, and result-output mode;
- remote-log required mode without an HTTPS collector URL; and
- invalid-certificate selection without the explicit interception-risk confirmation.

The browser can allow a syntactically numeric value that the strict bridge still rejects. Effective
connect timeout is at most 600 seconds and notification cooldown is at least 60 seconds.

A save made while the daemon is syncing normally succeeds and reads **Saved, takes effect on the next
run** (`applies_next_run`); the run in flight keeps the settings it started with. It is refused,
with exit `75` and a message beginning `profile_running:`, only if it would change what that run is
using: removing or setting as default the profile, removing its routine or secrets, changing its
URL, username, source, remote, compare mode, deletion settings or excludes, or a confirmed resync.
Save again when the run has finished. See
[Saves beside a daemon run](operations.md#saves-beside-a-daemon-run).

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

`inotify` is the expected backend. Under the daemon (the default engine, see
[Service engine](operations.md#service-engine)) the daemon itself watches the source with inotify,
in process; under the shell engine the package's own watcher (`sdsync-dsm-api --watch`) does. Neither
needs inotify-tools. `polling` with `backend_reason=watch_limit`, `watch_setup_failed` or
`watch_unavailable` means the native watcher is not in use for that routine; the Overview lists
each such routine as an amber item. Do not claim native realtime operation unless the snapshot
reports `inotify`.

**Under the daemon, polling is the fallback.** It replaces the native watch only for the reason
named, and `routine_backend` in `controller.log` carries that reason with the watcher's own cause
(a path and what failed). While it polls, the daemon tries the native watch again after 10 minutes,
then 20, doubling up to every 6 hours, and on every save of the routine; when it succeeds the
routine reports `inotify` again and the whole tree is treated as changed once. The polling walk
reports a change from a single differing reading; the debounce is what waits for a copy to settle.
An entry that vanishes during a walk, or a subdirectory that cannot be read, is a named skip
(`routine_watch_notice`: the count and the first three paths with their cause, once per path) and the
walk goes on; every walk has a deadline, and one that overruns is `watch_setup_failed` naming the
directory it was in. Editing a realtime profile's source or excludes replaces the watch
(`routine_rearmed`). When the daemon starts, or a native watch is armed again, it runs one sync of
the whole tree if the source changed after the last success. Beside a healthy watch a stat-only floor
reading (every min(the poll interval, 1 hour), at most once a minute) catches what inotify missed:
`routine_watch_notice` with `watch_missed_change dirs=N first=… floor_seconds=…` (`warn`) names the
directories it had to sync, which usually means a nested mount (the arm-time note `nested_mounts=N
first=… watched_by=poll_floor` lists them) or a writer inotify cannot see. `poll_floor_failed cause=…`
means a floor reading itself failed. An armed routine also syncs its whole tree once every 24 hours
(`routine_full_reconcile`).

**Under the shell engine, polling is not merely a fallback:** it runs alongside a healthy native
watcher too, checking path/size/mtime fingerprints at the configured interval whenever no native
event is pending for that turn, and it requires one unchanged reading before it dispatches. Confirm
source ACLs and controller logs. Reducing the interval increases source traversal work.

An unreadable subtree fails only itself, not the whole poll. Under the shell engine
`backend_reason=poll_unreadable` in the routine's state means part of the source could not be read on
the last poll; the readable part stays observed, and the controller log names the failure
`routine_poll_unavailable`. Fix permissions on the unreadable path — `@eaDir`, `#recycle`,
`#snapshot`, and `@tmp` are already pruned from the walk and cannot be the cause.

## Realtime routine never runs

Check the routine's `backend_reason` first, in the Routines catalog row or the routine editor; it
names why, not just that something is wrong. `backend_reason` is absent on a state file written
before this field existed, and that reads as `ok`.

| `backend_reason` | What it means | What to do |
| --- | --- | --- |
| `ok` | Observing normally, natively or by polling. | Nothing; this is healthy. |
| `source_missing` | The configured source path does not exist. | Restore it, or point the profile at a path that exists. |
| `source_symlink` | The configured source path is a symlink. | Point the profile at the real directory it targets. |
| `watch_unavailable` | No native watcher is available: the package's own watcher failed its executable checks and no inotify-tools `inotifywait` was found. | Reinstall the package; `realtime_watcher_command` in `controller.log` (a `warn` with `path=none`) says which. Polling continues to observe the source meanwhile. |
| `watch_setup_failed` | The native watcher failed to start, or stopped, and is retried with a growing delay; after five failures the controller stops retrying. | `routine_watch_failed` in `controller.log` carries the watcher's own line, which names the cause and then the path: an unreadable directory (`cannot watch the directory: Permission denied`), an ignore file or profile fragment that does not parse (`rules:`), or a walk that did not finish within 120 seconds (`deadline=120s`, the directory it was stuck on); `start_timeout` is a watcher that had not started 30 seconds after the controller spawned it, and was stopped. Fix that, then save the routine to try again. |
| `watch_limit` | The source has more directories than the inotify watch budget allows. | Raise `fs.inotify.max_user_watches`, or narrow the source. Compare `cat /proc/sys/fs/inotify/max_user_watches` against `find <source> -type d \| wc -l`. The controller reads the limit whole and counts directories in a bounded background job (it stops one past the budget, and the result is cached for an hour per profile in `run/watch-<profile>.dircount`), so a Save on the dashboard does not walk every tree again. `routine_watch_limit` in `controller.log` carries `dirs=` and `budget=`. The native watcher checks the same budget itself, less the watches this user's other watchers already hold, and refuses with `watch_limit dirs=… budget=… limit=… held=…` (or `at=<directory>` when the kernel refused a watch with ENOSPC); that is logged as `routine_watch_limit` with `source=watcher`. The limit is per user, and only root can raise it (`sysctl -w fs.inotify.max_user_watches=…`). Until then the source is polled, which keeps its disks from hibernating. A limit that cannot be read or a count that fails is logged once per controller lifetime as `routine_watch_limit_unchecked` (`cause=absent`, `not_a_file`, `unreadable`, `malformed`, `count_failed`, `cache_unsafe` or `cache_unwritable`) and the watcher is started anyway; its normal ENOSPC handling still applies. |
| `watch_stopped` | A stopped native watcher was found and was replaced automatically. | Nothing, unless it recurs; if it does, inspect what is pausing the watcher process. |
| `poll_unreadable` | Part of the source could not be read during the last poll. | Fix permissions on the unreadable subtree; the readable part is still observed. |
| `poll_unavailable` | The last poll of the source failed outright. | Confirm the source path is reachable and readable. |

`backend` itself is only as current as the last poll that actually completed; a stalled or failing
poll leaves it holding whatever it last reported rather than updating to reflect the present.

The daemon uses only `ok`, `source_missing`, `watch_unavailable`, `watch_setup_failed` and
`watch_limit`, and reads them differently from the table: `watch_unavailable` means the native
watch could not be created at all (not that a helper binary failed its checks), `watch_setup_failed`
and `watch_limit` are retried on the 10 minute to 6 hour backoff above instead of being abandoned
after five failures, and `source_missing` is looked for again every 30 seconds. The cause is in
`routine_backend`, not in `routine_watch_failed`.

### Probe what inotify sees on a share

`sdsync-dsm-api --watch-probe` arms the same watcher the routine uses, prints the directory count
against the watch limit, then prints every change that arrives while you change files. Run it as the
package user, so the per-user limit and the share permissions are the routine's own. DSM may
collision-rename that account, so resolve it as [CLI parity](cli-parity.md) does:

```sh
PACKAGE_USER=$(stat -L -c '%U' /var/packages/synology-drive-sync/home)
sudo -u "$PACKAGE_USER" -- /var/packages/synology-drive-sync/target/bin/sdsync-dsm-api \
  --watch-probe --seconds 60 \
  --profile-fragment /var/packages/synology-drive-sync/home/config/profiles.d/<profile>.toml \
  /volume1/<share>/<folder>
```

While it waits, save a file into the folder over SMB, from File Station, and from Synology Drive. It
exits 0 when changes arrived, 1 when it was armed but nothing arrived (inotify does not see writes
made that way to that share), and with the watcher's own status and a `refused:` line when it could
not arm: `watch_limit` with the numbers, `source_missing`, or `watch_setup_failed` with the cause.

### Raising the inotify watch limit

This is **optional** and needs **root**; the package never does it, because the package user cannot.
It matters only for a source with more directories than the watch budget (80% of the limit, less what
the package user already holds), which shows as `watch_limit` and is polled in the meantime. Compare
`cat /proc/sys/fs/inotify/max_user_watches` with the `dirs=` and `budget=` the routine reports, or run
the [watch probe](#probe-what-inotify-sees-on-a-share), which prints both. The limit is per user, so
raise it for the whole NAS rather than guessing a per-package value. From an SSH session as an
administrator:

```sh
sudo sysctl -w fs.inotify.max_user_watches=524288
```

That lasts until the next reboot. To make it persistent on DSM, create a **Control Panel > Task
Scheduler > Create > Triggered Task > User-defined script** that runs at **Boot-up** as `root` and
executes the same `sysctl -w` line; adding the setting to `/etc/sysctl.conf` is also commonly done but
a DSM update can replace that file, so check it after upgrades. Neither route is exercised by this
repository's tests, and a value that is too large costs kernel memory (roughly 1 KiB per watch on a 64-bit
kernel), so raise it only as far as the source needs. After changing it, save the routine (or stop and
run the package) so the controller tries again.

## A run failed, died, or was refused

Start with the dashboard's attention list or the snapshot's `outcomes.run` (`reason` and `detail`);
the same cause is in Activity `run.failed`. A scheduled run's core stderr tail is in
`var/log/run-stderr.log`, which the dashboard shows under the attention list (*Core error output of
the last run*) and offers as the Logs source *Core error output* (`run-stderr`). A failed profile also
shows the core's last error line and the path it failed on when the runner recorded them.

| Reason | Meaning | What to do |
| --- | --- | --- |
| `preflight_config_invalid` | `config validate` failed for the profile set | Correct the highlighted setting on Profiles. |
| `preflight_log_file_path` | The profile's log file is not the package-managed path (exit `66`) | Reset the profile's log-file setting, or repair the package. If it began after an upgrade, see the path-alias section above. |
| `preflight_policy` | The security policy refused the profile (plain HTTP, unverified TLS, destructive options) | Review Security. |
| `preflight_unsafe_state` | A package file failed its ownership or mode check (exit `73`/`77`) | Repair or reinstall; do not chmod by hand. |
| `no_password:<profile>` | That profile's password file is missing | Enter the password on Profiles. |
| `supervisor_refused` | The package supervisor refused before the core ran (exit `78`) | Read the stderr line in `api.log`/`controller.log`; reinstall if the binary identity mismatch persists. |
| `runner_died` | The runner was killed or lost before it finished (out of memory, power loss) | Review Logs, then run again. Nothing is rolled back. |
| `core_failed` / a core reason (`authentication`, `network`, `timeout`, `share_not_writable`, ...) | The core ran and failed; the reason is the class of the core's error. An HTTP request that timed out is `timeout`, not `network`, and its detail leads with the innermost cause: `HTTP request failed during <op>: operation timed out (error sending request for url (…): …)`. An exit `137` is never `core_failed`: see the next two rows | Follow the dashboard's next step; the one-line `detail` is the core's last error line (a profile's own record keeps up to 240 bytes of it; `run.state` and Activity keep 160). A refused `config validate` reads "configuration did not validate: <the core's line>". |
| `core_oom_killed` | The core was killed with exit `137` and this cgroup's out-of-memory counter moved: the source holds more entries than the NAS has memory for. The detail reads "killed by the out-of-memory killer" and names the memory a run could use. Under the daemon the profile is then held, with no retries and no automatic runs, and the Overview shows **not synced until you fix this** | Split the profile (a scope, or subfolder profiles), then click **Run now**, or change the profile's settings or the performance level: either lifts the hold, and a save that changes nothing does not. `lasting_state_unreadable` below is the same hold with an unreadable record. The core normally refuses such a tree itself first, as `tree-too-large-for-memory` below; this is the backstop when its estimate was wrong. `core_killed` in `controller.log` records it. |
| `core_killed` | The core was killed with exit `137` without reporting an error, and nothing proves the out-of-memory killer sent it. The detail reads "the sync core was killed (SIGKILL, exit 137) without reporting an error; out of memory?" | Check the DSM system log for the OOM killer; split the profile if the source is very large. `core_killed` in `controller.log` records it. |
| `tree-too-large-for-memory` (a deferred path `.`) | The core estimated that the tree needs more memory than this run may use (the cgroup limit's headroom and `MemAvailable`, with 20% kept back) and refused before listing the NAS. The run is complete with a warning, exit `0`; nothing was synced. The detail names the entry counts, the estimate in MiB and the memory available | Split the profile with a scope or into subfolder profiles. The `memory.tree_too_large` event in the sync log carries the same numbers. |
| `run_stalled` | Daemon only. The core made no progress (its read and write counters and progress record) for 45 minutes and was stopped; a warning of the same name is logged at 15 minutes. The failure consumes a retry | Check that the source and the NAS are reachable (a hung SMB or NFS mount), then run again. |
| `launch_failed` | Daemon only. The core could not be started (the cause is in `controller.log`) | Review Logs; repair the package if it repeats. |
| `state_unwritable` | A package state file could not be written. When a run was being started, it was not started because its record could not be written | Check free space on the package volume and file ownership. |
| `consumer_timeout` | Daemon only. A dashboard request did not finish within its lane deadline, so its outcome is unknown | Try again; see [Named terminal failures](#named-terminal-failures). |
| `lasting_state_unreadable` | Daemon only. The record that holds a profile after a lasting failure (`var/state/lasting/<profile>.state`) could not be read, so automatic runs stay held | **Run now** clears it; repair the package if it repeats. |
| `profiles_skipped` | An aggregate ran without the profiles that failed preflight | Fix those profiles; each has its own failed record. |
| `batch_target_failed:<profile>` / `not_reached` | An aggregate stopped at, or never reached, this profile | Fix the named profile, then run again. |
| `deferred_paths` | The core exited `3`: finished, with some paths deferred to the next run. Recorded as `succeeded` with outcome `partial`, an Activity warning counting the waiting paths, no alert and no failure count | Nothing; the next run picks them up. Read `entry.deferred` in the sync log if it repeats. |
| `lock_busy` | Another run, Doctor, or management action held the run lock | Nothing for a routine: it is a deferral, retried 30 seconds later, and is not counted as a failure. A run you started yourself is not repeated; run it again if you still need it. |

**Failure alerts unavailable** means the alert state file is unsafe or corrupt, so the failure counter
and its notifications are disabled too; the consecutive-failure count is shown as unavailable rather
than zero. Repair the file through the manager or a verified reinstall. **Some records of this run
could not be written** (the `degraded` marker) means the run's result is sound but an echo, such as
the Activity line, the alert counter, or the DSM log, was not written; check `controller.log` for
`activity_append_failed`, `alert_unavailable`, or `dsm_log_emit_failed`.

A **partial** run (exit `3`) is not a failure: some paths were deferred and sync next time. A run with
**skipped or deferred entries and a warning** exited `0`; the entries are named by `entry.skipped` and
`entry.deferred` records in the sync log, and the reasons are listed under
[partial runs, deferrals and skips](../observability.md#partial-runs-deferrals-and-skips). Names or
permissions that only a person can fix (a name the NAS refuses, an unreadable folder, a link, a case
rename File Station would not make) are in that group and stay amber on every run until fixed; if a
refusal covers most of the share the run instead **fails**, naming the cause.

**A run that never finishes (shell engine).** The attention list flags a run going for more than six hours
(`long_running`) and one whose heartbeat is more than 30 minutes old (`stalled`: its read and write
counters stopped growing by 16 KiB a minute; where the kernel hides them, `stalled` is `null` with the
warning `stall_unobservable` and a hang cannot be detected). Neither stops the run, and nothing in the package stops the core on its
own. There is no dashboard *Cancel run*: a package stop is the way to end a hung run (Package Center
stop or `synopkg stop`; DSM offers Stop and Run, not Restart). The controller stops within 60 seconds, the run is recorded as `cancelled`
(not `runner_died`), with an Activity `run.cancelled` row, and a core that ignores the termination request is killed after the grace. Meanwhile every routine waits behind the run as “Waiting (busy)”. Check the NAS side first: a hung SMB or NFS
source mount, or a File Station request with no deadline. The daemon engine does stop a hung run:
after 45 minutes without progress it ends the run as `run_stalled`; a package stop cancels with a
20-second grace before the core is killed. It has no total-duration cap, because a first upload can
take days.

**A save that is not applied.** A green save followed by the red notice *Saved, but it is not fully in
effect* (and an Overview item *Saved, not yet applied; stop and run the package service*) means the change
is on disk but the running controller could not be told to reload; the reason is one of
`pid_unreadable`, `identity_refused`, `identity_changed` or `signal_failed`. Stop the package in
Package Center, then Run it again; the controller reads the saved files when it starts. *Cannot tell whether saved changes
are applied* means the marker file itself failed its safety check.

## Routine is deferred

Check active weekday/time window, NAS clock/time zone, dependency latest-success states, and deletion
approval. A deferral is written once when the routine enters it (Activity and `controller.log`), not
on every controller turn, and a busy run lock (exit `75`) defers the routine for 30 seconds without
consuming a retry. The daemon also defers a run while a save or a Doctor write test holds the
management lock (`run_deferred`, `reason=lock_busy`). If a profile's deletion cap is greater than its routine approval ceiling, the controller
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

## DSM system log entry does not appear

This channel is off by default and has no dashboard field, so the first thing to check is whether it
was ever enabled. In order:

1. `sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-alerts --system-log true --system-log-message-id
   0xNNNNNNNN` must have been run, with an identifier from DSM's own catalogue. The package ships no
   identifier and refuses to enable delivery without one, because the catalogue is Synology-owned and
   cannot be extended by a third-party package.
2. `--system-log-level` defaults to `warn`, so the `info` events (sync success, service start/stop,
   install, upgrade) are dropped until it is lowered.
3. Delivery is rate-limited per event group by the alert `cooldown_seconds`. Consult
   `var/state/system-log.state`; each `*_epoch` line is the last delivery for that group. A group
   inside its cooldown is silent by design.
4. Look for `notification.unavailable` with the message `DSM system log delivery unavailable` in
   Activity. That is recorded when `/usr/syno/bin/synologset1` is missing, is a symlink, or returns
   non-zero. Logging never fails the run that raised the event, so the run's own result is unaffected.

To establish the identifier on a new DSM build, run `synologset1` by hand as the package user with a
candidate identifier, then confirm in Log Center that the entry rendered the expected text. Record
the DSM version alongside the identifier: it is a Synology-internal value and is not promised stable
across DSM releases.

The package acquires no `conf/resource` worker, so it installs no Log Center syslog-ng patterndb and
registers no Notification Center rule. An entry that does not appear under a Log Center filter you
expected is more likely a classification gap than a delivery failure; check the unfiltered log first.

## Exit `75`, active lock, or package will not stop

Status and logs should identify the active operation/PID. Wait for a legitimate operation to finish.
Under the daemon engine a save made during a sync is not a cause of `75`: it commits, unless it
would change a running profile, and then it says `profile_running:` and names the profile and the
operation; retry when the run has ended. A run that is deferred because a save was in progress logs
`run_deferred` with `reason=lock_busy` and is retried on its own.
Stop uses cooperative termination and can wait for the core to unwind; it deliberately refuses an
untrusted PID and does not force-kill after timeout. When the run lock is held by the controller
itself (it takes the lock to reconcile a dead run), stop verifies and stops it as the controller
instead of refusing to signal an "unverified plan/sync PID", and recovers the lock afterwards as stale.

Stale locks are recovered only when their recorded PID is no longer live (an owner record torn by
power loss before the last boot counts as dead and is recovered, rather than ending the controller).
At controller start it also removes capture, admission and fingerprint leftovers of processes
that are no longer live and logs `runtime_leftovers_pruned`. Do not recursively delete
package run/control directories. If identity/path validation reports `73`, preserve evidence and
repair through a verified reinstall/upgrade rather than manual chmod/chown edits. A `73` from the
identity check now says why in the refusal line (`refusing unsafe controller identity before signal`,
`cannot validate package lifecycle admission`, and the CLI's `refusing to signal/wake unverified
controller PID`): `boot_identity_unreadable`, `invalid_pid`, `invalid_start`, `package_owner_unknown`,
`status_fields_missing`, `status_unreadable`, `process_exited`, `stat_unreadable`, `start_mismatch`,
`uid_mismatch` or `state_unknown`. Reading a busy process's identity from `/proc` is done from one
snapshot (under bash, `/bin/sh` on DSM, in a single read), so a loaded NAS can no longer make a live,
healthy controller look unprovable; if a field is missing the check retries up to three bounded passes
while the process exists, then fails closed with `status_fields_missing`.

**Stop or Run refuses after a crash, a power loss or a reused PID (daemon engine).** The daemon's
records (`run/daemon.pid`, `daemon.ready`, `ctl.sock`, `supervisor.pid`) can outlive it, and the PID
they name can by then belong to an unrelated process. `daemon.lock` decides: the kernel drops it
when the daemon dies, so with nobody holding it the records are stale whatever they say, and `status`
reports `1` ("stale daemon record of a daemon that is gone: <path>") while Stop and Run remove them,
printing `removed stale daemon record <path>` for each. A lock that is held, or one `flock` cannot
probe, keeps the strict identity check: the refusal is "refusing to stop an unverified daemon" with its
cause (for example `start_mismatch`), and nothing is signalled or removed on a guess. The same
reading applies to a prepared `run/api.sock` (mode `0000`) left by an API child that died between bind
and activation: with no `api.pid` and no holder of `daemon.lock` it is stale, status reports it
(`stale API socket: <path>`), and Stop and Run remove it (`removed stale API socket <path>`); beside a
live `api.pid` or a held lock it is left alone. Stop is bounded by one deadline,
`SDSYNC_DSM_STOP_TIMEOUT` (default and maximum 120 s) plus KILL's 10 s margin. What a daemon says
on stderr before its own log opens (a panic, a refusal) is in `var/log/daemon-stderr.log` and its one
rotation `.1`; the Logs source `controller` lists both ahead of `controller.log`, as errors.

**The controller died but the API service is still up.** After a SIGKILL or an out-of-memory kill of the
controller alone, Package Center may still report the package running. `status` now logs
`controller_found_dead phase=status` once for that dead controller and service pair, and **Start** (or
Run in Package Center) repairs it: it logs `controller_found_dead phase=start`, stops the surviving
service and any orphaned run through the normal teardown, and starts both again, writing an Activity
`service.restarted` row (`warn`). It does that only when it can prove the controller is dead (no live
PID, no ready file, no live lock owner, no startup lease) and the service is the verified one; anything
unprovable is still refused, and no run is ever concurrent with the repair. If the stop step fails, Start
refuses with a named message and you stop the package by hand.

**An overlapping deleting pair blocks the routines of its two profiles.** The runner validates the
configuration before every run, scoped to the profile being run (an all-profiles run is validated
unscoped). Two profiles whose NAS destinations are equal or nested, where either deletes, are a
configuration error (exit `2`), so such a pair that already exists in a configuration written before
this release makes the runs of those two profiles fail as `config_invalid` until the pair is fixed;
other profiles are unaffected. The profile's detail reads "configuration did not validate: error:
profiles "a" and "b" have overlapping NAS destinations and ..." and names both profiles and the remedy. The dashboard and `configure-profile` now refuse to create
the pair (exit `64`); fix an existing one by giving the two profiles separate destinations or turning
deletion off on both.

Lock owner records torn by a power loss are recovered by claiming the record (a rename to
`<lock>.torn.<pid>.<start>.<boot>`), judging the claimed copy, and deleting it only if it is still the
torn record that was judged; otherwise it is put back without clobbering, and a record that a
competing process published while it was being judged is treated as a race, not as damage. A live
owner's record is never deleted. If a record cannot be put back (exit `75`) it is kept and
`lock_owner_restore_failed` is written to standard error.

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
  `type=app`, Package Center and desktop Open instantiate that native AppWindow class, the
  bundle/style load, and DSM's Webman link resolves to `target/ui`; an ordinary browser request for
  the package directory or absent `index.html` may return 404 without constituting a native-launch
  failure, while `/webman/3rdparty/synology-drive-sync/api.cgi` is reached without a routing 404;
- on every claimed DSM branch—and specifically DSM 7.0/7.1 where supported—the same AppWindow
  launch, rendering, assets, and API path succeed on physical NAS hardware; and
- a fresh native launch performs or joins one in-flight official same-origin `SYNO.API.Auth` version
  6 `method=token` request, retries a failed bootstrap only after the bounded cooldown, retains the
  exactly-once-encoded value only in module memory, sends it through this package's private
  `X-SYNO-TOKEN` browser-to-CGI bridge, never inspects/rewrites the DSM shell location, and records
  whether the installed helper used the direct or `EACCES` loopback branch without recording the
  token itself. The official contract is `method=token`, `data.synotoken`, and the `SynoToken`
  parameter—not our private header name.

Automated Chrome fixture QA passed against the captured DSM control structure. It does not prove
native physical-DSM rendering, accessibility interaction, or browser-header-to-CGI forwarding;
record those results from the installed AppWindow.

### Identity, ACL, and dashboard security

- the actual DSM package identity has read/traverse permission only to intended source shares;
- `conf/privilege` is the exact package-run-as document above, all installed executables are `0755`,
  and no package file carries an identity-changing permission bit or Linux capability;
- `ui/api.cgi` is package-owned `0755` and DSM executes it with real/effective UID equal to the
  exact non-root package UID used by the API `--serve` process; `var/run/api.sock` is package-owned
  `0000` before startup commit and the same inode is `0600` when active;
- the CGI and service reject a substituted socket, wrong owner/mode, wrong peer UID, symlink,
  extra hard link, unsafe parent, oversized frame, or malformed relay schema;
- another internal user/root cannot use manager mutations in place of the package identity;
- non-administrator DSM users cannot launch or call the API;
- DSM maps a browser request containing exactly `X-SDSYNC-Request: 1` into the CGI environment as
  `HTTP_X_SDSYNC_REQUEST=1`, the bounded relay preserves that marker to the package-user service,
  and an omitted or wrong-value marker is rejected;
- the fixed `authenticate.cgi` entry is probed before metadata validation; `X_OK` success performs
  full chain/canonical-target validation and revalidation before direct execution, while `EACCES`
  skips the validator and succeeds through the loopback user service without any chmod, group, root,
  privilege, or resource change; all other probe failures fail closed;
- the loopback branch reaches only literal `127.0.0.1` on the current CGI port with no proxy,
  redirect, remote host, cookie URL, or token URL; malformed/oversized responses, missing
  `Session.user`, non-Boolean/false `is_admin`, and independent NSS/group mismatches fail closed;
- a stale cookie, malformed/mismatched SynoToken, missing/expired CSRF, wrong methods/fields, and
  direct CGI calls fail closed, while the official token bootstrap and package-private header bridge
  succeed; and
- no secret appears in a launch/package URL, URL history, Referer, package request body, browser
  storage, Activity, logs, DSM desktop alerts, queue result, or support evidence.

### Profile, target, and Doctor

- one `/home/Drive/...` target when home sync is intended and one Team Folder/shared-folder target
  where applicable;
- a missing nested destination is created beneath an existing writable parent while an out-of-scope
  sibling canary remains unchanged;
- source hash scan, API discovery, authentication, target inventory, and prepared disposable write
  test succeed;
- reverse-proxy prefix, certificate/private CA, upload limit, timeouts, largest file, password, TOTP,
  and NAS clock are proven;
- Synology Drive visibly indexes the File Station result when that behavior is required; and
- the [questions only a real NAS can answer](../production-acceptance.md#questions-only-a-real-nas-can-answer)
  are put to the exact DSM build, including whether DSM destroys the dashboard when its window closes.

### Automation and observability

- interval, daily, and realtime routines, weekdays/windows, debounce, native watcher or polling
  fallback, retries/backoff, and dependencies behave as documented;
- manual and scheduled actions do not overlap on the source NAS;
- queued configuration and Doctor terminal results, plus asynchronous Plan/Run status, are
  observable;
- Activity/log bounds and rotation work through restart;
- direct DSM success/failure/Doctor desktop alerts obey threshold/cooldown, use only fixed I18N keys,
  expose details only through Activity/logs, and do not register Notification Center channels;
- the desktop-only claim is confirmed rather than assumed: configure email notifications in Control
  Panel > Notification, force a sync failure, and record that the alert reaches the DSM desktop
  notification panel and that **no** email, SMS, or push message is delivered. Synology documents
  `synodsmnotify` as reaching the desktop panel and attributes the mail/SMS/push fan-out to the
  `conf/resource` `sysnotify` worker this package does not acquire, and `synodsmnotify` carries no
  event identifier for a notification rule to match — but that reasoning is documentary, and this is
  the step that settles it on real hardware. If mail does arrive, the desktop-only boundary claimed
  throughout these documents is wrong and must be corrected;
- the DSM system log channel is verified end to end on the exact DSM build: a candidate message
  identifier is established by hand, `configure-alerts --system-log` accepts it, an event of each
  severity renders the expected fixed sentence, Log Center displays it, the per-group cooldown
  suppresses a repeated failure, and an intentionally wrong identifier or a removed
  `synologset1` degrades to `notification.unavailable` without failing the run. Record the DSM
  version with the identifier, which is a Synology-internal value with no cross-release promise;
- a hung source mount holds the run lock without failing: `running_for_seconds`, `long_running` and
  `stalled` appear in the snapshot, routines wait as “Waiting (busy)”, and nothing is reported healthy;
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
particular, Synology documents direct `authenticate.cgi` use by a custom CGI and the official
`SYNO.API.Auth` version 6 `method=token` JavaScript bootstrap. This root-free design probes the fixed
helper first, validates/revalidates it only after `X_OK` succeeds, and selects bounded loopback without
validation only on `EACCES`. The private `SYNO.Core.Desktop.Initdata` response shape is corroborated
by saved first-party DSM runtime sources, not by the supplied HTTP capture; Synology does not publicly
promise it as a package API. The daemon invokes neither authentication path. Webman's package-owner
CGI identity, official token response, package-private header bridge, and selected direct/fallback
branch remain live-DSM acceptance requirements. QuickConnect relay access is outside Synology's
third-party application support; use a supported route for that acceptance record. A complete record
from the exact environment is the acceptance evidence.
