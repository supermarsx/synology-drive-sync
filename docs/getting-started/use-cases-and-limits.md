# Use cases, operations, and limits

This page holds the reference material that used to sit in the project README: what the tool is
for and where it is the wrong tool, the four ways to run it, the operations and comparison modes,
and the requirements and deliberate limitations.

## What it does

Each run scans both sides, creates missing directories, uploads missing or changed files, preserves
empty directories, verifies transfers, and finishes with a fresh plan. Remote-only entries are
preserved by default. Content correspondence is rebuilt from live state on every run.

The destination is a File Station logical path such as `/home/Drive/NAS-A Backup` or
`/TeamShare/Project`, never a DSM filesystem path such as `/volume1/...`. DSM must already provide
the account, File Station access, user-home or shared-folder root, and permissions. The sync may
create descendants beneath an existing writable parent, but it cannot create DSM users or shares,
enable User Home or Team Folders, or change ACLs.

## What it is for

Each of these assumes one authoritative copy on the source side and a File Station destination the
account already has access to.

- **A workstation publishing finished work to a NAS share.** A Windows, macOS, or Linux machine
  holds the working copy and a scheduled `sync` pushes it to a File Station path, so the NAS always
  carries the current state. The password lives in Windows Credential Manager, macOS Keychain, or
  Linux Secret Service rather than in a script, and Windows Task Scheduler, cron, systemd, or a
  LaunchAgent starts the finite run.
- **One Synology pushing to another.** The DSM 7 package runs on the NAS that holds the
  authoritative folder and sends it over HTTPS to a File Station path on a second Synology.
  Interval and daily routines cover a schedule; a realtime routine watches with the package's own
  `inotify` watcher (DSM ships no `inotifywait`), falls back to polling only when that watcher
  cannot run (and says why), and launches a finite run after a debounce.
- **Getting a folder into Synology Drive without speaking Drive's protocol.** Point the destination
  at a path under the remote account's My Drive or an enabled Team Folder and Drive indexes what
  File Station wrote, so the result reaches Drive clients without this tool implementing anything
  Drive-specific.
- **A pipeline publishing build output.** The container runs non-root with every capability
  dropped, a read-only root filesystem, a read-only bind mount of the source, and the password
  passed as a file rather than an environment variable; it exits when the work is done.
  `--output json` or `--output ndjson` feeds a log collector, and `plan --exit-code` returns `10`
  when changes are pending, which makes it usable as a CI or health check.
- **An application that needs the engine in-process.** Rust callers use the synchronous
  `synology_drive_sync::sdk::Engine`; other languages use the versioned JSON-over-C ABI. Secrets,
  cancellation, and process policy stay with the caller.

### Where it is not the right tool

Synchronisation is not backup. It sends the current state one way and keeps no history of what it
replaced, so a local file that is corrupted or encrypted in place is exactly what the next run
uploads over the good remote copy. With mirror deletion enabled, a local deletion removes the remote
file too. Keep separate, tested recovery; see [Safety first](../configuration/safety.md). It does not reconcile
in two directions either: an edit made on the NAS is never brought back, and the next local change
to that path overwrites it. It is not a Synology Drive client, so there is no Drive conflict
protocol and no on-demand or selective sync. And it carries content, hierarchy, names, and file
mtime only; ACLs, ownership, modes, and extended attributes are not preserved.

## Choose how to run it

| Surface | Best fit | Runtime model |
| --- | --- | --- |
| Native CLI | A source on Windows, macOS, Linux, or an OS-mounted network share | One explicit finite process; run manually or with the native scheduler |
| DSM 7 package and dashboard | The authoritative source is physically available to a Synology NAS | Non-root package service, graphical profiles and diagnostics, and interval/daily/realtime routines |
| Docker / Compose | A portable, isolated unattended job | Non-root finite container with a read-only source mount |
| Rust SDK or C ABI | Another application needs to embed the engine | Synchronous library call with caller-owned secrets, cancellation, and process policy |

The core binary is not a resident watcher. In **realtime** routine mode, the DSM package controller
watches with its own `inotify` watcher (`sdsync-dsm-api --watch`) and polls the source on a floor
regardless, then launches a
finite plan or sync after debounce. Interval and daily routines also launch finite runs. The
dashboard and the
`sdsync-dsm` recovery/automation manager operate the same package-owned profiles, credentials,
routines, state, and logs.


## Operations and comparison modes

| Operation | Remote effect | Use it for |
| --- | --- | --- |
| `doctor source [SOURCE] [--hash]` | None | Validate readability, names, exclusions, and optionally every payload fingerprint |
| `doctor --level quick target [REMOTE]` | None | Check TLS, reverse-proxy routing, and API discovery without authenticating |
| `doctor target [REMOTE]` | None | Run the default Standard check: authenticate, then sample at most five visible share roots when no destination is selected or five direct children when one is selected, and log out |
| `doctor --level extensive target [REMOTE]` | None | Require the fullest target API/capability evidence; still read-only |
| `doctor --level extensive target [REMOTE] --write-test` | Disposable probe and cleanup only | Exercise live create, upload, copy, MD5/CRC32/SHA-256 verification, and cleanup |
| `plan` | None | Review the exact pending work; `--exit-code` returns `10` when changes exist; it exits `3` whenever a path had to be deferred (for example a file changing while it was fingerprinted) |
| `sync` | Creates and updates | Safe default; remote-only entries remain |
| `sync` with effective deletion enabled | Creates, updates, and guarded removals | Deliberate one-way mirror after separate recovery testing |

Every target level reports an overall verdict plus timed, per-section `PASS`, `WARN`, `FAIL`, or
`SKIP` evidence. A bare Standard or Extensive target Doctor authenticates and reports no more than
five File Station-reported shared-folder roots under `visible_shared_folders`; it never chooses one
or treats discovery as proof of browse/read/write permission
for the user. Supplying `REMOTE`, directly or through a profile, instead reports at most five
non-recursive folder/file entries under `direct_children`. An empty sample is retained as explicit
evidence. Extensive never writes unless the separate `--write-test` opt-in is present. See
[Diagnostics and multi-profile batches](../diagnostics-and-batch.md)
for the exact checks, bounds, and limitations.

Comparison is independent of the operation:

| Mode | A same-path file matches when | Trade-off |
| --- | --- | --- |
| `content` | Size and server-side MD5 match; modification time alone never triggers an upload. Deletion and server-copy guards need the full MD5/CRC32/SHA-256 fingerprint | Safest and default; one server-side MD5 per same-size file, streams remote bytes only for those guards, and verifies uploads |
| `metadata` | Size and whole-second mtime match (floor or rounded) | Faster, but can miss equal-size changes within the same second |
| `size-only` | Size matches | Fastest and weakest; no content verification |

Use `--no-delete` to defeat deletion selected by a profile or environment for one invocation. A
mirror additionally enforces destination containment, per-profile and aggregate deletion caps,
empty-source protection, managed-path and remote-mount boundaries, fresh remote snapshots, and
failure-before-delete ordering.


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
  testing only. QuickConnect is supported for the CLI and engine, in both the relay
  (`<alias>.<relay>.quickconnect.to`) and direct (`<alias>.direct.quickconnect.to`) forms. The relay
  adds latency, so the LAN address or the direct form is faster; `doctor` tells you which path you
  are on.
- Passwords and supported authenticator-app TOTP can come from an OS vault, protected files,
  standard input, prompts, or dedicated environment variables. DSM Secure SignIn approval and
  hardware/security-key challenges are not supported by the documented File Station login flow.
- Sources may be local directories, mounted SMB/CIFS/NFS folders, mapped-drive subdirectories, or
  ordinary Windows UNC share roots such as `\\nas\media`. Filesystem roots (`/`, `C:\`), mapped-drive
  roots such as `Z:\`, and Windows administrative shares such as `C$`, `ADMIN$`, and `IPC$` are
  rejected. The operating system remains responsible for mounting and source-share authentication.
- Symlinks, junctions, SYSTEM/TEMPORARY entries, dehydrated cloud placeholders, and names that
  collide by case or Unicode normalisation are skipped and reported by name, never uploaded and never
  deleted from the NAS; OS metadata files (`desktop.ini`, `Thumbs.db`, `.DS_Store`) and office lock
  and temporary files (`~$*`, `~*.tmp`, `.~lock.*#`, `*.laccdb`, `*.ldb`) are excluded by default. Special files, unreadable folders, unsafe or over-long names and names the
  NAS refuses are set aside by name, never uploaded and never deleted from the NAS, and the rest of the
  run proceeds with exit `0` and a named warning; so is a NAS folder this account may not list. A file
  that changes or is locked mid-run is deferred, and the run exits `3` (partial) so it is retried. Keep
  the source quiescent and run as an unprivileged identity that owns or exclusively controls it.
- Safety refusals that stop a run: a scope that is, or lies beneath, a NAS symbolic link (exit `1`); two
  profiles whose NAS destinations overlap when either deletes (exit `2`); a share-wide refusal or a
  link-shaped `real_path` answer on most of the share. Excludes match Unicode-normalised on every source
  and protect a renamed folder's NAS spelling too, but content an exclude newly covers stays on the NAS
  as it is: delete it by hand.
- There is no two-way reconciliation, Drive conflict protocol, block-level delta, resumable upload,
  transactional multi-profile rollback, run-level transaction, or crash-atomic deletion/type
  replacement. A file's own upload is crash-atomic (`deposit = "atomic"`, the default): while a file
  is being replaced its name is missing for one request, and a crash at that moment leaves the old
  version whole beside it until the next run puts it back; see
  [Crash-atomic deposit](../deposit.md).
- File content, hierarchy, names, and file mtime are in scope. ACLs, ownership, modes, xattrs, hard
  links, sparse layout, and directory mtime are not preserved. Different URLs that reach the same
  NAS are aliases the client cannot identify.
