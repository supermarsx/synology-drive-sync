#!/usr/bin/env bash
set -euo pipefail

version=0.9.1
target=x86_64-unknown-linux-musl
archive_name="cargo-about-$version-$target.tar.gz"
package_name="cargo-about-$version-$target"
archive_sha256=c0e7dc6f5d74b0beec5c0053d39ab24514c717d19acd91886907a22457ea9e98
download_url="https://github.com/EmbarkStudios/cargo-about/releases/download/$version/$archive_name"

repository_root=$(git rev-parse --show-toplevel)
cd "$repository_root"
output=${1:-THIRD_PARTY_LICENSES.html}

temporary_root=$(mktemp -d "${TMPDIR:-/tmp}/sdsync-cargo-about.XXXXXXXX")
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
    "$package_name/LICENSE-APACHE" \
    "$package_name/LICENSE-MIT" \
    "$package_name/README.md" \
    "$package_name/cargo-about" | LC_ALL=C sort)
actual_members=$(tar -tzf "$archive" | LC_ALL=C sort)
if [[ "$actual_members" != "$expected_members" ]]; then
    echo "cargo-about archive member allowlist mismatch" >&2
    exit 1
fi

tar -xzf "$archive" -C "$temporary_root" --strip-components=1 \
    "$package_name/cargo-about"
tool="$temporary_root/cargo-about"
chmod 0755 "$tool"
[[ "$($tool --version)" == "cargo-about $version" ]]

# Fetch every target-specific crate by Cargo.lock checksum, then prohibit the notice
# generator from consulting mutable network license metadata.
cargo fetch --locked \
    --target x86_64-unknown-linux-gnu \
    --target aarch64-unknown-linux-gnu \
    --target x86_64-pc-windows-msvc \
    --target aarch64-pc-windows-msvc \
    --target x86_64-apple-darwin \
    --target aarch64-apple-darwin

generate_arguments=(--frozen --all-features --fail --config about.toml)
"$tool" generate "${generate_arguments[@]}" --output-file "$output" about.hbs
test -s "$output"

license_report="$temporary_root/licenses.json"
"$tool" generate "${generate_arguments[@]}" --format json --output-file "$license_report"
python3 .github/scripts/verify-third-party-notices.py "$license_report" "$output"
