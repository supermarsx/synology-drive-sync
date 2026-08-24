# Configuration model

Configuration is deliberately non-secret and layered. A named TOML profile supplies durable job
settings; environment variables adapt a deployment; command-line values make one explicit
invocation different. The effective value is resolved in this order:

```text
command line -> Clap environment variable -> selected profile -> built-in default
```

Selection itself follows `--profile`, then `SDSYNC_PROFILE`, then the file's `default-profile`.
`--config` or `SDSYNC_CONFIG` selects a non-default configuration path.

Three boolean negations deliberately override lower layers:

- `--no-delete` defeats `delete = true` or `SDSYNC_DELETE=true`;
- `--vault` defeats `no-vault = true` or `SDSYNC_NO_VAULT=true`;
- `--no-quiet` defeats `quiet = true` or `SDSYNC_QUIET=true`.

Command-line exclusion rules are appended to the selected profile's `excludes`; they do not replace
the profile list. Profiles never inherit from one another.

## Configuration files

Run `synology-drive-sync config path` to print the default location:

| Platform | Default path |
| --- | --- |
| Linux | `$XDG_CONFIG_HOME/synology-drive-sync/config.toml`, otherwise `~/.config/synology-drive-sync/config.toml` |
| macOS | `~/Library/Application Support/synology-drive-sync/config.toml` |
| Windows | `%APPDATA%\synology-drive-sync\config.toml` |

`config init` copies the complete commented starter. It refuses to replace an existing file unless
`--force` is supplied. Relative paths inside TOML are anchored to the configuration file's
directory, not the caller's current working directory.

```bash
synology-drive-sync config init
synology-drive-sync config validate
synology-drive-sync config show --profile production
```

`config show` prints an intentionally non-secret effective profile. Paths and environment-variable
names may appear; password, seed, OTP, and bearer-token values never do.

## Strict schema

Unknown top-level keys, unknown profile keys, invalid enum values, and invalid combinations fail
before synchronization. The schema accepts paths to protected secret files and the *name* of a
remote-log token environment variable, but has no password, TOTP seed, current OTP code, or bearer
token value field.

Use these pages as the contract:

- [Complete TOML reference](reference.md) — every top-level and profile key.
- [CLI and environment variables](cli-and-environment.md) — core inputs versus wrapper-only inputs.
- [Profiles and precedence](profiles-and-precedence.md) — complete jobs and batch selection.
- [Passwords, TOTP, and secret sources](credentials.md) — secret resolution and unattended use.
- [Network, reverse proxy, and TLS](network.md) — URL, trust, timeout, retry, and rate controls.
- [Comparison, exclusions, and deletion](safety.md) — parity and destructive-operation guards.
- [Full configuration example](example.md) — the shipped starter rendered from the repository file.
