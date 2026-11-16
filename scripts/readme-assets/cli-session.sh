#!/bin/sh
# The demo session captured for the README's CLI image. Sourced by capture-cli.sh, which provides
# /out/cli-session.ansi, the in-tree File Station mock and a profile named "documents".
#
# `show` prints a prompt line for the command it is about to run, then that command's real output,
# captured under a pseudo-terminal so the binary behaves as it does in a terminal (progress line,
# colours). The prompt line is the command being run; nothing else is written by hand.
#
# One line of real output is dropped: the loopback mock can only speak plain HTTP, so the binary
# prints its "HTTP is enabled" warning once per command. A picture of that warning would suggest
# HTTP is the normal way to use the tool, so the single warning line is removed. Everything else,
# including every progress frame, is kept byte for byte.

export SDSYNC_LOG_LEVEL=error

show() {
    {
        printf '\033[32m$\033[0m %s\n' "$*"
        script -qec "stty cols 120 rows 50; $*" /dev/null |
            sed '/^warning: HTTP is enabled; DSM credentials, OTP codes, and file data may be exposed in transit/d'
        printf '\n'
    } >>/out/cli-session.ansi
}

show synology-drive-sync plan --profile documents
show synology-drive-sync sync --profile documents
show synology-drive-sync sync --profile documents
