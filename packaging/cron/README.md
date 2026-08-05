# cron fallback

Prefer systemd, launchd, or Task Scheduler. Cron has no native service lifecycle, credential store, structured status, or reliable failure retry policy. This wrapper supplies strict configuration parsing, file-permission checks, a shared non-blocking `flock`, bounded application logs, and meaningful exit `75` on overlap.

## Install or upgrade

Install the binary and wrapper, create private config/state directories, and copy the examples without overwriting an existing configuration:

```sh
install -d -m 0700 "$HOME/.config/synology-drive-sync" \
  "$HOME/.local/state/synology-drive-sync"
install -d -m 0755 "$HOME/.local/bin"
install -m 0755 packaging/cron/run-sync.sh "$HOME/.local/bin/synology-drive-sync-cron"
test -e "$HOME/.config/synology-drive-sync/cron.env" || \
  install -m 0600 packaging/cron/synology-drive-sync.env.example \
    "$HOME/.config/synology-drive-sync/cron.env"
chmod 0400 "$HOME/.config/synology-drive-sync/dsm-password"
```

The environment file and every password, TOTP, or remote-log token file must be a readable non-symlink regular file owned by the cron account with mode `0400` or `0600`. The parser accepts only documented assignments, rejects duplicates and raw secret variables, and assigns values literally; it never `source`s or `eval`s the file. Do not add shell quotes, substitutions, or `export`; those bytes would be part of the literal value rather than shell syntax.

Run validation and one real job manually as the exact cron account before installing the schedule:

```sh
"$HOME/.local/bin/synology-drive-sync-cron" \
  "$HOME/.config/synology-drive-sync/cron.env"
tail -n 100 "$HOME/.local/state/synology-drive-sync/sync.log"
```

An upgrade replaces only the wrapper and binary. It retains configuration, secrets, logs, and the lock. The wrapper validates `SDSYNC_CONFIG` before a profile job, but `config validate` does not prove source parity or DSM access; separately run `doctor source`, authenticated `doctor target`, and a reviewed `plan`.

## Enable, disable, status, stop, and restart

Merge the sample line with `crontab -e`; never replace an unrelated crontab wholesale. Set `MAILTO` to a monitored destination and deliberately exercise delivery with an invalid disposable configuration. Disable future runs by commenting or removing only this job's line.

The lock lives under the mode-`0700` state directory and is held across configuration validation and the complete sequential workload. The current wrapper/CLI PID is stored in the locked file. Check status without trusting a stale PID:

```sh
lock="$HOME/.local/state/synology-drive-sync/service.lock"
if flock -n "$lock" true; then echo idle; else echo running; fi
```

For an intentional stop, first confirm the lock is held, read a numeric PID, inspect that exact process, and send `TERM`—never use a broad `pkill`:

```sh
lock="$HOME/.local/state/synology-drive-sync/service.lock"
if ! flock -n "$lock" true; then
  pid=$(sed -n '1p' "$lock")
  case "$pid" in ''|*[!0-9]*) echo 'invalid managed PID' >&2; exit 1;; esac
  ps -p "$pid" -o pid=,args=
  kill -TERM "$pid"
fi
```

The CLI handles `TERM` cooperatively, but an upload or remote task may need its configured timeout to unwind. Wait for the lock to become free; do not escalate blindly. A restart means stop, inspect the last failure and a fresh `plan`, then invoke the wrapper manually. Cron itself does not retry.

Successful runs are quiet. Every failure and overlap emits stderr for cron mail, while the CLI retains one 10 MiB JSON log plus three backups. Inspect exit status, mail, and the log before rerunning.

## Profiles, batches, and locking scope

Direct mode uses `SDSYNC_URL`, `SDSYNC_USERNAME`, `SDSYNC_SOURCE`, and `SDSYNC_REMOTE`. Profile mode uses `SDSYNC_CONFIG` plus an optional `SDSYNC_PROFILE`; batch mode uses `SDSYNC_PROFILES=nas-a,nas-b` or `SDSYNC_ALL_PROFILES=true`, optionally with `SDSYNC_MAX_TOTAL_DELETE`.

One batch is preflighted before mutation and processes targets sequentially in deterministic profile-name order while holding one lock. The wrapper's password and optional TOTP file are common command-line overrides for every selected profile, so batch mode fails closed unless `SDSYNC_BATCH_SHARED_CREDENTIALS=true` explicitly confirms that sharing is intentional. Use separate cron configs when credentials differ, and set the same `SDSYNC_LOCK_FILE` for every related job under the same cron identity. A local `flock` cannot coordinate another account or host; use one scheduler identity or an external distributed lock.

`SDSYNC_DELETE=false` forces `--no-delete` even if a profile enables deletion. Do not set it true until the [disposable production acceptance runbook](../../docs/production-acceptance.md) passes and both per-profile and aggregate limits are reviewed.

## Uninstall

Remove only this job's line with `crontab -e`, wait for or cooperatively stop an active run, then remove the wrapper. Retain config, secret files, logs, and the lock until recovery/audit needs are resolved. Remove the binary separately with `install.sh --uninstall --bin-dir "$HOME/.local/bin"`. Never recursively remove `.config` or `.local/state` merely to uninstall this job.
