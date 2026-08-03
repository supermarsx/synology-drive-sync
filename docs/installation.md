# Installation and deployment

Release installers are the shortest path to a native binary. Containers and scheduler examples are finite one-shot jobs: each invocation scans, plans, applies the requested operations, logs out of File Station, and exits. Ctrl+C/SIGINT and service or container SIGTERM request cooperative cancellation; a cancelled run exits with status `130`.

Before scheduling anything, configure the File Station reverse proxy, use a dedicated DSM account, run `doctor`, and review a non-critical `plan`. Keep mirror deletion disabled until the complete deployment path has been tested.

## Verified native installer

The installers detect OS and architecture, resolve only a strict `YY.N` release, download the matching archive plus `SHA256SUMS`, verify SHA-256, check the embedded binary version, and atomically replace only the selected executable. They do not invoke a package manager or silently change system-wide configuration.

Download and inspect the script before running it.

### Linux and macOS

```bash
curl --proto '=https' --tlsv1.2 -fL \
  https://github.com/supermarsx/synology-drive-sync/releases/latest/download/install.sh \
  -o install.sh
less install.sh
sh install.sh
```

The default destination is `~/.local/bin/synology-drive-sync`. Select an exact release or directory explicitly:

```bash
sh install.sh --version 26.1 --bin-dir "$HOME/bin"
```

The script does not modify `PATH`. It reports when the chosen directory is absent from the current value.

### Windows

```powershell
Invoke-WebRequest `
  -Uri 'https://github.com/supermarsx/synology-drive-sync/releases/latest/download/install.ps1' `
  -OutFile '.\install.ps1'
Get-Content '.\install.ps1'
Unblock-File '.\install.ps1'
& '.\install.ps1'
```

The default destination is `%LOCALAPPDATA%\Programs\synology-drive-sync\synology-drive-sync.exe`. Pin a release, choose a location, or opt into a current-user `PATH` update:

```powershell
& '.\install.ps1' `
  -Version '26.1' `
  -InstallDir "$env:LOCALAPPDATA\Programs\synology-drive-sync" `
  -AddToUserPath
```

Both installers also accept an alternate `OWNER/REPO` for audited forks. Their built-in checksum verification detects corruption and asset mismatch. For publisher provenance, perform the attestation checks in [Release artifacts and verification](releases.md).

## Manual archive install

Choose the exact asset listed in [Release artifacts and verification](releases.md), download it together with `SHA256SUMS`, and verify before extracting.

Linux:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

macOS:

```bash
expected=$(awk '$2 == "synology-drive-sync-26.1-macos-aarch64.tar.gz" { print $1 }' SHA256SUMS)
actual=$(shasum -a 256 synology-drive-sync-26.1-macos-aarch64.tar.gz | awk '{ print $1 }')
test "$actual" = "$expected"
```

Windows:

```powershell
$asset = 'synology-drive-sync-26.1-windows-x86_64.zip'
$line = Get-Content '.\SHA256SUMS' | Where-Object { $_ -match "\s\*?$([Regex]::Escape($asset))$" }
if (@($line).Count -ne 1) { throw "Expected one checksum for $asset" }
$expected = ($line -split '\s+')[0]
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $asset).Hash
if ($actual -ne $expected) { throw 'SHA-256 mismatch' }
```

Each archive contains one top-level version/platform directory with:

- `synology-drive-sync` or `synology-drive-sync.exe`;
- `LICENSE`, `THIRD_PARTY_LICENSES.html`, `README.md`, and `SECURITY.md`;
- completion source for Bash, Zsh, Fish, PowerShell, and Elvish under `completions/`;
- 15 roff pages under `man/`: the root `synology-drive-sync.1` page and one page for every top-level and nested subcommand.

The automated installers install only the executable. Install the optional files where appropriate for your shell and OS, or regenerate them from the installed binary:

```bash
synology-drive-sync completions bash > synology-drive-sync.bash
synology-drive-sync completions zsh > _synology-drive-sync
synology-drive-sync completions fish > synology-drive-sync.fish
synology-drive-sync manpage > synology-drive-sync.1
synology-drive-sync manpage --all ./man
```

The last form creates the directory when needed and writes the root page plus a separate page for every nested subcommand; existing generated page names there are replaced. Install the complete set together so the root page's subcommand references resolve.

## Build from source

Rust 1.88 or newer is required:

```bash
git clone https://github.com/supermarsx/synology-drive-sync.git
cd synology-drive-sync
cargo test --all-targets
cargo build --release --locked
```

The executable is `target/release/synology-drive-sync` (`.exe` on Windows). HTTPS uses rustls and platform certificate roots; there is no OpenSSL runtime dependency.

## Docker and Compose

The image is a finite sync job built in two stages. The runtime uses Debian Bookworm, a fixed non-root UID/GID (`10001`), no shell entry logic beyond `exec`, and a side-effect-free binary health probe. The project license and generated dependency notices are installed under `/usr/share/licenses/synology-drive-sync/`. The supplied Compose service additionally drops every capability, enables `no-new-privileges`, makes the root filesystem read-only, and mounts the authoritative source read-only.

### Build from the checkout

Prepare a password file outside the repository and restrict it to the account invoking Docker:

```bash
export SDSYNC_URL=https://files.example.com
export SDSYNC_USERNAME=mirror-bot
export SDSYNC_SOURCE=/srv/export
export SDSYNC_REMOTE=/team/export
export SDSYNC_PASSWORD_FILE=/secure/location/dsm-password
docker compose run --rm sync
```

Compose mounts the password at `/run/secrets/sdsync_password`; the CLI receives only that path through `--password-file` and uses `--no-vault`.

For unattended TOTP, put the Base32 DSM seed or provisioning URI in a separate protected file and add the overlay:

```bash
export SDSYNC_TOTP_SECRET_FILE=/secure/location/dsm-totp
docker compose -f compose.yaml -f compose.totp.yaml run --rm sync
```

The overlay mounts `/run/secrets/sdsync_totp` and adds `--totp-secret-file`. Do not put the seed in a Compose environment block. `SDSYNC_OTP` may carry one current six-digit code for a deliberately ephemeral run; it is not seed storage and expires quickly.

### Published GHCR image

Pull a calendar version rather than the mutable convenience tag:

```bash
docker pull ghcr.io/supermarsx/synology-drive-sync:26.1
```

Anonymous pulls require the GHCR package itself to be public. If the repository
owner has not yet enabled public package visibility, authenticate with
`docker login ghcr.io` first.

The image supports `linux/amd64` and `linux/arm64`. To use it without Compose, preserve the same controls and protected mounts:

```bash
docker run --rm --init \
  --user 10001:10001 \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,size=16m \
  --mount type=bind,src="$SDSYNC_SOURCE",dst=/source,readonly \
  --mount type=bind,src="$SDSYNC_PASSWORD_FILE",dst=/run/secrets/sdsync_password,readonly \
  --env SDSYNC_URL \
  --env SDSYNC_USERNAME \
  ghcr.io/supermarsx/synology-drive-sync:26.1 \
  sync /source "$SDSYNC_REMOTE" \
  --password-file /run/secrets/sdsync_password --no-vault
```

Add a second read-only bind mount and `--totp-secret-file /run/secrets/sdsync_totp` for a 2FA seed. Pinning the image by digest provides the strongest deployment identity; see [Releases](releases.md).

## Scheduled native deployments

Always run the scheduler as the same least-privileged OS identity that owns the intended vault entries or protected credential files. Grant that identity read/traverse access to the local source and no more DSM permission than the destination requires. The examples prevent overlapping runs and leave deletion disabled.

### Linux systemd timer

[The systemd assets](../packaging/systemd/README.md) provide a hardened `Type=oneshot` service and daily timer. The service runs as a dedicated `sdsync` account with no capabilities, a read-only filesystem namespace, socket creation limited to Unix/IPv4/IPv6 families, and a password supplied through `LoadCredential`. `RestrictAddressFamilies` does not filter by protocol or destination; use host/network firewall policy when egress must be limited to the reverse proxy.

For TOTP, install the documented `synology-drive-sync-totp.conf.example` drop-in. It adds a second `LoadCredential` mount and passes only its protected path to `--totp-secret-file`. Neither seed belongs in `sync.env`.

Test the service before enabling the timer:

```bash
sudo systemctl daemon-reload
sudo systemctl start synology-drive-sync.service
sudo journalctl -u synology-drive-sync.service
sudo systemctl enable --now synology-drive-sync.timer
```

### Cron fallback

[The cron assets](../packaging/cron/README.md) use a mode-`0600` environment file containing only non-secret settings and protected secret-file paths. The wrapper forces `--no-vault`, supports an optional `SDSYNC_TOTP_SECRET_FILE`, and the sample crontab uses `flock` to reject overlap.

Prefer systemd on Linux. A cron session commonly lacks the D-Bus and unlocked Secret Service collection required for OS-vault access.

### macOS LaunchAgent

[The LaunchAgent example](../packaging/launchd/README.md) runs in the logged-in user's GUI session so the same user's login Keychain is available. Enroll the password and optional TOTP seed first, replace every plist placeholder with a non-secret absolute value, validate with `plutil`, and bootstrap it with `launchctl`.

The login Keychain generally must be unlocked. Do not place secret values in the plist.

### Windows Task Scheduler

[The Task Scheduler installer](../packaging/windows/README.md) creates a current-user, interactive-token, limited-privilege daily task. It uses the same user's Windows Credential Manager entries, refuses to replace an existing task unless `-Force` is explicit, and configures Task Scheduler to ignore overlap.

Its default executable is the release installer's per-user path, `%LOCALAPPDATA%\Programs\synology-drive-sync\synology-drive-sync.exe`; pass `-Executable` when the binary is installed elsewhere.

```powershell
.\packaging\windows\Install-SynologyDriveSyncTask.ps1 `
  -Source 'C:\Data\Export' `
  -Remote '/team/export' `
  -Url 'https://files.example.com' `
  -Username 'mirror-bot' `
  -At '03:00'
```

A task configured to run while no user is logged in may not have access to the same vault material. Test that logon mode with the exact service account rather than falling back to plaintext arguments.

## Updating and removing

Rerun an installer with an explicit `--version`/`-Version` to upgrade or roll back. It replaces only the executable in the selected directory. Remove that exact executable manually to uninstall; scheduler units, logs, configuration, and OS-vault entries are intentionally left untouched so their deletion remains a separate decision.

Use `credentials remove ... password|totp|all` before retiring a credential profile. Remove scheduler definitions with the platform's normal `systemctl`, `launchctl`, Task Scheduler, or crontab tooling after confirming the exact target.
