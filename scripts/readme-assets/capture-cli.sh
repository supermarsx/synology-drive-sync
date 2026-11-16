#!/bin/sh
# Runs INSIDE the rust container (see generate.sh). Builds the real Linux release CLI, starts the
# in-tree File Station mock (tests/support/file_station_mock.rs) on loopback, and runs the commands
# listed in /src/scripts/readme-assets/cli-session.sh against it, capturing the real output,
# colours included, into /out/cli-session.ansi.
#
# Nothing is typed or invented: every byte in the capture was written by the binary. Only the demo
# fixtures (a few generic files, a NAS account and host name) are chosen here, and the mock's
# built-in account "e2e-user" and host name "MOCKSTATION" are renamed in the scratch copy so the
# picture shows neutral names. The repository itself is mounted read-only and never modified.
set -eu

mkdir -p /work /out
cd /src
tar --exclude=./target --exclude=./.git --exclude=node_modules --exclude=./.claude -cf - . | tar -xf - -C /work
cd /work

# Neutral names for the mock's account and host.
sed -i 's/"e2e-user"/"deposit-bot"/; s/"MOCKSTATION"/"NAS-A"/' tests/support/file_station_mock.rs
cp scripts/readme-assets/mock_file_station.rs tests/readme_mock_file_station.rs

export CARGO_TARGET_DIR=/target
cargo build --release --locked --bin synology-drive-sync
cargo test --release --locked --test readme_mock_file_station --no-run

rm -f /tmp/mock.url /tmp/mock.stop
README_MOCK_URL_FILE=/tmp/mock.url README_MOCK_STOP_FILE=/tmp/mock.stop \
    cargo test --release --locked --test readme_mock_file_station -- --ignored --nocapture >/tmp/mock.log 2>&1 &
mock_pid=$!
tries=0
while [ ! -s /tmp/mock.url ]; do
    tries=$((tries + 1))
    [ "$tries" -lt 200 ] || { echo "the mock did not start" >&2; cat /tmp/mock.log >&2; exit 1; }
    sleep 0.1
done
README_MOCK_URL=$(cat /tmp/mock.url)
export README_MOCK_URL

# Demo fixtures: a plain folder and a password file, in a throwaway home.
export HOME=/tmp/demo-home
rm -rf "$HOME"
mkdir -p "$HOME/documents/reports" "$HOME/.config"
head -c 49152 /dev/zero | tr '\0' 'H' >"$HOME/documents/handbook.pdf"
head -c 21504 /dev/zero | tr '\0' '1' >"$HOME/documents/reports/q1-summary.pdf"
head -c 38912 /dev/zero | tr '\0' '2' >"$HOME/documents/reports/q2-summary.pdf"
head -c 7168 /dev/zero | tr '\0' 'n' >"$HOME/documents/notes.md"
head -c 2560 /dev/zero | tr '\0' 'b' >"$HOME/documents/budget.csv"
# The two files the mock already holds keep their old mtime, so the first plan shows them as unchanged.
touch -d '2026-01-01 00:00:00 UTC' "$HOME/documents/handbook.pdf" "$HOME/documents/reports/q1-summary.pdf"
printf 'correct horse battery staple\n' >"$HOME/.config/nas-password"
chmod 600 "$HOME/.config/nas-password"

cat >"$HOME/.config/synology-drive-sync.toml" <<EOF
default-profile = "documents"

[profiles.documents]
source = "$HOME/documents"
remote = "/home/Backup/documents"
url = "$README_MOCK_URL"
username = "deposit-bot"
password-file = "$HOME/.config/nas-password"
no-vault = true
allow-http = true
EOF

export PATH="/target/release:$PATH"
export XDG_CONFIG_HOME="$HOME/.config"
export SDSYNC_CONFIG="$HOME/.config/synology-drive-sync.toml"
export TERM=xterm-256color

# cli-session.sh runs each demo command through `show`, which prints a prompt line and then the
# command's real output, under a pseudo-terminal so the binary behaves as it does in a terminal.
: >/out/cli-session.ansi
# shellcheck disable=SC1091
# shellcheck disable=SC1090
. "${CLI_SESSION:-/src/scripts/readme-assets/cli-session.sh}"

touch /tmp/mock.stop
wait "$mock_pid" || true
echo "captured $(wc -c </out/cli-session.ansi) bytes"

python3 /src/scripts/readme-assets/ansi_to_svg.py /out/cli-session.ansi /out/cli-sync.svg \
    --cols 120 --title "synology-drive-sync: plan, sync, sync again"
ls -l /out
