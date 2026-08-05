# Windows Task Scheduler

The installer creates a current-user, limited-privilege task with `MultipleInstances=IgnoreNew`. That prevents overlap for one task name only. Prefer one multi-profile batch task for related targets; different task names, users, or hosts require an external shared lock.

## Install or upgrade

Install the verified release executable, then enroll each DSM password and optional TOTP seed in Windows Credential Manager under the exact Windows account that owns the task:

```powershell
$exe = "$env:LOCALAPPDATA\Programs\synology-drive-sync\synology-drive-sync.exe"
& $exe credentials set-password --url https://files.example.com --username mirror-bot
& $exe credentials status --url https://files.example.com --username mirror-bot
```

Create a direct daily update-only task:

```powershell
.\packaging\windows\Install-SynologyDriveSyncTask.ps1 `
  -Source 'C:\Data\Export' -Remote '/team/export' `
  -Url 'https://files.example.com' -Username 'mirror-bot' -At '03:00'
```

Or create one config-backed batch:

```powershell
.\packaging\windows\Install-SynologyDriveSyncTask.ps1 `
  -Config "$env:APPDATA\synology-drive-sync\config.toml" `
  -Profiles 'nas-a','nas-b' -MaxTotalDelete 150 -At '03:00'
```

Use `-AllProfiles` instead of `-Profiles`, or `-Profile production` for one named job. The installer executes `config validate`, rejects mixed direct/profile inputs, resolves all paths, rejects reparse-point executable/source/config/log targets, and safely quotes every native argument before task registration. It refuses to replace an existing task unless `-Force` is explicit. `-Force` is the idempotent task-definition upgrade path.

For a binary upgrade, disable/stop the task first, run the release installer, run `--version`, then reinstall the task with `-Force` if its arguments/assets changed. A failed verified download leaves the old executable intact. Do not replace an executable while the task is running.

The interactive-token task can access the same user's Credential Manager only while that logon mode is available. `credentials status` proves entry presence, not successful authentication; run authenticated `doctor target` and a reviewed `plan` as the task owner. For file-backed profile credentials, remove inherited ACLs and grant read only to that task identity; never put a secret value in task XML or arguments.

## Lifecycle and diagnostics

Use the management helper so status and removal remain scoped to the exact task name:

```powershell
$manager = '.\packaging\windows\Manage-SynologyDriveSyncTask.ps1'
& $manager status
& $manager start
& $manager stop
& $manager restart
& $manager disable
& $manager enable
& $manager diagnostics
& $manager logs -Tail 200
```

The task writes JSON diagnostics through the CLI's bounded rotating sink at `%LOCALAPPDATA%\synology-drive-sync\logs\sync.log` by default. The active file and three backups use about 40 MiB maximum. `diagnostics` prints scheduler state, last/next run data, exact `LastTaskResult`, and the log tail without exposing task arguments.

Task Scheduler does not retain ordinary process stderr in this example, so argument/config validation during installation and the structured file are important. Connect every nonzero `LastTaskResult`, including overlap/rejected-start signals, to the normal Windows event/task monitor. Exit `10` is meaningful for an explicitly scheduled `plan`; it is not success for a sync task.

`stop` and Task Scheduler's execution-limit termination may not deliver the same cooperative console signal as an interactive Ctrl+C. The manager waits but never escalates to a broader kill. After any forced/timeout stop, inspect the NAS and run a fresh `plan` before restarting. Do not configure blind retries.

The default 24-hour `ExecutionTimeLimit` covers the whole workload: scanning, hashing, inventory, every upload/copy/delete/retry, final reconciliation, and shutdown. Measure the slowest accepted batch and pass `-ExecutionTimeLimitHours` with headroom.

## Batch and locking scope

Batch jobs are preflighted before mutation and processed sequentially in deterministic profile-name order. Every profile can resolve a separate vault entry by URL/username. `--password-stdin` is rejected for batch services; use the task-owner vault or tightly ACLed per-profile files.

`IgnoreNew` does not coordinate different task names. If profiles share a source or NAS scope, put them in one batch task. If separate tasks are unavoidable, use a site-owned named mutex/lock wrapper shared by every related task. Task Scheduler alone cannot coordinate another host.

The installer adds `--no-delete` unless `-Delete` is explicit, overriding a profile that might otherwise enable deletion. Require `-MaxDelete` and, for a batch, `-MaxTotalDelete`; complete the [disposable production acceptance runbook](../../docs/production-acceptance.md) first.

## Disable and uninstall

`disable` prevents future triggers but does not stop an already-running task; call `stop` separately and inspect completion. Remove only the managed task with:

```powershell
.\packaging\windows\Manage-SynologyDriveSyncTask.ps1 uninstall
```

Uninstall retains logs and Credential Manager entries for recovery/audit. Remove credentials explicitly with `credentials remove` only when no other profile uses them. Remove the executable separately with `packaging\install.ps1 -Uninstall`; that command also retains tasks, logs, config, vault entries, and PATH state.
