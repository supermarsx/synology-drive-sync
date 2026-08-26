# Deployment and service assets

The workstation release installers manage one verified executable; the DSM SPK manages its own
packaged executable, controller, and private state. Native managers own schedules, identities,
lifecycle, logs, and overlap control. Uninstalling a workstation executable never silently removes
those operational records, while DSM package uninstall explicitly purges only its package-owned
configuration, credentials, state, and logs after a confirmation.

| Deployment | Identity and credentials | Non-overlap boundary | Authoritative diagnostics |
| --- | --- | --- | --- |
| DSM 7 SPK | actual DSM package identity discovered from package-home ownership; package-owned `0600` password/TOTP/token files | one package run lock across dashboard, manual, and scheduled jobs on that NAS | native DSM AppWindow dashboard, `sdsync-dsm status`, bounded package logs, and exit status |
| systemd | dedicated `sdsync` user; `LoadCredential` files | shared packaged `flock` path across related units on one host | journald plus exit status |
| launchd | logged-in user; login Keychain | one launchd label only | rotating JSON file plus unified-log fallback |
| Task Scheduler | current interactive user; Credential Manager | one task name (`IgnoreNew`) only | rotating JSON file plus `LastTaskResult` |
| cron | cron account; owner-only secret files | shared packaged `flock` path for that account/host | rotating JSON file, stderr, and cron mail |
| Docker/Compose | caller UID/GID for managed runs; direct Compose defaults to `10001:10001`; Docker secret mounts | managed host wrapper lock/container name only | retained exit state plus bounded host log |

Prefer systemd, launchd, or Task Scheduler over cron. Use the managed Compose wrapper instead of direct one-off commands when repeatable locking/status/log retention matter. No local lock coordinates another host; multi-host writers require an external distributed operational lock.

One multi-profile `--profiles`/`--all-profiles` invocation preflights and sequentially processes targets while its manager lock is held. The systemd and cron wrappers reject batch mode unless their explicit shared-credential acknowledgement is enabled; this prevents one password/TOTP override from silently replacing distinct profile credentials. Separate units, labels, tasks, cron accounts, direct Docker commands, and hosts are independent unless they deliberately share a lock. Never document the binary's in-process overlap validation as a process lock—it is not one.

Every unattended deployment must:

- validate the non-secret config and exact scheduler identity;
- run local `doctor source`, authenticated `doctor target`, and a reviewed `plan`;
- keep passwords, TOTP seeds, current OTPs, and logging tokens out of arguments/unit/plist/task/TOML values;
- retain bounded diagnostics and alert on every unexpected nonzero result;
- avoid automatic blind restart after mutation or timeout;
- use a whole-workload scheduler ceiling, not one request-timeout formula;
- complete the [disposable production acceptance runbook](../docs/production-acceptance.md) before enabling deletion.

The per-manager directories provide exact install/upgrade/uninstall, enable/disable, start/stop/restart, status/log, locking, batch, and recovery guidance.

## Synology DSM package lifecycle

[`packaging/synology`](synology/) assembles a manually installable DSM 7 SPK around two validated
static musl ELFs and the pinned native AppWindow bundle. Release 26.10 introduced this native
AppWindow; published 26.7-26.9 assets retain their original UI. Releases contain
four separate ABI packages: `x86_64`, `armv8`, ARMv7-A hard-float
(`INFO` arch `armv7 armada370 armada375 armada38x armadaxp comcerto2k monaco`), and `i686` for
Evansport on the DSM 7.0/7.1 line. Use the
[release selector](../docs/release-selector.md) instead of guessing from a CPU label; ARMv5, PowerPC,
unknown, and conflicting inputs fail closed. The SPK runs without root, Linux capabilities, joined
web groups, or any set-user-ID/set-group-ID file. `defaults.run-as=package` makes DSM execute the
ordinary package-owned `0755` CGI with the same exact non-root package UID as the API service. The
CGI relays bounded requests over a fixed package-owned socket that is `0000` before startup commit
and activates on the same inode as `0600`. The service reauthenticates the DSM session and enforces
administrator membership and package CSRF before reaching private state. The
package keeps scheduling disabled after installation, registers the administrator-only native DSM
AppWindow `SYNO.SDS.App.SynologyDriveSync.Instance`, and supplies the CLI recovery/automation manager at:

```text
/var/packages/synology-drive-sync/target/bin/sdsync-dsm
```

Install it through **Package Center > Manual Install** only after verifying `SHA256SUMS` and the
optional GitHub attestation. DSM's normal unsigned third-party publisher warning is expected; the
project does not suppress or bypass it. A refusal that says the package requires root or a lower
privilege level is different and must be investigated with `/var/log/synopkg.log`,
`/var/log/messages`, and the package log. Grant the actual package **System internal user** read-only
permission to each local source share, then configure, diagnose, plan, and run as that identity. DSM
may collision-rename its NSS username; discover it from package-home ownership as documented in
[CLI parity](../docs/dsm/cli-parity.md#discover-the-actual-package-identity). Manager operations
reject root or any identity other than the installed package owner, so a broad administrator ACL
cannot make source validation pass accidentally.

The manager supports arbitrary remote File Station logical destinations. `/home/Drive/...` selects
the target account's Drive home, while `/<share>/...` selects any writable Team Folder/shared-folder
subdirectory. DSM must create the user home or shared-folder root and establish its ACL first. Sync
creates missing descendant folders beneath an existing writable parent; it never creates a DSM
shared folder or enables a Team Folder.

Multiple profiles may use different sources, URLs, accounts, credentials, and destination roots.
`doctor --all`, `plan --all`, and `run --all` reuse the core all-target preflight and deterministic
batch behavior. Passwords and optional TOTP seeds enter through masked prompts or readable
non-symlink input files and are copied into private package storage; secret values never enter
arguments or generated TOML.

The package controller provides per-profile interval/daily/realtime routines, native-watcher polling
fallback, dependencies, bounded retry/backoff, a legacy global interval schedule, cooperative
start/stop, one run lock, state, bounded logs, and fixed DSM notifications. Package Center lifecycle
also controls the package-user API service. The dashboard uses DSM cookie authentication, an
independent administrator check, mandatory package CSRF, and a private controller queue; the native
UI does not inspect the DSM shell location or send a `SynoToken`, and stored secrets are never returned. Deletion requires profile and
action-level approval. Upgrade retains
private configuration and credentials and validates them; uninstall removes package-owned
configuration, credentials, state, locks, socket, and logs while leaving both NAS data trees untouched.

See the [complete DSM package and dashboard guide](../docs/synology-package.md) for exact install,
ACL, graphical configuration, secret, diagnostic, routine, security, CLI, upgrade, and acceptance
behavior. The package has static/mock validation but no recorded physical installation or live
two-NAS test. Package-user `authenticate.cgi` execution, browser request-marker forwarding to CGI
`HTTP_X_SDSYNC_REQUEST=1`, and AppWindow loading also remain live
acceptance checks;
token absence is supported.

## Verified binary installer lifecycle

`install.sh` and `install.ps1` detect OS/architecture, resolve a strict `YY.N` release, verify the selected archive against `SHA256SUMS`, enforce an archive-member allowlist, run the embedded version probe, and atomically install or upgrade one executable. They do not invoke a package manager or mutate a scheduler.

```sh
sh packaging/install.sh --version YY.N --bin-dir "$HOME/.local/bin"
sh packaging/install.sh --uninstall --bin-dir "$HOME/.local/bin"
```

```powershell
.\packaging\install.ps1 -Version YY.N -AddToUserPath
.\packaging\install.ps1 -Uninstall
```

Uninstall is idempotent and refuses linked/non-regular targets. It retains config, credentials, schedules, logs, state directories, and PATH entries so removal is recoverable and auditable. Stop every managed job before upgrading/removing the executable, and uninstall the native manager definition before deleting retained state.
