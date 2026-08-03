#!/usr/bin/env bash
set -euo pipefail

version=0.22.2
target=x86_64-unknown-linux-gnu
archive_name="cargo-audit-$target-v$version.tgz"
package_name="cargo-audit-$target-v$version"
archive_sha256=ab28a1bdb54db4d5d8ad5981cf1f959410370b3d28250dbd35f6a44248620e39
download_url="https://github.com/rustsec/rustsec/releases/download/cargo-audit/v$version/$archive_name"

temporary_root=$(mktemp -d "${TMPDIR:-/tmp}/sdsync-cargo-audit.XXXXXXXX")
cleanup() {
    rm -rf -- "$temporary_root"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

archive="$temporary_root/$archive_name"
curl --fail --location --proto '=https' --tlsv1.2 \
    --retry 4 --retry-all-errors --output "$archive" "$download_url"
printf '%s  %s\n' "$archive_sha256" "$archive" | sha256sum --check --status

expected_members=$(printf '%s\n' \
    "$package_name/" \
    "$package_name/CHANGELOG.md" \
    "$package_name/LICENSE-APACHE" \
    "$package_name/LICENSE-MIT" \
    "$package_name/README.md" \
    "$package_name/cargo-audit" | LC_ALL=C sort)
actual_members=$(tar -tzf "$archive" | LC_ALL=C sort)
if [[ "$actual_members" != "$expected_members" ]]; then
    echo "cargo-audit archive member allowlist mismatch" >&2
    exit 1
fi

tar -xzf "$archive" -C "$temporary_root" --strip-components=1 \
    "$package_name/cargo-audit"
tool="$temporary_root/cargo-audit"
chmod 0755 "$tool"
[[ "$($tool --version)" == "cargo-audit $version" ]]

# cargo-audit fetches the current RustSec advisory database on every clean runner.
# Treat vulnerabilities and all warning classes (including yanked/unsound/unmaintained)
# as failures; any future exception must therefore be an explicit reviewed change.
"$tool" audit --file Cargo.lock --deny warnings --color never
