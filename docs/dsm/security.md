# Dashboard security model

The DSM page is an administrative control plane over a package identity that can read explicitly
granted source shares and authenticate to remote NAS accounts. Its web entry point is intentionally
narrow: an ordinary CGI authenticates the native DSM request once and relays one bounded assertion
and request over a fixed Unix socket. The package-user API service independently validates the relay,
account, policy, and request, returns no stored secret, and sends mutations through a private
controller queue instead of running sync work in CGI.

## Installed privilege and socket boundary

The package requests no root execution, Linux capability, set-user-ID bit, or set-group-ID bit. Its
entire `conf/privilege` contract is:

```json
{
  "defaults": {
    "run-as": "package"
  }
}
```

The default makes lifecycle scripts and services run as the actual DSM package identity. DSM may
collision-rename its NSS username, so neither the security boundary nor the documentation assumes a
literal account name. The package requests no joined group. There is no `tool`, per-action root
override, or capability declaration.

| Installed path | Owner | Mode | Runtime identity and purpose |
| --- | --- | ---: | --- |
| `bin/synology-drive-sync` | package | `0755` | Package-user sync engine |
| `bin/sdsync-dsm` | package | `0755` | Package-user shell control-plane manager |
| `bin/sdsync-dsm-api` | package | `0755` | Package-user API service and private job consumer |
| `ui/api.cgi` | package | `0755` | Fail-closed package-UID DSM CGI; one DSM authentication and one bounded relay |
| `var/run/api.sock` | package | `0000` prepared, `0600` active | Fixed API-service Unix socket; never configurable |

The CGI and service are byte-identical copies of the compiled helper, but their command-line
arguments select different modes. The CGI fails closed unless Webman starts it with real and
effective UID equal to the executable's exact non-root package owner. The long-lived `--serve`
process uses that package UID through `defaults.run-as=package`. Synology's privilege guide defines
package run-as for package scripts and services; it does not promise Webman's executable-owner CGI
identity, so physical-DSM acceptance must prove that runtime fact. The server creates `var/run/api.sock`
under DSM's package-owned mutable runtime directory, binds it with exact mode `0000`, and only
after its worker pool and exact readiness identity exist activates that same inode with exact mode
`0600` and one link. Group ownership is not an authorization input because `0600` grants no group
access.

Both peers authenticate the local transport. The CGI validates the socket owner, mode, link count,
and inode stability and requires the server's kernel-reported peer UID to equal the package UID. The
server accepts only a kernel-reported CGI peer with that same exact non-root package UID. A symlink,
wrong owner/mode, additional hard link, replaced inode, wrong peer, unsafe parent, or missing socket
fails closed.

`SO_PEERCRED` binds both ends to the package's exact non-root UID, but peer credentials alone are not
request authorization: any process already compromised under that UID shares its authority. DSM
cookie authentication, administrator authorization, strict relay parsing, and package CSRF therefore
remain mandatory above the local transport check. Physical-NAS acceptance must exercise this
boundary with the installed DSM web stack.

DSM's official [privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html)
defines package run-as behavior. The builder stores every executable as ordinary `0755`. The
validator enforces the exact package-only manifest above, rejects any archive
set-user-ID/set-group-ID member or privilege-bearing tool/capability/joined-group declaration, and
rejects mode, ownership, identity, or byte mismatches between the two helper copies.

## DSM cookie and SynoToken authentication at the native shell boundary

The DSM session cookie remains the authoritative browser session credential. JavaScript never reads
that cookie. Before its first package API request, the AppWindow makes the official same-origin
`GET /webapi/entry.cgi?api=SYNO.API.Auth&version=6&method=token` request with
`credentials: "same-origin"`, `cache: "no-store"`, redirect rejection, JSON-only bounded response
handling, and no request body. It accepts only `success=true` with a bounded nonempty
`data.synotoken`, as documented for `method=token`, URL-component-encodes that raw value exactly
once, shares one in-flight bootstrap, and caches only the normalized value in JavaScript module
memory for the current AppWindow. A reload creates a new module and reacquires the token. Our
package-specific browser bridge then carries that value only in `X-SYNO-TOKEN` alongside
`X-SDSYNC-Request: 1`; `X-SYNO-TOKEN` is not presented as a Synology-documented browser header. The
browser independently attaches the DSM cookie through same-origin credentials. The package CGI
canonicalizes the private header into its `SynoToken` helper query; Synology documents `SynoToken`
as a Web API parameter, while this helper transport remains package-observed/private behavior.

The native AppWindow never reads SynoToken from or writes it to a browser-visible launch URL,
`window.location`, history, Referer, bookmark, package action URL/query string, package request body,
`localStorage`, `sessionStorage`, IndexedDB, Activity, logs, notifications, queue results, or support
diagnostics.
Direct `authenticate.cgi` execution is the sole query-string transport: its child process receives a
process-local, token-only `QUERY_STRING` containing `SynoToken=<exactly-once-encoded-value>` and no
package action. The fixed token-bootstrap URL contains no token, and the package action URL and body
contain no token. A failed or invalid bounded bootstrap leaves the package header absent for the
retry cooldown; it never activates launch-URL, browser-global, storage, or client-side login fallback
behavior.

The DSM-launched CGI first asks the kernel whether its exact fixed
`/usr/syno/synoman/webman/modules/authenticate.cgi` entry is executable by the package UID. This
`X_OK` probe occurs before helper metadata, ancestor, or symlink validation. If the probe succeeds,
and only then, the CGI performs the full trusted-path validation described below. If the probe
returns `EACCES`, it skips that validator and selects the bounded loopback user-service branch. Every
other probe error fails closed as `dsm_authentication_helper_unavailable`; it does not select
loopback.

For an executable helper entry, the CGI resolves it without following an unchecked path: every
absolute ancestor directory must be root-owned and not group/world writable, every lexical symlink
boundary must be root-owned and stable across inspection, relative targets may not escape the
validation root, and loops are rejected. The final canonical target must be an executable regular
file that is not group/world writable and is owned by DSM's standard exact built-in `system:system`
identity (`UID:GID 1:1`) or by root for compatibility with legacy layouts. A `system:system` target
must not carry setuid or setgid bits. Only the final executable may use the `system` identity; every
ancestor and symlink boundary remains root-owned. The final device, inode, UID, GID, and mode are
revalidated immediately before direct execution. Symlink mode `0777` is not itself treated as target
writability because the validated parent directory is the link-mutation boundary. This validation
grants no privilege and does not change the package UID. Any other owner, unsafe helper mode,
symlink, ancestor, or revalidation failure always fails closed.

Direct execution clears the inherited environment, forces `REQUEST_METHOD=GET`, and replaces the
package action query with either an empty query or the percent-encoded `SynoToken` field only. The
bounded allowlist retains the cookie, optional `X-SYNO-TOKEN`, address/server fields, and native DSM
CGI context including `GATEWAY_INTERFACE`, `HTTP_HOST`, `REMOTE_PORT`, `REQUEST_SCHEME`,
`SERVER_PROTOCOL`, `SCRIPT_NAME`, `SCRIPT_FILENAME`, `DOCUMENT_ROOT`, `SCGI`, and `SOCKET` when
present. Loader variables, package request/CSRF headers, transfer/content metadata, and the original
package action query are never inherited. The CGI accepts one bounded safe non-root identity and
independently requires administrator membership before relay.

When the initial fixed-path `X_OK` probe returns `EACCES`—as on a protected `root:system 0750`
layout—the CGI does not validate or attempt to execute the inaccessible helper. It issues one bounded
HTTP/1 request to literal `127.0.0.1:$SERVER_PORT/webapi/entry.cgi` using the current request's
`HTTPS` state. The only query fields are `api=SYNO.Core.Desktop.Initdata`, `version=1`, and
`method=get_user_service`. Proxy use and redirects are disabled, `SERVER_NAME` and remote addresses
are not destination inputs, connect and request time are bounded, and at most 1 MiB of response is
accepted. The DSM cookie and optional normalized token are sensitive headers; the token is sent as
`X-SYNO-TOKEN`. There is no cookie or token URL.

The loopback response must be HTTP 200 and valid JSON with `success=true`, a valid string
`data.Session.user`, and Boolean `data.Session.is_admin=true`. Missing, malformed, oversized,
redirected, timed-out, non-administrator, and failed responses fail closed. The CGI then resolves the
reported username through NSS, rejects root, and independently requires membership in DSM's
`administrators` group. A validated helper that is executable but fails to start, times out, changes,
or rejects the cookie does not fall back to the user service; nor does any kernel probe failure other
than permission denial.

The package daemon never executes the DSM authentication helper or calls the loopback user service. It
independently resolves the relayed username, checks exact UID/name and administrator consistency, and
recomputes the cookie/token session binding before authorizing any action.

The native AppWindow has no standalone HTML launch document and receives no package-owned launch
URL. It does not inspect or rewrite `window.location` and does not depend on DSM browser globals. DSM
owns the containing shell document and its document-level policies.

Synology's official DSM 7 [application authentication guide](https://help.synology.com/developer-guide/integrate_dsm/web_authentication.html)
documents direct `authenticate.cgi` use by a custom CGI and the native request environment it needs.
The official [DSM Login Web API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Os/DSM/All/enu/DSM_Login_Web_API_Guide_enu.pdf)
documents `SYNO.API.Auth` version 6 `method=token`, keeping the returned SynoToken in a JavaScript
variable, querying it again after a reload, and using `SynoToken` as an API parameter. The
AppWindow-to-package `X-SYNO-TOKEN` header and its exactly-once canonicalization are this package's
private transport, not an official Synology header contract. Neither guide publishes
`SYNO.Core.Desktop.Initdata/get_user_service` as a package API. Its typed response shape is
corroborated by saved first-party DSM runtime sources, not by the supplied HTTP capture, and remains
private/unpublished behavior. The fallback is therefore still a physical-NAS acceptance requirement
on every supported DSM branch. It remains server-side, loopback-pinned, header-only for credentials,
and fail-closed. Physical-NAS acceptance must also prove Webman's package-owner CGI identity and the
official token bootstrap/direct-helper behavior. The authenticated identity transport
accepts exact valid UTF-8 names from 1 through
256 bytes, including spaces, `DOMAIN\\Username`, and `Username@LDAP_FQDN`; control, bidi-format, and
activity-delimiter characters are rejected. The static package binary cannot directly load
DSM/glibc NSS modules (though an available NSCD path may serve lookups), so local, LDAP, and AD
administrator/non-administrator accounts—including qualified, Unicode, nested-group, and
name-collision cases—remain a required physical-DSM acceptance matrix.

QuickConnect is not an authentication compatibility target for this third-party package. Synology's
[supported-services list](https://kb.synology.com/en-eu/DSM/tutorial/Which_services_support_QuickConnect)
explicitly excludes all third-party services and applications, while its
[QuickConnect white paper](https://global.download.synology.com/download/Document/Software/WhitePaper/Os/DSM/All/enu/Synology_QuickConnect_White_Paper_enu.pdf)
documents direct, relay, and WebRTC connection modes. A relay can therefore present a different
native request/source context to `authenticate.cgi`; DSM authentication remains authoritative. The
package does not trust or replay `X-Forwarded-*`/`X-Real-IP`, and an executable-helper rejection
never falls back to the private user service. After such a rejection, only a strictly parsed
`*.quickconnect.to` `HTTP_HOST` changes the fixed diagnostic code to
`dsm_authentication_quickconnect_unsupported`; it cannot change acceptance. Use LAN, DDNS, VPN, or a
separately tested DSM reverse-proxy/custom-domain route for physical acceptance.

## Authentication and authorization sequence

Every native AppWindow API request goes through these checks:

1. The AppWindow performs or joins the bounded official same-origin `SYNO.API.Auth` version 6
   `method=token` bootstrap, retains an exactly-once-encoded valid token only in module memory, and
   puts it only in the package request's `X-SYNO-TOKEN` header when available.
2. CGI environment values, query, cookie, content length, content type, method, and headers are
   copied into bounded Rust-owned buffers.
3. The CGI verifies that it is a regular package-owned file with exact mode `0755`, that both its
   real and effective UID equal the executable owner's exact package UID, and that UID is not root.
4. The CGI probes `X_OK` on DSM's exact fixed `authenticate.cgi` entry before inspecting helper
   metadata. `EACCES` selects loopback without calling the validator; any other probe error fails
   closed.
5. Only after `X_OK` succeeds does the CGI validate the canonical target, ancestors, ownership,
   writability, type, and stable identity, then revalidate immediately before direct execution.
6. The direct helper receives the bounded native CGI allowlist, forced GET, token-only query, cookie,
   and optional `X-SYNO-TOKEN`. The `EACCES` branch instead calls the fixed bounded loopback user
   service with credentials in headers only. That fallback requires `Session.user` and
   `is_admin=true`; it uses no proxy, redirect, remote host, or token URL.
7. The CGI validates one safe returned identity, rejects root, independently resolves its numeric NSS
   UID, and requires DSM `administrators` membership regardless of the selected branch.
8. It clears its environment and sends one length-bounded frame containing that exact authenticated
   username, numeric UID, and session binding to the fixed package-owned `0600` socket after
   validating the socket, inode, and server peer identity. The pre-commit `0000` state is never
   connectable.
9. The package-user server validates the CGI peer UID, decodes one strict relay schema, and repeats
   method, query, header, body, cookie, request marker, and optional SynoToken validation.
10. Without executing `authenticate.cgi` or calling the user service again, the server independently
   resolves the asserted username through DSM's account database. Its numeric UID must exactly match
   the CGI assertion, and the server recomputes the cookie/token session binding from the strict
   relay fields.
11. Root is rejected, and independent membership in the DSM `administrators` group is required even
   though the desktop app is also registered with `allUsers: false`.
12. The server reads package-private state or queues a mutation only after authentication,
   authorization, and—on POST—independent package CSRF verification succeed.

UI registration and socket access are not authorization. The server repeats the HTTP validation and
independent administrator check even when a caller reaches the CGI URL or socket path directly.

## Independent package CSRF

DSM cookie authentication is necessary but not sufficient for POST. An authenticated GET to
`action=csrf` returns a five-minute HMAC-SHA256 token bound to:

- authenticated username and UID;
- current DSM cookie;
- the normalized SynoToken supplied in `X-SYNO-TOKEN` when the official bootstrap succeeded;
- issue and expiry times; and
- a random nonce.

The signing key is a package-owned private file. Mutation POSTs require the token in
`X-SDSYNC-CSRF`; expired, malformed, replayed in another session, or incorrectly signed values are
rejected with the stable pre-acceptance code `csrf_rejected`. The UI holds it only in memory and
automatically replays ambiguous POST delivery at most twice with the identical serialized body,
client request ID, DSM authentication snapshot, and CSRF token. After that exact-request recovery is
exhausted, it never invents a new request automatically. If a bounded token-bootstrap retry changes
the AppWindow's DSM authentication generation, the client first reissues CSRF under that exact
generation, replaces its module-memory token, and only then serializes the mutation and begins its
bounded exact-delivery recovery.

## Exact HTTP surface

Allowed authenticated GET actions are:

| Action | Exact query fields |
| --- | --- |
| `csrf` | none beyond `action` |
| `snapshot` | none beyond `action` |
| `logs` | `lines=1..1000`, optional fixed source `all`, `api`, `controller`, `scheduler`, `sync`, or `audit` |
| `activity` | `lines=1..1000` |
| `result` | one 48-character lowercase hexadecimal server job ID |

The API parser accepts a bounded token from `X-SYNO-TOKEN` or the legacy `SynoToken` query field and
requires exact agreement if both are present. The native AppWindow uses only the header and never puts
the token in its package URL/query. GET rejects a request body, content type, CSRF header,
duplicate/unknown query key, invalid transfer encoding, or unsupported action. Every browser API
request also requires the fixed custom marker `X-SDSYNC-Request: 1`; the package emits no CORS
permission that would let a foreign origin manufacture that header.

POST accepts only `application/json` with a canonical content length up to 64 KiB and no query
parameters. Its flat envelope is:

```json
{
  "schema": "sdsync.dsm-request.v1",
  "request_id": "32-lowercase-hex-characters",
  "operation": "configure-profile",
  "arguments": {}
}
```

Operations and their exact argument keys are allowlisted by both browser and API service:
`configure-profile`, `remove-profile`, `set-default`, `set-secret`, `schedule`, `routine`,
`remove-routine`, `alert-policy`, `security-policy`, `client-event`, and `action`. Unknown, missing,
duplicate, out-of-range, nested, or operation-inapplicable fields fail closed. `security-policy`
accepts exactly 28 editable fields; the persisted complete document additionally carries immutable
`policy_version=1`. Upgrade migrates only the exact private pre-version 28-key shape, while corrupt,
symlinked, hard-linked, incomplete, or unknown-version policy state remains fail closed and can be
repaired only by supplying a complete replacement through the recovery command. That command emits
its fixed conservative recovery intent without parsing the broken document, atomically installs the
complete replacement, and only then reconciles unrelated pending audit records under the repaired
policy. New profile identifiers are limited to 64 safe ASCII bytes. A released pre-limit profile
identifier of 65 through 255 safe ASCII bytes remains observable, auditable, actionable, and
removable for upgrade recovery, but cannot be newly created.

## Why mutations use a private queue

The package-user API service can return bounded read-only state after authentication. It does not
perform Doctor, Plan, Run, source validation, or configuration changes in the HTTP request handler.
Those operations are published for the controller so serialization, retention, asynchronous result
tracking, and overlap rules do not depend on a CGI or browser connection remaining open.

Queue behavior:

- the authenticated API service allocates a sortable 48-hex job ID under an exclusive private
  enqueue lock;
- the exact 32-hex client request ID is durably mapped to the authenticated session and request
  fingerprint for idempotent replay, and appears in mandatory bridge audit/activity correlation;
- job JSON and any secret file are package-owned, bounded, non-symlink regular files;
- publication uses private temporary files, hard-link/rename-style atomicity, directory sync, and
  no-follow checks;
- the controller claims jobs in server-ID order and invokes the ordinary `0755` consumer under the
  package identity;
- while a routine or action is active, the controller may service connection-only **Test
  authentication** or **Browse target** probes beside it, one at a time, but only when the next
  probe is the current FIFO head;
- it never scans past an earlier action or configuration mutation to reach a connection probe, and
  it does not open the connection lane while a serialized mutation is active;
- jobs older than the accepted window, malformed jobs, unexpected secret files, unsafe paths, and
  output containing sensitive material fail closed; and
- the result endpoint returns only pending or a sanitized terminal manager result.

The configurable `max_outstanding_jobs` value `N` is in `1..256` and applies as two separate caps:
at most `N` active request-plus-processing jobs and, independently, at most `N` retained terminal
responses (worst case `2N` JSON job/result artifacts). Published requests and their separate secret
files have a 24-hour stale ceiling; processing orphans have a one-hour ceiling. Completed response
retention is separately configurable from 300 through 86400 seconds. A response marked
`audit_pending` is pinned until its known terminal audit is durably reconciled; only then can normal
retention remove it. A result lookup after removal returns terminal `expired_or_missing`; it does not
remain pending forever.

If power is lost after a job is claimed into processing, the controller deliberately does not replay
that job on restart. Replaying a partially executed sync or configuration mutation could be more
dangerous than leaving its outcome indeterminate. Inspect snapshot, Activity, health, and target
state, then explicitly repeat the operation only when it is safe.

Configuration and secret saves, routine/policy changes, and Doctor observe terminal results before
the UI reports success. The AppWindow serializes each POST once and, when delivery or acknowledgement
is ambiguous, makes no more than two automatic replays with the identical body, client request ID,
DSM authentication snapshot, and CSRF token. Replaying the same authenticated request ID and payload
returns the original job ID; reusing it for a different payload is a conflict.

Profile saves, connection probes, and autosave mutations have explicit overall observation limits;
transient result-read failures remain retryable until that limit. Unbounded terminal observers report
outcome-unknown after `expired_or_missing` evidence, five consecutive result-observation failures,
invalid evidence, or AppWindow shutdown. The typed error carries the client request ID and, after an
accepted acknowledgement, the job ID because the server job may still have applied. Only the
affected UI scope and operations that depend on it are paused: profile, routine, alerts, security,
interface, or Run/Doctor. Connection probes use a separate incident scope, but an unresolved probe
freezes the affected profile and credential fields and blocks profile submission, authentication,
and File Station requests until an exact read-only reconciliation settles it. Plan and Run remain
asynchronous: the UI retains their job IDs only in memory and follows normal run, Activity, and log
evidence.

## Mandatory audit and log policy

Every accepted mutation first creates a private, fsynced audit-outbox intent and mandatory requested
record before entering an executing or queued phase. Terminal success, failure, or unknown outcome is
recorded in the outbox before log delivery; if the log sink is unavailable, the truthful operation
result is preserved with `audit_pending=true` and the controller retries reconciliation before queue
work and pruning. Exact immutable identity, file and directory sync, and the event-log lock are part
of successful delivery. The package UID owns both mutation and audit state and is therefore the audit
integrity trust boundary; the public CLI still clears caller-supplied attribution variables.

Audit and Activity files are canonical newline-delimited records. Before deduplication or append,
the active file and rotated history are validated while the event-log lock remains held. A complete
active final record missing only its newline is durably terminated; an incomplete active final
record is durably truncated to the last verified newline and a bounded recovery Activity event is
written. Interior blank or malformed records, malformed newline-terminated active history, and any
malformed rotated history remain fail closed. Exact record verification and file/directory sync
complete before the outbox record can be retired.

Category thresholds suppress optional Activity/controller/scheduler and structured pre-relay API
records below the configured level before persistence. Each pre-relay API record uses the exact
stage-derived `bridge`, `authentication`, or `security` category; a corrupt or unsafe policy fails
closed without writing the record or its coalescing state. Log reads scan bounded rotated history and
return up to the requested number of matching records, so newer trace/debug noise cannot hide an
older allowed error. A selected log source is emitted alone; `all` divides a fixed encoded-output
budget across all five sources so the complete JSON remains below the bridge's 1 MiB capture limit.
Minimal mutation accountability records are mandatory and remain visible even when the optional
`audit` category level is `off`.

## Secret and response non-disclosure

Snapshot reports only presence booleans. The API service recursively redacts response keys that imply a
password, secret, token, authorization value, or cookie, except reviewed `has_*` flags. It also
redacts an exact submitted secret if a child unexpectedly echoes it. Unsafe output becomes a generic
failure document.

Package-generated failures use the bounded `sdsync.dsm-error.v1` document with `ok:false`, the
original numeric application `status`, a stable `code`, a bounded `stage`, and a generic `message`.
Webman can discard or replace non-2xx CGI bodies, so read-only GET failures use `Status: 200 OK` only
as the CGI transport while preserving their real 4xx/5xx status in that trusted JSON document. The
AppWindow treats `ok:false` as failure and surfaces the semantic status/code/stage. POST failures keep
their real HTTP status so mutation pre-acceptance and outcome-unknown rules are unchanged. A raw
empty or HTML 4xx/5xx response is outside this package envelope and remains Webman/proxy/pre-CGI
evidence.

Secret replacements travel in a separate private queue file, enter the manager through standard
input, and are removed after claim. See [Secrets and protected values](secrets.md).

## Desktop alert boundary

The package does not acquire DSM's `conf/resource` `sysnotify` worker and does not register
Notification Center rules or email, SMS, mobile, or CMS channels. Its optional package alert policy
invokes the documented `/usr/syno/bin/synodsmnotify -c` desktop path directly for logged-in DSM
administrators.

Each of the three accepted internal triggers maps to literal application, recipient, title-I18N, and
message-I18N arguments. The command is an absolute path and is invoked directly—never through
`eval`, `sh -c`, `xargs`, or a constructed command string. Profile names, exit codes, paths, URLs,
account names, log/error text, cookies, CSRF or compatibility-token values, passwords, TOTP material,
and remote-log tokens never enter notifier arguments. The fixed desktop text tells the administrator
to inspect package Activity and bounded logs for details.

Repository validation rejects the legacy `synonotify` event/custom-variable path, a
`conf/resource` member, sysnotify mail templates, dynamic notification placeholders, and drift from
the exact fixed I18N argv. It does not prove that `synodsmnotify` accepts package-user calls or renders
those keys on a particular DSM build; that remains live-NAS acceptance.

## Browser content policy

The page uses a restrictive self-only Content Security Policy, no inline event handlers, no `eval`,
no dynamic HTML injection, and no external fetch/WebSocket/EventSource URL. DOM output is assigned as
text. `connect-src 'self'` permits only same-origin DSM endpoints: the fixed official token-bootstrap
endpoint and the local package CGI are the authentication/control destinations. The Unix socket is
never a browser endpoint.

Local storage contains only theme/refresh/open-session-notification preferences. It never contains a
DSM cookie, SynoToken, or package CSRF token. Cookies remain DSM's responsibility and are sent with
same-origin credentials; SynoToken remains module-memory-only.

## Security acceptance limits

Repository tests cover parsing, CGI/service identity predicates, Unix-socket ownership/mode and peer
checks, the official same-origin `method=token` bootstrap plus the package-private
memory-only/header-only bridge contract, direct
CGI authentication under synthetic executable-helper permissions, `X_OK`-before-validation ordering,
the kernel-inaccessible `root:system 0750` loopback fallback, response bounds and malformed
identities, daemon non-execution of the helper/user service, GET semantic error transport, admin
membership, CSRF binding, schema rejection, queue
paths/modes/order, redaction, response bounds, native bundle/style isolation, direct fixed notifier
arguments, and SPK privilege/resource layout. They do not prove DSM's physical executable-owner CGI
runtime behavior, the official token response and package-private `X-SYNO-TOKEN` forwarding in a
native physical AppWindow,
direct protected-helper execution on a DSM where it is permitted, the private loopback user-service
behavior on every DSM branch,
package-identity execution of `synodsmnotify`, DSM forwarding of `X-SDSYNC-Request: 1` as
`HTTP_X_SDSYNC_REQUEST=1`, or reverse-proxy/origin behavior of a physical DSM release. QuickConnect
relay access is explicitly outside third-party package support; validate supported LAN, DDNS, VPN,
or custom reverse-proxy routes on every supported DSM branch before calling the dashboard
production-ready.
