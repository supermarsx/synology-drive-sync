# Network, reverse proxy, and TLS

Every DSM request is derived from one configured base URL. The client does not discover LAN
addresses, probe ports 5000/5001, bypass the reverse proxy, or fall back to another transport.

Valid shapes include:

```text
https://files.example.com
https://files.example.com:443
https://gateway.example.com/nas/
```

An optional path prefix is supported. For `/nas/`, the proxy must rewrite `/nas/webapi/*` to DSM's
`/webapi/*`. A File Station browser alias alone is not proof that WebAPI routing works.

## Forward proxies from the environment

The client honours the standard proxy environment variables, so traffic can leave through a forward
proxy you did not configure here. For the configured URL's scheme it reads `HTTPS_PROXY` or
`HTTP_PROXY` (upper case first), then `ALL_PROXY`; an empty value means none; no proxy is used under
CGI (`REQUEST_METHOD` set); and `NO_PROXY` exempts hosts by `*`, exact host, domain suffix, IP address
or CIDR range. Credentials in the proxy URL are never printed.

Because nothing else says the NAS is reached through a third party, `sync`, `plan`, `status` and
`doctor` print a warning after connecting that names the proxy and the variable it came from, and tell
you to set `NO_PROXY=<nas host>` to connect directly. With `--allow-http` the warning adds that the
password crosses the proxy unencrypted. `--quiet` suppresses it. When API discovery fails, the error
says where it failed: `File Station API discovery failed at http://host:port; ...` for a direct
connection, or `... through the forward proxy http://proxy:port (from HTTP_PROXY); ...` when one was
used. Whether a job run by the DSM package inherits a proxy from the Control Panel is not known from
the code alone; [the NAS questions](../production-acceptance.md#questions-only-a-real-nas-can-answer)
say how to check what such a job really sees.

## Routing-only diagnostic

Before provisioning credentials, test TLS, proxy routing, and API discovery:

```bash
synology-drive-sync doctor \
  --url https://files.example.com \
  --routing-only
```

The endpoint should return DSM JSON for WebAPI requests, not HTML, a login-portal redirect, or a
proxy-branded error page.

## Authenticated session handoff

DSM returns a SID and, when CSRF protection is enabled, a SynoToken after login. Authenticated
requests carry each value through both documented DSM representations: the SID is sent as the
`id` cookie and `_sid` request parameter, while the token is sent as the `X-SYNO-TOKEN` header and
`SynoToken` request parameter. This accommodates current `entry.cgi` handling without depending on
one proxy-specific representation. Redirects stay disabled, every endpoint remains confined to the
configured origin and path prefix, and cookies advertised by the reverse proxy are not retained or
replayed.

The Standard and Extensive target diagnostics do not treat a successful login response as final
proof. Before reporting session authentication as healthy, they make one bounded, non-mutating File
Station request. DSM code `119` at that boundary means the SID was not accepted; destination paths
and permissions have not been evaluated yet. See Synology's
[DSM Login Web API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Os/DSM/All/enu/DSM_Login_Web_API_Guide_enu.pdf)
and [File Station API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Package/FileStation/All/enu/Synology_File_Station_API_Guide.pdf)
for the upstream session and error-code contracts.

## URL validation

`url` must be absolute and include a host. Embedded usernames/passwords, query strings, and
fragments are rejected. HTTPS is mandatory unless `allow-http` is explicitly enabled.

`remote-log-url` has a stricter independent contract: it always requires HTTPS and similarly rejects
embedded credentials, query strings, and fragments.

## Private certificate authorities

Use `ca-certificate` to add a PEM root/intermediate certificate for a private reverse-proxy PKI:

```toml
ca-certificate = "./pki/private-root.pem"
```

This retains hostname and chain verification. `danger-accept-invalid-certs = true` disables
certificate verification and should not be used in production. `allow-http = true` is intended only
for controlled LAN testing and sends credentials without TLS protection.

## Timeouts

| Setting | Default | Scope |
| --- | --- | --- |
| `connect-timeout` | 15 seconds | TCP/TLS connection setup |
| `timeout` | 7200 seconds | An upload's progress and answer windows (below), and one File Station background operation (checksum, server-side copy) |
| Control-plane cap | 10 seconds | Discovery, login, inventory, hash, permission, and similar requests |

The application timeout is not a whole-job deadline. An outer service-manager timeout must cover
local scan/hash work, remote inventory, every upload and retry, verification, final replan, and
cooperative shutdown.

### What `timeout` means for an upload

An upload attempt is no longer cut off by one total deadline. `timeout` bounds it three ways:

| Bound | Value | When it ends the attempt |
| --- | --- | --- |
| Stall window | `min(timeout, 300 s)` | The connection took no body bytes for that long. The attempt is retried like any timeout, after the check for a copy that landed. |
| Response wait | `timeout` plus a drain allowance | DSM did not answer within `timeout` plus the drain allowance after the last body byte was handed to the connection. The allowance is the time the bytes still buffered in the connection need at the 16 KiB/s floor, counting at most 16 MiB of them, so the wait is at most `timeout` + 1024 s. |
| Ceiling | `max(timeout, file size / 16 KiB/s)` | The whole body took longer than this, even though it never quite stalled (a trickle). **Not retried**, because the next attempt would end the same way. |

A transfer that keeps moving is therefore not cut by `timeout` alone, and a large file on a slow but
steady link no longer needs a `timeout` as long as the whole transfer. What it cannot do is average
below 16 KiB/s. Each error names the bound that ended the attempt, for example "the upload stalled: the
connection took no body bytes for N s (X of Y bytes sent in T s); check the link and any proxy in front
of DSM, or raise --timeout"; for the response wait: "DSM did not answer within N s (--timeout) after the last
byte of FILE (X bytes) was sent, allowing M s more for what the connection was still carrying" (M is the
drain allowance). The background-operation timeout (checksums, server-side copy) and the
10-second control-plane cap are unchanged.

## Retries and upload behavior

`retries` defaults to 2 and accepts 0 through 5. Retryable transport, busy, HTTP 408/429, and
502/503/504 failures are bounded. File Station has no resumable upload protocol. In content mode, a
lost response first triggers a check of the destination's exact size, modification time and server-side MD5; if completion cannot be proved, the
retry restarts the whole file.

What is retried, and what never is:

- **Login** is retried within `retries` on a connection failure and on HTTP 502/503/504 only (a proxy
  that is still starting). A timeout, a refused password or any DSM answer is never retried.
- **Reads** (listings, for example) and **uploads** are also retried when the connection was closed
  after the request was sent but before an answer. The reads that verify or guard a mutation
  (the post-upload and post-copy check, and the live check before a delete) are retried too. A
  `Delete`, `Rename`, or server-copy start is sent once.
- **A torn overwrite** (the destination has the right size and modification time but the wrong
  checksum after the last attempt) gets exactly one more full attempt in the same run. If that also
  fails, the error names the path and the repair.
- **Session expiry.** DSM ending the session mid-run is recovered by logging in again, at most 4 times
  in any 10 minutes and at most `4 + elapsed / 15 minutes` over the run. When the allowance is spent
  the original session error is reported with "DSM ended N sessions in M minutes, so re-authentication
  stopped": a NAS or proxy that kills sessions faster than that is the problem, not the file. The SDK
  does not install a re-authenticator.
- The background copy poll starts at 50 ms and backs off to a 2 second ceiling.

HTTP 301-308, 413, 502 and 504 answers from a reverse proxy are named with a hint (raise the
request-body limit, fix the route or the redirect) for every API, without echoing the response body.

## Upload rate limit

`max-rate` is a plain byte count per second and is shared by all concurrent uploads:

```toml
max-rate = 1048576 # 1 MiB/s total
jobs = 4
```

Four jobs divide the one budget; they do not each receive 1 MiB/s. A limit stretches every transfer by
definition. At 16 KiB/s or more an upload's own ceiling (`max(timeout, size / 16 KiB/s)`) already allows
for it, and a transfer that keeps moving is not cut by `timeout`. Below 16 KiB/s, raise `timeout` above the
largest single file divided by the configured rate: a 4 GiB file at 8 KiB/s needs roughly 524,288 seconds.

The client enforces that relationship before sending anything. An upload whose minimum duration at
`max-rate` (its size less one second of budget, divided by the rate) already exceeds the upload's
ceiling (`max(timeout, size / 16 KiB/s)`, so only a rate below 16 KiB/s can trigger it) is refused
with a configuration error that names the file, the seconds needed and both settings, instead of being
sent `retries + 1` times and timing out each time. The check is a lower bound: other concurrent uploads
sharing the budget only make a transfer longer.

The reverse proxy and DSM must also permit the largest request body and a connection long enough for
that rate. Prove both with the [production acceptance runbook](../production-acceptance.md).
