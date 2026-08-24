# Docker and Compose

The container is a finite, non-root job. The source is bind-mounted read-only, configuration and
secret files are mounted explicitly, the root filesystem is read-only, and the process exits after
doctor, plan, or sync.

Use the version-matched tracked material:

- [Docker/Compose deployment guide](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/docker/README.md)
- [Compose helper](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/docker/run-compose.sh)
- [base Compose definition](https://github.com/supermarsx/synology-drive-sync/blob/main/compose.yaml)
- [optional TOTP overlay](https://github.com/supermarsx/synology-drive-sync/blob/main/compose.totp.yaml)

## Required inputs

The helper validates absolute `SDSYNC_SOURCE`, logical `SDSYNC_REMOTE`, endpoint/account settings,
runtime UID/GID, and protected password/TOTP file paths before invoking Compose. It rejects direct
secret-value environment variables.

```bash
export SDSYNC_SOURCE=/srv/export
export SDSYNC_REMOTE=/team/export
export SDSYNC_URL=https://files.example.com
export SDSYNC_USERNAME=mirror-bot
export SDSYNC_PASSWORD_FILE=/run/private/dsm-password
```

First run configuration validation, source/target diagnostics, and `plan`. A container restart policy
must not create an uncontrolled retry loop after an operational safety failure. Prefer an outer
scheduler that records the exit code and alerts an owner.

Published images are multi-architecture for `linux/amd64` and `linux/arm64`. Pin a calendar tag or,
preferably, the verified digest described in [release artifacts and verification](../releases.md).
