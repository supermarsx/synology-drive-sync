# Troubleshooting

Start with the narrowest non-mutating diagnostic and increase scope only after it passes:

```bash
synology-drive-sync config validate
synology-drive-sync doctor source --profile production --hash
synology-drive-sync doctor --url https://files.example.com --routing-only
synology-drive-sync doctor --profile production target
synology-drive-sync plan --profile production -v
```

## Reading a debug log

`--log-level debug` reports the endpoint identity, the session shape, and every failed HTTP round
trip; `--log-level trace` adds the successful ones. One such run is normally enough to place a
failure, and it is what to attach when reporting a problem:

```text
1788730847263 INFO  build synology-drive-sync 0.1.0 (43cc1d26b27a) x86_64-unknown-linux-gnu release
1788730847267 DEBUG connection established scheme=https host=nas.example.test port=5001 base_path=/ redirects=refused certificate_verification=enabled
1788730847290 DEBUG session established sid_length=24 token=present login_format=sid server_set_cookie=absent
1788730847294 TRACE API call completed SYNO.FileStation.List.list_share v2 session=cookie+token-header+sid-field+token-field status=200 dsm=ok elapsed_ms=3
1788730847299 DEBUG API call completed SYNO.FileStation.List.getinfo v2 session=cookie+token-header+sid-field+token-field status=200 dsm=119 outcome=dsm-error elapsed_ms=4 detail="session is invalid; rerun to authenticate again"
```

Read it as follows:

- `session=` names which credential channels that request carried, as booleans. It never contains a
  value.
- Two adjacent calls with the **same** `session=` where the first succeeds and the second returns
  `106`, `107`, or `119` means the credential is fine and the session is not surviving between
  requests — look at relay or load-balancer affinity before looking at the account.
- `server_set_cookie=absent` alongside `login_format=sid` is expected: DSM's `format=sid` login does
  not set a cookie.
- `set_cookie=` on a **successful** call means DSM rotated the session on that response. Any later
  request still carrying the previous identifier is then stale, which is a sufficient explanation
  for a `119` on the very next call. If no response in the run reports `set_cookie=`, rotation is
  ruled out and the cause lies elsewhere. Only cookie names are logged, never their values.
- `attempt=1/3` that never becomes `2/3` means the failure was not retryable; DSM `119` is not.
- A large `elapsed_ms` on `SYNO.API.Auth.login` is relay round-trip cost, not a slow password check.

No credential material appears at any level. See the
[observability contract](../observability.md) for what these records can and cannot contain, and
note that debug-level records do name the NAS host and the WebAPI paths.

## Reverse proxy and DSM clues

DSM can return HTTP 200 with a JSON API failure, so both transport status and response body matter.

| Symptom | Likely boundary | What to verify |
| --- | --- | --- |
| HTML instead of JSON | `/webapi/*` reached a UI, redirect, or different service | Proxy route and prefix rewrite. |
| HTTP 413 | Proxy request-body limit | Raise it above the largest whole-file upload. |
| HTTP 502 | Wrong/unavailable File Station backend | Backend address, scheme, and path rewrite. |
| HTTP 504 or DSM `1801` | Proxy/File Station/application timeout | Long-upload timeout, rate cap, and largest file. |
| DSM `150` | Requests appear from different client IPs | Proxy source-IP/session consistency. |
| DSM `1800` | Multipart content length absent/inconsistent | Proxy buffering/body transformations and client version. |
| DSM `106`, `107`, or `119` immediately after login | File Station rejected, expired, or lost the DSM session before evaluating the path | Run `doctor` and read the **DSM session channel ablation** section: it presents the same request through one session channel at a time and says whether the fault is which channel this client uses or the path itself. Then compare the same account through a direct LAN or tested custom-domain route. |
| Login succeeds but inventory returns DSM `105` | Account or package permission mismatch | File Station application permission and the exact destination ACL. |
| Destination missing | Root or ancestor is not writable/does not exist | Create/enable the share or user home in DSM; the client only creates below it. |

### Separating a bad session from a bad path

`doctor` answers this in one run, at every level including `quick`, without needing credentials for
the transport half. Read the three transport sections together:

- **Network reachability and connect timing** measures TCP connect latency (never ICMP: no ping
  packet is sent) and the unauthenticated HTTP round trip. A hostname resolving to more than one
  address, or a connect time that splits into two clusters, means consecutive requests are not
  landing in one place.
- **Intermediaries and reverse proxies** names what is in the path and classifies the endpoint.
  `[alias].[relay_id].quickconnect.to` is the **relay** form; `[alias].direct.quickconnect.to` and
  `[ip].[alias].direct.quickconnect.to` are direct. More than one `Server` banner inside a single
  run is direct evidence that two origins answered.
- **Session cookie permanence** follows every cookie the server set. A session cookie re-issued
  under a *changed* value on a call the server also reported as **successful** is the decisive
  observation: this client keeps no cookie jar, so it discarded that value and went on sending the
  previous one, and the next call will fail with `119` for a reason that has nothing to do with the
  path.

At Standard and above, two authenticated sections settle it rather than leaving it to inference:

- **DSM session channel ablation** makes the same read-only request once per variant, varying only
  which channels carry the session. If DSM accepts the documented `_sid` request field on its own
  and rejects the combination this client normally sends, the fault is the client's session
  transport and the section says so with the fix. If only the *full combination* is accepted --
  the cookie alone rejected as well -- DSM is authenticating browser-style and the fix is to keep
  sending both channels, not to remove one. If every channel is rejected, the session identifier
  itself is gone server-side, which no client-side change addresses. At Extensive a fifth variant
  logs in a second time *without* `enable_syno_token` and offers that session through `_sid`
  alone, which settles whether the request-parameter path is refused in general or only for
  browser-style sessions. That second login is deliberately taken after the run's own session has
  been logged out, so a duplicate-login collision (DSM `107`) cannot interrupt the run itself.
- **File Station capability diagnosis** reads `SYNO.FileStation.Info` `get` at the start and again
  at the end of the run. Two *different* host names inside one run is the only positive proof in
  this report that consecutive requests reached different DSM hosts. The same host name at both ends
  excludes nothing on its own -- a single NAS behind a relay has one host name.

If the cookie ledger reports a rotation on a successful call, or the ablation accepts `_sid` alone
and rejects the full combination, fix the session transport. If neither, and the reachability or
intermediary sections show more than one address, more than one `Server` banner, or a relay
hostname, re-run against a direct address before changing anything in the client.

`doctor` cannot prove a relay is at fault directly: DSM returns no backend or instance identifier in
any documented response, so that hypothesis is reachable only by elimination -- or by the host-name
comparison above, which is the one mechanism that can confirm it outright.

## Source failures

- Run `doctor source --hash` as the scheduler identity, not only interactively.
- A share root itself is rejected; choose a subdirectory.
- Links, Windows reparse points, cloud placeholders, offline/HSM files, unreadable entries, invalid
  portable names, and files changing during scan/hash fail closed.
- A mapped drive letter may be absent in a service logon. Use a proven UNC path or mount lifecycle.

See [local, mapped-drive, and SMB sources](../local-and-smb-sources.md).

## Uploads retry from zero

File Station exposes no resumable upload protocol. After a lost response, content mode checks whether
the exact expected size and complete MD5/CRC32/SHA-256 fingerprint already arrived. If it cannot prove completion, the retry restarts the
whole file. Account for this in bandwidth, proxy, and scheduler limits.

## A plan changed before execution

That is a safety signal. The executor refreshes relevant state and refuses stale destructive
assumptions. Quiesce local and remote writers, obtain a new plan, and inspect it. Do not increase a
deletion cap merely to silence a changed plan.

## Unattended run has no useful output

`--quiet` suppresses terminal diagnostics and progress, but retains configured file and remote sinks.
Provision a writable `log-file`, select JSON diagnostics when appropriate, and make remote logging
`required` only when its delivery should determine run success. See [observability](../observability.md).

## Collecting evidence safely

Record the exact release/tag or image digest, command shape with secret values removed, selected
profile name, exit code, DSM/File Station versions, reverse-proxy product/configuration boundary, and
redacted structured logs. Never attach password/TOTP/token files, raw secret environment blocks, or
unredacted crash dumps.
