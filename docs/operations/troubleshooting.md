# Troubleshooting

Start with the narrowest non-mutating diagnostic and increase scope only after it passes:

```bash
synology-drive-sync config validate
synology-drive-sync doctor source --profile production --hash
synology-drive-sync doctor --url https://files.example.com --routing-only
synology-drive-sync doctor --profile production target
synology-drive-sync plan --profile production -v
```

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
| Login succeeds but inventory fails | Account/package permission mismatch | File Station permission and exact destination ACL. |
| Destination missing | Root or ancestor is not writable/does not exist | Create/enable the share or user home in DSM; the client only creates below it. |

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
