# Security policy

Please report suspected vulnerabilities privately through GitHub's **Security > Report a vulnerability** flow. Do not include DSM passwords, TOTP seeds, `otpauth` provisioning URIs, one-time codes, session IDs, SynoTokens, private hostnames, or file contents in a public issue.

Only the latest release and the current `main` branch receive security fixes.

OS credential vaults protect enrolled secrets at rest; they do not isolate them from every process running as the same unlocked OS user. Storing both a DSM password and its TOTP seed supports unattended operation but reduces factor separation if that local account is compromised. Store only the password and enter current codes interactively when that boundary matters.

Application-owned password, TOTP, session, bearer-token, request-field, and raw DSM response buffers are erased on drop where the Rust types permit it. This is defense in depth, not a guarantee that a secret existed in only one allocation: URL-form/multipart serialization, TLS and HTTP libraries, environment storage, the allocator, and operating-system network buffers can retain intermediary copies outside this crate's control. Avoid crash dumps and hostile same-user processes on machines handling credentials.

Native use does not require a plaintext credential file: prefer the OS vault or an external secret provider through the documented stdin/environment interfaces. Headless systemd, cron, and container deployments may instead reference protected password and TOTP files because no unlocked user vault is available. Keep those files outside the repository, restrict them to the service identity, expose them through scheduler/container secret mounts where possible, and pass only their paths with `--password-file` or `--totp-secret-file` together with `--no-vault`. Never put secret values in TOML, unit files, process arguments, or images.

Treat the local source tree as a security boundary. It should be exclusively owned by the unprivileged account running the sync and remain quiescent for the duration of a run, especially with `--delete`. The scanner rejects links and reparse points it observes, but portable filesystem APIs cannot make every path check and later open atomic: another principal able to rename or replace source components concurrently could race those checks and redirect a later traversal or upload outside the originally scanned tree. Do not run this tool elevated over a source writable by a less-trusted user or process.

This tool can delete remote data only when `--delete` is explicitly enabled. Reproduce destructive issues against a disposable destination and redact all credentials and paths from diagnostics.
