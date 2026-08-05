# macOS LaunchAgent

Use a per-user LaunchAgent so the process runs in the same login session that owns the login Keychain. The supplied label does not overlap with itself: launchd will not start a second instance while that label is running. Different labels have no shared lock; combine related profiles into one batch or add one site-owned lock wrapper.

## Install or upgrade

Install the verified binary and wrapper at stable absolute non-symlink paths, enroll credentials with the exact profile URL/username, and render every plist placeholder:

```sh
install -d -m 0755 "$HOME/.local/bin" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
install -m 0755 synology-drive-sync "$HOME/.local/bin/synology-drive-sync"
install -m 0755 packaging/launchd/launchd-run.sh "$HOME/.local/bin/synology-drive-sync-launchd"
install -m 0644 packaging/launchd/io.github.supermarsx.synology-drive-sync.plist \
  "$HOME/Library/LaunchAgents/io.github.supermarsx.synology-drive-sync.plist"

"$HOME/.local/bin/synology-drive-sync" credentials set-password \
  --url https://files.example.com --username mirror-bot
"$HOME/.local/bin/synology-drive-sync" credentials status \
  --url https://files.example.com --username mirror-bot
plutil -lint "$HOME/Library/LaunchAgents/io.github.supermarsx.synology-drive-sync.plist"
```

Do not copy the sample without replacing `/ABSOLUTE/...`, `/Users/YOU`, URL, username, source, and remote values. The plist contains no password, seed, OTP, or logging token. `credentials status` proves only that vault entries exist; authenticated `doctor target` proves they work.

For an upgrade, boot out the job and wait for it to exit before atomically replacing the binary/wrapper/plist. Validate, bootstrap, and manually kickstart once before relying on the calendar:

```sh
domain="gui/$(id -u)"
label=io.github.supermarsx.synology-drive-sync
launchctl bootout "$domain/$label" 2>/dev/null || true
plutil -lint "$HOME/Library/LaunchAgents/$label.plist"
launchctl bootstrap "$domain" "$HOME/Library/LaunchAgents/$label.plist"
launchctl enable "$domain/$label"
launchctl kickstart "$domain/$label"
```

Repeated `bootout`/`bootstrap` operations are the idempotent replacement path. The wrapper forwards termination to the CLI, reaps it, and streams otherwise-lost stderr through a private FIFO to macOS unified logging under `synology-drive-sync`; it waits for that logger to flush before returning the CLI status. The CLI's own JSON file sink remains the bounded detailed log.

## Lifecycle and diagnostics

```sh
domain="gui/$(id -u)"
label=io.github.supermarsx.synology-drive-sync

# enable/disable future launchd starts
launchctl enable "$domain/$label"
launchctl disable "$domain/$label"

# start, cooperative stop, and explicit restart
launchctl kickstart "$domain/$label"
launchctl kill TERM "$domain/$label"
launchctl kickstart -k "$domain/$label"

# status and logs
launchctl print "$domain/$label"
log show --last 1d --predicate 'process == "logger" AND eventMessage CONTAINS "synology-drive-sync"'
tail -n 100 "$HOME/Library/Logs/synology-drive-sync.log"
```

`kickstart -k` intentionally stops and starts; do not use it as a blind retry. Inspect `last exit code`, unified fallback diagnostics, and a fresh `plan` first. `ExitTimeOut=120` gives cooperative cancellation two minutes before launchd escalation; size this and the application's operation timeout for the accepted workload.

The sample uses `--quiet`, JSON logging, disabled terminal progress, and the CLI's 10 MiB active file plus three backups. The fallback FIFO applies backpressure instead of accumulating an unbounded temporary file. Log retention for unified logging is controlled by macOS policy.

## Profiles and batches

The sample shows a direct job. To use a config, replace the direct source/remote/URL/username arguments with:

```xml
<string>--config</string><string>/ABSOLUTE/PATH/TO/config.toml</string>
<string>--profile</string><string>production</string>
<string>sync</string>
```

For a batch, use `--profiles` plus one comma-separated value or `--all-profiles`, and add `--max-total-delete` when appropriate. Batch targets are preflighted then run sequentially in deterministic profile-name order. Each profile can resolve its own login Keychain entry by URL/username. `--password-stdin` is not suitable for an unattended batch.

One label prevents only its own overlap. Prefer one batch label for a shared window. Separate labels, other users, or other hosts require a shared local/distributed lock; launchd itself does not provide one across labels.

The sample forces update-only behavior by omitting `--delete`; if a config profile could enable deletion, add `--no-delete`. Enable deletion only after the [disposable production acceptance runbook](../../docs/production-acceptance.md) and explicit per-profile/aggregate limits.

## Uninstall

```sh
domain="gui/$(id -u)"
label=io.github.supermarsx.synology-drive-sync
launchctl bootout "$domain/$label" 2>/dev/null || true
rm -f -- "$HOME/Library/LaunchAgents/$label.plist" \
  "$HOME/.local/bin/synology-drive-sync-launchd"
```

This retains the binary, Keychain entries, and logs for recovery/audit. Remove vault entries explicitly with `credentials remove` only after confirming no other profile uses them. The release installer removes only the binary when invoked with `install.sh --uninstall --bin-dir "$HOME/.local/bin"`.
