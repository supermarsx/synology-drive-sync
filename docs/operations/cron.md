# Portable cron

The cron fallback is for POSIX hosts without an appropriate service manager. Its wrapper validates a
protected environment file, rejects secret-value variables, checks private secret-file permissions,
holds a local `flock`, and then executes the core binary.

Use the version-matched tracked material:

- [cron deployment guide](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/cron/README.md)
- [run wrapper](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/cron/run-sync.sh)
- [protected environment example](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/cron/synology-drive-sync.env.example)
- [crontab example](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/cron/synology-drive-sync.crontab.example)

## Operational rules

- Run cron under the same unprivileged identity used for diagnostics.
- Protect the environment and every secret file with mode `0600` and reject symlinks.
- Set an absolute `SDSYNC_EXECUTABLE` and stable absolute source/config paths.
- Use one shared `SDSYNC_LOCK_FILE` for related jobs.
- Set `SDSYNC_PROGRESS=never`; write durable diagnostics to the bounded rotating log sink.
- Cron cannot provide an interactive OS-vault session reliably; use protected files unless the
  exact platform/session behavior has been proven.

The wrapper's `SDSYNC_PROFILES` and `SDSYNC_ALL_PROFILES` are not direct core CLI environment
variables. See [the environment reference](../configuration/cli-and-environment.md).
