# Passwords, TOTP, and secret sources

The TOML schema is non-secret. It accepts secret-file paths, but no password, reusable TOTP seed,
current OTP code, or bearer-token value. Command-line secret value arguments are intentionally
absent so process listings and shell history cannot capture them.

## Password resolution

The password is resolved in this order:

1. `--password-stdin` or `SDSYNC_PASSWORD_STDIN=true`;
2. `--password-file`, `SDSYNC_PASSWORD_FILE`, or profile `password-file`;
3. the secret value in `SDSYNC_PASSWORD`;
4. the current user's OS vault, unless disabled;
5. a masked interactive prompt when the command and terminal permit it.

Interactive password and TOTP prompts print one `*` for each entered character. Backspace removes
the corresponding marker, so the terminal confirms that input was received without exposing the
secret itself. This feedback applies only to a terminal prompt; vault, file, environment, and
standard-input sources remain silent.

`--password-stdin` suppresses a lower-layer password file. Batch commands reject it because a single
stream cannot safely distinguish independent profile credentials.

## TOTP resolution

When DSM challenges for a second factor, the client resolves:

1. `SDSYNC_OTP`, containing one current six-digit code;
2. a code generated from `--totp-secret-file`, `SDSYNC_TOTP_SECRET_FILE`, or profile
   `totp-secret-file`;
3. a code generated from the current user's OS-vault seed, unless disabled;
4. an interactive current-code prompt when possible.

The reusable seed may be Base32 or an `otpauth://` URI. It is read only after DSM requests TOTP;
accounts without a challenge do not cause seed generation.

## OS vault

Use the credential commands rather than exposing values as arguments:

```bash
synology-drive-sync credentials set-password --profile production
synology-drive-sync credentials set-totp --profile production
synology-drive-sync credentials status --profile production
synology-drive-sync credentials remove --profile production password
synology-drive-sync credentials remove --profile production all
```

For non-interactive enrollment, `credentials set-totp --secret-stdin` reads the reusable Base32 seed
or `otpauth://` URI from the first line of standard input and suppresses any lower-layer TOTP file.
Pipe it directly from a protected secret provider; do not place the value in a command argument or
shell history. `--totp-secret-file FILE` is clearer when the value already lives in a protected file.

Vault records are scoped by the normalized endpoint and username. They belong to the operating-system
user running the command; a scheduled service under another identity cannot see an interactive
user's vault.

`--no-vault` or `no-vault = true` disables both reads. `--vault` explicitly re-enables them over a
profile or environment setting.

## Protected files

Headless services commonly use separate files for the password, TOTP seed, and remote-log token.
Each should be:

- a regular non-symlink file;
- owned or readable only by the service identity;
- stored outside the repository and source tree;
- limited to one secret on its first line;
- excluded from backup/logging paths that do not have equivalent protection.

Use systemd `LoadCredential` where available. The shipped Windows Task Scheduler helper stores
credential-file paths and depends on Windows ACLs; cron requires an explicitly protected environment
file and secret files.

## Secret-bearing environment variables

`SDSYNC_PASSWORD`, `SDSYNC_OTP`, and `SDSYNC_REMOTE_LOG_TOKEN` carry actual secrets. They are
supported fallbacks, not preferred durable service configuration. Environment blocks may be visible
to same-user diagnostics, crash tools, child processes, or service managers.

`SDSYNC_REMOTE_LOG_TOKEN_ENV` is different: its value is only the **name** of another variable that
holds the bearer token.

## TOTP operational requirements

Generated codes depend on accurate system time. Keep the runner synchronized with a trusted time
source and include clock behavior in the live acceptance test. Never store one current OTP for a
scheduled job; use a protected reusable seed only when the account policy permits it.

## Session expiry and how long the password is held

DSM can expire or drop a session under a long run (codes `106`, `107` and `119`, and `105` only when
a follow-up `list_share` liveness probe also fails). The CLI re-authenticates once, transparently,
and resends the request that met the expiry. Re-authentication is serialised, capped at four per run,
and counted in the result's `recovered` object; a failed re-login surfaces its own cause (a one-time
code required, a changed password, a locked account) rather than a generic session error. A keepalive
touches the session every five minutes during long plan hashing and promotion, and once before the
first mutation.

**The password is therefore held for the whole run.** Before this, a run read the password, logged
in, and let the value go; an overnight mirror whose session expired failed after hours of work and
the next run repeated them. To re-login without a prompt nobody is watching, the process now keeps the
password in zeroizing memory until the run ends, unless it came from a `--password-file`: that file
is read again on each re-login and nothing is retained. A one-time code is generated again from the
TOTP seed file or the vault exactly as at first login; a password supplied through `--password-stdin`
or `SDSYNC_PASSWORD` cannot be re-read, which is why it is held. The re-login closure never logs or
prints the secret. The SDK does not install a re-authenticator (its secret provider is borrowed for
the call); it gets the keepalive and the pre-mutation touch only.

This widens the window in which the password exists in the process. Prefer `--password-file` or the OS
vault on a host where that matters, and treat a core dump of a running sync as containing the
password.

Application-owned secret buffers are erased on drop where their Rust types permit, but serialization,
TLS, HTTP, allocator, and operating-system buffers may retain intermediary copies. Avoid crash dumps
and hostile same-user processes on credential-bearing hosts.
