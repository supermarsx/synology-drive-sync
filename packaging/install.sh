#!/bin/sh
set -eu

repository=supermarsx/synology-drive-sync
version=
bin_dir=${HOME:+"$HOME/.local/bin"}
action=install

usage() {
    cat <<'EOF'
Usage: install.sh [--version YY.N] [--bin-dir PATH] [--repository OWNER/REPO]
       install.sh --uninstall [--bin-dir PATH]

Downloads the native GitHub Release archive, verifies it against SHA256SUMS,
and atomically installs or upgrades only the synology-drive-sync executable.
Uninstall removes only that executable; scheduler configuration and credentials
are deliberately left for their native manager.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { echo "--version requires a value" >&2; exit 64; }
            version=$2
            shift 2
            ;;
        --bin-dir)
            [ "$#" -ge 2 ] || { echo "--bin-dir requires a value" >&2; exit 64; }
            bin_dir=$2
            shift 2
            ;;
        --repository)
            [ "$#" -ge 2 ] || { echo "--repository requires a value" >&2; exit 64; }
            repository=$2
            shift 2
            ;;
        --uninstall)
            action=uninstall
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 64
            ;;
    esac
done

[ -n "$bin_dir" ] || { echo "HOME is unset; pass --bin-dir" >&2; exit 64; }

if [ "$action" = uninstall ]; then
    [ -z "$version" ] || { echo "--version cannot be combined with --uninstall" >&2; exit 64; }
    [ "$repository" = supermarsx/synology-drive-sync ] || {
        echo "--repository cannot be combined with --uninstall" >&2
        exit 64
    }
    if [ ! -e "$bin_dir" ]; then
        echo "synology-drive-sync is already absent from $bin_dir"
        exit 0
    fi
    [ -d "$bin_dir" ] && [ ! -L "$bin_dir" ] || {
        echo "install directory is not a non-symlink directory: $bin_dir" >&2
        exit 73
    }
    bin_dir=$(CDPATH='' cd -- "$bin_dir" && pwd -P)
    target="$bin_dir/synology-drive-sync"
    if [ ! -e "$target" ] && [ ! -L "$target" ]; then
        echo "synology-drive-sync is already absent from $target"
        exit 0
    fi
    [ -f "$target" ] && [ ! -L "$target" ] || {
        echo "refusing to remove a non-regular or linked install target: $target" >&2
        exit 73
    }
    rm -f -- "$target"
    echo "Removed $target"
    echo "Scheduler definitions, configuration, logs, and credentials were not removed."
    exit 0
fi

printf '%s\n' "$repository" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' || {
    echo "repository must be OWNER/REPO" >&2
    exit 64
}

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 69; }
command -v tar >/dev/null 2>&1 || { echo "tar is required" >&2; exit 69; }

case "$(uname -s)" in
    Linux) platform=linux ;;
    Darwin) platform=macos ;;
    *) echo "unsupported operating system: $(uname -s)" >&2; exit 69 ;;
esac

case "$(uname -m)" in
    x86_64|amd64) architecture=x86_64 ;;
    arm64|aarch64) architecture=aarch64 ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 69 ;;
esac

if [ -z "$version" ]; then
    latest_url=$(curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
        --location --output /dev/null --write-out '%{url_effective}' \
        "https://github.com/$repository/releases/latest")
    version=${latest_url##*/}
fi

printf '%s\n' "$version" | grep -Eq '^[0-9]{2}\.[1-9][0-9]*$' || {
    echo "release version must use calendar form YY.N, received: $version" >&2
    exit 65
}

asset="synology-drive-sync-$version-$platform-$architecture.tar.gz"
release_url="https://github.com/$repository/releases/download/$version"
temp_root=${TMPDIR:-/tmp}
temp_dir=$(mktemp -d "$temp_root/sdsync-install.XXXXXX")
staged_target=

cleanup() {
    if [ -n "$staged_target" ] && [ -f "$staged_target" ]; then
        rm -f -- "$staged_target"
    fi
    case "$temp_dir" in
        "$temp_root"/sdsync-install.*) rm -rf -- "$temp_dir" ;;
        *) echo "refusing to clean unexpected temporary path: $temp_dir" >&2 ;;
    esac
}
trap cleanup 0 1 2 15

curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    --output "$temp_dir/$asset" "$release_url/$asset"
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    --output "$temp_dir/SHA256SUMS" "$release_url/SHA256SUMS"

checksum_matches=$(awk -v wanted="$asset" '
    $2 == wanted || $2 == "*" wanted { print tolower($1) }
' "$temp_dir/SHA256SUMS")
match_count=$(printf '%s\n' "$checksum_matches" | awk 'NF { count++ } END { print count + 0 }')
[ "$match_count" -eq 1 ] || {
    echo "checksum file must contain exactly one entry for $asset" >&2
    exit 65
}
expected=$checksum_matches
printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' || {
    echo "release checksum for $asset is missing or malformed" >&2
    exit 65
}

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temp_dir/$asset" | awk '{print tolower($1)}')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$temp_dir/$asset" | awk '{print tolower($1)}')
else
    echo "sha256sum or shasum is required" >&2
    exit 69
fi
[ "$actual" = "$expected" ] || { echo "SHA-256 verification failed for $asset" >&2; exit 65; }

archive_root="synology-drive-sync-$version-$platform-$architecture"
member_list="$temp_dir/archive-members"
tar -tzf "$temp_dir/$asset" > "$member_list" || {
    echo "failed to list verified archive $asset" >&2
    exit 65
}

if ! tar -tvzf "$temp_dir/$asset" | awk '
    { kind = substr($0, 1, 1) }
    kind != "-" && kind != "d" { exit 1 }
    / -> / || / link to / { exit 1 }
    END { if (NR == 0) exit 1 }
'; then
    echo "verified archive contains a symlink, hardlink, or unsupported member type" >&2
    exit 65
fi

binary_members=0
while read -r member; do
    [ -n "$member" ] || {
        echo "verified archive contains an empty member name" >&2
        exit 65
    }
    case "$member" in
        /*|../*|*/../*|*/..)
            echo "verified archive contains an unsafe member path: $member" >&2
            exit 65
            ;;
    esac
    normalized=${member%/}
    case "$normalized" in
        "$archive_root"|\
        "$archive_root/synology-drive-sync"|\
        "$archive_root/LICENSE"|\
        "$archive_root/THIRD_PARTY_LICENSES.html"|\
        "$archive_root/README.md"|\
        "$archive_root/SECURITY.md"|\
        "$archive_root/completions"|\
        "$archive_root/completions/synology-drive-sync.bash"|\
        "$archive_root/completions/_synology-drive-sync"|\
        "$archive_root/completions/synology-drive-sync.fish"|\
        "$archive_root/completions/synology-drive-sync.ps1"|\
        "$archive_root/completions/synology-drive-sync.elv"|\
        "$archive_root/man"|\
        "$archive_root/man/synology-drive-sync-completions.1"|\
        "$archive_root/man/synology-drive-sync-config-path.1"|\
        "$archive_root/man/synology-drive-sync-config-show.1"|\
        "$archive_root/man/synology-drive-sync-config-validate.1"|\
        "$archive_root/man/synology-drive-sync-config.1"|\
        "$archive_root/man/synology-drive-sync-credentials-remove.1"|\
        "$archive_root/man/synology-drive-sync-credentials-set-password.1"|\
        "$archive_root/man/synology-drive-sync-credentials-set-totp.1"|\
        "$archive_root/man/synology-drive-sync-credentials-status.1"|\
        "$archive_root/man/synology-drive-sync-credentials.1"|\
        "$archive_root/man/synology-drive-sync-doctor.1"|\
        "$archive_root/man/synology-drive-sync-doctor-source.1"|\
        "$archive_root/man/synology-drive-sync-doctor-target.1"|\
        "$archive_root/man/synology-drive-sync-manpage.1"|\
        "$archive_root/man/synology-drive-sync-plan.1"|\
        "$archive_root/man/synology-drive-sync-sync.1"|\
        "$archive_root/man/synology-drive-sync.1")
            ;;
        *)
            echo "verified archive contains an unexpected member: $member" >&2
            exit 65
            ;;
    esac
    if [ "$normalized" = "$archive_root/synology-drive-sync" ]; then
        binary_members=$((binary_members + 1))
    fi
done < "$member_list"

[ "$binary_members" -eq 1 ] || {
    echo "verified archive must contain exactly one native executable" >&2
    exit 65
}

mkdir "$temp_dir/extract"
tar -xzf "$temp_dir/$asset" -C "$temp_dir/extract" \
    "$archive_root/synology-drive-sync"
candidate="$temp_dir/extract/$archive_root/synology-drive-sync"
if [ ! -f "$candidate" ] || [ -L "$candidate" ]; then
    echo "verified archive did not contain the expected regular executable" >&2
    exit 65
fi
chmod 0755 "$candidate"
candidate_version=$("$candidate" --version)
expected_version="synology-drive-sync $version"
[ "$candidate_version" = "$expected_version" ] || {
    echo "archive binary version did not exactly match $expected_version: $candidate_version" >&2
    exit 65
}

mkdir -p -- "$bin_dir"
bin_dir=$(CDPATH='' cd -- "$bin_dir" && pwd -P)
target="$bin_dir/synology-drive-sync"
if [ -e "$target" ] || [ -L "$target" ]; then
    [ -f "$target" ] && [ ! -L "$target" ] || {
        echo "install target is not a non-symlink regular file: $target" >&2
        exit 73
    }
fi
staged_target=$(mktemp "$bin_dir/.synology-drive-sync.install.XXXXXX")
cp -- "$candidate" "$staged_target"
chmod 0755 "$staged_target"
mv -f -- "$staged_target" "$target"
staged_target=

echo "Installed synology-drive-sync $version to $target"
case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *) echo "Add $bin_dir to PATH to invoke it by name." ;;
esac
