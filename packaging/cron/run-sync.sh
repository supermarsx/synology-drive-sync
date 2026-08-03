#!/bin/sh
set -eu
umask 077

config=${1:-"${XDG_CONFIG_HOME:-$HOME/.config}/synology-drive-sync/cron.env"}
if [ ! -r "$config" ]; then
    echo "cron configuration is not readable: $config" >&2
    exit 66
fi

# The configuration is user-owned and mode 0600; it contains assignments only.
# shellcheck disable=SC1090
. "$config"

: "${SDSYNC_URL:?SDSYNC_URL is missing from cron.env}"
: "${SDSYNC_USERNAME:?SDSYNC_USERNAME is missing from cron.env}"
: "${SDSYNC_SOURCE:?SDSYNC_SOURCE is missing from cron.env}"
: "${SDSYNC_REMOTE:?SDSYNC_REMOTE is missing from cron.env}"
: "${SDSYNC_PASSWORD_FILE:?SDSYNC_PASSWORD_FILE is missing from cron.env}"

if [ ! -r "$SDSYNC_PASSWORD_FILE" ]; then
    echo "password file is not readable: $SDSYNC_PASSWORD_FILE" >&2
    exit 66
fi

set -- sync "$SDSYNC_SOURCE" "$SDSYNC_REMOTE" --password-file "$SDSYNC_PASSWORD_FILE" --no-vault

if [ -n "${SDSYNC_TOTP_SECRET_FILE:-}" ]; then
    if [ ! -r "$SDSYNC_TOTP_SECRET_FILE" ]; then
        echo "TOTP secret file is not readable: $SDSYNC_TOTP_SECRET_FILE" >&2
        exit 66
    fi
    set -- "$@" --totp-secret-file "$SDSYNC_TOTP_SECRET_FILE"
fi

jobs=${SDSYNC_JOBS:-2}
case "$jobs" in
    ''|*[!0-9]*)
        echo "SDSYNC_JOBS must be an integer" >&2
        exit 64
        ;;
esac
set -- "$@" --jobs "$jobs"

case "${SDSYNC_DELETE:-false}" in
    true)
        maximum=${SDSYNC_MAX_DELETE:-100}
        case "$maximum" in
            ''|*[!0-9]*)
                echo "SDSYNC_MAX_DELETE must be an integer" >&2
                exit 64
                ;;
        esac
        set -- "$@" --delete --max-delete "$maximum"
        ;;
    false)
        ;;
    *)
        echo "SDSYNC_DELETE must be true or false" >&2
        exit 64
        ;;
esac

exec /usr/local/bin/synology-drive-sync "$@"
