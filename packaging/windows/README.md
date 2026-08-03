# Windows Task Scheduler

Install the matching release executable, then enroll the DSM password and optional TOTP seed in Windows Credential Manager under the same Windows account that will own the task:

```powershell
& "$env:LOCALAPPDATA\Programs\synology-drive-sync\synology-drive-sync.exe" credentials set-password `
  --url https://files.example.com --username mirror-bot
```

Create a daily update-only task:

```powershell
.\packaging\windows\Install-SynologyDriveSyncTask.ps1 `
  -Source 'C:\Data\Export' `
  -Remote '/team/export' `
  -Url 'https://files.example.com' `
  -Username 'mirror-bot' `
  -At '03:00'
```

The installer uses an interactive-token, limited-privilege task so Windows Credential Manager is available to the same logged-in user. It refuses to replace an existing task unless `-Force` is explicit and configures Task Scheduler to ignore overlapping runs. No password, TOTP seed, or OTP code is placed in the task XML or process arguments.

By default, the task helper uses the release installer's per-user executable at `%LOCALAPPDATA%\Programs\synology-drive-sync\synology-drive-sync.exe`. Pass `-Executable 'C:\path\to\synology-drive-sync.exe'` when you installed it elsewhere.

For a machine that must run while nobody is logged in, configure and test the task's logon mode manually for the intended service account; vault access varies with Windows logon type. Do not switch to plaintext command-line secrets. Mirror deletion remains opt-in through `-Delete -MaxDelete N`.
