# Docker and Compose finite jobs

The image is a finite sync job, not a daemon. It runs as UID/GID `10001`; the Compose model drops capabilities, enables `no-new-privileges`, makes the container filesystem read-only, and bind-mounts the authoritative source at `/source` read-only.

Use `run-compose.sh` for managed host runs. It validates the rendered model and host inputs, rejects linked/broadly readable secret files, exports the calling account's nonzero UID/GID for the container, holds a private host `flock`, assigns one fixed collision-detecting managed container identity, retains the stopped container for status, and captures output in a mode-`0600` host log rotated at 10 MiB with three backups. Running as the caller lets the native Linux container read that caller-owned mode-`0400`/`0600` secret and source files. Validated `SDSYNC_RUNTIME_UID`/`SDSYNC_RUNTIME_GID` overrides are available for deliberately mapped deployments.

Direct `docker compose run` bypasses the wrapper's ownership mapping, input validation, locking, and diagnostics. Its hardened default UID/GID is `10001:10001`, so every bind-mounted source and secret must be readable by that identity; do not make a secret broadly readable as a workaround.

## Install, validate, and upgrade

Set required values without putting secret contents in the environment:

```sh
export SDSYNC_URL=https://files.example.com
export SDSYNC_USERNAME=mirror-bot
export SDSYNC_SOURCE=/srv/export
export SDSYNC_REMOTE=/team/export
export SDSYNC_PASSWORD_FILE=/secure/location/dsm-password
chmod 0600 "$SDSYNC_PASSWORD_FILE"

packaging/docker/run-compose.sh validate
packaging/docker/run-compose.sh build
```

`build` uses `docker compose build --pull` so an explicit upgrade refreshes pinned build inputs and replaces the local tag only after a successful build. Re-run `validate`, `doctor`, and `plan` before the first sync from a new image. If the repository/Compose files live elsewhere, set `SDSYNC_COMPOSE_DIR` to that absolute non-symlink directory.

Compose mounts the password as `/run/secrets/sdsync_password`; only its path reaches `--password-file`, and the job uses `--no-vault`. For TOTP, set `SDSYNC_TOTP_SECRET_FILE` to a separate owner-only Base32 seed file. The runner automatically adds `compose.totp.yaml` and uses `/run/secrets/sdsync_totp`. A current OTP can be supplied only for a deliberately ephemeral direct run; the managed wrapper intentionally accepts long-lived secret files instead.

## Lifecycle and diagnostics

```sh
# non-mutating validation
packaging/docker/run-compose.sh doctor
packaging/docker/run-compose.sh plan       # exits 10 when changes are pending

# explicit disposable write proof; never schedule this by default
packaging/docker/run-compose.sh write-test

# finite sync lifecycle
packaging/docker/run-compose.sh run
packaging/docker/run-compose.sh status
packaging/docker/run-compose.sh logs
packaging/docker/run-compose.sh stop
packaging/docker/run-compose.sh restart
packaging/docker/run-compose.sh cleanup
```

`run` retains the stopped container so `status` preserves its exact exit code; `logs` reads the bounded host copy. Run `cleanup` before a later fresh `run`. `restart` is explicit: it validates inputs, records the managed container ID, stops that exact container with a 120-second grace period, waits up to 10 seconds for its old runner lock, rechecks the ID/state, removes it, and starts one new job. An identity change or lock timeout fails closed instead of touching a replacement. Do not use restart as a blind failure retry—inspect `status`, `logs`, and a fresh `plan` first. `stop` sends `SIGTERM` and allows 120 seconds; the CLI attempts cooperative cancellation and remote-task cleanup.

The fixed managed container name and host lock coordinate invocations using this wrapper and state directory on one host. They do not coordinate direct Docker commands, a different `SDSYNC_COMPOSE_CONTAINER_NAME`, another OS account/state directory, or another Docker host. Use one scheduler invocation or an external distributed mutex for those cases. Schedule the finite wrapper from an orchestrator that records nonzero exit codes; never add `restart: always` or an automatic blind retry.

The Dockerfile health check is a side-effect-free executable/version probe. Because a successful finite job exits, container health is not a scheduler-success signal; the process exit code and retained host log are authoritative.

## Profiles and batches

The supplied Compose model deliberately mounts one source at `/source` and defines one direct target. It does not pretend that multiple arbitrary host folders are available inside the container. For a multi-profile batch, create a reviewed site-owned Compose override that bind-mounts every configured source read-only plus one non-secret config, then invoke the CLI's `--config ... sync --profiles ...`/`--all-profiles` form under the same host lock. Every profile must use secret-file paths available in the container; `--password-stdin` is unsuitable for unattended batches.

If those mount and credential requirements are not uniform, use separate finite jobs but coordinate them with one host/distributed lock. A batch processes targets sequentially in deterministic profile-name order and needs a whole-workload timeout plus `--max-total-delete` when deletion is enabled.

The packaged direct command is update-only. Complete the [disposable production acceptance runbook](../../docs/production-acceptance.md) before creating any deletion-enabled override.

## Disable and uninstall

Disable future runs in the external scheduler first, then `stop`, inspect, and `cleanup`. `docker compose down --remove-orphans` removes project containers/networks but intentionally does not remove host source, secret, state, or log files. Remove the local image only after confirming no other deployment uses its tag. Keep credentials and logs until recovery/audit needs are resolved.
