# Deployment and service assets

The release installers manage one verified executable. Native managers own schedules, identities, lifecycle, logs, and overlap control; uninstalling the executable never silently removes those operational records.

| Deployment | Identity and credentials | Non-overlap boundary | Authoritative diagnostics |
| --- | --- | --- | --- |
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
