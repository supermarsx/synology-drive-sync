# systemd timer

Install the release binary and wrapper, create a dedicated account that can read the source, then install the units:

```sh
sudo install -o root -g root -m 0755 synology-drive-sync /usr/local/bin/synology-drive-sync
sudo install -d -o root -g root -m 0755 /usr/local/libexec/synology-drive-sync
sudo install -o root -g root -m 0755 packaging/systemd/systemd-run.sh /usr/local/libexec/synology-drive-sync/systemd-run
sudo install -o root -g root -m 0644 packaging/systemd/synology-drive-sync.service /etc/systemd/system/
sudo install -o root -g root -m 0644 packaging/systemd/synology-drive-sync.timer /etc/systemd/system/
sudo useradd --system --home-dir /nonexistent --shell /usr/sbin/nologin sdsync
sudo install -d -o root -g sdsync -m 0750 /etc/synology-drive-sync
sudo install -o root -g root -m 0644 packaging/systemd/sync.env.example /etc/synology-drive-sync/sync.env
sudo install -o root -g root -m 0600 /secure/location/dsm-password /etc/synology-drive-sync/dsm-password
```

Edit `sync.env`, grant the `sdsync` account read/traverse access to the source, and test before enabling the schedule:

```sh
sudo systemctl daemon-reload
sudo systemctl start synology-drive-sync.service
sudo journalctl -u synology-drive-sync.service
sudo systemctl enable --now synology-drive-sync.timer
systemctl list-timers synology-drive-sync.timer
```

`LoadCredential` exposes the root-owned password only in the service credential directory; the wrapper passes that protected path to `--password-file` and enables `--no-vault`. The unit mounts the service filesystem namespace read-only and has no Linux capabilities. If the source is on a path hidden by local systemd policy, adjust the sandbox deliberately rather than disabling all hardening.

For an account that requires TOTP, install a second root-readable mode-`0600`
seed file, copy `synology-drive-sync-totp.conf.example` to
`/etc/systemd/system/synology-drive-sync.service.d/totp.conf`, and adjust its
source path. The drop-in enables a second systemd credential and the wrapper
passes only its protected path to `--totp-secret-file`; the seed never belongs
in `sync.env`.
