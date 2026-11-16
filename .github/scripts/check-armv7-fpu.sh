#!/usr/bin/env bash
# Refuse armv7 DSM binaries holding instructions a VFPv3-D16 CPU without NEON cannot execute.
#
# Usage: check-armv7-fpu.sh RUST_TARGET
#
# Run after the release build of synology-drive-sync and sdsync-dsm-api for RUST_TARGET, in the same
# environment (SDSYNC_BUILD_VERSION, CARGO_TARGET_DIR), and after every step that uses the built
# binaries: each binary is rebuilt with `cargo-zigbuild rustc ... -C strip=none` for its symbol
# table, and the shipped file is then put back, so a later cargo build would relink it. The QEMU
# smoke runs only the paths it reaches; check-armv7-fpu.py reads every instruction. It needs the
# llvm-tools component of the active toolchain.
set -euo pipefail

die() {
    printf 'check-armv7-fpu: %s\n' "$*" >&2
    exit 1
}

[[ $# -eq 1 ]] || die "usage: check-armv7-fpu.sh RUST_TARGET"
target=$1
release="${CARGO_TARGET_DIR:-target}/$target/release"
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

host=$(rustc -vV | sed -n 's/^host: //p')
[[ -n "$host" ]] || die "rustc -vV named no host"
tools="$(rustc --print sysroot)/lib/rustlib/$host/bin"
for tool in llvm-objdump llvm-objcopy; do
    [[ -x "$tools/$tool" ]] || die "$tools/$tool is missing: add the llvm-tools component"
done

work=$(mktemp -d)
binaries=(synology-drive-sync sdsync-dsm-api)
restore() {
    local bin
    for bin in "${binaries[@]}"; do
        if [[ -f "$work/$bin.shipped" ]]; then
            cp -p "$work/$bin.shipped" "$release/$bin"
        fi
    done
    rm -rf "$work"
}
trap restore EXIT

pairs=()
for bin in "${binaries[@]}"; do
    [[ -x "$release/$bin" ]] || die "not built: $release/$bin"
    cp -p "$release/$bin" "$work/$bin.shipped"
done
for bin in "${binaries[@]}"; do
    cargo-zigbuild rustc --release --locked --package synology-drive-sync --bin "$bin" \
        --target "$target" -- -C strip=none ||
        die "the symbolized rebuild of $bin failed"
    cp -p "$release/$bin" "$work/$bin.twin"
    cp -p "$work/$bin.shipped" "$release/$bin"
    pairs+=("$work/$bin.shipped" "$work/$bin.twin")
done

python3 "$script_dir/check-armv7-fpu.py" \
    --objdump "$tools/llvm-objdump" --objcopy "$tools/llvm-objcopy" "${pairs[@]}"
