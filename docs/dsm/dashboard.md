# Dashboard and navigation

The SPK registers **Synology Drive Sync** as an administrator-only DSM desktop application. Package
Center's **Open** action targets the same application. It is self-contained inside the SPK: the page
loads no CDN scripts, fonts, images, analytics, or hosted search service.

## DSM Webman launch route

The application remains a native DSM `type=url` pop-up. Its registered entry point is the canonical,
root-absolute path `/webman/3rdparty/synology-drive-sync/index.html`. `dsmuidir="ui"` tells DSM to
create and own the corresponding
`/usr/syno/synoman/webman/3rdparty/synology-drive-sync` link to the installed package's `ui`
directory. The page then reaches its packaged CGI through the same-directory relative URL
`./api.cgi`.

Package lifecycle scripts never create, replace, or repair that `/usr/syno` link. Doing so would
cross DSM's framework boundary and would require privileges the package deliberately does not have.
If Package Center's **Open** action shows DSM's “page not found” response, use the read-only
[Webman launch diagnostics](troubleshooting.md#dsm-says-the-page-is-not-found-when-opening-the-app)
instead of hand-creating a link or changing ownership.

> [!NOTE]
> The first live request still depends on DSM launching the application with a valid `SynoToken`.
> That forwarding behavior has not yet been observed on physical DSM 7 hardware. If the token is
> absent, malformed, contains whitespace, or exceeds 1,024 bytes, the application removes any token
> text from the visible URL, disables mutations, and reports a compatibility error. Use
> [`sdsync-dsm`](cli-parity.md) instead of trying to manufacture or persist a token.

## Connection and read-only states

The footer distinguishes these states:

- **Authenticated control service** means the ordinary DSM `http` CGI reached the package-user API
  service over its fixed private socket, the current session passed authentication, administrator
  membership, SynoToken, and CSRF bootstrap checks, and the snapshot explicitly grants mutation
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
the controller completes it. For a new profile with accompanying secret replacements, the page also
waits until a refreshed snapshot shows the profile before it enqueues the secret jobs. This is
defense in depth; the controller's private queue uses sortable identifiers and serial processing.

## Routines

Routines configures automation independently for each profile. It shows each routine's requested
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
only in a prepared non-critical destination. The action is queued, so the immediate result is not a
Doctor verdict; follow Activity and the refreshed cached health evidence.

The target-health table never fabricates evidence. Missing reachability, authentication,
writability, latency, or timestamp data is shown as **Unavailable**. Free space is displayed only
when the backend sets an explicit proof flag. See [Health and Doctor](operations.md#health-and-doctor).

## Activity / Logs

Activity presents structured, fixed-code events. Logs presents bounded lines from controller,
scheduler, and sync sources. The page supports `100`, `200`, `500`, or `1000` lines and can pause
live updates without stopping package logging. **Clear view** clears only the browser presentation;
it does not delete package logs.

Snapshot polling pauses while the document is hidden. Log polling occurs only while Activity is
open and not paused. Refresh intervals are controlled in Settings. Full event and retention details
are in [Health, activity, logs, and notifications](operations.md).

## Notifications

The package alert policy controls direct DSM desktop alerts. It is separate from the optional
open-browser fallback:

- DSM desktop alerts can be enabled, triggered on success and/or failure, delayed until a bounded
  consecutive failure threshold, and rate-limited by a cooldown. They are sent directly to logged-in
  DSM administrators through `synodsmnotify` with fixed, preloaded I18N keys.
- Open-session notifications and the audible cue are local interface preferences. They operate only
  while this application is open and the browser grants permission.

The desktop message never contains a profile name, exit code, path, URL, account name, password,
TOTP material, bearer token, or arbitrary log text. Inspect Activity and bounded package logs for the
specific operation and result. The package does not acquire the `sysnotify` resource or register
Notification Center rules, email, SMS, mobile, or CMS delivery channels.

## Settings

Settings changes only non-secret interface preferences:

| Preference | Values |
| --- | --- |
| Theme | Dark, follow system, or light; dark is the default |
| Status refresh | 3, 5, 10, or 30 seconds |
| Log refresh | 5, 10, or 30 seconds |
| Open-session notification | Off/on, subject to browser permission |
| Audible cue | Off/on, best effort |

These preferences use the browser's local storage. SynoToken, package CSRF, DSM cookies, passwords,
TOTP seeds, and remote-log tokens are memory-only and are never included in that storage object.

## Accessibility and narrow windows

The application provides labeled controls, a skip link, keyboard focus indicators, semantic tables
and forms, polite live regions, a real confirmation dialog, and reduced-motion handling. Navigation
collapses for DSM iframe and narrow-window widths. These contracts are covered by static tests, but
rendered browser QA was unavailable in the development environment; verify keyboard flow, focus,
contrast, zoom, and the exact DSM window sizes during [live acceptance](troubleshooting.md#live-nas-acceptance).

## Queue and completion evidence

Every changing request receives a client request ID and is published into a private package queue.
The API service first returns HTTP `202` with state `queued`. The controller later claims and validates
the job, runs the package manager under a clean package identity, and writes a private response.
Configuration/secret/routine/policy calls poll that response before their success toast. Operational
Doctor/Plan/Run calls remain asynchronous. Therefore:

1. A configuration success toast proves a sanitized terminal manager result, but the refreshed
   snapshot remains the authoritative displayed state.
2. A configuration timeout or `expired_or_missing` result leaves the outcome unknown; inspect state
   before retrying.
3. For Doctor, Plan, and Run, “queued” is not success; follow structured Activity, run state, and
   bounded logs.
4. Investigate a failed or stale job through the [CLI recovery path](cli-parity.md), not browser
   developer tools containing session material.
