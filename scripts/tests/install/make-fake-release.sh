#!/bin/sh
# Builds a local fake GitHub release tree for the installer tests (no network, no real binary).
#
#   make-fake-release.sh OUTDIR
#
# Layout (what scripts/install.sh reads through SDSYNC_RELEASE_BASE_URL):
#   OUTDIR/latest-version            26.65
#   OUTDIR/<version>/SHA256SUMS
#   OUTDIR/<version>/synology-drive-sync-<version>-<linux|macos>-<x86_64|aarch64>.tar.gz
#
# Versions: 26.64 and 26.65 are good; 26.63 has a tampered archive (checksum mismatch);
# 26.62 lists an asset in SHA256SUMS that was never uploaded; 26.61 has no SHA256SUMS;
# 26.60's binary reports a different version than the release tag.
# The "binary" is a shell script that prints "synology-drive-sync <version>" for --version.
set -eu

out=${1:?usage: make-fake-release.sh OUTDIR}
mkdir -p "$out"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# build_release VERSION REPORTED_VERSION
build_release() {
    version=$1
    reported=$2
    dir=$out/$version
    mkdir -p "$dir"
    : >"$dir/SHA256SUMS"
    for platform in linux macos; do
        for arch in x86_64 aarch64; do
            root=synology-drive-sync-$version-$platform-$arch
            stage=$(mktemp -d "${TMPDIR:-/tmp}/fakerel.XXXXXX")
            mkdir -p "$stage/$root"
            cat >"$stage/$root/synology-drive-sync" <<EOF
#!/bin/sh
case "\${1:-}" in
    --version) echo "synology-drive-sync $reported" ;;
    *) echo "fake synology-drive-sync $reported" ;;
esac
EOF
            chmod 0755 "$stage/$root/synology-drive-sync"
            printf 'fake license\n' >"$stage/$root/LICENSE"
            tar -czf "$dir/$root.tar.gz" -C "$stage" "$root"
            rm -rf "$stage"
            printf '%s  %s\n' "$(sha256_of "$dir/$root.tar.gz")" "$root.tar.gz" >>"$dir/SHA256SUMS"
        done
    done
}

build_release 26.64 26.64
build_release 26.65 26.65
printf '26.65\n' >"$out/latest-version"

# 26.63: SHA256SUMS describes the original archives; the archives are then altered.
build_release 26.63 26.63
for archive in "$out"/26.63/*.tar.gz; do
    printf 'tampered' >>"$archive"
done

# 26.62: checksums list assets that were never uploaded.
build_release 26.62 26.62
rm -f "$out"/26.62/*.tar.gz

# 26.61: archives but no SHA256SUMS.
build_release 26.61 26.61
rm -f "$out/26.61/SHA256SUMS"

# 26.60: the binary inside reports a different version than the tag.
build_release 26.60 26.59
