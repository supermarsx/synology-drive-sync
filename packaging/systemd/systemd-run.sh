#!/bin/sh
set -eu

binary=/usr/local/bin/synology-drive-sync

: "${CREDENTIALS_DIRECTORY:?systemd did not provide its credentials directory}"

if [ "${SDSYNC_PASSWORD+x}" = x ] || [ "${SDSYNC_OTP+x}" = x ] ||
    [ "${SDSYNC_REMOTE_LOG_TOKEN+x}" = x ] || [ "${SDSYNC_REMOTE_LOG_TOKEN_ENV+x}" = x ]; then
    echo "sync.env must use credential-file paths, never password, OTP, or logging-token values" >&2
    exit 64
fi

lock_file=${SDSYNC_LOCK_FILE:-/var/lib/synology-drive-sync/service.lock}
case "$lock_file" in
    /*) ;;
    *) echo "SDSYNC_LOCK_FILE must be an absolute path" >&2; exit 64 ;;
esac
if [ -L "$lock_file" ] || { [ -e "$lock_file" ] && [ ! -f "$lock_file" ]; }; then
    echo "service lock must be a non-symlink regular file: $lock_file" >&2
    exit 73
fi
command -v flock >/dev/null 2>&1 || {
    echo "flock is required for shared service non-overlap" >&2
    exit 69
}
: >> "$lock_file"
chmod 0600 "$lock_file"
exec 9>>"$lock_file"
if ! flock -n 9; then
    echo "synology-drive-sync skipped: another managed run holds $lock_file" >&2
    exit 75
fi

config=${SDSYNC_CONFIG:-}
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
if { [ -n "$profiles" ] || [ "$all_profiles" = true ]; } && [ -z "$config" ]; then
    echo "batch profile selection requires SDSYNC_CONFIG" >&2
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
    echo "batch mode is blocked by default because one systemd password/TOTP override would be applied to every profile" >&2
    echo "set SDSYNC_BATCH_SHARED_CREDENTIALS=true only when every selected profile intentionally shares those credentials; otherwise use separate units" >&2
    exit 64
fi
if [ "$batch_requested" = false ] && [ "$shared_batch_credentials" = true ]; then
    echo "SDSYNC_BATCH_SHARED_CREDENTIALS=true requires SDSYNC_PROFILES or SDSYNC_ALL_PROFILES=true" >&2
    exit 64
fi

password_file="$CREDENTIALS_DIRECTORY/dsm-password"
if [ -L "$password_file" ] || [ ! -f "$password_file" ] || [ ! -r "$password_file" ]; then
    echo "systemd credential dsm-password is not a readable regular file" >&2
    exit 66
fi
if [ -n "$profile" ] && [ -z "$config" ]; then
    echo "SDSYNC_PROFILE requires SDSYNC_CONFIG" >&2
    exit 64
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

if [ -n "$config" ]; then
    if [ -L "$config" ] || [ ! -f "$config" ] || [ ! -r "$config" ]; then
        echo "SDSYNC_CONFIG must be a readable, non-symlink regular file: $config" >&2
        exit 66
    fi
    "$binary" --config "$config" --quiet config validate
fi

if [ -n "$profiles" ]; then
    set -- --config "$config" sync --profiles "$profiles"
elif [ "$all_profiles" = true ]; then
    set -- --config "$config" sync --all-profiles
elif [ -n "$config" ]; then
    set -- --config "$config" sync
    if [ -n "$profile" ]; then
        set -- "$@" --profile "$profile"
    fi
else
    : "${SDSYNC_URL:?SDSYNC_URL is required without SDSYNC_CONFIG}"
    : "${SDSYNC_USERNAME:?SDSYNC_USERNAME is required without SDSYNC_CONFIG}"
    : "${SDSYNC_SOURCE:?SDSYNC_SOURCE is required without SDSYNC_CONFIG}"
    : "${SDSYNC_REMOTE:?SDSYNC_REMOTE is required without SDSYNC_CONFIG}"
    set -- sync "$SDSYNC_SOURCE" "$SDSYNC_REMOTE" \
        --url "$SDSYNC_URL" --username "$SDSYNC_USERNAME"
fi

set -- "$@" --password-file "$password_file" --no-vault

case "${SDSYNC_USE_TOTP_CREDENTIAL:-false}" in
    true)
        totp_file="$CREDENTIALS_DIRECTORY/dsm-totp"
        if [ -L "$totp_file" ] || [ ! -f "$totp_file" ] || [ ! -r "$totp_file" ]; then
            echo "systemd credential dsm-totp is not a readable regular file" >&2
            exit 66
        fi
        set -- "$@" --totp-secret-file "$totp_file"
        ;;
    false) ;;
    *) echo "SDSYNC_USE_TOTP_CREDENTIAL must be true or false" >&2; exit 64 ;;
esac

case "${SDSYNC_USE_REMOTE_LOG_CREDENTIAL:-false}" in
    true)
        remote_log_token_file="$CREDENTIALS_DIRECTORY/remote-log-token"
        if [ -L "$remote_log_token_file" ] || [ ! -f "$remote_log_token_file" ] ||
            [ ! -r "$remote_log_token_file" ]; then
            echo "systemd credential remote-log-token is not a readable regular file" >&2
            exit 66
        fi
        set -- "$@" --remote-log-token-file "$remote_log_token_file"
        ;;
    false) ;;
    *) echo "SDSYNC_USE_REMOTE_LOG_CREDENTIAL must be true or false" >&2; exit 64 ;;
esac

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
    false)
        # Override a profile that might otherwise enable destructive mirroring.
        set -- "$@" --no-delete
        ;;
    *) echo "SDSYNC_DELETE must be true or false" >&2; exit 64 ;;
esac

if [ -n "$maximum_total" ]; then
    set -- "$@" --max-total-delete "$maximum_total"
fi

exec "$binary" "$@"
