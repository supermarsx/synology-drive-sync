# macOS LaunchAgent

Use a per-user LaunchAgent so the process runs as the same logged-in user that owns the login Keychain.

1. Install the correct release binary in a stable absolute location.
2. Enroll the password and, optionally, the DSM TOTP seed with `synology-drive-sync credentials` using the exact URL and username that will appear in the plist.
3. Copy the plist to `~/Library/LaunchAgents/io.github.supermarsx.synology-drive-sync.plist` and replace every placeholder with an absolute path or real non-secret value. Do not add passwords or OTP values.
4. Validate and load it:

```sh
plutil -lint ~/Library/LaunchAgents/io.github.supermarsx.synology-drive-sync.plist
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/io.github.supermarsx.synology-drive-sync.plist
launchctl kickstart -k "gui/$(id -u)/io.github.supermarsx.synology-drive-sync"
```

Inspect `~/Library/Logs/synology-drive-sync.log`. To unload it, use `launchctl bootout "gui/$(id -u)/io.github.supermarsx.synology-drive-sync"`.

The login Keychain generally must be unlocked, so this example is appropriate for a user login session. It intentionally does not use `--delete`; add that flag only after inspecting a dry run and setting an explicit deletion limit.
