# Linux systemd

The tracked systemd assets implement a hardened one-shot service plus timer. The wrapper validates
its environment, requires a shared lock, maps systemd credentials into protected file arguments, and
executes the real binary.

Use the repository's complete, version-matched instructions and assets:

- [systemd deployment guide](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/systemd/README.md)
- [service unit](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/systemd/synology-drive-sync.service)
- [timer unit](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/systemd/synology-drive-sync.timer)
- [environment example](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/systemd/sync.env.example)

## Important controls

- Install the binary and configuration at paths readable by the service identity.
- Store the DSM password with systemd `LoadCredential`; enable the optional TOTP and remote-log
  credential mappings only when configured.
- Keep `SDSYNC_LOCK_FILE` on a private writable local filesystem and share it across related units.
- Use `systemctl start` for one immediate run and the timer only after a reviewed plan.
- `systemctl stop` sends SIGTERM, which requests cooperative cancellation.
- Keep the service-manager deadline above worst-case scan, transfer, retry, and shutdown time.

Direct mode uses wrapper `SDSYNC_SOURCE`/`SDSYNC_REMOTE`; profile mode uses `SDSYNC_CONFIG` plus one
profile selector. See [core versus wrapper environment variables](../configuration/cli-and-environment.md).
