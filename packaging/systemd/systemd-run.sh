#!/bin/sh
set -eu

: "${SDSYNC_URL:?SDSYNC_URL is missing from sync.env}"
: "${SDSYNC_USERNAME:?SDSYNC_USERNAME is missing from sync.env}"
: "${SDSYNC_SOURCE:?SDSYNC_SOURCE is missing from sync.env}"
: "${SDSYNC_REMOTE:?SDSYNC_REMOTE is missing from sync.env}"
: "${CREDENTIALS_DIRECTORY:?systemd did not provide its credentials directory}"

password_file="$CREDENTIALS_DIRECTORY/dsm-password"
if [ ! -r "$password_file" ]; then
    echo "systemd credential dsm-password is not readable" >&2
    exit 66
fi

set -- sync "$SDSYNC_SOURCE" "$SDSYNC_REMOTE" --password-file "$password_file" --no-vault

case "${SDSYNC_USE_TOTP_CREDENTIAL:-false}" in
    true)
        totp_file="$CREDENTIALS_DIRECTORY/dsm-totp"
        if [ ! -r "$totp_file" ]; then
            echo "systemd credential dsm-totp is not readable" >&2
            exit 66
        fi
        set -- "$@" --totp-secret-file "$totp_file"
        ;;
    false)
        ;;
    *)
        echo "SDSYNC_USE_TOTP_CREDENTIAL must be true or false" >&2
        exit 64
        ;;
esac

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
