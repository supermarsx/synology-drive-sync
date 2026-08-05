# systemd timer

This deployment uses one hardened `oneshot` service plus a timer. The service wrapper validates configuration, requires file-backed systemd credentials, takes a shared non-blocking `flock`, and then replaces itself with the sync process. A lock collision exits `75` and is a monitoring event, not a successful run.

## Install or upgrade

Install the verified release binary first. Stop an active job before replacing it; the CLI handles `SIGTERM` cooperatively, while systemd waits up to `TimeoutStopSec` before escalation.

```sh
sudo systemctl disable --now synology-drive-sync.timer 2>/dev/null || true
sudo systemctl stop synology-drive-sync.service 2>/dev/null || true
getent passwd sdsync >/dev/null || \
  sudo useradd --system --home-dir /nonexistent --shell /usr/sbin/nologin sdsync
sudo install -o root -g root -m 0755 synology-drive-sync /usr/local/bin/synology-drive-sync
sudo install -d -o root -g root -m 0755 /usr/local/libexec/synology-drive-sync
sudo install -o root -g root -m 0755 packaging/systemd/systemd-run.sh \
  /usr/local/libexec/synology-drive-sync/systemd-run
sudo install -o root -g root -m 0644 packaging/systemd/synology-drive-sync.service \
  /etc/systemd/system/synology-drive-sync.service
sudo install -o root -g root -m 0644 packaging/systemd/synology-drive-sync.timer \
  /etc/systemd/system/synology-drive-sync.timer
sudo install -d -o root -g sdsync -m 0750 /etc/synology-drive-sync
sudo test -e /etc/synology-drive-sync/sync.env || \
  sudo install -o root -g root -m 0640 packaging/systemd/sync.env.example \
    /etc/synology-drive-sync/sync.env
sudo test -e /etc/synology-drive-sync/dsm-password || \
  sudo install -o root -g root -m 0600 /secure/location/dsm-password \
    /etc/synology-drive-sync/dsm-password
sudo systemd-analyze verify /etc/systemd/system/synology-drive-sync.service \
  /etc/systemd/system/synology-drive-sync.timer
sudo systemctl daemon-reload
```

Repeated `install` commands replace the managed program and unit files without broad directory deletion. The guarded initialization commands retain an existing `sync.env` and password credential; replace either deliberately when its value must change. An upgrade also retains drop-ins, journal history, and the lock state directory. Review packaged unit changes before replacing local copies or drop-ins.

`LoadCredential` exposes the root-owned password only in the service credential directory; the wrapper passes that protected path to `--password-file` with `--no-vault`. For TOTP or remote logging, install separate root-owned mode-`0600` files and the supplied `totp.conf` or `remote-log.conf` drop-in examples. Never place a password, TOTP seed, OTP, or logging token value in `sync.env`.

Edit `sync.env`, grant `sdsync` read/traverse access to every configured source and referenced non-secret config/CA file, then validate under the real identity:

```sh
sudo -u sdsync /usr/local/bin/synology-drive-sync \
  --config /etc/synology-drive-sync/config.toml config validate
sudo systemctl start synology-drive-sync.service
sudo systemctl show synology-drive-sync.service -p ActiveState -p Result -p ExecMainStatus
sudo journalctl -u synology-drive-sync.service --since today
```

The wrapper also runs `config validate` before every configured-profile job. That catches syntax and profile constraints, not DSM reachability or source parity; run `doctor source`, authenticated `doctor target`, and a reviewed `plan` separately before scheduling.

## Lifecycle and diagnostics

```sh
# enable/disable future schedules
sudo systemctl enable --now synology-drive-sync.timer
sudo systemctl disable --now synology-drive-sync.timer

# start, stop, or deliberately restart one job
sudo systemctl start synology-drive-sync.service
sudo systemctl stop synology-drive-sync.service
sudo systemctl restart synology-drive-sync.service

# status, schedule, logs, and failure reset
systemctl status synology-drive-sync.service synology-drive-sync.timer
systemctl list-timers synology-drive-sync.timer
sudo journalctl -u synology-drive-sync.service -n 100 --no-pager
sudo systemctl reset-failed synology-drive-sync.service
```

Do not blindly restart a failed mutation. Inspect the journal, run a fresh `plan`, and confirm the source and NAS state first. `Restart=no` prevents automatic retry. The next timer occurrence is a new scheduled attempt, so alert on every nonzero `ExecMainStatus`, including lock contention `75`. Attach a site-owned `OnFailure=` drop-in and exercise it with an intentionally invalid disposable configuration.

The unit sends both streams to journald and has a conservative 24-hour whole-workload ceiling. `TimeoutStartSec` covers scanning, hashing, remote inventory, every upload/copy/delete/retry, final reconciliation, and shutdown. Measure the slowest accepted workload and override the ceiling with headroom:

```ini
[Service]
TimeoutStartSec=36h
TimeoutStopSec=5m
```

Install that with `systemctl edit synology-drive-sync.service`, then run `daemon-reload`. If the source is hidden by local systemd policy, add the narrow required path rather than removing the sandbox. `StateDirectory=synology-drive-sync` is the only packaged writable service directory.

## Single profile and batch jobs

Direct `SDSYNC_URL`/`USERNAME`/`SOURCE`/`REMOTE` values run one job. Alternatively set `SDSYNC_CONFIG` plus one of `SDSYNC_PROFILE`, `SDSYNC_PROFILES=nas-a,nas-b`, or `SDSYNC_ALL_PROFILES=true`. A batch is preflighted first, runs targets sequentially in deterministic profile-name order, and can add `SDSYNC_MAX_TOTAL_DELETE`.

The wrapper's systemd password and optional TOTP credentials are command-line file overrides shared by every selected profile. Batch mode therefore fails closed by default. Set `SDSYNC_BATCH_SHARED_CREDENTIALS=true` only when that common credential is intentionally correct for every selected profile. Otherwise create separate units with per-unit credentials, but point every unit that can touch the same source or NAS scope at the same writable `SDSYNC_LOCK_FILE`. Native systemd non-overlap applies only to one unit name; the packaged shared flock coordinates related units using the same service identity and lock path. It cannot coordinate another host.

`SDSYNC_DELETE=false` forces `--no-delete`, even when a profile says `delete=true`. Enabling deletion requires both `SDSYNC_DELETE=true` and bounded per-profile/aggregate limits. Complete the [disposable production acceptance runbook](../../docs/production-acceptance.md) first.

## Uninstall

Disable and stop before removing only the managed service assets:

```sh
sudo systemctl disable --now synology-drive-sync.timer
sudo systemctl stop synology-drive-sync.service 2>/dev/null || true
sudo rm -f -- /etc/systemd/system/synology-drive-sync.timer \
  /etc/systemd/system/synology-drive-sync.service \
  /usr/local/libexec/synology-drive-sync/systemd-run
sudo systemctl daemon-reload
sudo systemctl reset-failed synology-drive-sync.service
```

This intentionally retains `/etc/synology-drive-sync`, `/var/lib/synology-drive-sync`, the `sdsync` account, journal history, and the binary for recovery or audit. Inspect and remove those separately only after confirming no other unit uses them. The release installer can remove only the binary with `install.sh --uninstall --bin-dir /usr/local/bin`.
