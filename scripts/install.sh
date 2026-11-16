#!/bin/sh
# synology-drive-sync installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/supermarsx/synology-drive-sync/main/scripts/install.sh | sh
#
# Installs the native CLI for your OS and CPU into ~/.local/bin (no sudo). It downloads the release
# archive and SHA256SUMS over HTTPS from github.com, checks the archive against SHA256SUMS, checks
# the binary reports the release version, and only then replaces the installed file. Any failed
# check stops the install with a message and a non-zero exit; nothing unverified is ever installed.
# It contacts only github.com (and the objects.githubusercontent.com host GitHub redirects
# downloads to), sends no telemetry, and never uses sudo.
#
# Environment:
#   SDSYNC_VERSION      release to install, e.g. 26.66 (default: the latest release)
#   SDSYNC_INSTALL_DIR  directory to install into (default: $HOME/.local/bin)
#
# The DSM package (.spk) is not installed by this script; see the project documentation.
#
# The whole script runs inside main(), called on the last line, so a download cut short can never
# execute a partial command.

set -eu

REPOSITORY=supermarsx/synology-drive-sync
BINARY=synology-drive-sync

say() { printf '%s\n' "$*"; }
fail() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: install.sh [--version YY.N] [--install-dir DIR]

Installs the synology-drive-sync CLI for this OS and CPU, verified against the release SHA256SUMS.

  --version YY.N     install this release instead of the latest (or set SDSYNC_VERSION)
  --install-dir DIR  install into DIR instead of ~/.local/bin (or set SDSYNC_INSTALL_DIR)

To uninstall, delete the installed file: rm "$HOME/.local/bin/synology-drive-sync"
EOF
}

# Release downloads come from GitHub over HTTPS. The one exception is a test fixture:
# SDSYNC_RELEASE_BASE_URL, honoured only when it names a local file:// directory or a loopback
# 127.0.0.1 server, so a hostile environment variable cannot redirect a real install elsewhere.
# Layout of the fixture: <base>/latest-version (one line) and <base>/<version>/<asset>.
resolve_base() {
    base=${SDSYNC_RELEASE_BASE_URL:-}
    if [ -z "$base" ]; then
        fixture=
        return 0
    fi
    case $base in
        file:///* | http://127.0.0.1 | http://127.0.0.1/* | http://127.0.0.1:*) fixture=$base ;;
        *) fail "SDSYNC_RELEASE_BASE_URL is only honoured for file:// or http://127.0.0.1 URLs (got: $base)" ;;
    esac
}

# fetch URL DEST: HTTPS only (TLS 1.2+) for GitHub; a plain copy or loopback HTTP for the fixture.
fetch() {
    url=$1
    dest=$2
    case $url in
        file://*) cp -- "${url#file://}" "$dest" 2>/dev/null || return 1 ;;
        http://127.0.0.1*)
            curl --proto '=http' --fail --silent --show-error --location --output "$dest" "$url" || return 1
            ;;
        https://*)
            curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location --output "$dest" "$url" || return 1
            ;;
        *) fail "refusing to download from a non-HTTPS URL: $url" ;;
    esac
}

latest_version() {
    if [ -n "$fixture" ]; then
        tmp_latest=$work/latest-version
        fetch "$fixture/latest-version" "$tmp_latest" || fail "could not read $fixture/latest-version"
        head -n 1 "$tmp_latest" | tr -d '[:space:]'
        return 0
    fi
    # github.com/OWNER/REPO/releases/latest redirects to .../releases/tag/<version>.
    effective=$(curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
        --output /dev/null --write-out '%{url_effective}' \
        "https://github.com/$REPOSITORY/releases/latest") ||
        fail "could not look up the latest release at github.com/$REPOSITORY"
    printf '%s\n' "${effective##*/}"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print tolower($1)}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print tolower($1)}'
    else
        fail "sha256sum or shasum is required to verify the download"
    fi
}

main() {
    version=${SDSYNC_VERSION:-}
    install_dir=${SDSYNC_INSTALL_DIR:-}

    while [ "$#" -gt 0 ]; do
        case $1 in
            --version)
                [ "$#" -ge 2 ] || fail "--version needs a value"
                version=$2
                shift 2
                ;;
            --install-dir)
                [ "$#" -ge 2 ] || fail "--install-dir needs a value"
                install_dir=$2
                shift 2
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            *)
                usage >&2
                fail "unknown argument: $1"
                ;;
        esac
    done

    if [ -z "$install_dir" ]; then
        [ -n "${HOME:-}" ] || fail "HOME is not set; set SDSYNC_INSTALL_DIR"
        install_dir=$HOME/.local/bin
    fi

    resolve_base
    command -v tar >/dev/null 2>&1 || fail "tar is required"
    if [ -z "$fixture" ] || [ "${fixture#http}" != "$fixture" ]; then
        command -v curl >/dev/null 2>&1 || fail "curl is required"
    fi

    case $(uname -s) in
        Linux) platform=linux ;;
        Darwin) platform=macos ;;
        *) fail "unsupported operating system: $(uname -s). This script covers Linux and macOS; Windows has install.ps1, and the DSM package installs from Package Center." ;;
    esac
    case $(uname -m) in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) fail "unsupported CPU architecture: $(uname -m). Releases ship x86_64 and aarch64 builds for $platform." ;;
    esac

    work=$(mktemp -d "${TMPDIR:-/tmp}/sdsync-install.XXXXXX") || fail "could not create a temporary directory"
    staged=
    # shellcheck disable=SC2329  # invoked through the trap below
    cleanup() {
        [ -z "$staged" ] || rm -f -- "$staged"
        rm -rf -- "$work"
    }
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    if [ -z "$version" ]; then
        version=$(latest_version)
    fi
    printf '%s\n' "$version" | grep -Eq '^[0-9]{2}\.[1-9][0-9]*$' ||
        fail "release version must look like 26.66 (got: ${version:-empty})"

    asset=$BINARY-$version-$platform-$arch.tar.gz
    if [ -n "$fixture" ]; then
        release_url=$fixture/$version
    else
        release_url=https://github.com/$REPOSITORY/releases/download/$version
    fi

    say "Installing $BINARY $version for $platform-$arch"
    fetch "$release_url/$asset" "$work/$asset" ||
        fail "release $version has no $asset (is $version a published release for $platform-$arch?)"
    fetch "$release_url/SHA256SUMS" "$work/SHA256SUMS" ||
        fail "release $version has no SHA256SUMS; refusing to install an unverified archive"

    expected=$(awk -v wanted="$asset" '$2 == wanted || $2 == "*" wanted { print tolower($1) }' "$work/SHA256SUMS")
    [ "$(printf '%s\n' "$expected" | awk 'NF { n++ } END { print n + 0 }')" -eq 1 ] ||
        fail "SHA256SUMS must list exactly one checksum for $asset; refusing to install"
    printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' ||
        fail "the SHA256SUMS entry for $asset is malformed; refusing to install"
    actual=$(sha256_of "$work/$asset")
    [ "$actual" = "$expected" ] ||
        fail "SHA-256 mismatch for $asset (expected $expected, got $actual); nothing was installed"
    say "Checksum verified against SHA256SUMS"

    member=$BINARY-$version-$platform-$arch/$BINARY
    tar -tzf "$work/$asset" >"$work/members" || fail "the verified archive could not be listed"
    while IFS= read -r name; do
        case $name in
            /* | ../* | */../* | */..) fail "the archive contains an unsafe path: $name" ;;
        esac
    done <"$work/members"
    [ "$(grep -Fxc "$member" "$work/members")" -eq 1 ] ||
        fail "the archive does not contain exactly one $member"
    # The binary must be a regular file, never a link.
    regular=$(tar -tvzf "$work/$asset" | awk -v member="$member" 'substr($0, 1, 1) == "-" && $NF == member { n++ } END { print n + 0 }')
    [ "$regular" -eq 1 ] || fail "$member in the archive is not a regular file"

    mkdir "$work/extract"
    tar -xzf "$work/$asset" -C "$work/extract" "$member" || fail "could not extract $member"
    candidate=$work/extract/$member
    [ -f "$candidate" ] && [ ! -L "$candidate" ] || fail "the extracted $BINARY is not a regular file"
    chmod 0755 "$candidate"
    reported=$("$candidate" --version 2>&1) || fail "the downloaded binary did not run: $reported"
    [ "$reported" = "$BINARY $version" ] ||
        fail "the downloaded binary reports '$reported', expected '$BINARY $version'; nothing was installed"

    mkdir -p -- "$install_dir" || fail "could not create $install_dir"
    install_dir=$(CDPATH='' cd -- "$install_dir" && pwd -P) || fail "could not enter $install_dir"
    target=$install_dir/$BINARY
    if [ -e "$target" ] || [ -L "$target" ]; then
        [ -f "$target" ] && [ ! -L "$target" ] ||
            fail "$target exists and is not a regular file; remove it first"
        previous=$("$target" --version 2>/dev/null || true)
    else
        previous=
    fi

    # Stage next to the target and rename over it, so a re-run replaces it in one step.
    staged=$(mktemp "$install_dir/.$BINARY.install.XXXXXX") || fail "could not write to $install_dir"
    cp -- "$candidate" "$staged"
    chmod 0755 "$staged"
    mv -f -- "$staged" "$target"
    staged=

    # macOS marks downloaded files as quarantined. Clear the mark on the file just verified, and
    # on nothing else.
    if [ "$platform" = macos ] && command -v xattr >/dev/null 2>&1; then
        if xattr -p com.apple.quarantine "$target" >/dev/null 2>&1; then
            xattr -d com.apple.quarantine "$target" ||
                fail "could not clear the quarantine attribute on $target"
            say "Cleared the macOS quarantine attribute on $target (the checksum-verified file)."
        fi
    fi

    installed=$("$target" --version 2>&1) || fail "the installed $target did not run: $installed"
    [ "$installed" = "$BINARY $version" ] || fail "the installed binary reports '$installed', expected '$BINARY $version'"

    if [ -z "$previous" ]; then
        say "Installed $installed to $target"
    elif [ "$previous" = "$installed" ]; then
        say "Reinstalled $installed at $target (already at this version)"
    else
        say "Upgraded $previous -> $installed at $target"
    fi

    case ":${PATH:-}:" in
        *":$install_dir:"*) ;;
        *)
            say ""
            say "$install_dir is not on your PATH. Add it for this shell with:"
            say "    export PATH=\"$install_dir:\$PATH\""
            say "and add the same line to your shell profile (~/.profile, ~/.zshrc or ~/.bashrc) to keep it."
            ;;
    esac
    say ""
    say "Uninstall: rm \"$target\"  (profiles, logs and saved credentials are kept)"
}

main "$@"
