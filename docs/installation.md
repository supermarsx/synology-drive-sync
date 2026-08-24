# Installation and deployment

Release installers are the shortest path to a native workstation binary; use the architecture-specific
SPK when the source is on a Synology NAS. Containers and scheduler examples are finite one-shot
jobs: each invocation scans, plans, applies the requested operations, logs out of File Station, and
exits. Ctrl+C/SIGINT and service or container SIGTERM request cooperative cancellation; a cancelled
run exits with status `130`.

Before scheduling anything, configure the File Station reverse proxy, use a dedicated DSM account, run `doctor`, and review a non-critical `plan`. Keep mirror deletion disabled until the complete deployment path has been tested.

Passing unit tests or `doctor` is not production acceptance. Before trusting real data, complete the [disposable live-NAS acceptance and recovery runbook](production-acceptance.md), including external byte verification, Synology Drive indexing visibility, a retry exercise, alert delivery, and a restore drill.

## Synology DSM 7 package

Install the architecture-specific SPK when the source directory is physically on a Synology NAS:

| CPU / official model Package Arch | SPK `INFO` arch | Release asset |
| --- | --- | --- |
| x86-64 / supported DSM 7 x86-64 member platforms (resolved by selector) | `x86_64` | `synology-drive-sync-YY.N-x86_64.spk` |
| AArch64 / `armada37xx`, `rtd1296`, or `rtd1619b` | `armv8` | `synology-drive-sync-YY.N-armv8.spk` |
| ARMv7-A hard-float / `alpine`, `alpine4k`, `armada370`, `armada375`, `armada38x`, `armadaxp`, `comcerto2k`, or `monaco` | `armv7 armada370 armada375 armada38x armadaxp comcerto2k monaco` | `synology-drive-sync-YY.N-armv7.spk` |
| Intel `i686` / `evansport` (DSM 7.0/7.1 only) | `i686` | `synology-drive-sync-YY.N-i686.spk` |

All four packages embed one fully static musl ELF (ELF32 or ELF64) and require DSM `7.0-40759` or
newer. Availability is the intersection of the exact model's DSM lifecycle and the DSM 7.0–7.4
`pkgscripts-ng` interval for its Package Arch. A platform can remain in a toolkit after an older
model on that platform stops receiving DSM, and a new model can require a later DSM even when its
CPU family already existed. Use the [release selector](release-selector.md) with the searchable
model, explicit OS product/version, and reported architecture. The exact build is optional for DSM
7.1–7.3 but mandatory for DSM 7.0 and 7.4 so the `7.0-40759` floor and `7.4-99999` ceiling can be
proven. The selector covers all 231 models in the captured official CPU table, including
informational DSM 5.2/6.2 systems that
receive no asset. It rejects model/branch conflicts, pre-introduction and post-removal toolkit
branches, unknown or contradictory runtime inputs, and `PAS7700`, whose product line is DSM
Enterprise 1.0 rather than ordinary DSM. For an unsupported NAS, run the desktop CLI or container
on a supported workstation that can read the source over an intentionally configured share; do not
relabel an SPK. Download the selected SPK and `SHA256SUMS`,
verify the file as described in [Release artifacts and verification](releases.md), then use **Package
Center > Manual Install**. DSM displays its normal warning for a package not published by Synology.
Treat that warning as a trust decision and proceed only after verifying the repository, exact asset
name, SHA-256, and optional GitHub attestation.

The native DSM package installs the CLI manager here:

```text
/var/packages/synology-drive-sync/target/bin/sdsync-dsm
```

The schedule is disabled at installation. Grant the `synology-drive-sync` system-internal user
read-only access to the intended local source share, then configure it from an administrator SSH
session as the package identity. Mutating and sync/diagnostic manager commands refuse root or a
different identity with exit `77`, preventing an administrator ACL from masking a missing package
source permission. For example:

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm

sudo -u synology-drive-sync -- "$MANAGER" configure-profile \
  --name nas-b \
  --source '/volume1/Source' \
  --url 'https://files-b.example.com' \
  --username 'mirror-bot' \
  --remote '/home/Drive/NAS-A Backup' \
  --default
sudo -u synology-drive-sync -- "$MANAGER" set-password nas-b
sudo -u synology-drive-sync -- "$MANAGER" set-totp nas-b # only for app TOTP
sudo -u synology-drive-sync -- "$MANAGER" doctor nas-b
sudo -u synology-drive-sync -- "$MANAGER" plan nas-b
sudo -u synology-drive-sync -- "$MANAGER" run nas-b
sudo -u synology-drive-sync -- "$MANAGER" enable --interval 3600
sudo synopkg start synology-drive-sync
```

The target is your choice, not a provisioned constant. `/home/Drive/...` addresses the configured
remote user's Drive home; `/<share>/...` addresses a writable Team Folder or ordinary shared-folder
subdirectory. DSM must first provision the user home or shared-folder root and grant the remote
account access. The sync creates a missing selected subdirectory and all descendants below an
existing writable share, but it does not create a DSM shared folder, enable User Home service, or
enable a Team Folder.

Configure multiple profiles to address multiple target NAS devices or directories, then use
`doctor --all`, `plan --all`, and `run --all`. The package uses protected per-profile password and
optional TOTP files, one package-local run lock, bounded logs/state, and interval scheduling.
Deletion remains suppressed unless it is enabled independently in the profile and for a reviewed
manual/scheduled invocation.

Upgrades retain package-private configuration and secrets and validate the existing config. A
completed uninstall removes the package's configuration, credentials, state, and logs, but never
the local source or remote target data. Stop the package before either operation. See the
[complete Synology DSM package guide](synology-package.md) for ACL setup, arbitrary destination
examples, exact lifecycle commands and paths, build instructions, deletion controls, and the
mandatory live two-NAS acceptance. No live NAS install has been validated by the automated suite.

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
- 17 roff pages under `man/`: the root `synology-drive-sync.1` page and one page for every top-level and nested subcommand.

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

The image is a finite sync job built in two stages. The runtime image defaults to non-root UID/GID `10001`, has no shell entry logic beyond `exec`, and uses a side-effect-free binary health probe. The managed Compose wrapper renders the service with the invoking account's nonzero UID/GID so its owner-only source and bind-mounted secret files remain readable without broadening host permissions. The project license and generated dependency notices are installed under `/usr/share/licenses/synology-drive-sync/`. The supplied Compose service additionally drops every capability, enables `no-new-privileges`, makes the root filesystem read-only, and mounts the authoritative source read-only.

### Build from the checkout

Prepare a password file outside the repository and restrict it to the account invoking Docker:

```bash
export SDSYNC_URL=https://files.example.com
export SDSYNC_USERNAME=mirror-bot
export SDSYNC_SOURCE=/srv/export
export SDSYNC_REMOTE=/team/export
export SDSYNC_PASSWORD_FILE=/secure/location/dsm-password
chmod 0600 "$SDSYNC_PASSWORD_FILE"
packaging/docker/run-compose.sh validate
packaging/docker/run-compose.sh build
packaging/docker/run-compose.sh run
```

Compose mounts the password at `/run/secrets/sdsync_password`; the CLI receives only that path through `--password-file` and uses `--no-vault`. Direct `docker compose run` defaults to UID/GID `10001` and therefore requires the source and owner-only secret files to be readable by that numeric identity; prefer the managed wrapper, which validates and exports the current non-root UID/GID.

For unattended TOTP, put the Base32 DSM seed or provisioning URI in a separate protected file and add the overlay:

```bash
export SDSYNC_TOTP_SECRET_FILE=/secure/location/dsm-totp
chmod 0600 "$SDSYNC_TOTP_SECRET_FILE"
packaging/docker/run-compose.sh run
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
  --user "$(id -u):$(id -g)" \
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

Always run the scheduler as the same least-privileged OS identity that owns the intended vault entries or protected credential files. Grant that identity read/traverse access to the local source and no more DSM permission than the destination requires. The examples prevent overlapping runs and leave deletion disabled. They retain diagnostics through journald or the CLI's bounded rotating file sink; connect their documented nonzero result to a real alert destination and test that alert before relying on the schedule.

For several profiles in one operational window, prefer one batch invocation so every selected
target is preflighted before the first mutation and jobs then run sequentially in deterministic
profile-name order:

```bash
synology-drive-sync sync --config /etc/synology-drive-sync/config.toml \
  --all-profiles --max-total-delete 20 --output ndjson
```

The built-in overlap check covers only profiles selected by that process. It is not a lock and
cannot coordinate separate services, containers, hosts, or manual commands. If related profiles
must use separate scheduler entries, put every batch and single-profile invocation behind the same
host-level lock; use a distributed operational lock as well when more than one host can reach the
same destination. Size the scheduler's outer timeout for all source scans and hashes, every target
preflight, all sequential operations and retries, final reconciliation, log flushing, and cleanup.
See [Diagnostics and multi-profile batches](diagnostics-and-batch.md) for overlap, deletion-budget,
and partial-failure behavior.

### Linux systemd timer

[The systemd deployment](operations/systemd.md) provides a hardened `Type=oneshot` service and daily timer. The service runs as a dedicated `sdsync` account with no capabilities, a read-only filesystem namespace, socket creation limited to Unix/IPv4/IPv6 families, and a password supplied through `LoadCredential`. `RestrictAddressFamilies` does not filter by protocol or destination; use host/network firewall policy when egress must be limited to the reverse proxy.

For TOTP, install the documented `synology-drive-sync-totp.conf.example` drop-in. It adds a second `LoadCredential` mount and passes only its protected path to `--totp-secret-file`. Neither seed belongs in `sync.env`.

Test the service before enabling the timer:

```bash
sudo systemctl daemon-reload
sudo systemctl start synology-drive-sync.service
sudo journalctl -u synology-drive-sync.service
sudo systemctl enable --now synology-drive-sync.timer
```

### Cron fallback

[The cron deployment](operations/cron.md) uses a mode-`0600` environment file containing only non-secret settings and protected secret-file paths. The wrapper forces `--no-vault`, supports an optional `SDSYNC_TOTP_SECRET_FILE`, and the sample crontab uses `flock` to reject overlap.

Prefer systemd on Linux. A cron session commonly lacks the D-Bus and unlocked Secret Service collection required for OS-vault access.

### macOS LaunchAgent

[The LaunchAgent deployment](operations/launchd.md) runs in the logged-in user's GUI session so the same user's login Keychain is available. Enroll the password and optional TOTP seed first, replace every plist placeholder with a non-secret absolute value, validate with `plutil`, and bootstrap it with `launchctl`.

The login Keychain generally must be unlocked. Do not place secret values in the plist.

### Windows Task Scheduler

[The Task Scheduler installer](operations/windows.md) creates a current-user, interactive-token, limited-privilege daily task. It uses the same user's Windows Credential Manager entries, refuses to replace an existing task unless `-Force` is explicit, and configures Task Scheduler to ignore overlap.

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

For the DSM SPK, disable the package schedule, stop the package, verify the new matching-architecture
SPK, and use Package Center's manual upgrade flow. The upgrade retains and validates package-private
profiles and credentials. Run package `doctor` and review an additive `plan` before restarting.
Package Center uninstall removes this package's private config, secrets, state, and logs after the
controller has stopped; it leaves both source and target data untouched. See
[Synology DSM package](synology-package.md#upgrade-rollback-and-uninstall) before accepting the
non-Synology-package warning or uninstall data-removal prompt.

Stop the scheduler and record the currently installed version before an upgrade. Rerun an installer with an explicit `--version`/`-Version`; it replaces only the executable in the selected directory. Then run `--version`, `config validate`, authenticated `doctor target`, and a fresh additive `plan` before restarting the schedule. If validation fails, keep the schedule stopped and rerun the installer with the recorded calendar version. Binary rollback does not reverse remote writes already performed by a sync.

Remove only the exact binary to uninstall:

```bash
rm -- "$HOME/.local/bin/synology-drive-sync"
```

```powershell
Remove-Item -LiteralPath "$env:LOCALAPPDATA\Programs\synology-drive-sync\synology-drive-sync.exe"
```

Scheduler definitions, logs, configuration, and OS-vault entries are intentionally retained so data removal remains explicit. Disable and remove the exact scheduler definition with `systemctl disable --now synology-drive-sync.timer`, `launchctl bootout "gui/$(id -u)/io.github.supermarsx.synology-drive-sync"`, `Unregister-ScheduledTask -TaskName 'Synology Drive Sync'`, or an inspected `crontab -e` as appropriate. Review paths before removing units, plists, log files, or configuration.

Use `credentials remove ... password|totp|all` before retiring a credential profile; removal kind is deliberately explicit.
