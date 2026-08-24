# Security

The authoritative disclosure policy and supported-version statement live in the repository's
[SECURITY.md](https://github.com/supermarsx/synology-drive-sync/blob/main/SECURITY.md). Report suspected
vulnerabilities through the private channel named there rather than a public issue.

## Operational security baseline

- Use a dedicated non-administrator DSM account with access only to the chosen destination.
- Require HTTPS and install a private CA explicitly when needed.
- Keep password, TOTP, and bearer-token values out of TOML, argv, logs, and issue reports.
- Run under an unprivileged identity that exclusively controls the source tree.
- Treat concurrent source path replacement and remote writers as hazards; schedule a single-writer
  window.
- Enable and prove an independent recovery layer before additive overwrite or mirror deletion.
- Verify release checksums and provenance before deploying into a sensitive environment.

The application uses memory erasure for owned secret buffers where the Rust types permit it, but no
application can guarantee a secret existed in only one allocation. HTTP/TLS serialization,
environment storage, allocators, operating-system buffers, and crash dumps may retain intermediary
copies.

Complete the [live-NAS acceptance and recovery runbook](production-acceptance.md) for the exact
deployment rather than treating local tests as production proof.
