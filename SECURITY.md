# Security policy

Please report suspected vulnerabilities privately through GitHub's **Security > Report a vulnerability** flow. Do not include DSM passwords, TOTP seeds, `otpauth` provisioning URIs, one-time codes, session IDs, SynoTokens, private hostnames, or file contents in a public issue.

Only the latest release and the current `main` branch receive security fixes.

OS credential vaults protect enrolled secrets at rest; they do not isolate them from every process running as the same unlocked OS user. Storing both a DSM password and its TOTP seed supports unattended operation but reduces factor separation if that local account is compromised. Store only the password and enter current codes interactively when that boundary matters.

The project never needs a plaintext credentials file. On systems where the native vault is unavailable, use an external secret provider through the documented stdin/environment interfaces or disable vault lookup with `--no-vault`.

This tool can delete remote data only when `--delete` is explicitly enabled. Reproduce destructive issues against a disposable destination and redact all credentials and paths from diagnostics.
