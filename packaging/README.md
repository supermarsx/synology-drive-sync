# Deployment and service assets

The workstation release installers manage one verified executable; the DSM SPK manages its own
packaged executable, controller, and private state. Native managers own schedules, identities,
lifecycle, logs, and overlap control. Uninstalling a workstation executable never silently removes
those operational records, while DSM package uninstall explicitly purges only its package-owned
configuration, credentials, state, and logs after a confirmation.

| Deployment | Identity and credentials | Non-overlap boundary | Authoritative diagnostics |
| --- | --- | --- | --- |
| DSM 7 SPK | `synology-drive-sync` system-internal user; package-owned `0600` password/TOTP files | one package run lock across manual and scheduled jobs on that NAS | `sdsync-dsm status`, bounded package logs, and exit status |
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

[`packaging/synology`](synology/) assembles a manually installable DSM 7 SPK around one validated
static musl ELF. Releases contain four separate ABI packages: `x86_64`, `armv8`, ARMv7-A hard-float
(`INFO` arch `armv7 armada370 armada375 armada38x armadaxp comcerto2k monaco`), and `i686` for
Evansport on the DSM 7.0/7.1 line. Use the
[release selector](../docs/release-selector.md) instead of guessing from a CPU label; ARMv5, PowerPC,
unknown, and conflicting inputs fail closed. The SPK runs without root or Linux capabilities, keeps
scheduling disabled after installation, and supplies the headless manager at:

```text
/var/packages/synology-drive-sync/target/bin/sdsync-dsm
```

Install it through **Package Center > Manual Install** only after verifying `SHA256SUMS` and the
optional GitHub attestation. DSM warns for a non-Synology package; the project does not suppress or
bypass that warning. Grant the `synology-drive-sync` system-internal user read-only permission to
each local source share, then configure, diagnose, plan, and run as that identity. Those manager
operations reject root or any identity other than the installed package owner, so a broad
administrator ACL cannot make source validation pass accidentally.

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

The package controller provides interval scheduling, cooperative start/stop, one run lock, state,
and bounded logs. Deletion requires both profile-level `--delete --max-delete N` and a manager-level
`--allow-delete` opt-in. Upgrade retains private configuration and credentials and validates them;
uninstall removes package-owned configuration, credentials, state, locks, and logs while leaving
both NAS data trees untouched.

See the [complete DSM package guide](../docs/synology-package.md) for exact install, ACL, profile,
secret, diagnostic, scheduling, upgrade, and acceptance commands. The package has static/mock
validation but no recorded live two-NAS installation test.

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
