#!/bin/sh
set -eu
umask 077

binary=/usr/local/bin/synology-drive-sync
config=${1:-"${XDG_CONFIG_HOME:-$HOME/.config}/synology-drive-sync/cron.env"}

private_file_metadata() {
    field=$1
    path=$2
    case "$field" in
        mode) stat -c '%a' -- "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null ;;
        owner) stat -c '%u' -- "$path" 2>/dev/null || stat -f '%u' "$path" 2>/dev/null ;;
        *) return 1 ;;
    esac
}

require_private_file() {
    path=$1
    label=$2
    if [ -L "$path" ] || [ ! -f "$path" ] || [ ! -r "$path" ]; then
        echo "$label must be a readable, non-symlink regular file: $path" >&2
        exit 66
    fi
    mode=$(private_file_metadata mode "$path") || {
        echo "cannot verify permissions for $label: $path" >&2
        exit 66
    }
    case "$mode" in
        400|600) ;;
        *) echo "$label must have mode 0400 or 0600, found $mode: $path" >&2; exit 77 ;;
    esac
    owner=$(private_file_metadata owner "$path") || {
        echo "cannot verify ownership for $label: $path" >&2
        exit 66
    }
    if [ "$owner" != "$(id -u)" ]; then
        echo "$label must be owned by the cron account: $path" >&2
        exit 77
    fi
}

require_private_directory() {
    path=$1
    label=$2
    if [ -L "$path" ] || [ ! -d "$path" ]; then
        echo "$label must be a non-symlink directory: $path" >&2
        exit 73
    fi
    mode=$(private_file_metadata mode "$path") || {
        echo "cannot verify permissions for $label: $path" >&2
        exit 66
    }
    [ "$mode" = 700 ] || {
        echo "$label must have mode 0700, found $mode: $path" >&2
        exit 77
    }
    owner=$(private_file_metadata owner "$path") || {
        echo "cannot verify ownership for $label: $path" >&2
        exit 66
    }
    [ "$owner" = "$(id -u)" ] || {
        echo "$label must be owned by the cron account: $path" >&2
        exit 77
    }
}

if [ "${SDSYNC_PASSWORD+x}" = x ] || [ "${SDSYNC_OTP+x}" = x ] ||
    [ "${SDSYNC_REMOTE_LOG_TOKEN+x}" = x ] || [ "${SDSYNC_REMOTE_LOG_TOKEN_ENV+x}" = x ]; then
    echo "the cron environment must not inherit password, OTP, or logging-token values" >&2
    exit 64
fi

require_private_file "$config" "cron configuration"
seen_names='|'
while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
        ''|'#'*) continue ;;
        *=*) name=${line%%=*} ;;
        *) echo "cron.env contains a non-assignment line" >&2; exit 64 ;;
    esac
    case "$name" in
        SDSYNC_EXECUTABLE|SDSYNC_CONFIG|SDSYNC_PROFILE|SDSYNC_PROFILES|SDSYNC_ALL_PROFILES|SDSYNC_MAX_TOTAL_DELETE|SDSYNC_BATCH_SHARED_CREDENTIALS|SDSYNC_LOCK_FILE)
            ;;
        SDSYNC_OUTPUT|SDSYNC_URL|SDSYNC_USERNAME|SDSYNC_SOURCE|SDSYNC_REMOTE)
            ;;
        SDSYNC_PASSWORD_FILE|SDSYNC_TOTP_SECRET_FILE|SDSYNC_COMPARE|SDSYNC_JOBS|SDSYNC_ALLOW_EMPTY_SOURCE)
            ;;
        SDSYNC_RETRIES|SDSYNC_TIMEOUT|SDSYNC_CONNECT_TIMEOUT|SDSYNC_CA_CERTIFICATE|SDSYNC_ALLOW_HTTP|SDSYNC_DANGER_ACCEPT_INVALID_CERTS)
            ;;
        SDSYNC_QUIET|SDSYNC_LOG_LEVEL|SDSYNC_LOG_FORMAT|SDSYNC_LOG_FILE|SDSYNC_PROGRESS)
            ;;
        SDSYNC_REMOTE_LOG_URL|SDSYNC_REMOTE_LOG_TOKEN_FILE|SDSYNC_REMOTE_LOG_MODE|SDSYNC_DELETE|SDSYNC_MAX_DELETE)
            ;;
        SDSYNC_PASSWORD|SDSYNC_OTP|SDSYNC_REMOTE_LOG_TOKEN|SDSYNC_REMOTE_LOG_TOKEN_ENV)
            echo "cron.env must use secret-file paths, not $name" >&2
            exit 64
            ;;
        *) echo "cron.env contains unsupported assignment: $name" >&2; exit 64 ;;
    esac
    case "$seen_names" in
        *"|$name|"*) echo "cron.env assigns $name more than once" >&2; exit 64 ;;
    esac
    seen_names="${seen_names}${name}|"
    # Quoted export assigns the already-read bytes literally. The file is never sourced or eval'd.
    export "$line"
done < "$config"

binary=${SDSYNC_EXECUTABLE:-$binary}
case "$binary" in
    /*) ;;
    *) echo "SDSYNC_EXECUTABLE must be an absolute path" >&2; exit 64 ;;
esac
if [ -L "$binary" ] || [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
    echo "SDSYNC_EXECUTABLE must be an executable, non-symlink regular file: $binary" >&2
    exit 69
fi

if [ -n "${SDSYNC_REMOTE_LOG_TOKEN_FILE:-}" ]; then
    require_private_file "$SDSYNC_REMOTE_LOG_TOKEN_FILE" "remote logging token file"
fi

state_root=${XDG_STATE_HOME:-"$HOME/.local/state"}
case "$state_root" in
    /*) ;;
    *) echo "XDG_STATE_HOME must be an absolute path for unattended cron use" >&2; exit 64 ;;
esac
[ "$state_root" != / ] || { echo "XDG_STATE_HOME must not be filesystem root" >&2; exit 64; }
state_root=${state_root%/}
if [ -L "$state_root" ] || { [ -e "$state_root" ] && [ ! -d "$state_root" ]; }; then
    echo "XDG_STATE_HOME must be a non-symlink directory: $state_root" >&2
    exit 73
fi
if [ ! -e "$state_root" ]; then
    mkdir -p -- "$state_root"
fi
state_root=$(CDPATH='' cd -- "$state_root" && pwd -P)
state_dir="$state_root/synology-drive-sync"
if [ ! -e "$state_dir" ]; then
    mkdir -m 0700 -- "$state_dir"
fi
require_private_directory "$state_dir" "cron state directory"
lock_file=${SDSYNC_LOCK_FILE:-"$state_dir/service.lock"}
case "$lock_file" in
    /*) ;;
    *) echo "SDSYNC_LOCK_FILE must be an absolute path" >&2; exit 64 ;;
esac
case "$lock_file" in
    "$state_dir"/*/*) echo "SDSYNC_LOCK_FILE must be a direct child of $state_dir" >&2; exit 64 ;;
    "$state_dir"/*) ;;
    *) echo "SDSYNC_LOCK_FILE must be a direct child of $state_dir" >&2; exit 64 ;;
esac
if [ -e "$lock_file" ] || [ -L "$lock_file" ]; then
    require_private_file "$lock_file" "cron lock file"
else
    : > "$lock_file"
    chmod 0600 "$lock_file"
fi
command -v flock >/dev/null 2>&1 || {
    echo "flock is required for non-overlapping cron execution" >&2
    exit 69
}
exec 9>>"$lock_file"
if ! flock -n 9; then
    echo "synology-drive-sync skipped: previous run still holds $lock_file" >&2
    exit 75
fi
printf '%s\n' "$$" > "$lock_file"

config_file=${SDSYNC_CONFIG:-}
profile=${SDSYNC_PROFILE:-}
profiles=${SDSYNC_PROFILES:-}
all_profiles=${SDSYNC_ALL_PROFILES:-false}
maximum_total=${SDSYNC_MAX_TOTAL_DELETE:-}
case "$all_profiles" in
    true|false) ;;
    *) echo "SDSYNC_ALL_PROFILES must be true or false" >&2; exit 64 ;;
esac
if [ -n "$profiles" ] && [ "$all_profiles" = true ]; then
    echo "SDSYNC_PROFILES and SDSYNC_ALL_PROFILES cannot be combined" >&2
    exit 64
fi
if { [ -n "$profiles" ] || [ "$all_profiles" = true ]; } && [ -n "$profile" ]; then
    echo "SDSYNC_PROFILE cannot be combined with batch profile selection" >&2
    exit 64
fi
if { [ -n "$profiles" ] || [ "$all_profiles" = true ] || [ -n "$profile" ]; } &&
    [ -z "$config_file" ]; then
    echo "profile selection requires SDSYNC_CONFIG" >&2
    exit 64
fi

batch_requested=false
if [ -n "$profiles" ] || [ "$all_profiles" = true ]; then
    batch_requested=true
fi
shared_batch_credentials=${SDSYNC_BATCH_SHARED_CREDENTIALS:-false}
case "$shared_batch_credentials" in
    true|false) ;;
    *) echo "SDSYNC_BATCH_SHARED_CREDENTIALS must be true or false" >&2; exit 64 ;;
esac
if [ "$batch_requested" = true ] && [ "$shared_batch_credentials" != true ]; then
    echo "batch mode is blocked by default because one cron password/TOTP override would be applied to every profile" >&2
    echo "set SDSYNC_BATCH_SHARED_CREDENTIALS=true only when every selected profile intentionally shares those credentials; otherwise use separate cron jobs" >&2
    exit 64
fi
if [ "$batch_requested" = false ] && [ "$shared_batch_credentials" = true ]; then
    echo "SDSYNC_BATCH_SHARED_CREDENTIALS=true requires SDSYNC_PROFILES or SDSYNC_ALL_PROFILES=true" >&2
    exit 64
fi

: "${SDSYNC_PASSWORD_FILE:?SDSYNC_PASSWORD_FILE is missing from cron.env}"
require_private_file "$SDSYNC_PASSWORD_FILE" "password file"
if [ -n "${SDSYNC_TOTP_SECRET_FILE:-}" ]; then
    require_private_file "$SDSYNC_TOTP_SECRET_FILE" "TOTP secret file"
fi
if [ -n "$maximum_total" ]; then
    case "$maximum_total" in
        *[!0-9]*) echo "SDSYNC_MAX_TOTAL_DELETE must be a non-negative integer" >&2; exit 64 ;;
    esac
    if [ -z "$profiles" ] && [ "$all_profiles" = false ]; then
        echo "SDSYNC_MAX_TOTAL_DELETE requires batch profile selection" >&2
        exit 64
    fi
fi

if [ -n "$config_file" ]; then
    if [ -L "$config_file" ] || [ ! -f "$config_file" ] || [ ! -r "$config_file" ]; then
        echo "SDSYNC_CONFIG must be a readable, non-symlink regular file: $config_file" >&2
        exit 66
    fi
    "$binary" --config "$config_file" --quiet config validate
fi

if [ -n "$profiles" ]; then
    set -- --config "$config_file" sync --profiles "$profiles"
elif [ "$all_profiles" = true ]; then
    set -- --config "$config_file" sync --all-profiles
elif [ -n "$config_file" ]; then
    set -- --config "$config_file" sync
    [ -z "$profile" ] || set -- "$@" --profile "$profile"
else
    : "${SDSYNC_URL:?SDSYNC_URL is required without SDSYNC_CONFIG}"
    : "${SDSYNC_USERNAME:?SDSYNC_USERNAME is required without SDSYNC_CONFIG}"
    : "${SDSYNC_SOURCE:?SDSYNC_SOURCE is required without SDSYNC_CONFIG}"
    : "${SDSYNC_REMOTE:?SDSYNC_REMOTE is required without SDSYNC_CONFIG}"
    set -- sync "$SDSYNC_SOURCE" "$SDSYNC_REMOTE" \
        --url "$SDSYNC_URL" --username "$SDSYNC_USERNAME"
fi

set -- "$@" --password-file "$SDSYNC_PASSWORD_FILE" --no-vault
[ -z "${SDSYNC_TOTP_SECRET_FILE:-}" ] ||
    set -- "$@" --totp-secret-file "$SDSYNC_TOTP_SECRET_FILE"

jobs=${SDSYNC_JOBS:-2}
case "$jobs" in
    ''|*[!0-9]*) echo "SDSYNC_JOBS must be an integer from 1 through 16" >&2; exit 64 ;;
esac
if [ "$jobs" -lt 1 ] || [ "$jobs" -gt 16 ]; then
    echo "SDSYNC_JOBS must be an integer from 1 through 16" >&2
    exit 64
fi
set -- "$@" --jobs "$jobs"

case "${SDSYNC_DELETE:-false}" in
    true)
        maximum=${SDSYNC_MAX_DELETE:-100}
        case "$maximum" in
            ''|*[!0-9]*) echo "SDSYNC_MAX_DELETE must be a non-negative integer" >&2; exit 64 ;;
        esac
        set -- "$@" --delete --max-delete "$maximum"
        ;;
    false) set -- "$@" --no-delete ;;
    *) echo "SDSYNC_DELETE must be true or false" >&2; exit 64 ;;
esac
[ -z "$maximum_total" ] || set -- "$@" --max-total-delete "$maximum_total"

exec "$binary" "$@"
