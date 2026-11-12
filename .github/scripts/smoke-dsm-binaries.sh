#!/usr/bin/env bash
# Run the shipped DSM binaries, not a debug build, on the CPU the package targets.
#
# Usage: smoke-dsm-binaries.sh CORE_BINARY API_BINARY
#
# Cross-built binaries run under the pinned QEMU user emulator (QEMU_BINARY, in RUNNER_TEMP) on
# QEMU_CPU, which for armv7 is a D16-only cortex-a9: an illegal instruction there (a toolchain that
# emits VFP D17 or NEON) kills the process, and nothing else in CI executes a release binary on
# that CPU. Native builds run as they are. Every check names itself and the step fails on the first
# one that does not hold.
set -euo pipefail

die() {
    printf 'smoke-dsm-binaries: %s\n' "$*" >&2
    exit 1
}

[[ $# -eq 2 ]] || die "usage: smoke-dsm-binaries.sh CORE_BINARY API_BINARY"
core=$1
api=$2
[[ -x "$core" ]] || die "not an executable: $core"
[[ -x "$api" ]] || die "not an executable: $api"

emulator=""
if [[ -n "${QEMU_BINARY:-}" ]]; then
    [[ -n "${RUNNER_TEMP:-}" ]] || die "RUNNER_TEMP is missing"
    emulator="$RUNNER_TEMP/$QEMU_BINARY"
    [[ -x "$emulator" ]] || die "pinned QEMU emulator is missing: $emulator"
    [[ -n "${QEMU_CPU:-}" ]] || die "QEMU_CPU is missing for $QEMU_BINARY"
fi

# run BINARY ARGS...: the extracted binfmt QEMU takes the first guest argument as argv[0].
run() {
    local binary=$1
    shift
    if [[ -n "$emulator" ]]; then
        QEMU_CPU="$QEMU_CPU" "$emulator" "$binary" "$(basename "$binary")" "$@"
    else
        "$binary" "$@"
    fi
}

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

version=$(run "$core" --version) || die "synology-drive-sync --version exited $? (a signal is 128 plus its number)"
[[ "$version" == synology-drive-sync\ * ]] || die "synology-drive-sync --version printed: $version"
printf 'ok: %s\n' "$version"

help_text=$(run "$core" --help) || die "synology-drive-sync --help exited $?"
grep -Fq 'synology-drive-sync doctor' <<<"$help_text" || die "synology-drive-sync --help did not print its examples"
printf 'ok: synology-drive-sync --help\n'

# A local-only scan: walks, stats and hashes real files and needs no network, so it runs code the
# version string does not reach.
mkdir -p "$workdir/source/folder"
printf 'smoke\n' >"$workdir/source/folder/file.txt"
scan=$(run "$core" doctor source "$workdir/source" 2>&1) || die "synology-drive-sync doctor source exited $?: $scan"
grep -Fq 'Source is healthy' <<<"$scan" || die "synology-drive-sync doctor source did not report a healthy source: $scan"
printf 'ok: synology-drive-sync doctor source\n'

# The daemon's lock probe: no config, no network, one flock(2) on a file.
: >"$workdir/daemon.lock"
run "$api" --daemon-lock-probe "$workdir/daemon.lock" || die "sdsync-dsm-api --daemon-lock-probe exited $?"
printf 'ok: sdsync-dsm-api --daemon-lock-probe\n'
