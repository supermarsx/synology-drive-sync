# Command reference

The explicit command tree is preferred. Run `synology-drive-sync --help` and
`synology-drive-sync <command> --help` for the exact interface shipped by the installed build.

| Command | Purpose and mutation boundary |
| --- | --- |
| `sync SOURCE REMOTE` | Apply one finite local-to-remote synchronization. Remote-only entries remain unless deletion is explicitly armed. |
| `plan SOURCE REMOTE` | Build and print the same plan without mutation. `--exit-code` returns 10 when work is pending. |
| `doctor source [SOURCE] [--hash]` | Local-only source validation; `--hash` reads and verifies every payload file. No DSM access. |
| `doctor --level quick target [REMOTE]` | Validate endpoint policy, TLS, reverse-proxy routing, API discovery, and baseline capabilities without credentials or destination access. |
| `doctor target [REMOTE]` | Run the default Standard authenticated permission, bounded-inventory, and logout checks without mutation. |
| `doctor --level extensive target [REMOTE]` | Require the fullest target content/download/delete/copy capability evidence without mutation. |
| `doctor --level extensive target [REMOTE] --write-test` | Explicit disposable create/upload/copy/MD5/CRC32/SHA-256 verify/cleanup probe. Mutating and never automatic. |
| `doctor --routing-only` | Legacy Quick spelling without a source/target subcommand. |
| `config path` | Print the platform default configuration path. |
| `config init` | Write the complete non-secret starter; refuses replacement unless `--force` is used. |
| `config validate` | Parse and validate the strict TOML schema and profile-local constraints. |
| `config show` | Print one non-secret effective profile. |
| `credentials set-password` | Prompt, read protected stdin/file input when selected, and store the password in the current user's OS vault. |
| `credentials set-totp` | Store a reusable Base32/otpauth seed in the OS vault; `--secret-stdin` reads it from the first line of standard input. |
| `credentials status` | Report whether password and TOTP vault entries exist, without reading their values into output. |
| `credentials remove` | Remove `password`, `totp`, or `all` vault material for the endpoint/account. |
| `completions SHELL` | Generate Bash, Zsh, Fish, PowerShell, or Elvish completion source. |
| `manpage [--all DIRECTORY]` | Generate the root roff page on stdout or every nested command page in a directory. |

Target Doctor reports fixed timed sections as `pass`, `warn`, `fail`, or `skip`. Standard and
Extensive inventory never recurses: it emits File Station's direct-child total and a deterministic
sample of at most five entries with explicit truncation state. Extensive is read-only unless the
separate `--write-test` flag is present; Quick and Standard reject that flag. See
[Diagnostics and multi-profile batches](../diagnostics-and-batch.md) for the complete contract.

The legacy spelling without an explicit `sync` subcommand remains supported:

```bash
synology-drive-sync ./project /team-folder/project \
  --url https://files.example.com \
  --username mirror-bot
```

Legacy `--dry-run` is equivalent to planning. New scripts should use `plan`.

## Stable exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success; for `plan --exit-code`, no changes are pending. |
| `10` | `plan --exit-code` found pending changes. |
| `2` | Command-line usage or configuration error. |
| `1` | Operational failure: filesystem, network, DSM, vault, deletion guard, or required-log delivery. |
| `130` | Cooperative Ctrl+C/SIGINT or SIGTERM cancellation. |

Treat every other nonzero value as failure and consume JSON/NDJSON output rather than parsing human
diagnostic prose. An aggregate deletion-cap breach is an operational safety failure (`1`), not a
usage error.

## Global output controls

Global configuration and output options may appear before or after a subcommand. Result output on
stdout is selected independently from diagnostics/progress on stderr:

```bash
synology-drive-sync --config ./config.toml \
  plan --profile production \
  --output json \
  --log-format json \
  --progress never
```

`-v` and its long form `--verbose` raise diagnostics to debug. Repeat the option (`-vv` or
`--verbose --verbose`) for trace. An explicit `--log-level` takes precedence.

Credential enrollment has separate non-secret input selectors: `credentials set-password
--password-stdin` reads a password from the first line of standard input, while `credentials
set-totp --secret-stdin` reads a reusable Base32 seed or `otpauth://` URI. Pipe from protected input;
never pass either value as an argument.

See the [CLI and environment reference](../configuration/cli-and-environment.md) and
[observability contract](../observability.md) for every layer and schema.
