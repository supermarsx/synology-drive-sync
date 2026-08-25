# Dashboard security model

The DSM page is an administrative control plane over a package identity that can read explicitly
granted source shares and authenticate to remote NAS accounts. Its web entry point is intentionally
narrow: an ordinary CGI relays one bounded request over a fixed Unix socket, while a package-user
API service authenticates and authorizes the request, returns no stored secret, validates an exact
schema, and sends mutations through a private controller queue instead of running sync work in CGI.

## Installed privilege and socket boundary

The package requests no root execution, Linux capability, set-user-ID bit, or set-group-ID bit. Its
entire `conf/privilege` contract is:

```json
{
  "defaults": {
    "run-as": "package"
  },
  "join-groupname": "http"
}
```

The default makes lifecycle scripts and services run as the actual DSM package identity. DSM may
collision-rename its NSS username, so neither the security boundary nor the documentation assumes a
literal account name. Joining DSM's `http` group lets that non-root service create the one socket
shared with DSM's web identity. There is no `tool`, per-action root override, or capability declaration.

| Installed path | Owner | Mode | Runtime identity and purpose |
| --- | --- | ---: | --- |
| `bin/synology-drive-sync` | package | `0755` | Package-user sync engine |
| `bin/sdsync-dsm` | package | `0755` | Package-user shell control-plane manager |
| `bin/sdsync-dsm-api` | package | `0755` | Package-user API service and private job consumer |
| `ui/api.cgi` | package | `0755` | DSM `http` CGI process; bounded socket relay only |
| `ui/api.sock` | package:`http` | `0660` | Fixed API-service Unix socket; never configurable |

The CGI and service are byte-identical copies of the compiled helper, but their arguments and
runtime identities select different modes. The CGI must have real and effective UID `http`; it does
not change identity and cannot read package-private state. The long-lived `--serve` process must have
real and effective UID equal to the package executable's owner. The server creates `ui/api.sock` under a
package-owned directory that is not group/other-writable, assigns the socket group `http`, and
requires exact mode `0660` and one link.

Both peers authenticate the local transport. The CGI validates the socket owner/group/mode and the
server's kernel-reported peer UID as the package user. The server accepts only a kernel-reported peer
UID matching DSM `http`. A symlink, wrong owner/group/mode, additional hard link, wrong peer, unsafe
parent, or missing socket fails closed.

DSM's official [privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html)
defines package ownership and joined groups. The builder stores every executable as ordinary `0755`.
The validator enforces the exact two-key manifest above, rejects any archive set-user-ID/set-group-ID
member or privilege-bearing tool/capability declaration, and rejects mode, ownership, identity, or
byte mismatches between the two helper copies.

## DSM cookie authentication and the native shell boundary

The DSM session cookie is the authoritative browser authentication input. JavaScript never reads
that cookie; `credentials: "same-origin"` lets the browser attach it to the packaged `api.cgi`
request, and the server validates it with DSM's `authenticate.cgi` before authorizing any action.

The native AppWindow has no standalone HTML launch document and receives no package-owned launch
URL. It does not inspect or rewrite `window.location`, does not try to extract a token from the DSM
shell location, and sends no `X-SYNO-TOKEN` header. Cookie authentication is therefore the active
native browser path. DSM owns the containing shell document and its document-level policies.

Synology's DSM 7 [application authentication guide](https://help.synology.com/developer-guide/integrate_dsm/web_authentication.html)
documents cookie validation through `authenticate.cgi`; it does not document a DSM 7 JavaScript API
for retrieving a launch token. This application therefore does not call an undocumented login
endpoint or depend on DSM browser globals for authentication. Physical-NAS acceptance must still
prove that the package-user API service can execute `authenticate.cgi` with the bounded relayed CGI
environment.

## Authentication and authorization sequence

Every API request goes through these checks:

1. CGI environment values, query, cookie, content length, content type, method, and headers are
   copied into bounded Rust-owned buffers.
2. The CGI verifies that it is a regular package-owned file with exact mode `0755`, that both its
   real and effective UID equal DSM `http`, and that neither web nor package UID is root.
3. It clears its environment and sends one length-bounded frame to the fixed `package:http` `0660`
   socket after validating the socket and server peer identity.
4. The package-user server validates the CGI peer UID, decodes one strict relay schema, and repeats
   method, query, header, body, cookie, request marker, and optional compatibility-token validation.
   The native UI leaves that optional field absent.
5. The server executes DSM's root-owned
   `/usr/syno/synoman/webman/modules/authenticate.cgi` as the package user with only the bounded
   authentication environment DSM expects.
6. The returned username is validated and looked up through DSM's account database.
7. Root is rejected, and independent membership in the DSM `administrators` group is required even
   though the desktop app is also registered with `allUsers: false`.
8. The server reads package-private state or queues a mutation only after authentication,
   authorization, and—on POST—independent package CSRF verification succeed.

UI registration and socket access are not authorization. The server repeats the HTTP validation and
independent administrator check even when a caller reaches the CGI URL or socket path directly.

## Independent package CSRF

DSM cookie authentication is necessary but not sufficient for POST. An authenticated GET to
`action=csrf` returns a five-minute HMAC-SHA256 token bound to:

- authenticated username and UID;
- current DSM cookie;
- any optional compatibility token supplied directly to the API parser (absent for the native UI);
- issue and expiry times; and
- a random nonce.

The signing key is a package-owned private file. Mutation POSTs require the token in
`X-SDSYNC-CSRF`; expired, malformed, replayed in another session, or incorrectly signed values are
rejected. The UI holds it only in memory.

## Exact HTTP surface

Allowed authenticated GET actions are:

| Action | Exact query fields |
| --- | --- |
| `csrf` | none beyond `action` |
| `snapshot` | none beyond `action` |
| `logs` | `lines=1..1000`, optional fixed source `all`, `controller`, `scheduler`, or `sync` |
| `activity` | `lines=1..1000` |
| `result` | one 48-character lowercase hexadecimal server job ID |

The server parser retains a bounded optional token field for compatibility and rejects a malformed or
mismatched supplied value rather than silently downgrading it. The native AppWindow never populates
that field. GET rejects a request body, content type, CSRF header,
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
`remove-routine`, `alert-policy`, and `action`. Unknown, missing, duplicate, out-of-range, nested, or
operation-inapplicable fields fail closed.

## Why mutations use a private queue

The package-user API service can return bounded read-only state after authentication. It does not
perform Doctor, Plan, Run, source validation, or configuration changes in the HTTP request handler.
Those operations are published for the controller so serialization, retention, asynchronous result
tracking, and overlap rules do not depend on a CGI or browser connection remaining open.

Queue behavior:

- the authenticated API service allocates a sortable 48-hex job ID under an exclusive private
  enqueue lock;
- the client request ID remains separately recorded for correlation;
- job JSON and any secret file are package-owned, bounded, non-symlink regular files;
- publication uses private temporary files, hard-link/rename-style atomicity, directory sync, and
  no-follow checks;
- the controller claims jobs in server-ID order and invokes the ordinary `0755` consumer under the
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

Configuration and secret saves, routine/policy changes, and Doctor observe terminal results before
the UI reports success. Pending observations have no client deadline: they continue until terminal
or `expired_or_missing` evidence, five consecutive result-observation failures, invalid result
evidence, or AppWindow shutdown. Repeated observation failures and invalid/expired evidence produce
a typed outcome-unknown result because the accepted server job may still have applied. Plan and Run
remain asynchronous: the UI retains their job IDs only in memory and follows normal run, Activity,
and log evidence.

## Secret and response non-disclosure

Snapshot reports only presence booleans. The API service recursively redacts response keys that imply a
password, secret, token, authorization value, or cookie, except reviewed `has_*` flags. It also
redacts an exact submitted secret if a child unexpectedly echoes it. Unsafe output becomes a generic
failure document.

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
text. Only the local CGI endpoint is reachable under `connect-src 'self'`; the Unix socket is never a
browser endpoint.

Local storage contains only theme/refresh/open-session-notification preferences. Cookies remain DSM's
responsibility and are sent with same-origin credentials.

## Security acceptance limits

Repository tests cover parsing, CGI/service identity predicates, Unix-socket ownership/mode and peer
checks, admin membership, CSRF binding, schema rejection, queue paths/modes/order, redaction,
response bounds, native bundle/style isolation, direct fixed notifier arguments, and SPK privilege/resource layout.
They do not prove the actual DSM `http` identity/group database, package-identity execution of
`authenticate.cgi` or `synodsmnotify`, DSM forwarding of `X-SDSYNC-Request: 1` as
`HTTP_X_SDSYNC_REQUEST=1`, or reverse-proxy/origin
behavior of a physical DSM release. Validate those on every supported DSM branch before calling the
dashboard production-ready.
