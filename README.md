# synology-drive-sync

[![CI](https://github.com/supermarsx/synology-drive-sync/actions/workflows/ci.yml/badge.svg)](https://github.com/supermarsx/synology-drive-sync/actions/workflows/ci.yml)

A lean Rust CLI that pushes one local directory—or a deterministic batch of complete named profile jobs—to Synology Drive-backed folders through the documented File Station WebAPI and HTTPS reverse-proxy URLs.

The sync engine is deliberately one-way and stateless. It is not a Synology Drive protocol client,
continuous watcher, two-way reconciler, SMB/WebDAV wrapper, or QuickConnect client. It writes to the
underlying DSM folder through File Station; Synology Drive can index that folder when it belongs to
My Drive or an enabled Team Folder. Service managers, including the DSM package controller, only
schedule isolated finite runs.

An architecture-specific DSM 7 `.spk` can run the same engine directly on the source NAS. Its
remote destination is configurable: `/home/Drive/...` targets the remote account's Drive home, and
any writable `/<shared-folder>/...` subdirectory can be selected instead. DSM must provision the
remote user home or shared-folder root and its permissions first; the sync creates a missing chosen
subdirectory and all descendants beneath an existing writable parent. See the
[Synology DSM package guide](docs/synology-package.md).

> [!IMPORTANT]
> The automated suite uses deterministic local and mock-HTTP tests; it does not log in to a live NAS. Before trusting a deployment, run the [source and target diagnostics](docs/diagnostics-and-batch.md), review `plan`, complete the [disposable live-NAS acceptance](docs/production-acceptance.md), and keep `--delete` disabled.

## Safety contract

- The local source is authoritative and is never modified.
- Missing and changed local files are uploaded; empty local directories are created.
- Remote-only content is preserved unless `--delete` is explicit.
- The default content comparison hashes every local payload and the remote files needed for comparison, deletion guards, or safe reuse on every run; it verifies uploads after transfer and can reuse a unique matching remote file with a non-overwriting server-side copy when that is safer than uploading it again.
- A normal sync stops on file/directory type conflicts rather than removing them.
- `plan` performs discovery, authentication, scanning, and planning without remote mutation.
- `doctor source` validates one or more local trees without contacting DSM; `doctor target` is non-mutating unless its explicit disposable `--write-test` is selected.
- A mutating profile batch plans and preflights every selected source and target before its first remote mutation, then executes jobs sequentially in deterministic profile-name order.
- Mirror deletion is guarded by path containment, an explicit deletion cap, empty-source protection, protected-path handling, fresh remote snapshot checks, and failure-before-delete ordering.

| Command | Creates/uploads | Deletes remote-only data | Intended use |
| --- | ---: | ---: | --- |
| `doctor source [SOURCE] [--hash]` | No | No | Validate a local tree, optionally reading and hashing every file |
| `doctor target [REMOTE]` | No | No | Validate routing, authentication, permission, and inventory |
| `doctor target [REMOTE] --write-test` | Disposable probe only | Probe cleanup only | Live create/upload/copy/verify/cleanup acceptance |
| `plan` | No | No | Inspect the exact pending work |
| `sync` | Yes | No | Safe additive/update-only push |
| `sync --delete` | Yes | Yes, within guards | Deliberate exact remote mirror |

## Quick start

Download a verified release with the platform installer described in [Installation](docs/installation.md), or build with Rust 1.88 or newer:

```bash
git clone https://github.com/supermarsx/synology-drive-sync.git
cd synology-drive-sync
cargo build --release --locked
```

Validate the local source with exactly the exclusions used by sync. Add `--hash` to read every payload file and require a stable MD5 snapshot:

```bash
synology-drive-sync doctor source ./project --hash
```

Use a dedicated, non-administrator DSM account with File Station application permission and read/write access to the destination shared folder. Then verify the proxy without authenticating:

```bash
synology-drive-sync doctor \
  --url https://files.example.com \
  --routing-only
```

Enroll the DSM password in the current user's OS vault:

```bash
synology-drive-sync credentials set-password \
  --url https://files.example.com \
  --username mirror-bot
```

If the account uses authenticator-app TOTP, import the existing DSM manual key or `otpauth://` URI as well:

```bash
synology-drive-sync credentials set-totp \
  --url https://files.example.com \
  --username mirror-bot
```

Authenticate and inspect the exact destination without changing it:

```bash
synology-drive-sync doctor \
  --url https://files.example.com \
  --username mirror-bot \
  target /team-folder/project
```

Review the plan, then run the additive sync:

```bash
synology-drive-sync plan ./project /team-folder/project \
  --url https://files.example.com \
  --username mirror-bot

synology-drive-sync sync ./project /team-folder/project \
  --url https://files.example.com \
  --username mirror-bot
```

`/team-folder/project` is a File Station logical path beginning with a shared folder. Never pass a physical path such as `/volume1/team-folder/project`.

PowerShell uses the same command tree:

```powershell
$env:SDSYNC_URL = 'https://files.example.com'
$env:SDSYNC_USERNAME = 'mirror-bot'
synology-drive-sync.exe plan 'C:\Data\Project' '/team-folder/project'
synology-drive-sync.exe sync 'C:\Data\Project' '/team-folder/project'
```

## Reverse-proxy requirements

The smallest recommended topology is a dedicated public hostname routed to File Station's customized HTTPS port:

```text
https://files.example.com:443
        |
        | DSM reverse proxy
        v
https://nas.lan:7001       File Station customized HTTPS port
```

On DSM 7:

1. In **Control Panel > Login Portal > Applications**, give File Station a customized HTTPS port.
2. Create a host-based reverse-proxy rule from the public HTTPS hostname to that port.
3. Assign a certificate valid for the public hostname.
4. Set request-body limits and proxy send/read timeouts high enough for the largest file.
5. Confirm the same public origin routes `/webapi/entry.cgi`; routing only the browser UI is insufficient.

No WebSocket upgrade is required. Discovery, authentication, listing, creation, upload, and deletion are ordinary HTTP requests. HTTPS is mandatory by default; `--allow-http` exists only for controlled LAN testing. Prefer `--ca-certificate` for private PKI instead of disabling certificate validation.

Every DSM endpoint is derived from the one configured public base URL. The CLI does not probe ports 5000/5001, discover a LAN address, bypass the proxy, or fall back to another transport.

An optional public prefix is supported, for example `https://gateway.example.com/nas/`. The proxy must rewrite `/nas/webapi/*` to the backend's `/webapi/*`. A dedicated hostname without a prefix is simpler, and a File Station browser alias is not a substitute for routing the WebAPI.

You can probe the raw route before installing the CLI:

```bash
curl -fsS -X POST https://files.example.com/webapi/entry.cgi \
  --data-urlencode api=SYNO.API.Info \
  --data-urlencode version=1 \
  --data-urlencode method=query \
  --data-urlencode query=SYNO.API.Auth,SYNO.FileStation.List
```

The response should be JSON with `"success": true`, not HTML or a redirect to a login page. `doctor --routing-only` performs the corresponding TLS, routing, and API-discovery checks. `doctor target REMOTE` also authenticates, inventories the destination, and uses File Station's non-mutating permission check for the exact logical destination, or the first missing component under its nearest existing ancestor. See [Diagnostics and multi-profile batches](docs/diagnostics-and-batch.md) before using the mutating disposable `target --write-test`.

## Commands

The explicit command tree is preferred:

| Command | Purpose |
| --- | --- |
| `sync SOURCE REMOTE` | Apply a one-way push |
| `plan SOURCE REMOTE` | Print the pending work without mutation; `--exit-code` returns 10 when changes exist |
| `doctor source [SOURCE] [--hash]` | Validate a local source without DSM access, optionally hashing every file |
| `doctor target [REMOTE] [--write-test]` | Diagnose a File Station destination; write only with the explicit disposable probe |
| `doctor --routing-only` | Validate TLS, proxy routing, and API discovery without authentication |
| `config path\|validate\|show` | Locate, validate, or inspect non-secret effective configuration |
| `credentials set-password\|set-totp\|status\|remove` | Manage the current user's OS-vault entries |
| `completions SHELL` | Generate Bash, Zsh, Fish, PowerShell, or Elvish completion source |
| `manpage [--all DIRECTORY]` | Generate the root roff page on stdout, or every nested command page in a directory |

The former positional spelling remains compatible and is interpreted as `sync`:

```bash
synology-drive-sync ./project /team-folder/project \
  --url https://files.example.com \
  --username mirror-bot
```

Legacy `--dry-run` is equivalent to planning, but new scripts should use the explicit `plan` command. Use `synology-drive-sync --help` and `<command> --help` for the complete current interface.

## Configuration and profiles

Copy [config.example.toml](config.example.toml) and ask the CLI for the platform-specific default location:

```bash
synology-drive-sync config path
synology-drive-sync config validate --config ./config.toml
synology-drive-sync config show --config ./config.toml --profile production
```

Default locations are:

- Linux: `$XDG_CONFIG_HOME/synology-drive-sync/config.toml`, or `~/.config/synology-drive-sync/config.toml`;
- macOS: `~/Library/Application Support/synology-drive-sync/config.toml`;
- Windows: `%APPDATA%\synology-drive-sync\config.toml`.

Resolution is deterministic:

1. command-line value;
2. the matching `SDSYNC_*` environment value parsed by the CLI;
3. selected profile (`--profile`, `SDSYNC_PROFILE`, then `default-profile`);
4. built-in default.

Command-line exclusion rules are appended to profile exclusions. `--no-delete` can disable a profile's `delete=true`, `--vault` can override a profile's `no-vault=true`, and `--no-quiet` can re-enable terminal diagnostics over `quiet=true`. Relative paths in a profile are anchored to the configuration file's directory.

Select complete profile jobs with `--profiles NAME[,NAME...]` or every named profile with
`--all-profiles`. Batch sync/plan takes SOURCE and REMOTE from each profile, rejects equal or nested
roots on one normalized endpoint, preflights every job before mutation, and executes sequentially in
deterministic profile-name order. Every job keeps its own `max-delete`; `--max-total-delete` adds an
aggregate cap (default 100). For examples, partial-failure semantics, URL-alias limits, and scheduler
locking, see [Diagnostics and multi-profile batches](docs/diagnostics-and-batch.md).

Batch plan/sync and target diagnostics reject `--password-stdin`; use the OS vault or protected
per-profile password/TOTP files so each job resolves its own credentials.

```bash
synology-drive-sync plan --config ./config.toml \
  --profiles photos,documents --max-total-delete 20 --output json
synology-drive-sync sync --config ./config.toml \
  --all-profiles --max-total-delete 20 --output ndjson
```

The TOML schema is strict and non-secret. It accepts protected `password-file`, `totp-secret-file`, and remote-log token-file paths, but has no password, seed, current-code, or bearer-token value field. Unknown keys are rejected, and `config show` can display paths or environment-variable names but never secret values.

## Password and two-factor authentication

Passwords, TOTP seeds, and current OTP codes have no secret-valued command-line option, keeping values out of process listings. A normal sync never stores credentials: vault writes happen only through an explicit `credentials set-*` command.

Native vault backends are Windows Credential Manager, the macOS login Keychain, and freedesktop Secret Service on Linux. Each profile is scoped to the normalized reverse-proxy URL and exact DSM username; the remote destination is intentionally excluded.

Useful vault operations are:

```bash
synology-drive-sync credentials status \
  --url https://files.example.com --username mirror-bot
synology-drive-sync credentials remove \
  --url https://files.example.com --username mirror-bot password
synology-drive-sync credentials remove \
  --url https://files.example.com --username mirror-bot totp
synology-drive-sync credentials remove \
  --url https://files.example.com --username mirror-bot all
```

The removal kind is required; the CLI never assumes `all`.

For DSM TOTP, import the manual Base32 key or original provisioning URI shown during DSM enrollment. The CLI does not enroll a new factor and cannot recover a seed DSM no longer displays. Supported provisioning data is SHA-1, six digits, and a 30-second period. Codes are generated only when DSM challenges for OTP, are refreshed once across a time-step boundary, and are never persisted.

Effective password resolution is:

1. `--password-stdin`, when selected;
2. `--password-file`, `SDSYNC_PASSWORD_FILE`, or the profile's protected file path;
3. `SDSYNC_PASSWORD`;
4. the OS vault;
5. a masked terminal prompt.

Effective OTP resolution is:

1. `SDSYNC_OTP`, containing one current six-digit code;
2. a code generated from `--totp-secret-file`, `SDSYNC_TOTP_SECRET_FILE`, or the profile's seed-file path;
3. a code generated from the OS-vault seed;
4. a masked current-code prompt after DSM requests OTP.

`--no-vault` disables both vault reads; `--vault` re-enables them over a profile default. Protect referenced files with OS permissions and store one secret on the first line. For unattended use, prefer the OS vault in a real user session or scheduler-native credentials. `SDSYNC_OTP` is an ephemeral fallback, not seed storage.

Headless Linux services and containers usually do not have an unlocked Secret Service session. The supplied systemd, cron, and Compose examples therefore use protected secret-file mounts with `--no-vault`. Storing both password and TOTP seed in one vault enables unattended login but reduces factor separation to the security of that OS account.

DSM Secure SignIn approval and hardware/security-key challenges have no documented File Station WebAPI flow and are not supported. Configure an app-compatible TOTP factor for this account.

## Planning, deletion, and exclusions

Exact mirror mode is deliberately noisy:

```bash
synology-drive-sync plan ./project /team-folder/project \
  --delete --max-delete 25 \
  --url https://files.example.com --username mirror-bot

synology-drive-sync sync ./project /team-folder/project \
  --delete --max-delete 25 \
  --url https://files.example.com --username mirror-bot
```

Independent guards include:

- `/` can never be a destination, and every deletion must remain a strict child of the configured root;
- the default maximum is 100 deletions and `--max-delete` changes it explicitly;
- a selected profile batch also refuses a combined count above `--max-total-delete` (default 100), after checking every per-job cap;
- a source with no payload files cannot drive deletion without `--allow-empty-source`;
- ignored, DSM-managed, and File Station-mounted paths are protected;
- the local preflight, remote scan, directory creation, copies, and uploads must succeed before remote-only deletion;
- immediately before a planned file deletion, the current remote kind, size, mtime, and—under content comparison—MD5 must still match the inventoried snapshot;
- a copied source is removed only after the local snapshot and copied destination are reverified;
- remote-only directories are removed deepest-first and non-recursively, so a concurrent new child prevents their removal.

Type replacement under `--delete` is not transactionally atomic: a conflicting remote entry may be removed before its local replacement upload, and a later failure can leave it absent. File Station provides no atomic compare-and-delete primitive, so a remote writer can still race the final check and deletion. Use `plan`, an intentionally small deletion cap, quiescent single-writer source and destination trees, and a tested DSM snapshot/versioning/recycle recovery layer.

Place gitignore-style rules in `.sdsyncignore` at the source root or repeat `--exclude`:

```gitignore
target/
*.tmp
.cache/
```

Excluded paths are outside the sync scope, not considered absent. Matching remote entries and required parent directories are preserved even under `--delete`. The root `.sdsyncignore` itself is never uploaded.

Hidden regular files are included. Symlinks, junctions/reparse points, special or unreadable entries, non-UTF-8 names, unsafe Drive names, case collisions, and obvious platform path overflows fail preflight before remote mutation. The selected remote prefix is included in Drive portability and path-length checks, and case variants across the local and remote hierarchies fail before File Station can create both spellings. File Station CIFS/NFS/ISO/remote mounts are never traversed or deleted.

Path checks and later file opens are not a transactional filesystem snapshot. Run under an unprivileged account that exclusively owns the source, keep the tree quiescent during synchronization, and never run elevated over a source that another user or less-trusted process can rename or replace. A concurrent path-component swap can otherwise race portable link/reparse checks and redirect a later traversal or upload outside the tree that was originally inspected. See [Security policy](SECURITY.md).

The default `--compare content` requires matching byte length, MD5, and file mtime at File Station's one-second resolution. An upload is successful only after the local file is rehashed and the exact remote destination reports the expected bytes; a final rescan and replan also enforce the expected mtime before success. The correspondence is rebuilt from current local and NAS state on every run; there is no persistent path/hash database that can become stale.

When one missing local file has one unique remote-only counterpart with the same byte length, MD5, second-resolution mtime, and basename in a different directory, the plan can use File Station's non-overwriting server-side copy instead of retransmitting the bytes. Additive sync keeps the old remote path. Mirror mode removes it only after the local snapshot and new copy have been reverified. Ambiguous duplicate-content groups, same-parent renames, basename changes, unavailable copy support, and any unsafe case fall back to a normal verified upload. Inspect `server_copies` and `upload_bytes` in JSON/NDJSON plan output before execution.

MD5 is the strongest content digest exposed by the documented File Station API, but it is not collision-resistant against maliciously constructed files. Use an independently downloaded SHA-256 manifest for production acceptance. Explicit `--compare metadata` also compares length and modification time at File Station's one-second resolution but omits content hashing; `--compare size-only` checks length only. Those performance modes can miss changes and do not provide post-upload content verification.

Missing folders are planned shallowest-first and created before copies/uploads. The client also requests parent creation from File Station, while all generated remote paths remain contained under the configured logical root. This preserves the source hierarchy and empty directories; directory mtimes, ACLs, ownership, modes, xattrs, and other filesystem metadata remain outside the parity contract.

## Output, progress, and logs

Command results and diagnostics are independent streams:

- `--output human|json|ndjson` controls result records on standard output;
- `--log-format human|json` controls secret-free diagnostic events on standard error and optional sinks;
- `-v` selects debug logging, `-vv` trace, while `--log-level` is explicit;
- `--progress auto|always|never` controls progress for human result output; `auto` additionally requires a terminal and human-formatted logs, while machine result modes suppress progress;
- `--quiet` suppresses non-error terminal diagnostics and progress without disabling configured file or remote logs;
- `--log-file` appends local logs;
- `--remote-log-url` sends structured events to an HTTPS collector, with a bearer token read from a protected file or named environment variable;
- `--remote-log-mode best-effort|required` decides whether collector failure can fail the run.

Batch JSON/NDJSON includes deterministic per-profile status and an aggregate summary. Each sync job
separates its non-mutating `preflight_plan` from its fresh `execution_plan` and reports whether
`mutation_authorized` was reached. A job marked `partial` may have mutated before it failed; earlier
successful jobs are not rolled back and later jobs are left `not-run`. The aggregate separately
reports initial `preflight_deletions` and fresh `execution_reserved_deletions`; its
`all_targets_preflighted_before_mutation` field is observed evidence and is false after an
interrupted or failed preflight. See
[Diagnostics and multi-profile batches](docs/diagnostics-and-batch.md) for the execution contract.

For schemas, redaction boundaries, rotation, remote-delivery behavior, and operational examples, see [Observability](docs/observability.md).

## Exit codes

Stable automation behavior is:

- `0`: command completed successfully; for `plan --exit-code`, no changes are pending;
- `10`: `plan --exit-code` found pending changes;
- `2`: command-line usage or configuration error;
- `1`: operational failure, including network, DSM, filesystem, vault, or required-log-delivery failure;
- `130`: cooperative cancellation requested with Ctrl+C/SIGINT or SIGTERM.

Scripts should treat any other nonzero value as failure and should not infer success from human-readable output.

An initial aggregate deletion-cap breach and a breach caused by a fresh execution replan are
operational safety failures with exit `1`, not configuration/usage errors. The initial breach occurs
before any selected mutation. A fresh breach denies mutation for that profile, but earlier completed
profiles remain committed.

## Installation and unattended operation

See [Installation and deployment](docs/installation.md) for:

- manually installable DSM 7 SPKs for `x86_64` and `armv8`, with a package-owned scheduler and
  multi-target manager;
- checksum-verifying Unix and Windows installers;
- manual archive installation, completions, and manpage setup;
- the non-root, read-only Docker/Compose job and optional TOTP secret overlay;
- hardened systemd service/timer and cron fallback;
- per-user macOS LaunchAgent and Windows Task Scheduler setup using the OS vault.

The native schedulers are preferred because their identity and credential-session behavior is clearer. Prefer one scheduled batch for profiles sharing an operational window; otherwise make every related job use one shared host-level lock. The built-in overlap check does not coordinate separate processes or hosts, and a scheduler timeout must cover the complete sequential batch. Never place a password, TOTP seed, current OTP, or logging token value directly in a unit, plist, crontab, task argument, or TOML profile.

## Releases and supply-chain verification

Calendar releases use `YY.N` tags and provide native Linux, Windows, and macOS archives for both x86-64 and ARM64, plus architecture-specific DSM 7 SPKs for `x86_64` and `armv8`. Each release also includes `SHA256SUMS`, a CycloneDX dependency SBOM, generated third-party license notices, installer scripts, and GitHub artifact provenance/SBOM attestations. CI audits the locked graph against current RustSec data and refuses stale notices. The GHCR image is published for `linux/amd64` and `linux/arm64` as both `YY.N` and `latest`.

See [Release artifacts and verification](docs/releases.md) before deploying a binary or container in a sensitive environment. Pin a calendar version or container digest rather than relying on mutable `latest`.

## Failure clues

DSM often returns HTTP 200 with a JSON API error, so the CLI validates both layers. Common proxy symptoms are:

- HTML instead of JSON: `/webapi/*` reached a UI or another service;
- HTTP 413: raise the proxy request-body limit;
- HTTP 502: correct the File Station backend route;
- HTTP 504 or File Station `1801`: raise proxy/File Station upload timeouts;
- DSM `150`: login and later requests appear to originate from different client IPs;
- DSM `1800`: multipart content length is absent or inconsistent.

Transient transport, busy, 408/429, and 502/503/504 failures use bounded retry. File Station exposes no resumable upload protocol. In the default content mode, the client first checks whether a lost/retryable response nevertheless left the exact expected size and MD5 at the destination; it accepts that completed upload, otherwise the retry restarts the entire file. A source file that changes during transfer fails the run before remote-only deletion. Before reporting success, the client rescans and rehashes both sides and requires a fresh plan with no pending in-scope operation.

## Deliberate limitations

- One direction only: local to remote.
- File Station WebAPI only; no private Drive protocol, SMB, WebDAV, SSH, or QuickConnect.
- A mapped or mounted NAS share can be the source only when the operating system exposes it as an ordinary readable directory. The DSM package is intended to run where the source is physically local; File Station has no direct remote-NAS-to-remote-NAS transfer operation. File Station CIFS/NFS/ISO mount points are protected destination boundaries and cannot be sync roots.
- No block-level delta or resumable upload.
- No persistent content index or universal rename operation. Only the explicitly safe server-copy case above avoids retransmission; other rename/duplicate cases use verified upload fallback.
- No claim of crash-atomic overwrite or transactional type replacement.
- No transactional multi-profile batch or rollback: a failed job can follow already completed jobs, and later jobs are then not run.
- Remote overlap detection compares normalized configured URLs; different DNS names or reverse-proxy prefixes that reach the same NAS are aliases the client cannot identify.
- Content, names, hierarchy, and file mtime only; no ACL, owner, mode, xattr, hard-link, sparse-file, or directory-mtime preservation.
- No Drive conflict resolution or client identity semantics; Drive indexing may lag behind File Station writes.
- A Drive-locked file can still be changed through File Station according to Synology's documented behavior.
- Explicit metadata comparison can miss content changed while both size and second-level mtime are preserved; size-only comparison is weaker still.
- The automated suite does not prove compatibility with every DSM/File Station release or reverse-proxy product, and currently contains no live-NAS end-to-end job; `target --write-test` must therefore be exercised against a disposable destination during acceptance.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

See [Testing and coverage](docs/testing.md), [CONTRIBUTING.md](CONTRIBUTING.md), and
[SECURITY.md](SECURITY.md). Tests do not read or write the host OS credential vault.

## Official references

- [Synology File Station API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Package/FileStation/All/enu/Synology_File_Station_API_Guide.pdf)
- [DSM Login WebAPI Guide, including API v6 OTP](https://kb.synology.com/en-my/DG/DSM_Login_Web_API_Guide/3)
- [DSM Login Portal applications](https://kb.synology.com/en-global/DSM/help/DSM/AdminCenter/system_login_portal_applications?version=7)
- [DSM Login Portal and reverse proxy](https://kb.synology.com/en-global/DSM/help/DSM/AdminCenter/system_login_portal?version=7)
- [Synology Drive Admin Console and Team Folders](https://kb.synology.com/en-global/DSM/help/SynologyDrive/drive_admin_console)
- [Synology DSM Package Developer Guide](https://help.synology.com/developer-guide/)
- [RFC 6238: Time-Based One-Time Password Algorithm](https://www.rfc-editor.org/rfc/rfc6238.html)
- [freedesktop Secret Service specification](https://specifications.freedesktop.org/secret-service/latest-single/)

## License

[MIT](LICENSE). Dependency license texts and attributions are in
[THIRD_PARTY_LICENSES.html](THIRD_PARTY_LICENSES.html).
