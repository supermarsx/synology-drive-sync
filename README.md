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

The password is read from the OS credential vault when one has been enrolled, otherwise it is prompted with terminal echo disabled. If DSM requires two-factor authentication, the CLI can generate a current code from an explicitly enrolled vault seed or prompt for a one-time code.

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

Passwords, TOTP seeds, and one-time codes are never accepted as command-line values, so they do not appear in process listings. Vault writes are always explicit: a normal sync never saves a prompted or environment-provided secret.

### OS credential vault

Enroll the password for one reverse-proxy URL and DSM username:

```bash
synology-drive-sync credentials set-password \
  --url https://files.example.com \
  --username mirror-bot
```

Interactive input is masked and confirmed. For a pipe or secret-provider integration, pass `--password-stdin`; the first line is stored without a second read. `SDSYNC_PASSWORD` is also accepted by `set-password`, but a command-line password value deliberately is not.

If the DSM account uses authenticator-app TOTP, capture the manual key during DSM's 2FA setup or re-enrollment (the wizard's **Can't scan it** path), or use the original `otpauth://totp` provisioning URI. The CLI cannot recover a seed that DSM no longer displays:

```bash
synology-drive-sync credentials set-totp \
  --url https://files.example.com \
  --username mirror-bot
```

The input is masked and can instead come from the first line of standard input with `--secret-stdin`. The command imports an existing DSM seed; it does not create or enroll a new factor. Base32 keys may contain spaces or hyphens. Provisioning URIs must describe SHA-1, six-digit, 30-second TOTP. Only a canonical unpadded Base32 seed is stored, and generated six-digit codes are never persisted.

Inspect presence without revealing values, rotate by rerunning either `set` command, or remove entries independently:

```bash
synology-drive-sync credentials status --url https://files.example.com --username mirror-bot
synology-drive-sync credentials remove --url https://files.example.com --username mirror-bot password
synology-drive-sync credentials remove --url https://files.example.com --username mirror-bot totp
synology-drive-sync credentials remove --url https://files.example.com --username mirror-bot all
```

The removal target is required; the command never assumes `all`.

The profile key is derived from the normalized reverse-proxy URL and exact DSM username. The remote folder is intentionally excluded, so one account works across destinations. A path-prefixed proxy URL and a host-only URL are different profiles; trailing-slash variants are the same.

The native backends are:

- Windows Credential Manager, with current-user local-machine persistence;
- macOS login Keychain;
- freedesktop Secret Service on Linux, using a provider such as GNOME Keyring, KWallet, or KeePassXC.

On headless Linux, Secret Service needs a usable user-session D-Bus, an unlocked default collection, and the same OS user that enrolled the entries. Cron, containers, SSH-only sessions, and system services frequently lack that environment. In those cases use a deliberately configured stdin/environment source or `--no-vault`; the program never falls back to a plaintext credential file and never tries to start or unlock a vault daemon.

An OS vault protects secrets at rest, but software running as the same unlocked OS user may still retrieve them. Storing both the password and TOTP seed enables unattended login, but it also places both factors behind that one OS account. For stronger factor separation, store only the password and keep the current TOTP code interactive or provide `SDSYNC_OTP` for a single run.

### Resolution order

Credential precedence during sync is:

Password:

1. first line of standard input with `--password-stdin`;
2. `SDSYNC_PASSWORD`;
3. the OS credential vault;
4. masked terminal prompt.

OTP:

1. `SDSYNC_OTP`, containing a current six-digit code, not a seed;
2. a code generated just in time from the explicitly stored vault seed;
3. a masked terminal prompt when DSM reports that OTP is required.

Explicit stdin/environment sources bypass the corresponding vault lookup. `--no-vault` disables both password and TOTP-seed reads for that sync. If a vault-generated code is rejected, synchronize the client and NAS clocks, verify the enrolled seed, or provide one fresh code interactively; the CLI does not guess adjacent time windows.

For a deliberately environment-driven unattended run:

```bash
export SDSYNC_PASSWORD='use-a-secret-provider-in-production'
export SDSYNC_OTP='123456'
synology-drive-sync /srv/export /team/export
unset SDSYNC_PASSWORD SDSYNC_OTP
```

Prefer the OS vault or inject these variables from the scheduler's secret store instead of saving them in a script. `SDSYNC_OTP` expires every 30 seconds, so the stored TOTP seed is the practical unattended option when its factor-separation trade-off is acceptable.

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
       synology-drive-sync <COMMAND>

Commands:
  credentials  Store, inspect, or remove credentials in the current user's OS vault

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
      --no-vault
      --ca-certificate <PEM>
      --timeout <SECONDS>          [default: 7200]
  -v, --verbose
```

Run `synology-drive-sync --help` or `synology-drive-sync credentials --help` for the complete current lists.

`credentials` and `help` are reserved command names. Prefix a same-named relative source with `./` (or `.\` on Windows), or place positional paths after `--`.

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

Tests cover path containment and traversal rejection, reverse-proxy prefix joining, API discovery, DSM 7 object-shaped OTP challenges, lazy vault-TOTP challenge handling, RFC 6238 vectors, 80-bit DSM-seed compatibility, provisioning-input redaction, vault profile scoping, credential CLI parsing, SID/SynoToken placement, pagination, mounted-filesystem boundaries, multipart `Content-Length`, binary-part ordering, Drive-name and ignore protection, planning, type conflicts, deletion limits, real dry-run execution, upload preflight ordering, and failure-before-delete behavior. Tests never read or write the host OS credential vault.

## Official references

- [Synology File Station API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Package/FileStation/All/enu/Synology_File_Station_API_Guide.pdf)
- [DSM Login Web API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Os/DSM/All/enu/DSM_Login_Web_API_Guide_enu.pdf)
- [DSM reverse proxy settings](https://kb.synology.com/en-global/DSM/help/DSM/AdminCenter/system_login_portal_advanced?version=7)
- [DSM Login Portal applications](https://kb.synology.com/en-global/DSM/help/DSM/AdminCenter/system_login_portal_applications?version=7)
- [DSM two-factor authentication](https://kb.synology.com/en-global/DSM/help/DSM/SecureSignIn/2factor_authentication?version=7)
- [RFC 6238: Time-Based One-Time Password Algorithm](https://www.rfc-editor.org/rfc/rfc6238.html)
- [Keyring platform backends](https://docs.rs/keyring/latest/keyring/v1/)
- [freedesktop Secret Service specification](https://specifications.freedesktop.org/secret-service/latest-single/)
- [Synology Drive Admin Console and Team Folders](https://kb.synology.com/en-global/DSM/help/SynologyDrive/drive_admin_console)
- [Synology Drive file-name, path, and attribute limits](https://kb.synology.com/en-uk/DSM/tutorial/Why_are_files_not_synced_between_Synology_Drive_and_Drive_desktop_application)

## License

MIT
