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
| `timeout` | 7200 seconds | One upload or File Station background operation |
| Control-plane cap | 10 seconds | Discovery, login, inventory, hash, permission, and similar requests |

The application timeout is not a whole-job deadline. An outer service-manager timeout must cover
local scan/hash work, remote inventory, every upload and retry, verification, final replan, and
cooperative shutdown.

## Retries and upload behavior

`retries` defaults to 2 and accepts 0 through 5. Retryable transport, busy, HTTP 408/429, and
502/503/504 failures are bounded. File Station has no resumable upload protocol. In content mode, a
lost response first triggers an exact remote size and MD5/CRC32/SHA-256 fingerprint check; if completion cannot be proved, the
retry restarts the whole file.

## Upload rate limit

`max-rate` is a plain byte count per second and is shared by all concurrent uploads:

```toml
max-rate = 1048576 # 1 MiB/s total
jobs = 4
```

Four jobs divide the one budget; they do not each receive 1 MiB/s. Raise `timeout` above the largest
single file divided by the configured rate. A 4 GiB file at 1 MiB/s needs roughly 4096 seconds before
protocol and retry overhead.

The reverse proxy and DSM must also permit the largest request body and a connection long enough for
that rate. Prove both with the [production acceptance runbook](../production-acceptance.md).
