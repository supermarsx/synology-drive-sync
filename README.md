# synology-drive-sync

A lean Rust CLI that pushes one local directory into a Synology Drive-backed folder using only the documented Synology File Station WebAPI through one reverse-proxy URL.

It is deliberately one-way:

- the local source is authoritative and is never modified;
- missing and changed local files are uploaded;
- empty local directories are created;
- remote-only content is preserved by default;
- `--delete` opts into an exact remote mirror.

This is **not** a Synology Drive protocol client. It writes to the underlying DSM folder through File Station. Synology Drive can then index that folder when it belongs to My Drive or an enabled Team Folder.

## Why this shape

File Station provides the small API surface a push mirror needs: API discovery, DSM authentication, directory listing, folder creation, upload/overwrite, and deletion. That keeps the tool stateless: no database, daemon, DSM port probing, SMB, WebDAV, SSH, QuickConnect, or private Drive protocol.

The implementation is synchronous Rust with two upload workers by default. Upload bodies are streamed from disk, carry a known `Content-Length`, and put the binary part last as File Station requires.

## Reverse proxy

The recommended topology is a dedicated public hostname:

```text
https://files.example.com:443
        |
        | Synology reverse proxy
        v
https://nas.lan:7001       # File Station customized HTTPS port
```

Configure DSM 7 under **Control Panel > Login Portal**:

1. Give File Station a customized HTTPS port.
2. Create a host-based reverse-proxy rule from the public HTTPS hostname to that port.
3. Assign a valid certificate to the public hostname.
4. Raise proxy send/read timeouts and any request-body limit to cover the largest upload.
5. Confirm the same origin exposes `/webapi/entry.cgi`; routing only the File Station browser UI is insufficient.

No WebSocket headers are required. File Station's documented sync operations use ordinary HTTP requests.

Probe routing without credentials:

```bash
curl -fsS -X POST https://files.example.com/webapi/entry.cgi \
  --data-urlencode api=SYNO.API.Info \
  --data-urlencode version=1 \
  --data-urlencode method=query \
  --data-urlencode query=SYNO.API.Auth,SYNO.FileStation.List
```

The response should be JSON with `"success": true`, not HTML or a redirect to a login page.

An optional URL prefix is supported, for example `https://gateway.example.com/nas/`. The proxy must explicitly rewrite `/nas/webapi/*` to the backend's `/webapi/*`; Synology's documented and simpler setup is a dedicated hostname without a prefix.

## Build

Rust 1.88 or newer is required.

```bash
git clone https://github.com/supermarsx/synology-drive-sync.git
cd synology-drive-sync
cargo build --release
```

The binary is `target/release/synology-drive-sync` (`.exe` on Windows). HTTPS uses rustls and platform certificate verification, so no OpenSSL runtime is needed.

## First run

Use a dedicated, non-administrator DSM account with:

- File Station application permission;
- read/write access to the destination shared folder;
- access to the relevant My Drive or Team Folder path.

Set the non-secret connection values, then perform a dry run:

```bash
export SDSYNC_URL=https://files.example.com
export SDSYNC_USERNAME=mirror-bot
synology-drive-sync ./project /team-folder/project --dry-run
```

The password is prompted with terminal echo disabled. If DSM requires two-factor authentication, the CLI recognizes the OTP-required response and prompts for a current TOTP code.

PowerShell:

```powershell
$env:SDSYNC_URL = 'https://files.example.com'
$env:SDSYNC_USERNAME = 'mirror-bot'
synology-drive-sync.exe C:\Data\Project /team-folder/project --dry-run
```

Apply the additive/update-only plan by removing `--dry-run`:

```bash
synology-drive-sync ./project /team-folder/project
```

The remote path is a File Station logical path beginning with a shared folder, never a physical path such as `/volume1/...`.

## Authentication and 2FA

Passwords and OTPs are never accepted as command-line values, so they do not appear in process listings. Credential precedence is:

Password:

1. first line of standard input with `--password-stdin`;
2. `SDSYNC_PASSWORD`;
3. masked terminal prompt.

OTP:

1. `SDSYNC_OTP`;
2. masked terminal prompt when DSM reports that OTP is required.

For an unattended run:

```bash
export SDSYNC_PASSWORD='use-a-secret-provider-in-production'
export SDSYNC_OTP='123456'
synology-drive-sync /srv/export /team/export
unset SDSYNC_PASSWORD SDSYNC_OTP
```

Prefer injecting these variables from the scheduler's or operating system's secret store instead of saving them in a script.

The CLI uses `SYNO.API.Auth` v6 when available, `session=FileStation`, `format=sid`, and `enable_syno_token=yes`. Authenticated requests are POST bodies, not URL queries. The returned SID and exact-cased `SynoToken` are sent on subsequent calls and the session is explicitly logged out.

The documented API supports DSM TOTP through `otp_code`. Synology Secure SignIn approval and hardware/security-key flows are browser-only and have no public File Station WebAPI challenge flow, so they are not supported here. Configure OTP as the account's app-compatible second factor.

## Exact mirror mode

`--delete` removes remote entries absent locally and permits file/directory type replacement:

```bash
synology-drive-sync ./project /team-folder/project \
  --delete --dry-run

synology-drive-sync ./project /team-folder/project \
  --delete
```

Deletion has several independent guards:

- it is off by default;
- `/` is never a valid destination;
- every delete must be a strict child of the configured destination;
- the default maximum is 100 entries (`--max-delete` changes it explicitly);
- a source with no payload files cannot trigger deletion unless `--allow-empty-source` is present;
- ignored, DSM-managed, and File Station-mounted paths are protected;
- all scans, directory creation, and uploads must succeed before remote-only deletion begins;
- entries are deleted deepest-first with `recursive=false`.

That final rule is an intentional race guard. If another client creates a file inside a directory after the inventory scan, File Station refuses to remove the now-nonempty directory instead of silently deleting the new file.

Enable the DSM shared-folder recycle bin as an additional operational safety net, and always inspect the first mirror run with `--dry-run`.

## Exclusions

Put gitignore-style rules in `.sdsyncignore` at the local source root:

```gitignore
target/
*.tmp
.cache/
```

Or add repeatable command-line rules:

```bash
synology-drive-sync ./project /team/project \
  --exclude 'target/' \
  --exclude '*.tmp'
```

Excluded paths are out of scope, not absent. In `--delete` mode, matching remote paths and the directories needed to contain them are preserved.

The root `.sdsyncignore` is control data: it is never uploaded, and an existing remote copy is preserved.

Hidden files are included unless excluded. In-scope symlinks, junctions/reparse points, special files, unreadable entries, non-UTF-8 names, and DSM/Drive working names fail the preflight scan. On Windows, Drive-incompatible `OFFLINE`, `SYSTEM`, `TEMPORARY`, and reparse-point entries also fail. Portable Drive name checks reject leading `~`, control characters, case-colliding paths, Windows-invalid/reserved names, and obvious documented name/path overflows before any remote mutation. A Drive client installed under a long local sync-root path can have a lower effective path limit.

Every planned upload is opened and rechecked before destructive type replacements, then rechecked on each upload attempt and after transfer. A process that can rewrite the source concurrently can still create an unavoidable scan-to-open race; use a quiescent, trusted source tree for mirror runs.

File Station CIFS/NFS/ISO/remote mounts are never traversed. A destination at or below a reported mount point is rejected, and a mount encountered inside the destination is preserved even with `--delete`.

## Change detection

The default `--compare metadata` considers a file unchanged when byte length and modification time match. File Station lists modification time in Unix seconds but accepts upload time in Unix milliseconds, so local milliseconds are truncated for comparison and preserved on upload.

`--compare size-only` ignores time. It can be useful when another service continually rewrites remote timestamps, but it can miss same-size content changes.

Metadata comparison can also miss a deliberately changed file whose size and second-level timestamp were both preserved. A future strict mode can use File Station's asynchronous MD5 API; MD5 would be an equality mechanism here, not a security hash.

## CLI

```text
Usage: synology-drive-sync [OPTIONS] --url <URL> --username <USERNAME> <SOURCE> <REMOTE>

Arguments:
  <SOURCE>  Authoritative local directory. It is never modified
  <REMOTE>  File Station path beginning with a shared folder

Important options:
      --dry-run
      --delete
      --max-delete <N>             [default: 100]
      --allow-empty-source
      --compare metadata|size-only [default: metadata]
      --jobs <N>                   [default: 2; accepted: 1..16]
      --exclude <PATTERN>
      --password-stdin
      --ca-certificate <PEM>
      --timeout <SECONDS>          [default: 7200]
  -v, --verbose
```

Run `synology-drive-sync --help` for the complete current list.

## Failure behavior

DSM frequently returns HTTP 200 with a JSON API error. The CLI checks both layers and translates common auth, permission, quota, no-space, illegal-path, session, upload, and reverse-proxy failures.

Useful diagnostics include:

- HTML instead of JSON: `/webapi/*` is routed to the UI or another service;
- HTTP 413: increase the proxy request-body limit;
- HTTP 502: fix the proxy's File Station backend route;
- HTTP 504 or File Station `1801`: increase proxy/File Station upload timeouts;
- DSM `150`: login and later requests appear to come from different source IPs;
- DSM `1800`: multipart `Content-Length` is absent or mismatched.

Transient transport, busy, 408/429, and 502/503/504 failures are retried with bounded backoff. Upload retry restarts the whole file because File Station documents no resumable upload protocol. `overwrite=true` makes retry converge on the same remote content. A source file that changes during an upload fails the run before mirror deletions.

## Deliberate limitations

- One direction only: local to remote.
- No block-level delta or resumable upload.
- No claim of crash-atomic overwrite; File Station does not document it.
- Type replacement under `--delete` is non-atomic: after source preflight, a later network, quota, or upload failure can leave the conflicting remote entry absent. The run still stops before remote-only deletion.
- File content, names, hierarchy, and file mtime only.
- No ACL, owner, POSIX mode, xattr, hard-link, sparse-file, or directory-mtime preservation.
- No Drive conflict-resolution or client identity semantics.
- Drive indexing may lag behind a successful File Station upload.
- A Drive-locked file can still be changed through File Station, per Synology's documented behavior.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

Tests cover path containment and traversal rejection, reverse-proxy prefix joining, API discovery, DSM 7 object-shaped OTP challenges, SID/SynoToken placement, pagination, mounted-filesystem boundaries, multipart `Content-Length`, binary-part ordering, Drive-name and ignore protection, planning, type conflicts, deletion limits, real dry-run execution, upload preflight ordering, and failure-before-delete behavior.

## Official references

- [Synology File Station API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Package/FileStation/All/enu/Synology_File_Station_API_Guide.pdf)
- [DSM Login Web API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Os/DSM/All/enu/DSM_Login_Web_API_Guide_enu.pdf)
- [DSM reverse proxy settings](https://kb.synology.com/en-global/DSM/help/DSM/AdminCenter/system_login_portal_advanced?version=7)
- [DSM Login Portal applications](https://kb.synology.com/en-global/DSM/help/DSM/AdminCenter/system_login_portal_applications?version=7)
- [DSM two-factor authentication](https://kb.synology.com/en-global/DSM/help/DSM/SecureSignIn/2factor_authentication?version=7)
- [Synology Drive Admin Console and Team Folders](https://kb.synology.com/en-global/DSM/help/SynologyDrive/drive_admin_console)
- [Synology Drive file-name, path, and attribute limits](https://kb.synology.com/en-uk/DSM/tutorial/Why_are_files_not_synced_between_Synology_Drive_and_Drive_desktop_application)

## License

MIT
