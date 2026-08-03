# Container deployment

The image is a finite sync job, not a long-running service. It runs as UID/GID `10001`, and the Compose example drops all capabilities, enables `no-new-privileges`, makes the container filesystem read-only, and bind-mounts the authoritative source at `/source` read-only.

Set the required Compose variables and point `SDSYNC_PASSWORD_FILE` at a mode-`0600` file outside this repository:

```sh
export SDSYNC_URL=https://files.example.com
export SDSYNC_USERNAME=mirror-bot
export SDSYNC_SOURCE=/srv/export
export SDSYNC_REMOTE=/team/export
export SDSYNC_PASSWORD_FILE=/secure/location/dsm-password
docker compose run --rm sync
```

Compose mounts the password as a Docker secret. The command passes only its protected mount path through `--password-file`, so the password itself is neither an image layer, an environment value, nor a command-line argument. Container deployments use `--no-vault` because a normal container has no persistent desktop Secret Service session.

For TOTP, point `SDSYNC_TOTP_SECRET_FILE` at a separate protected Base32 seed file and use the supplied overlay:

```sh
export SDSYNC_TOTP_SECRET_FILE=/secure/location/dsm-totp
docker compose -f compose.yaml -f compose.totp.yaml run --rm sync
```

The overlay mounts it at `/run/secrets/sdsync_totp` and passes only that path to
`--totp-secret-file`. A generated one-time code may instead be supplied as
`SDSYNC_OTP` for one ephemeral run. Keep the long-lived seed in a Docker secret,
never in Compose environment variables.

The Dockerfile health check runs `synology-drive-sync --version`. It is a side-effect-free executable probe suitable for this finite, one-shot job and does not require a configured DSM endpoint.
