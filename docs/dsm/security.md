# Dashboard security model

The DSM page is an administrative control plane over a package identity that can read explicitly
granted source shares and authenticate to remote NAS accounts. Its bridge is intentionally narrow:
it authenticates every request, returns no stored secret, validates an exact schema, and sends
mutations through a private controller queue rather than running sync work inside CGI.

## Installed privilege boundary

The package uses `run-as: package` for lifecycle scripts and ordinary tools. It requests no root
execution and no Linux capabilities.

| Installed path | Owner | Mode | Purpose |
| --- | --- | ---: | --- |
| `bin/synology-drive-sync` | package | `0755` | Static sync engine |
| `bin/sdsync-dsm` | package | `0755` | Shell control-plane manager |
| `bin/sdsync-dsm-api` | package | `0755` | Non-setuid private job consumer used by the controller |
| `ui/api.cgi` | package | `4755` | Same compiled helper bytes, setuid only to the non-root package user |

No other package file may be setuid/setgid or group/world-writable. The general CLI is never setuid.
The CGI's setuid bit does not grant root; it changes the effective identity from DSM's web user to
the non-root package user so private package state can be reached after authentication.

That table describes the installed state, not the distributable tar metadata. Every member of the
outer SPK and inner `package.tgz`, including `ui/api.cgi`, is archived without setuid/setgid bits;
the CGI enters `package.tgz` as ordinary `0755`. During installation DSM reads `conf/privilege`,
assigns `ui/api.cgi` to user/group `package`, and only then applies `4755`. This prevents Package
Center's pre-install scan from seeing a root-owned setuid archive entry while preserving the narrow
installed web-user-to-package-user bridge. The manifest contains no root run-as request or Linux
capability.

DSM's official [privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html)
defines package ownership and four-digit tool modes. The SPK validator enforces the exact reviewed
list, rejects any setuid/setgid archive member, requires the package-owned installed CGI declaration,
and rejects mode, ownership, identity, or byte mismatches between the two helper copies.

## Launch token and referrer handling

The application reads `SynoToken` only from its DSM launch URL. It accepts a non-empty value up to
1,024 bytes with no ASCII whitespace or control characters, removes the parameter from the visible
URL immediately, keeps it in memory, and sends it as `X-SYNO-TOKEN` on same-origin API requests.

A `no-referrer` meta policy appears before icons, CSS, or scripts so the initial token-bearing URL is
not sent as a Referer while the local application loads. The token is never written to local storage,
logs, a bookmark, Activity, or a profile.

Synology's DSM 7 [application authentication guide](https://help.synology.com/developer-guide/integrate_dsm/web_authentication.html)
documents the DSM cookie and `authenticate.cgi`, but it does not document a DSM 7 JavaScript API for
retrieving SynoToken. This application does not call a login endpoint or depend on undocumented
browser globals. It fails closed when AppLaunch does not provide the token. Whether current DSM 7
AppLaunch supplies it to this third-party `dsmuidir` application remains a physical-NAS acceptance
gap.

## Authentication and authorization sequence

Every API request goes through these checks:

1. CGI environment values, query, cookie, content length, content type, method, and headers are
   copied into bounded Rust-owned buffers.
2. The executable verifies that it is a regular, package-owned setuid file, invoked with DSM's web
   UID as the real user, not root, and not group/world-writable.
3. A non-empty DSM session cookie and matching query/header SynoToken are required.
4. The bridge executes DSM's root-owned
   `/usr/syno/synoman/webman/modules/authenticate.cgi` in a child permanently dropped to the web UID,
   forwarding only the bounded authentication environment DSM expects.
5. The returned username is validated and looked up through DSM's account database.
6. Root is rejected, and independent membership in the DSM `administrators` group is required even
   though the desktop app is also registered with `allUsers: false`.
7. The bridge clears its environment and permanently sets real, effective, and saved UIDs to the
   non-root package UID before reading package state or accepting a mutation.

UI registration is not authorization. The independent administrator check remains mandatory when
someone calls the CGI URL directly.

## Independent package CSRF

DSM authentication and SynoToken are necessary but not sufficient for POST. An authenticated GET
to `action=csrf` returns a five-minute HMAC-SHA256 token bound to:

- authenticated username and UID;
- current DSM cookie;
- current SynoToken;
- issue and expiry times; and
- a random nonce.

The signing key is a package-owned private file. Mutation POSTs require the token in
`X-SDSYNC-CSRF`; expired, malformed, replayed in another session, or incorrectly signed values are
rejected. The UI holds it only in memory.

## Exact HTTP surface

Allowed authenticated GET actions are:

| Action | Exact query fields |
| --- | --- |
| `csrf` | none beyond action/SynoToken |
| `snapshot` | none beyond action/SynoToken |
| `logs` | `lines=1..1000`, optional fixed source `all`, `controller`, `scheduler`, or `sync` |
| `activity` | `lines=1..1000` |
| `result` | one 48-character lowercase hexadecimal server job ID |

GET rejects a request body, content type, CSRF header, duplicate/unknown query key, invalid transfer
encoding, or unsupported action.

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

Operations and their exact argument keys are allowlisted by both browser and bridge:
`configure-profile`, `remove-profile`, `set-default`, `set-secret`, `schedule`, `routine`,
`remove-routine`, `alert-policy`, and `action`. Unknown, missing, duplicate, out-of-range, nested, or
operation-inapplicable fields fail closed.

## Why mutations use a private queue

After DSM authentication, the CGI can permanently drop its UIDs but cannot safely clear
supplementary web-process groups without root. It therefore executes only read-only manager API
commands directly. Doctor, Plan, Run, source validation, and every mutation are published for a clean
package-controller process.

Queue behavior:

- the bridge allocates a sortable 48-hex job ID under an exclusive private enqueue lock;
- the client request ID remains separately recorded for correlation;
- job JSON and any secret file are package-owned, bounded, non-symlink regular files;
- publication uses private temporary files, hard-link/rename-style atomicity, directory sync, and
  no-follow checks;
- the controller claims jobs in server-ID order and invokes the non-setuid consumer under the clean
  package identity;
- jobs older than the accepted window, malformed jobs, unexpected secret files, unsafe paths, and
  output containing sensitive material fail closed; and
- the result endpoint returns only pending or a sanitized terminal manager result.

The queue accepts at most 256 safe outstanding entries. Published requests and their separate secret
files have a 24-hour retention ceiling. Completed responses and unrecoverable processing-orphan
artifacts are retained for one hour, with at most 256 response entries. A result lookup after removal
returns terminal `expired_or_missing`; it does not remain pending forever.

If power is lost after a job is claimed into processing, the controller deliberately does not replay
that job on restart. Replaying a partially executed sync or configuration mutation could be more
dangerous than leaving its outcome indeterminate. Inspect snapshot, Activity, health, and target
state, then explicitly repeat the operation only when it is safe.

Configuration and secret saves poll terminal results before proceeding to dependent jobs. Long
Doctor/Plan/Run actions remain asynchronous: the UI retains the job ID only in memory and follows
normal run, Activity, and log evidence.

## Secret and response non-disclosure

Snapshot reports only presence booleans. The bridge recursively redacts response keys that imply a
password, secret, token, authorization value, or cookie, except reviewed `has_*` flags. It also
redacts an exact submitted secret if a child unexpectedly echoes it. Unsafe output becomes a generic
failure document.

Secret replacements travel in a separate private queue file, enter the manager through standard
input, and are removed after claim. See [Secrets and protected values](secrets.md).

## Browser content policy

The page uses a restrictive self-only Content Security Policy, no inline event handlers, no `eval`,
no dynamic HTML injection, and no external fetch/WebSocket/EventSource URL. DOM output is assigned as
text. Only the local API bridge is reachable under `connect-src 'self'`.

Local storage contains only theme/refresh/open-session-notification preferences. Cookies remain DSM's
responsibility and are sent with same-origin credentials.

## Security acceptance limits

Repository tests cover parsing, identity predicates, admin membership, UID drop, CSRF binding,
schema rejection, queue paths/modes/order, redaction, response bounds, static CSP, and SPK privilege
layout. They do not prove the actual web UID/group database, `authenticate.cgi` behavior, AppLaunch
token delivery, or reverse-proxy/origin behavior of a physical DSM release. Validate those on every
supported DSM branch before calling the dashboard production-ready.
