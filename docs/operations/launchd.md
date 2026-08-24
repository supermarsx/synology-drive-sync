# macOS launchd

The tracked LaunchAgent example runs as the logged-in user, which keeps configuration paths and
Keychain-backed vault behavior aligned with interactive diagnostics. It provides explicit install,
enable, start, cooperative stop, status, log, and uninstall commands.

Use the version-matched tracked material:

- [launchd deployment guide](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/launchd/README.md)
- [LaunchAgent plist](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/launchd/io.github.supermarsx.synology-drive-sync.plist)
- [lifecycle helper](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/launchd/launchd-run.sh)

## Operational rules

- Install it for the same user who owns the configuration and Keychain entries.
- Confirm that mounted/SMB sources are visible inside the LaunchAgent session.
- Keep passwords, TOTP seeds, current OTP codes, and bearer-token values out of the plist.
- Run a manual plan through the helper before enabling recurring starts.
- A stop requests cooperative cancellation; wait for the child to exit before upgrading.

If profiles can overlap, use one scheduled batch or an external/shared lock design appropriate to
the host. The core process does not coordinate separately launched processes.
