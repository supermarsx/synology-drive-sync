# synology-drive-sync

[![CI](https://github.com/supermarsx/synology-drive-sync/actions/workflows/ci.yml/badge.svg)](https://github.com/supermarsx/synology-drive-sync/actions/workflows/ci.yml)

`synology-drive-sync` is a finite, one-way synchronization engine. It sends an ordinary readable
folder to a chosen Synology File Station path over HTTPS, verifies the result, logs out, and exits.
The authoritative source is opened read-only and is never intentionally changed.

```text
local folder, mounted share, or NAS folder
                    |
                    | scan -> compare -> plan -> transfer -> verify
                    v
          HTTPS File Station WebAPI
                    |
                    v
       chosen File Station logical path
```

It is not a private Synology Drive protocol client. It writes through File Station; Synology Drive
can index the result when the destination belongs to the remote account's My Drive or an enabled
Team Folder.

**Documentation:** [install, configure, operate, integrate, and verify releases](https://supermarsx.github.io/synology-drive-sync/)

## What it does

Each run scans both sides, creates missing directories, uploads missing or changed files, preserves
empty directories, verifies transfers, and finishes with a fresh plan. Remote-only entries are
preserved by default. Content correspondence is rebuilt from live state on every run.

The destination is a File Station logical path such as `/home/Drive/NAS-A Backup` or
`/TeamShare/Project`, never a DSM filesystem path such as `/volume1/...`. DSM must already provide
the account, File Station access, user-home or shared-folder root, and permissions. The sync may
create descendants beneath an existing writable parent, but it cannot create DSM users or shares,
enable User Home or Team Folders, or change ACLs.

## Choose how to run it

| Surface | Best fit | Runtime model |
| --- | --- | --- |
| Native CLI | A source on Windows, macOS, Linux, or an OS-mounted network share | One explicit finite process; run manually or with the native scheduler |
| DSM 7 package and dashboard | The authoritative source is physically available to a Synology NAS | Non-root package service, graphical profiles and diagnostics, and interval/daily/realtime routines |
| Docker / Compose | A portable, isolated unattended job | Non-root finite container with a read-only source mount |
| Rust SDK or C ABI | Another application needs to embed the engine | Synchronous library call with caller-owned secrets, cancellation, and process policy |

The core binary is not a resident watcher. In **realtime** routine mode, the DSM package controller
can watch with `inotify` or fall back to polling, then launch a finite plan or sync after debounce.
Interval and daily routines also launch finite runs. The dashboard and the
`sdsync-dsm` recovery/automation manager operate the same package-owned profiles, credentials,
routines, state, and logs.

## Safety first

> [!WARNING]
> Synchronization is not backup. Even additive sync may replace a same-path remote file when the
> local file changed. Keep independent, tested recovery that this tool cannot overwrite.

> [!CAUTION]
> Mirror deletion can be enabled by the effective CLI, environment, or profile configuration and
> must also pass every applicable batch, scheduler, or DSM authorization layer. Deletion and remote
> type replacement are guarded, but they are not transactional or crash-atomic. Review `plan`, use
> deliberately small caps, keep both trees quiescent, and test recovery before enabling deletion.

> [!IMPORTANT]
> Automated tests use local filesystems and mock HTTP services; they do not log in to a live NAS.
> Before trusting production data, complete the
> [disposable live-NAS acceptance and recovery runbook](https://supermarsx.github.io/synology-drive-sync/production-acceptance.html)
> against the exact NAS, DSM build, reverse proxy, account, source, and scheduler identity you will
> use.

## Operations and comparison modes

| Operation | Remote effect | Use it for |
| --- | --- | --- |
| `doctor source [SOURCE] [--hash]` | None | Validate readability, names, exclusions, and optionally every payload fingerprint |
| `doctor target [REMOTE]` | None | Validate TLS, routing, authentication, permissions, and inventory |
| `doctor target [REMOTE] --write-test` | Disposable probe and cleanup only | Exercise live create, upload, copy, verify, and cleanup behavior |
| `plan` | None | Review the exact pending work; `--exit-code` returns `10` when changes exist |
| `sync` | Creates and updates | Safe default; remote-only entries remain |
| `sync` with effective deletion enabled | Creates, updates, and guarded removals | Deliberate one-way mirror after separate recovery testing |

Comparison is independent of the operation:

| Mode | A same-path file matches when | Trade-off |
| --- | --- | --- |
| `content` | Size, MD5, IEEE CRC32, SHA-256, and one-second mtime match | Safest and default; streams selected remote bytes and verifies uploads |
| `metadata` | Size and one-second mtime match | Faster, but can miss equal-size/equal-time content changes |
| `size-only` | Size matches | Fastest and weakest; no content verification |

Use `--no-delete` to defeat deletion selected by a profile or environment for one invocation. A
mirror additionally enforces destination containment, per-profile and aggregate deletion caps,
empty-source protection, managed-path and remote-mount boundaries, fresh remote snapshots, and
failure-before-delete ordering.

## Five-step quick start

1. **Install a verified release.** Download and inspect the supplied installer. It selects the
   native archive, verifies `SHA256SUMS`, checks the embedded version, and installs the executable.
   Windows, manual archives, DSM packages, containers, and provenance are covered in the
   [installation guide](https://supermarsx.github.io/synology-drive-sync/installation.html).

   ```bash
   curl --proto '=https' --tlsv1.2 -fL \
     https://github.com/supermarsx/synology-drive-sync/releases/latest/download/install.sh \
     -o install.sh
   less install.sh
   sh install.sh
   ```

2. **Prepare DSM.** Use a dedicated non-administrator account with File Station application
   permission and read/write access to the destination. Route the configured HTTPS origin to
   `/webapi/entry.cgi`; do not use a browser alias or physical `/volumeN` path.

3. **Store the password outside command history and TOML.** The native CLI can use the current
   user's Windows Credential Manager, macOS Keychain, or Linux Secret Service vault.

   ```bash
   synology-drive-sync credentials set-password \
     --url https://files.example.com --username mirror-bot
   ```

4. **Diagnose and plan without changing the destination.** Add `--hash` to the source diagnostic
   when you want every local payload read and fingerprinted.

   ```bash
   synology-drive-sync doctor source ./project --hash
   synology-drive-sync doctor --url https://files.example.com --username mirror-bot \
     target /TeamShare/project
   synology-drive-sync plan ./project /TeamShare/project \
     --url https://files.example.com --username mirror-bot
   ```

5. **Run the additive sync.** Keep deletion disabled through initial acceptance.

   ```bash
   synology-drive-sync sync ./project /TeamShare/project \
     --url https://files.example.com --username mirror-bot
   ```

## Profiles, batches, and unattended runs

Named TOML profiles keep a complete source, endpoint, account, destination, and non-secret policy.
CLI values override environment values, which override the selected profile and built-in defaults.
The schema accepts protected secret-file paths but never password, TOTP seed, current OTP, or bearer
token values.

```bash
synology-drive-sync config init
synology-drive-sync plan --profiles photos,documents --output json
synology-drive-sync sync --all-profiles --max-total-delete 20 --output ndjson
```

A batch preflights every source and target before its first mutation, then runs profiles in name
order. Completed earlier jobs are not rolled back when a later job fails.

For unattended operation, choose systemd, cron, LaunchAgent, Windows Task Scheduler,
Docker/Compose, or DSM routines. Validate source and secret access under the eventual identity,
prevent overlap, and allow enough time for the complete scan-transfer-verify workload.

## Requirements and deliberate limitations

- The one configured endpoint must use HTTPS by default and route the File Station WebAPI. Use a
  private CA certificate when needed; insecure HTTP or certificate bypasses are for controlled
  testing only. QuickConnect is not supported—use LAN, DDNS, VPN, or a tested reverse proxy.
- Passwords and supported authenticator-app TOTP can come from an OS vault, protected files,
  standard input, prompts, or dedicated environment variables. DSM Secure SignIn approval and
  hardware/security-key challenges are not supported by the documented File Station login flow.
- Sources may be local directories, mounted SMB/CIFS/NFS folders, mapped-drive subdirectories, or
  ordinary Windows UNC share roots such as `\\nas\media`. Filesystem roots (`/`, `C:\`), mapped-drive
  roots such as `Z:\`, and Windows administrative shares such as `C$`, `ADMIN$`, and `IPC$` are
  rejected. The operating system remains responsible for mounting and source-share authentication.
- Symlinks, junctions/reparse points, special or unreadable entries, unsafe names, and case
  collisions fail closed. Keep the source quiescent and run as an unprivileged identity that owns
  or exclusively controls it.
- There is no two-way reconciliation, Drive conflict protocol, block-level delta, resumable upload,
  transactional multi-profile rollback, or crash-atomic overwrite/type replacement.
- File content, hierarchy, names, and file mtime are in scope. ACLs, ownership, modes, xattrs, hard
  links, sparse layout, and directory mtime are not preserved. Different URLs that reach the same
  NAS are aliases the client cannot identify.

## Releases, integration, and further documentation

Calendar releases use `YY.N` tags. They publish native CLI archives for Linux, Windows, and macOS on
x86-64 and ARM64; DSM 7 SPKs for `x86_64`, `armv8`, ARMv7, and Evansport `i686`; Linux container
images for AMD64 and ARM64; and matching Rust and C SDK material. Verify the selected payload against
`SHA256SUMS` and, when publisher provenance matters, its GitHub artifact attestation.

> [!WARNING]
> Never install the immutable DSM SPKs from releases **26.5**, **26.6**, or **26.20**. Use the
> [release selector](https://supermarsx.github.io/synology-drive-sync/release-selector.html) for the
> exact NAS model, DSM build, and CPU, then verify the selected SPK and checksum from the same release.

Rust applications can pin the high-level synchronous `synology_drive_sync::sdk::Engine` to an exact
verified release tag. Non-Rust applications use the versioned JSON-over-C ABI only with the header
and DLL, `.so`, or `.dylib` from the same release SDK. See the
[Rust SDK guide](https://supermarsx.github.io/synology-drive-sync/sdk/index.html) and
[C ABI guide](https://supermarsx.github.io/synology-drive-sync/ffi/index.html).

| Task | Guide |
| --- | --- |
| Understand the safety model and first run | [Overview](https://supermarsx.github.io/synology-drive-sync/getting-started/overview.html) · [Quick start](https://supermarsx.github.io/synology-drive-sync/getting-started/quick-start.html) |
| Configure profiles, secrets, TLS, comparison, and deletion | [Configuration](https://supermarsx.github.io/synology-drive-sync/configuration/index.html) |
| Install and operate the DSM dashboard | [Synology package](https://supermarsx.github.io/synology-drive-sync/synology-package.html) |
| Schedule and monitor unattended jobs | [Scheduling](https://supermarsx.github.io/synology-drive-sync/operations/scheduling.html) · [Observability](https://supermarsx.github.io/synology-drive-sync/observability.html) |
| Diagnose failures and complete live acceptance | [Troubleshooting](https://supermarsx.github.io/synology-drive-sync/operations/troubleshooting.html) · [Acceptance runbook](https://supermarsx.github.io/synology-drive-sync/production-acceptance.html) |
| Inspect commands and release evidence | [CLI reference](https://supermarsx.github.io/synology-drive-sync/reference/cli.html) · [Release verification](https://supermarsx.github.io/synology-drive-sync/releases.html) |

### Development

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets
cargo build --release --locked -p synology-drive-sync
cargo build --profile ffi-release --locked -p synology-drive-sync-ffi
```

See [testing](https://supermarsx.github.io/synology-drive-sync/testing.html),
[security](https://supermarsx.github.io/synology-drive-sync/security.html), and
[contributing](https://supermarsx.github.io/synology-drive-sync/contributing.html). The project is
[MIT licensed](https://supermarsx.github.io/synology-drive-sync/legal.html).
