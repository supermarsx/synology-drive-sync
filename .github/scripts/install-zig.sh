#!/usr/bin/env bash
set -euo pipefail

readonly zig_version="0.16.0"
readonly download_base="https://ziglang.org/download/$zig_version"

die() {
    printf 'install-zig: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is unavailable: $1"
}

reject_line_breaks() {
    local name=$1
    local value=$2
    case "$value" in
        *$'\n'* | *$'\r'*) die "$name contains a line break" ;;
    esac
}

[[ -n "${RUNNER_TEMP:-}" ]] || die "RUNNER_TEMP is missing"
[[ -n "${GITHUB_PATH:-}" ]] || die "GITHUB_PATH is missing"
reject_line_breaks RUNNER_TEMP "$RUNNER_TEMP"
reject_line_breaks GITHUB_PATH "$GITHUB_PATH"

for required_command in basename curl dirname mktemp rm sha256sum tar uname; do
    require_command "$required_command"
done

kernel_name=$(uname -s)
machine_arch=$(uname -m)
[[ "$kernel_name" == "Linux" ]] || die "unsupported operating system: $kernel_name"

case "$machine_arch" in
    x86_64)
        archive_arch="x86_64"
        archive_sha256="70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00"
        ;;
    aarch64)
        archive_arch="aarch64"
        archive_sha256="ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17"
        ;;
    *)
        die "unsupported Linux architecture: $machine_arch"
        ;;
esac

case "$RUNNER_TEMP" in
    /*) ;;
    *) die "RUNNER_TEMP must be an absolute path" ;;
esac
[[ -d "$RUNNER_TEMP" ]] || die "RUNNER_TEMP is not an existing directory"
runner_temp=$(cd -- "$RUNNER_TEMP" && pwd -P)
[[ "$runner_temp" != "/" ]] || die "RUNNER_TEMP must not resolve to the filesystem root"
[[ -w "$runner_temp" ]] || die "RUNNER_TEMP is not writable"

case "$GITHUB_PATH" in
    /*) ;;
    *) die "GITHUB_PATH must be an absolute path" ;;
esac
[[ ! -L "$GITHUB_PATH" ]] || die "GITHUB_PATH must not be a symbolic link"
[[ -f "$GITHUB_PATH" ]] || die "GITHUB_PATH is not an existing regular file"
[[ -w "$GITHUB_PATH" ]] || die "GITHUB_PATH is not writable"
github_path_parent=$(cd -- "$(dirname -- "$GITHUB_PATH")" && pwd -P)
github_path="$github_path_parent/$(basename -- "$GITHUB_PATH")"
case "$github_path" in
    "$runner_temp"/*) ;;
    *) die "GITHUB_PATH must resolve beneath RUNNER_TEMP" ;;
esac

archive_root="zig-$archive_arch-linux-$zig_version"
archive_name="$archive_root.tar.xz"
archive_url="$download_base/$archive_name"
install_root=""

cleanup() {
    local status=$?
    trap - EXIT
    if [[ "$status" -ne 0 && -n "$install_root" ]]; then
        case "$install_root" in
            "$runner_temp"/sdsync-zig-*) rm -rf -- "$install_root" ;;
            *) printf 'install-zig: refusing to clean unexpected path: %s\n' "$install_root" >&2 ;;
        esac
    fi
    exit "$status"
}
trap cleanup EXIT

install_root=$(mktemp -d -- "$runner_temp/sdsync-zig-$zig_version-$archive_arch.XXXXXXXX")
case "$install_root" in
    "$runner_temp"/sdsync-zig-*) ;;
    *) die "mktemp returned a path outside RUNNER_TEMP" ;;
esac

archive_path="$install_root/$archive_name"
curl \
    --proto '=https' \
    --tlsv1.2 \
    --fail \
    --location \
    --silent \
    --show-error \
    --retry 4 \
    --retry-all-errors \
    --output "$archive_path" \
    "$archive_url"

printf '%s  %s\n' "$archive_sha256" "$archive_path" \
    | sha256sum --check --status \
    || die "SHA-256 verification failed for $archive_name"

archive_members="$install_root/archive-members.txt"
tar --list --xz --file "$archive_path" > "$archive_members"
[[ -s "$archive_members" ]] || die "verified archive has no members"
while IFS= read -r archive_member; do
    [[ -n "$archive_member" ]] || die "archive contains an empty member name"
    case "$archive_member" in
        "$archive_root" | "$archive_root"/*) ;;
        *) die "archive member escapes the expected root: $archive_member" ;;
    esac
    case "/$archive_member/" in
        */../*) die "archive member contains a parent traversal: $archive_member" ;;
    esac
done < "$archive_members"

tar \
    --extract \
    --xz \
    --file "$archive_path" \
    --directory "$install_root" \
    --no-same-owner \
    --no-same-permissions

bin_directory="$install_root/$archive_root"
zig_binary="$bin_directory/zig"
[[ -d "$bin_directory" && ! -L "$bin_directory" ]] \
    || die "verified archive did not create the expected Zig directory"
[[ -f "$zig_binary" && ! -L "$zig_binary" && -x "$zig_binary" ]] \
    || die "verified archive did not create an executable Zig binary"

reported_version=$("$zig_binary" version)
[[ "$reported_version" == "$zig_version" ]] \
    || die "Zig reported version '$reported_version'; expected '$zig_version'"

printf '%s\n' "$bin_directory" >> "$github_path"
printf 'Installed Zig %s for Linux %s at %s\n' \
    "$zig_version" "$archive_arch" "$bin_directory"
