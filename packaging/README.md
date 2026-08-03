# Deployment assets

These examples keep the authoritative source read-only wherever the scheduler supports it and keep passwords out of process arguments.

- `docker/`: non-root, read-only one-shot container using Docker secrets.
- `systemd/`: hardened system service and timer using systemd credentials.
- `launchd/`: per-user LaunchAgent using the macOS login Keychain.
- `windows/`: current-user Task Scheduler installer using Windows Credential Manager.
- `cron/`: conservative fallback using protected password/TOTP file paths.

Systemd, launchd, and Task Scheduler are preferred over cron because they have clearer overlap control and credential behavior.

`install.sh` and `install.ps1` detect the current OS/architecture, resolve only a strict `YY.N` GitHub release, verify the selected archive against the release `SHA256SUMS`, execute its embedded version probe, and atomically replace one binary in the selected install directory. They do not invoke a package manager or modify system-wide configuration. PowerShell changes the current-user `PATH` only when `-AddToUserPath` is explicit.
