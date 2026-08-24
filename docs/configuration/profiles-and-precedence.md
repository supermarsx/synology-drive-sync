# Profiles and precedence

Every named profile is an independent job. Profiles do not inherit, merge with, or alias one
another. This keeps the effective source, destination, endpoint, identity, deletion limit, and
secret locator auditable for each selected job.

## Selecting one profile

The configuration file is selected by `--config`, then `SDSYNC_CONFIG`, then the platform default.
Within it, the profile is selected by:

1. `--profile NAME`;
2. `SDSYNC_PROFILE=NAME`;
3. top-level `default-profile`.

```bash
synology-drive-sync plan --config ./config.toml --profile production
```

Single-profile commands may provide `SOURCE` and `REMOTE` positionally even when the profile omits
them. That is useful when one endpoint/account serves several ad-hoc local directories.

## Complete jobs and batches

Batch plan/sync selects explicit profiles with `--profiles` or every named profile with
`--all-profiles`:

```bash
synology-drive-sync plan --config ./config.toml \
  --profiles production,archive \
  --max-total-delete 20 \
  --output json

synology-drive-sync sync --config ./config.toml \
  --all-profiles \
  --max-total-delete 20 \
  --output ndjson
```

Each selected batch profile must independently resolve:

- `source` and `remote`;
- `url` and `username`;
- a viable password source and, when needed, TOTP input;
- all network, safety, and output constraints.

Selection is deduplicated and execution order is deterministic by profile name. Every job is
preflighted before the first remote mutation. Overlapping source or remote scopes, conflicting
endpoint identities, invalid credentials, or a deletion-budget breach prevent the batch from
starting.

## Endpoint identity and aliases

Use one canonical public URL for each NAS. Static overlap validation cannot prove that two DNS names,
ports, or reverse-proxy prefixes are aliases for the same appliance. Treat aliases as one endpoint in
your configuration and operational locking.

## Paths relative to configuration

These profile paths are resolved relative to the TOML file:

- `source`;
- `password-file` and `totp-secret-file`;
- `ca-certificate`;
- `log-file`;
- `remote-log-token-file`.

This is stable across interactive shells and schedulers. It also means moving a configuration file
can change every relative path; rerun `config validate`, `config show`, and diagnostics after a move.

## Shared credentials in wrapper batches

The systemd and cron wrappers normally load one password file and optional TOTP seed file. They
refuse to apply that common override to a batch unless
`SDSYNC_BATCH_SHARED_CREDENTIALS=true` explicitly confirms that every selected profile uses those
same credentials. When accounts differ, use profile-owned protected files or separate service jobs.

See [diagnostics and multi-profile batches](../diagnostics-and-batch.md) for overlap, aggregate
result, deletion-budget, failure, and shared-lock behavior.
