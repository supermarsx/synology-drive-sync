#!/usr/bin/env bash
set -euo pipefail
umask 077

usage() {
    cat <<'EOF'
Usage: run-compose.sh ACTION

Actions:
  validate    Validate host inputs and the rendered Compose model.
  build       Build or upgrade the local image with refreshed base images.
  run         Run the configured finite update-only sync job.
  plan        Run a non-mutating plan and preserve exit code 10 for changes.
  doctor      Authenticate and check the configured target without mutation.
  write-test  Explicitly run the disposable target write probe.
  restart     Stop the managed job, then start one new configured sync.
  stop        Send SIGTERM and wait up to 120 seconds for the managed job.
  status      Show the managed container state, if it exists.
  logs        Tail the bounded host-side managed-job log.
  cleanup     Remove one stopped managed-job container; retain the host log.
EOF
}

action=${1:-}
if [[ $# -ne 1 ]]; then
    usage >&2
    exit 64
fi
case "$action" in
    validate|build|run|plan|doctor|write-test|restart|stop|status|logs|cleanup) ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown action: $action" >&2; usage >&2; exit 64 ;;
esac

command -v docker >/dev/null 2>&1 || { echo 'docker is required' >&2; exit 69; }
script_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
compose_dir=${SDSYNC_COMPOSE_DIR:-"$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"}
[[ "$compose_dir" = /* && -d "$compose_dir" && ! -L "$compose_dir" ]] || {
    echo "SDSYNC_COMPOSE_DIR must be an absolute, non-symlink directory: $compose_dir" >&2
    exit 73
}
compose_file="$compose_dir/compose.yaml"
totp_compose_file="$compose_dir/compose.totp.yaml"
[[ -f "$compose_file" && ! -L "$compose_file" ]] || {
    echo "compose.yaml is missing or linked: $compose_file" >&2
    exit 66
}
compose=(docker compose --project-directory "$compose_dir" -f "$compose_file")

runtime_uid=${SDSYNC_RUNTIME_UID:-$(id -u)}
runtime_gid=${SDSYNC_RUNTIME_GID:-$(id -g)}
[[ "$runtime_uid" =~ ^[1-9][0-9]{0,9}$ ]] && (( runtime_uid <= 2147483647 )) || {
    echo 'SDSYNC_RUNTIME_UID must be an integer from 1 through 2147483647' >&2
    exit 64
}
[[ "$runtime_gid" =~ ^[1-9][0-9]{0,9}$ ]] && (( runtime_gid <= 2147483647 )) || {
    echo 'SDSYNC_RUNTIME_GID must be an integer from 1 through 2147483647' >&2
    exit 64
}
export SDSYNC_RUNTIME_UID=$runtime_uid SDSYNC_RUNTIME_GID=$runtime_gid

state_root=${XDG_STATE_HOME:-"${HOME:?HOME is required}/.local/state"}
[[ "$state_root" = /* && "$state_root" != / ]] || {
    echo "XDG_STATE_HOME must be an absolute path other than /" >&2
    exit 64
}
state_dir="$state_root/synology-drive-sync"
if [[ ! -e "$state_dir" ]]; then
    mkdir -p -m 0700 -- "$state_dir"
fi
[[ -d "$state_dir" && ! -L "$state_dir" ]] || {
    echo "state path must be a non-symlink directory: $state_dir" >&2
    exit 73
}
state_mode=$(stat -c '%a' -- "$state_dir" 2>/dev/null || stat -f '%Lp' "$state_dir")
state_owner=$(stat -c '%u' -- "$state_dir" 2>/dev/null || stat -f '%u' "$state_dir")
[[ "$state_mode" = 700 && "$state_owner" = "$(id -u)" ]] || {
    echo "state directory must be owned by this account with mode 0700: $state_dir" >&2
    exit 77
}
log_file="$state_dir/compose.log"
lock_file="$state_dir/compose.lock"
container_name=${SDSYNC_COMPOSE_CONTAINER_NAME:-synology-drive-sync-managed-job}
[[ "$container_name" =~ ^synology-drive-sync-[A-Za-z0-9_.-]{1,106}$ ]] || {
    echo "SDSYNC_COMPOSE_CONTAINER_NAME is invalid" >&2
    exit 64
}

container_exists() {
    docker container inspect "$container_name" >/dev/null 2>&1
}

case "$action" in
    status)
        if container_exists; then
            docker container inspect --format \
                'name={{.Name}} state={{.State.Status}} exit={{.State.ExitCode}} started={{.State.StartedAt}} finished={{.State.FinishedAt}}' \
                "$container_name"
        else
            echo "managed container is absent: $container_name"
        fi
        exit 0
        ;;
    logs)
        if [[ -f "$log_file" && ! -L "$log_file" ]]; then
            tail -n 100 -- "$log_file"
        else
            echo "managed host log is absent: $log_file"
        fi
        exit 0
        ;;
    stop)
        if container_exists; then
            docker stop --time 120 "$container_name"
        else
            echo "managed container is already absent: $container_name"
        fi
        exit 0
        ;;
    cleanup)
        if ! container_exists; then
            echo "managed container is already absent: $container_name"
            exit 0
        fi
        running=$(docker container inspect --format '{{.State.Running}}' "$container_name")
        [[ "$running" = false ]] || { echo 'refusing to remove a running managed container' >&2; exit 75; }
        docker rm "$container_name"
        exit 0
        ;;
esac

if [[ -n "${SDSYNC_PASSWORD+x}" || -n "${SDSYNC_REMOTE_LOG_TOKEN+x}" ]]; then
    echo 'raw password and remote-log token values are not supported; use secret files' >&2
    exit 64
fi

require_private_file() {
    local path=$1 label=$2 mode owner
    [[ "$path" = /* && -f "$path" && ! -L "$path" && -r "$path" ]] || {
        echo "$label must be an absolute, readable, non-symlink regular file: $path" >&2
        exit 66
    }
    mode=$(stat -c '%a' -- "$path" 2>/dev/null || stat -f '%Lp' "$path")
    owner=$(stat -c '%u' -- "$path" 2>/dev/null || stat -f '%u' "$path")
    [[ "$mode" =~ ^(400|600)$ && "$owner" = "$(id -u)" ]] || {
        echo "$label must be owned by this account with mode 0400 or 0600: $path" >&2
        exit 77
    }
}

: "${SDSYNC_URL:?SDSYNC_URL is required}"
: "${SDSYNC_USERNAME:?SDSYNC_USERNAME is required}"
: "${SDSYNC_SOURCE:?SDSYNC_SOURCE is required}"
: "${SDSYNC_REMOTE:?SDSYNC_REMOTE is required}"
: "${SDSYNC_PASSWORD_FILE:?SDSYNC_PASSWORD_FILE is required}"
[[ "$SDSYNC_SOURCE" = /* && -d "$SDSYNC_SOURCE" && ! -L "$SDSYNC_SOURCE" ]] || {
    echo "SDSYNC_SOURCE must be an absolute, non-symlink directory" >&2
    exit 66
}
require_private_file "$SDSYNC_PASSWORD_FILE" 'password file'
if [[ -n "${SDSYNC_TOTP_SECRET_FILE:-}" ]]; then
    require_private_file "$SDSYNC_TOTP_SECRET_FILE" 'TOTP secret file'
    [[ -f "$totp_compose_file" && ! -L "$totp_compose_file" ]] || {
        echo "TOTP overlay is missing or linked: $totp_compose_file" >&2
        exit 66
    }
    compose+=(-f "$totp_compose_file")
fi

"${compose[@]}" config --quiet
if [[ "$action" = validate ]]; then
    echo 'Compose inputs and rendered model are valid.'
    exit 0
fi
if [[ "$action" = build ]]; then
    "${compose[@]}" build --pull sync
    exit 0
fi

# A restart must be able to stop the active container before waiting for the
# lock held by its current managed runner. Remember the immutable container ID
# so a direct actor cannot swap the fixed name underneath this transition.
restart_container_id=
if [[ "$action" = restart ]] && container_exists; then
    restart_container_id=$(docker container inspect --format '{{.Id}}' "$container_name")
    [[ -n "$restart_container_id" ]] || { echo 'managed container has no inspectable ID' >&2; exit 75; }
    running=$(docker container inspect --format '{{.State.Running}}' "$restart_container_id")
    if [[ "$running" = true ]]; then
        docker stop --time 120 "$restart_container_id"
    elif [[ "$running" != false ]]; then
        echo "managed container has unexpected running state: $running" >&2
        exit 75
    fi
fi

command -v flock >/dev/null 2>&1 || {
    echo 'flock is required for managed Compose non-overlap' >&2
    exit 69
}
if [[ -L "$lock_file" || ( -e "$lock_file" && ! -f "$lock_file" ) ]]; then
    echo "Compose lock must be a non-symlink regular file: $lock_file" >&2
    exit 73
fi
touch -- "$lock_file"
chmod 0600 "$lock_file"
exec 9>>"$lock_file"
if [[ "$action" = restart ]]; then
    lock_arguments=(-w 10 9)
else
    lock_arguments=(-n 9)
fi
if ! flock "${lock_arguments[@]}"; then
    echo "another managed Compose run holds $lock_file" >&2
    exit 75
fi

if [[ "$action" = restart && -n "$restart_container_id" ]] && container_exists; then
    current_container_id=$(docker container inspect --format '{{.Id}}' "$container_name")
    if [[ "$current_container_id" != "$restart_container_id" ]]; then
        echo 'managed container identity changed during restart; refusing to affect its replacement' >&2
        exit 75
    fi
    running=$(docker container inspect --format '{{.State.Running}}' "$restart_container_id")
    [[ "$running" = false ]] || {
        echo 'managed container resumed during restart; refusing to remove it' >&2
        exit 75
    }
    docker rm "$restart_container_id"
fi
if container_exists; then
    echo "managed container already exists; inspect it with status/logs, then stop and cleanup intentionally" >&2
    exit 75
fi

if [[ -e "$log_file" && ( ! -f "$log_file" || -L "$log_file" ) ]]; then
    echo "managed log must be a non-symlink regular file: $log_file" >&2
    exit 73
fi
if [[ -f "$log_file" ]] && (( $(wc -c < "$log_file") >= 10485760 )); then
    [[ ! -e "$log_file.3" || ( -f "$log_file.3" && ! -L "$log_file.3" ) ]] || exit 73
    rm -f -- "$log_file.3"
    for index in 2 1; do
        [[ ! -e "$log_file.$index" || ( -f "$log_file.$index" && ! -L "$log_file.$index" ) ]] || exit 73
        [[ ! -f "$log_file.$index" ]] || mv -- "$log_file.$index" "$log_file.$((index + 1))"
    done
    mv -- "$log_file" "$log_file.1"
fi
touch -- "$log_file"
chmod 0600 "$log_file"

run_arguments=(run --name "$container_name" sync)
case "$action" in
    run|restart) ;;
    plan)
        run_arguments+=(plan /source "$SDSYNC_REMOTE" --password-file /run/secrets/sdsync_password --no-vault --jobs "${SDSYNC_JOBS:-2}" --exit-code)
        ;;
    doctor)
        run_arguments+=(doctor --password-file /run/secrets/sdsync_password --no-vault target "$SDSYNC_REMOTE")
        ;;
    write-test)
        run_arguments+=(doctor --password-file /run/secrets/sdsync_password --no-vault target "$SDSYNC_REMOTE" --write-test)
        ;;
esac
if [[ -n "${SDSYNC_TOTP_SECRET_FILE:-}" && "$action" != run && "$action" != restart ]]; then
    # Parent-command authentication flags must precede the doctor target subcommand.
    if [[ "$action" = doctor || "$action" = write-test ]]; then
        run_arguments=(run --name "$container_name" sync doctor --password-file /run/secrets/sdsync_password --totp-secret-file /run/secrets/sdsync_totp --no-vault target "$SDSYNC_REMOTE")
        [[ "$action" != write-test ]] || run_arguments+=(--write-test)
    else
        run_arguments+=(--totp-secret-file /run/secrets/sdsync_totp)
    fi
fi

set +e
"${compose[@]}" "${run_arguments[@]}" 2>&1 | tee -a "$log_file"
status=${PIPESTATUS[0]}
set -e
if (( status != 0 )); then
    echo "managed Compose action $action failed with exit status $status" >&2
fi
exit "$status"
