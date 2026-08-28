# Dashboard and navigation

The SPK registers **Synology Drive Sync** as an administrator-only native DSM Vue AppWindow. Package
Center's **Open** action targets the same application class. It is self-contained inside the SPK:
the bundle loads no CDN scripts, fonts, images, analytics, or hosted search service.

## Native DSM AppWindow contract

`INFO` binds `dsmuidir="synology-drive-sync:ui"` and
`dsmappname="SYNO.SDS.App.SynologyDriveSync.Instance"`. The installed `ui/config` is keyed first by
`SynologyDriveSync.js`, then by that exact application class. Its entry declares `type="app"` and
the matching `appWindow`; it is not a `type=url` pop-up or an iframe around a standalone HTML page.
The bundle registers the class through `SYNO.namespace` and `Vue.extend`, then renders the dashboard
inside DSM's `v-app-instance` and `v-app-window` components. DSM loads the packaged
`SynologyDriveSync.js` and `style.css` assets. The module-keyed native AppWindow is the only
application UI: there is no `type=url` entry, standalone `ui/index.html`, or undocumented
`launchApp` redirect. Opening the third-party directory in a separate browser tab is not the DSM
AppWindow launch contract and may legitimately return DSM's generic page-not-found response.

The AppWindow calls the canonical same-origin CGI endpoint
`/webman/3rdparty/synology-drive-sync/api.cgi` directly. The explicit
`dsmuidir="synology-drive-sync:ui"` mapping lets DSM expose that package-owned endpoint through its
framework-managed third-party path while the rootless CGI/socket authentication boundary remains
unchanged.

Package lifecycle scripts never create, replace, or repair that `/usr/syno` link. Doing so would
cross DSM's framework boundary and would require privileges the package deliberately does not have.
If Package Center's **Open** action shows DSM's “page not found” response, use the read-only
[AppWindow diagnostics](troubleshooting.md#dsm-says-the-page-is-not-found-when-opening-the-app)
instead of hand-creating a link or changing ownership.

> [!NOTE]
> The native AppWindow first uses the official same-origin
> `GET /webapi/entry.cgi?api=SYNO.API.Auth&version=6&method=token` bootstrap. It encodes a valid
> returned token exactly once, keeps it only in module memory, and sends it only as `X-SYNO-TOKEN`
> to the packaged CGI; the browser separately sends the DSM cookie through same-origin credentials.
> It never reads a token from or writes one to the launch URL, history, request body, persistent
> storage, or logs. Server-side authentication probes `X_OK` on the fixed DSM helper before metadata
> validation: an executable entry is fully validated/revalidated before direct execution, while
> `EACCES` skips the validator and selects the bounded loopback user-service path. Package mutation
> still requires the independently issued package CSRF token.

## Connection and read-only states

The footer distinguishes these states:

- **Authenticated control service** means the ordinary package-owned CGI passed its fail-closed
  exact non-root package-UID identity check and reached the package-user API service over its fixed private socket; the
  current session passed authentication, administrator membership, SynoToken validation when the
  bootstrap supplied one, and CSRF bootstrap checks; and the snapshot explicitly grants mutation
  capabilities.
- **Package status · read-only** means a snapshot was available but one or more mutation
  capabilities were not granted. Buttons that could change package state remain disabled.
- **Status unavailable** means snapshot refresh failed. Existing values may be stale; the interface
  does not silently treat them as current.

Mutation controls require both a valid independent package CSRF token and
`capabilities.mutations=true`. Secret controls additionally require `capabilities.secrets=true`,
and **Disposable write test** requires `capabilities.write_test=true`. A direct package-user
`sdsync-dsm api snapshot` intentionally reports those capabilities as false; only the authenticated
API service can grant them after every server-side check passes.

## Overview

Overview is an operational summary, not a substitute for Doctor or a reviewed Plan. It shows:

| Card or panel | Meaning |
| --- | --- |
| Service | `running`, `stopped`, or `untrusted` controller evidence from the package snapshot |
| Profiles | Configured profile count and how many have protected password material |
| Next routine | Earliest reported `next_run_epoch` among enabled per-profile routines |
| Last result | `never`, `running`, `succeeded`, or `failed`, plus its completion time when known |
| Active scope | Named profile or `all` while an operation is running |
| Realtime | Count of enabled realtime routines and whether polling fallback is visible |
| Profiles | Profile name, logical destination, default marker, and password-presence status |
| Last operation | Operation (`plan` or `sync`), state, scope, start, and finish evidence |

**Plan all profiles** and **Run all profiles** enqueue an action with scope `all`. The dashboard does
not enable deletion from these quick actions. Use the reviewed deletion flow documented under
[Deletion approval](routines.md#deletion-approval-is-layered).

## Profiles

Profiles lists every configured target and provides the graphical editor. Selecting an existing
profile makes its name read-only; renaming is not an implicit create-and-orphan operation. To use a
new name, create a new profile deliberately, validate it, and then remove the old profile.

The editor groups basic destination settings, deletion controls, advanced network/logging fields,
and three secret-state editors. See:

- [Profiles and destinations](profiles.md) for every displayed field and constraint;
- [Secrets and protected values](secrets.md) for keep, replace, clear, non-disclosure, and ordering.

The underlying save is queued, but the page polls its sanitized result and reports success only after
the controller completes it. Configuration is applied first, followed by each requested secret
operation in order. If a later stage fails or becomes outcome-unknown, the page reports the profile
as partially applied when an earlier stage completed, closes the editor, refreshes the snapshot, and
requires credential-presence inspection before any retry.

## Routines

Routines uses two keyboard-accessible subtabs. **Configured profiles** is first and shows each
saved routine; selecting one opens **Package controller**, which edits that profile's automation
policy. The page shows each routine's requested
mode, effective backend, state, next run, and last success. The Overview realtime card makes an
`inotify` backend or `polling` fallback visible rather than implying that a native watcher exists on
every NAS.

The timing panel describes the execution sequence: observe, debounce, preflight, run. Manual Plan,
manual Run, the legacy global interval schedule, and per-profile routines share one host-local run
lock. See [Routines and scheduling](routines.md).

## Health / Doctor

Doctor can target one named profile or all profiles. Its default diagnostic hashes the complete
source and checks File Station discovery, authentication, inventory, and the exact destination or
nearest writable ancestor without changing target contents.

**Disposable write test** is separately capability-gated and requires an explicit confirmation. It
briefly creates, uploads, verifies, may exercise same-target copy, and removes a unique probe. Use it
only in a prepared non-critical destination. The API initially queues the action, then the page polls
its sanitized terminal result before reporting the Doctor verdict. Pending observations have no
client deadline. An `expired_or_missing` result, an invalid result document, or five consecutive result
observation failures yield outcome-unknown; inspect Activity and refreshed cached health evidence
before retrying.

The target-health table never fabricates evidence. Missing reachability, authentication,
writability, latency, or timestamp data is shown as **Unavailable**. Free space is displayed only
when the backend sets an explicit proof flag. See [Health and Doctor](operations.md#health-and-doctor).

## Activity / Logs

Activity presents structured, fixed-code events. Logs presents bounded lines from API/CGI,
controller, scheduler, sync, and mandatory audit sources. The page supports `100`, `200`, `500`, or
`1000` lines and can pause live updates without stopping package logging. A selected source is read
alone; the `all` response is globally bounded below the bridge capture limit. **Clear view** clears
only the browser presentation; it does not delete package logs.

Snapshot polling pauses while the document is hidden. Log polling occurs only while Activity is
open and not paused. Refresh intervals are controlled in Settings. Full event and retention details
are in [Health, activity, logs, and notifications](operations.md).

## Notifications

Notifications separates **Package alerts** and **Session preferences** into two keyboard-accessible
subtabs. The package alert policy controls direct DSM desktop alerts. It is separate from the
optional open-browser fallback:

- DSM desktop alerts can be enabled, triggered on success and/or failure, delayed until a bounded
  consecutive failure threshold, and rate-limited by a cooldown. They are sent directly to logged-in
  DSM administrators through `synodsmnotify` with fixed, preloaded I18N keys.
- Open-session notifications and the audible cue are local interface preferences. They operate only
  while this application is open and the browser grants permission.

The desktop message never contains a profile name, exit code, path, URL, account name, password,
TOTP material, bearer token, or arbitrary log text. Inspect Activity and bounded package logs for the
specific operation and result. The package does not acquire the `sysnotify` resource or register
Notification Center rules, email, SMS, mobile, or CMS delivery channels.

## Security

Security uses **Permissions & risk** as its first subtab. It contains dashboard-operation
permissions and profile risk ceilings. **Observability & limits** is second and keeps bounded CSRF,
result-retention, and outstanding-job controls together with every structured log-category level.
Arrow Left/Right, Home, and End move between the tabs. Saving still validates and applies one
complete security policy, regardless of which subtab is visible. See [DSM security](security.md).

## Settings

Settings presents compact native horizontal rows: label and field help first, then the input or
dropdown. On narrow windows the rows stack without changing their label-control order. Settings
changes only non-secret interface preferences:

| Preference | Values |
| --- | --- |
| Theme | Dark, follow system, or light; dark is the default |
| Status refresh | 3, 5, 10, or 30 seconds |
| Log refresh | 5, 10, or 30 seconds |
| Open-session notification | Off/on, subject to browser permission |
| Audible cue | Off/on, best effort |

These preferences use the browser's local storage. Package CSRF, DSM cookies, passwords,
TOTP seeds, and remote-log tokens are memory-only and are never included in that storage object.

## Accessibility and narrow windows

The application provides labeled controls, keyboard focus indicators, semantic tables and forms,
polite live regions, accessible internal tablists, and confirmation overlays marked with
`role=dialog` and `aria-modal`. Primary sections and internal subtabs fade out before the next panel
fades in; the reduced-motion preference disables those transitions. A
confirmation moves focus inside the dialog, traps Tab and Shift+Tab, cancels on Escape, and restores
the prior focus when it closes. Navigation collapses for narrow AppWindow widths. The application
does not currently implement a skip link. Verify keyboard
flow, focus behavior, contrast, zoom, and the exact DSM window sizes during
[live acceptance](troubleshooting.md#live-nas-acceptance).

## Queue and completion evidence

Every changing request receives a client request ID and is published into a private package queue.
The API service first returns HTTP `202` with state `queued`. The controller later claims and validates
the job, runs the package manager under a clean package identity, and writes a private response.
Configuration/secret/routine/policy calls and Doctor poll that response before reporting a terminal
result. Plan and Run remain asynchronous. Therefore:

1. A configuration success toast proves a sanitized terminal manager result, but the refreshed
   snapshot remains the authoritative displayed state.
2. Pending result observations have no client deadline. They continue until a terminal or
   `expired_or_missing` response, five consecutive observation failures, an invalid result document,
   or AppWindow shutdown aborts observation.
3. `expired_or_missing`, invalid result evidence, and repeated observation failures make the accepted
   job outcome unknown; inspect the refreshed snapshot, structured Activity, and bounded logs before
   retrying. Closing the AppWindow stops observation, not the queued server job.
4. A multi-stage profile save can be partially applied when configuration or an earlier secret stage
   completed before a later failure or outcome-unknown result. Inspect configuration and every
   credential-presence marker before retrying.
5. For Plan and Run, “queued” is not success; follow run state, Activity, and logs.
6. Investigate a failed or stale job through the [CLI recovery path](cli-parity.md), not browser
   developer tools containing session material.
