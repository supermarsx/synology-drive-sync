#!/bin/sh
set -eu
umask 077

[ "$#" -ge 1 ] || { echo "launchd wrapper requires an executable" >&2; exit 64; }
binary=$1
shift
case "$binary" in
    /*) ;;
    *) echo "launchd executable must be an absolute path" >&2; exit 64 ;;
esac
if [ -L "$binary" ] || [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
    echo "launchd executable must be an executable, non-symlink regular file: $binary" >&2
    exit 69
fi

logger_binary=${SDSYNC_LOGGER_EXECUTABLE:-/usr/bin/logger}
case "$logger_binary" in
    /*) ;;
    *) echo "SDSYNC_LOGGER_EXECUTABLE must be an absolute path" >&2; exit 64 ;;
esac
if [ -L "$logger_binary" ] || [ ! -f "$logger_binary" ] || [ ! -x "$logger_binary" ]; then
    echo "logger must be an executable, non-symlink regular file: $logger_binary" >&2
    exit 69
fi

temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/synology-drive-sync.launchd.XXXXXX")
diagnostic_pipe="$temporary_directory/stderr"
mkfifo -m 0600 "$diagnostic_pipe"
child=
logger_pid=

forward_termination() {
    if [ -n "$child" ]; then
        kill -TERM "$child" 2>/dev/null || true
    fi
}

cleanup() {
    trap - 0 1 2 15
    forward_termination
    [ -z "$logger_pid" ] || kill -TERM "$logger_pid" 2>/dev/null || true
    [ ! -p "$diagnostic_pipe" ] || rm -f -- "$diagnostic_pipe"
    [ ! -d "$temporary_directory" ] || rmdir -- "$temporary_directory"
}
trap cleanup 0
trap forward_termination 1 2 15

"$logger_binary" -t synology-drive-sync < "$diagnostic_pipe" &
logger_pid=$!
"$binary" "$@" 2>"$diagnostic_pipe" &
child=$!

set +e
while :; do
    wait "$child"
    status=$?
    kill -0 "$child" 2>/dev/null || break
done
set -e
child=

# EOF reaches logger only after the child closes its stderr. Reap it so launchd
# never observes completion before the final initialization/error line is sent.
set +e
wait "$logger_pid"
logger_status=$?
set -e
logger_pid=
if [ "$status" -eq 0 ] && [ "$logger_status" -ne 0 ]; then
    echo "unified-log forwarding failed with exit status $logger_status" >&2
    exit "$logger_status"
fi
exit "$status"
