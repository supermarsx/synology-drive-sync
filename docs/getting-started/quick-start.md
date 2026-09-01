# Quick start

This path deliberately stops at a reviewed, non-mutating plan. Follow the linked production runbook
before trusting important data or enabling deletion.

## 1. Install a verified build

Download the matching release archive or DSM package, its `SHA256SUMS`, and optionally verify the
GitHub artifact attestation. The platform installers verify the checksum before replacing an
existing installation.

See [installation and deployment](../installation.md) for native, container, DSM, and source-build
instructions, and [release artifacts and verification](../releases.md) for the exact trust chain.

## 2. Create the starter configuration

```bash
synology-drive-sync config path
synology-drive-sync config init
```

`config init` writes the repository's commented starter without overwriting an existing file. Edit
the generated file and set one complete profile:

```toml
default-profile = "production"

[profiles.production]
source = "./data/export"
remote = "/team-folder/project"
url = "https://files.example.com"
username = "mirror-bot"
compare = "content"
jobs = 2
delete = false
```

Relative paths are anchored to the directory containing the configuration file. Unknown TOML keys
are rejected. See the [complete configuration reference](../configuration/reference.md) before
enabling advanced options.

## 3. Store the password

Prefer the current user's OS vault when the process runs in a real user session:

```bash
synology-drive-sync credentials set-password --profile production
synology-drive-sync credentials status --profile production
```

If the account uses TOTP, store the Base32 seed or `otpauth://` URI separately:

```bash
synology-drive-sync credentials set-totp --profile production
```

Headless services may instead use protected `password-file` and `totp-secret-file` paths with
`no-vault = true`. Secret values never belong in TOML or command-line arguments. See
[passwords, TOTP, and secret sources](../configuration/credentials.md).

## 4. Validate without changing either side

```bash
synology-drive-sync config validate
synology-drive-sync doctor source --profile production --hash
synology-drive-sync doctor --profile production --level standard target
synology-drive-sync plan --profile production
```

Standard Target Doctor authenticates, checks permission, samples no more than five deterministic
direct children with the total/truncated state, and logs out. Its timed section breakdown remains
non-mutating. Use `--level quick` for unauthenticated TLS/routing/discovery only or
`--level extensive` for the fullest read-only target capability evidence. `plan` prints the complete
work a sync would do but does not mutate the NAS.

> [!WARNING]
> `doctor --level extensive target --write-test` is a separate mutation opt-in. Use it only against
> the prepared, disposable destination required by the production acceptance runbook.

## 5. Run one additive sync

After reviewing the plan against non-critical data:

```bash
synology-drive-sync sync --profile production
```

The safe default preserves remote-only entries. To automate the command, choose an
[unattended-operation model](../operations/scheduling.md) and keep the same scheduler identity,
configuration, secret store, and source path you diagnosed interactively.

## Useful next pages

- [Profiles and precedence](../configuration/profiles-and-precedence.md)
- [Network, reverse proxy, and TLS](../configuration/network.md)
- [Diagnostics and multi-profile batches](../diagnostics-and-batch.md)
- [Troubleshooting](../operations/troubleshooting.md)
