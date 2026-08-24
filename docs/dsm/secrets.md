# Secrets and protected values

Each profile can own three protected values:

- target DSM password;
- optional DSM TOTP seed; and
- optional remote logging bearer token.

They are separate package-owned files, not TOML fields. A profile snapshot returns only
`has_password`, `has_totp`, and `has_remote_log_token` booleans. It never returns a stored value,
masked prefix, length, hash, or reusable derivative.

## Keep, replace, and clear

Every graphical secret editor has an explicit mode:

| Mode | Browser input | Package effect |
| --- | --- | --- |
| Keep existing | Hidden/disabled | No secret job is sent; existing protected file remains unchanged |
| Replace securely | Required password-style input | Enqueues a dedicated secret replacement after profile configuration is observable |
| Clear stored value | Hidden/disabled | Enqueues removal; JSON carries `null`, never an empty substitute secret |

No mode is inferred from an empty text box. Cancelling a dangerous profile confirmation leaves the
entered replacement available for correction; once submission begins, secret inputs are cleared.
They are also cleared on page exit and after an error.

For a new profile, configuration must be applied before a secret can reference it. The dashboard
therefore queues profile configuration, polls its sanitized terminal result, waits for snapshots to
show the profile for up to a bounded deadline, and only then queues and polls secret operations. If
the profile never appears, secret handoff stops instead of becoming an orphaned or misdirected job.

## Password

Password is required for target authentication. Use a dedicated, non-administrator target account
limited to File Station and the intended subtree. Replacing or clearing it does not change the target
NAS account; it changes only this package's protected copy.

Dashboard and manager secret inputs are limited to one non-empty line and 4,096 bytes. Newlines,
empty values, oversized inputs, unknown profile names, and unexpected request fields are rejected.

CLI equivalents:

```bash
sudo -u synology-drive-sync -- "$MANAGER" set-password personal
sudo -u synology-drive-sync -- "$MANAGER" remove-password personal
```

`set-password` uses a masked terminal prompt. `--from-file FILE` is available for controlled
provisioning; the input must be a readable non-symlink regular file whose first and only line is the
secret. Remove that provisioning file after the copy succeeds.

## TOTP seed

TOTP expects the existing Base32 manual seed or original `otpauth://` URI, not a current six-digit
code. The package generates a time-based code only after the File Station login flow requests it.
Synchronize both NAS clocks.

Secure SignIn push approval, interactive hardware/security keys, and other approval challenges are
not supported unattended File Station mechanisms. Storing a TOTP seed beside the password also
places both factors inside the package account's security boundary; decide whether that is acceptable
for the target account.

```bash
sudo -u synology-drive-sync -- "$MANAGER" set-totp personal
sudo -u synology-drive-sync -- "$MANAGER" remove-totp personal
```

## Remote logging token

Remote logging configuration contains a non-secret HTTPS collector URL and mode. Its bearer token is
stored separately and follows the same keep/replace/clear contract:

```bash
sudo -u synology-drive-sync -- "$MANAGER" set-remote-log-token personal
sudo -u synology-drive-sync -- "$MANAGER" remove-remote-log-token personal
```

Clearing the token while remote-log mode is `required` can make subsequent operations fail. Review
the profile and collector behavior before clearing it.

## Browser and bridge handling

The page does not place secret values in URLs, local storage, session storage, DOM text, logs, or
snapshot responses. A replacement travels in one bounded JSON POST over the current same-origin DSM
session. The authenticated bridge validates the exact operation/field schema, publishes the job and
a separate private secret file atomically, and returns only a queued identifier.

The CGI process does not run the secret-changing manager action. The clean package controller claims
the private job, reads the protected secret file without following symlinks, feeds it to the manager
over standard input, removes the claimed secret, and writes only a sanitized result. Sensitive JSON
key names and an exact submitted secret echoed by a child process are redacted; unsafe manager output
is replaced with a generic failure.

## Filesystem protection

Package directories are mode `0700`; generated configuration and secret files are mode `0600` and
owned by the package identity. Profiles set `no-vault = true`, avoiding a desktop D-Bus Secret
Service dependency on DSM. The dashboard cannot choose or browse these paths.

Do not copy secrets into:

- `configure-profile` arguments or generated TOML;
- shell history, environment files, screenshots, browser developer tools, or support transcripts;
- routine definitions, Notification Center messages, activity text, or remote-log URLs.

If exposure is suspected, rotate the target-side credential first, then replace the package copy and
run Doctor. Clearing a local copy does not revoke a password or token at its issuer.
