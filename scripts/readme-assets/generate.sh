#!/usr/bin/env bash
# Regenerates the README images in docs/assets/ from the real code, entirely inside Docker.
# Nothing runs on the host: no node, no pnpm, no browser (a browser on a Windows host opens windows
# on the desktop, so Chromium only ever runs in the container).
#
#   scripts/readme-assets/generate.sh
#
# Produces:
#   docs/assets/dashboard.png  the real built DSM AppWindow (packaging/synology/ui-src/dist)
#                              rendered in headless Chromium at 1280x800 against a mock package API
#                              with neutral demo data (ui/demo-data.js)
#   docs/assets/cli-sync.svg   the real Linux CLI binary running plan and sync against the in-tree
#                              File Station mock, captured and rendered as a dark-terminal SVG
#
# Needs Docker with network access (base images, apt, npm, crates). The first run builds the release
# CLI (a few minutes); the target and registry live in named volumes, so later runs are fast.
# Rebuild the dist first (docs/dsm/package-development.md) if the UI sources changed.
set -euo pipefail
export MSYS_NO_PATHCONV=1   # Git Bash on Windows must not rewrite the container paths below

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
mount_root=$root
mount_out=
if mount_root=$(cd "$root" && pwd -W 2>/dev/null); then :; else mount_root=$root; fi

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
if mount_out=$(cd "$out" && pwd -W 2>/dev/null); then :; else mount_out=$out; fi

rust_image=rust:1.90.0-bookworm
assets_image=sdsync-readme-assets

docker build -q -t "$assets_image" "$mount_root/scripts/readme-assets"
docker volume create sdsync-readme-target >/dev/null
docker volume create sdsync-readme-cargo >/dev/null

echo "== CLI capture (builds the Linux release binary; the first run takes a few minutes)"
docker run --rm \
    -v "$mount_root:/src:ro" \
    -v sdsync-readme-target:/target \
    -v sdsync-readme-cargo:/usr/local/cargo/registry \
    -v "$mount_out:/out" \
    "$rust_image" sh /src/scripts/readme-assets/capture-cli.sh

echo "== Dashboard screenshot"
docker run --rm \
    -e UI_ICON=/src/packaging/synology/package/ui/images/icon.svg \
    -v "$mount_root:/src:ro" \
    -v "$mount_out:/out" \
    "$assets_image" sh -c '
        set -eu
        node /src/scripts/readme-assets/ui/shoot.js /src/packaging/synology/ui-src/dist /out/dashboard-raw.png overview
        optipng -quiet -o7 -strip all -out /out/dashboard.png /out/dashboard-raw.png'

mkdir -p "$root/docs/assets"
cp "$out/dashboard.png" "$root/docs/assets/dashboard.png"
cp "$out/cli-sync.svg" "$root/docs/assets/cli-sync.svg"
ls -l "$root/docs/assets"
