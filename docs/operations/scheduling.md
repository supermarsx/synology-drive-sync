# Scheduling overview

Every supported deployment launches a finite process. The process scans, plans, mutates only when
authorized, verifies, logs out, and exits. There is no resident daemon in the core binary.

Choose the scheduler whose identity and secret model you can audit:

| Platform | Preferred mechanism | Identity and secret model |
| --- | --- | --- |
| Linux | [systemd service and timer](systemd.md) | Dedicated service user, hardening, `LoadCredential`, one-shot service. |
| Portable Unix | [cron wrapper](cron.md) | Explicit user, protected environment/secret files, `flock`. |
| macOS | [per-user LaunchAgent](launchd.md) | User session and Keychain-compatible vault access. |
| Windows | [Task Scheduler helper](windows.md) | Per-user scheduled task, OS vault or ACL-protected files. |
| Containers | [Docker and Compose](docker.md) | Read-only bind mount, non-root UID/GID, mounted secret files, finite container. |
| Synology DSM | [DSM package scheduler](../synology-package.md) | System-internal package user, package-owned config/secrets/state. |

## Before enabling a schedule

Run every validation under the exact eventual identity:

```bash
synology-drive-sync config validate --config /path/to/config.toml
synology-drive-sync doctor source --profile production --hash
synology-drive-sync doctor --profile production target
synology-drive-sync plan --profile production
```

A mapped drive, vault entry, certificate, config-relative path, or permission visible in your
interactive session may be absent from a service session.

## Non-overlap and time budgets

Prefer one scheduled batch for jobs sharing an operational window. Otherwise, make every related job
on one host use the same writable lock file. A local lock cannot coordinate another user or host;
use one scheduler identity or an external distributed lock when multiple hosts can touch the same
scope.

The scheduler timeout must cover the complete workload: local scan/hash, remote inventory, all
sequential profiles, every upload/retry, verification, final replan, required-log flush, and
cooperative shutdown. It is separate from the per-operation application `timeout`.

## Destructive schedules

Never enable scheduled deletion merely by setting `delete = true`. The DSM manager and native
wrappers add their own execution authorization, and all paths retain per-profile/aggregate caps.
Complete the [production acceptance and recovery runbook](../production-acceptance.md) first.

## Upgrade discipline

Disable future starts, wait for or cooperatively stop the active process, install the verified new
artifact, validate the same configuration under the same identity, run a plan, and only then re-enable
the schedule. Do not replace an executable beneath a running process.
