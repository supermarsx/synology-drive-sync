# cron fallback

Prefer systemd, launchd, or Task Scheduler when available. Headless cron commonly lacks a usable Secret Service D-Bus session, so this fallback supplies a mode-`0600` file through `--password-file` and uses `--no-vault`.

Install the binary and wrapper, copy `synology-drive-sync.env.example` to `~/.config/synology-drive-sync/cron.env`, and keep both that file and the referenced password file mode `0600`. Install the crontab line only after running the wrapper manually. `flock` prevents overlapping jobs.

The example defaults to update-only behavior. Do not enable `SDSYNC_DELETE` until a manual dry run has been reviewed. For unattended TOTP, set `SDSYNC_TOTP_SECRET_FILE` to a separate mode-`0600` seed file; put only its path in the cron environment. A seed or generated OTP must never be written into that environment file.
