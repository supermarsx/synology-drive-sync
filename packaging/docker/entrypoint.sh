#!/bin/sh
set -eu

binary=/usr/local/bin/synology-drive-sync

if [ "$#" -eq 0 ]; then
    set -- --help
fi

exec "$binary" "$@"
