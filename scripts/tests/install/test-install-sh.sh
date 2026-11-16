#!/bin/sh
# Tests scripts/install.sh against a local fake release. No network.
#
#   test-install-sh.sh [PATH_TO_INSTALL_SH]
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
installer=${1:-$here/../../install.sh}
installer=$(CDPATH='' cd -- "$(dirname -- "$installer")" && pwd)/$(basename -- "$installer")

root=$(mktemp -d "${TMPDIR:-/tmp}/sdsync-install-test.XXXXXX")
trap 'rm -rf "$root"' EXIT
sh "$here/make-fake-release.sh" "$root/release"

passed=0
failed=0
ok() {
    passed=$((passed + 1))
    printf 'ok   %s\n' "$1"
}
bad() {
    failed=$((failed + 1))
    printf 'FAIL %s\n' "$1"
    [ -z "${2:-}" ] || printf '%s\n' "$2" | sed 's/^/       | /'
}

# run NAME [ENV=VALUE ...]: runs the installer, leaves stdout+stderr in $log and the code in $code.
log=$root/log
code=0
run_installer() {
    env HOME="$root/home" SDSYNC_RELEASE_BASE_URL="file://$root/release" \
        SDSYNC_INSTALL_DIR="$dest" "$@" sh "$installer" >"$log" 2>&1 || code=$?
}
expect_success() {
    code=0
    run_installer "$@"
    [ "$code" -eq 0 ]
}
expect_failure() {
    code=0
    run_installer "$@"
    [ "$code" -ne 0 ]
}
contains() { grep -Fq -- "$1" "$log"; }

dest=$root/bin

# 1. Happy path: the latest release installs and runs.
if expect_success && [ "$("$dest/synology-drive-sync" --version)" = "synology-drive-sync 26.65" ] &&
    contains "Checksum verified" && contains "Installed synology-drive-sync 26.65" && contains "Uninstall:"; then
    ok "latest release installs and reports its version"
else
    bad "latest release installs and reports its version" "$(cat "$log")"
fi

# 2. The PATH hint appears when the directory is not on PATH.
if contains "is not on your PATH"; then ok "PATH hint printed"; else bad "PATH hint printed" "$(cat "$log")"; fi

# 3. Re-running the same version is idempotent.
if expect_success && contains "Reinstalled synology-drive-sync 26.65"; then
    ok "re-run of the same version is idempotent"
else
    bad "re-run of the same version is idempotent" "$(cat "$log")"
fi

# 4. A pinned older version, then the latest again, upgrades in place.
if expect_success SDSYNC_VERSION=26.64 && contains "Upgraded synology-drive-sync 26.65 -> synology-drive-sync 26.64"; then
    ok "pinning SDSYNC_VERSION installs that release"
else
    bad "pinning SDSYNC_VERSION installs that release" "$(cat "$log")"
fi
if expect_success && contains "Upgraded synology-drive-sync 26.64 -> synology-drive-sync 26.65" &&
    [ "$("$dest/synology-drive-sync" --version)" = "synology-drive-sync 26.65" ]; then
    ok "upgrade replaces the installed file"
else
    bad "upgrade replaces the installed file" "$(cat "$log")"
fi
if [ "$(find "$dest" -mindepth 1 | wc -l | tr -d ' ')" = 1 ]; then
    ok "no staging files left behind"
else
    bad "no staging files left behind" "$(find "$dest" -mindepth 1)"
fi

# 5. Checksum mismatch fails closed and leaves the installed file alone.
if expect_failure SDSYNC_VERSION=26.63 && contains "SHA-256 mismatch" &&
    [ "$("$dest/synology-drive-sync" --version)" = "synology-drive-sync 26.65" ]; then
    ok "checksum mismatch refuses to install and keeps the old binary"
else
    bad "checksum mismatch refuses to install and keeps the old binary" "$(cat "$log")"
fi

# 6. Missing asset.
if expect_failure SDSYNC_VERSION=26.62 && contains "has no synology-drive-sync-26.62-"; then
    ok "missing asset fails with a clear message"
else
    bad "missing asset fails with a clear message" "$(cat "$log")"
fi

# 7. Missing SHA256SUMS.
if expect_failure SDSYNC_VERSION=26.61 && contains "no SHA256SUMS"; then
    ok "missing SHA256SUMS refuses to install"
else
    bad "missing SHA256SUMS refuses to install" "$(cat "$log")"
fi

# 8. A binary that reports another version is refused.
if expect_failure SDSYNC_VERSION=26.60 && contains "expected 'synology-drive-sync 26.60'"; then
    ok "binary reporting the wrong version is refused"
else
    bad "binary reporting the wrong version is refused" "$(cat "$log")"
fi

# 9. A version that does not exist.
if expect_failure SDSYNC_VERSION=26.99 && contains "has no synology-drive-sync-26.99-"; then
    ok "unknown release fails with a clear message"
else
    bad "unknown release fails with a clear message" "$(cat "$log")"
fi

# 10. A malformed version is refused before any download.
if expect_failure SDSYNC_VERSION=latest && contains "release version must look like"; then
    ok "malformed version is refused"
else
    bad "malformed version is refused" "$(cat "$log")"
fi

# 11. Unsupported architecture and OS, through a uname shim.
mkdir -p "$root/shim"
cat >"$root/shim/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -m) echo "${FAKE_UNAME_M:-armv7l}" ;;
    -s) echo "${FAKE_UNAME_S:-Linux}" ;;
    *) echo Linux ;;
esac
EOF
chmod 0755 "$root/shim/uname"
shim_path=$root/shim:$PATH
if expect_failure PATH="$shim_path" FAKE_UNAME_M=armv7l && contains "unsupported CPU architecture: armv7l"; then
    ok "unsupported architecture is refused"
else
    bad "unsupported architecture is refused" "$(cat "$log")"
fi
if expect_failure PATH="$shim_path" FAKE_UNAME_S=FreeBSD FAKE_UNAME_M=x86_64 && contains "unsupported operating system: FreeBSD"; then
    ok "unsupported OS is refused"
else
    bad "unsupported OS is refused" "$(cat "$log")"
fi

# 12. macOS names resolve (Darwin + arm64 maps to macos-aarch64).
mac_dest=$root/macbin
dest=$mac_dest
if expect_success PATH="$shim_path" FAKE_UNAME_S=Darwin FAKE_UNAME_M=arm64 && contains "for macos-aarch64"; then
    ok "Darwin arm64 selects the macos-aarch64 asset"
else
    bad "Darwin arm64 selects the macos-aarch64 asset" "$(cat "$log")"
fi
dest=$root/bin

# 13. A non-local base URL is refused rather than silently ignored.
code=0
env HOME="$root/home" SDSYNC_RELEASE_BASE_URL="https://example.invalid/release" SDSYNC_INSTALL_DIR="$dest" \
    sh "$installer" >"$log" 2>&1 || code=$?
if [ "$code" -ne 0 ] && contains "only honoured for file:// or http://127.0.0.1"; then
    ok "a non-local SDSYNC_RELEASE_BASE_URL is refused"
else
    bad "a non-local SDSYNC_RELEASE_BASE_URL is refused" "$(cat "$log")"
fi

# 14. --version and --install-dir flags work like the environment variables.
flag_dest=$root/flagbin
code=0
env HOME="$root/home" SDSYNC_RELEASE_BASE_URL="file://$root/release" \
    sh "$installer" --version 26.64 --install-dir "$flag_dest" >"$log" 2>&1 || code=$?
if [ "$code" -eq 0 ] && [ "$("$flag_dest/synology-drive-sync" --version)" = "synology-drive-sync 26.64" ]; then
    ok "--version and --install-dir work"
else
    bad "--version and --install-dir work" "$(cat "$log")"
fi

printf '\n%s passed, %s failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
