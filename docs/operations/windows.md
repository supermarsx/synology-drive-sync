# Windows Task Scheduler

The PowerShell installer creates and manages a per-user scheduled task while keeping the executable,
configuration, logs, and credential-file ACLs explicit. A mapped drive created in another logon
session may not exist for the task; prefer a validated UNC path or prove the exact mapping under the
task identity.

Use the version-matched tracked material:

- [Task Scheduler deployment guide](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/windows/README.md)
- [task installer](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/windows/Install-SynologyDriveSyncTask.ps1)
- [task lifecycle helper](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/windows/Manage-SynologyDriveSyncTask.ps1)

## Operational rules

- Install and test under the final Windows user.
- Use the user's OS vault when the scheduled logon type exposes it, or ACL-protected secret files.
- Diagnose the exact UNC/mapped path from the task context.
- Keep progress disabled and enable durable file/remote logging for unattended runs.
- Use the helper for enable, disable, start, stop, status, and uninstall so lifecycle behavior remains
  consistent with the shipped contract.

Do not place secret values in task arguments. A cooperative stop can take time while a request or
safe read boundary completes; size Task Scheduler's execution limit accordingly.
