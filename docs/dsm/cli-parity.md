# CLI parity and private paths

The DSM dashboard and `sdsync-dsm` operate the same package-owned control plane. The dashboard is the
normal graphical surface; the CLI is the authoritative SSH recovery, provisioning, and inspection
surface.

## Discover the actual package identity

DSM normally bases the system-internal account name on the package name, but it may collision-rename
that NSS account. Do not assume the literal username `synology-drive-sync`. Resolve the owner of the
package home once in each administrator SSH session, then use that value for manager commands:

```bash
PACKAGE_USER=$(stat -L -c '%U' /var/packages/synology-drive-sync/home)
case "$PACKAGE_USER" in ''|root|UNKNOWN) echo 'unsafe package owner' >&2; exit 1 ;; esac
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo -u "$PACKAGE_USER" -- "$MANAGER" status
```

The runtime authorization does not trust a fixed username. It derives the package UID from trusted
executable or package-home ownership and compares real/effective process and socket-peer UIDs to that
value.

Profile, secret, routine, schedule, Doctor, Plan, and Run operations refuse root and other users with
exit `77`. Do not use plain `sudo "$MANAGER" ...`; even if it were permitted, it would test an
administrator's ACL instead of the package user's real source access.

## Graphical action to CLI command

| Dashboard action | CLI parity |
| --- | --- |
| New/save profile | `configure-profile --name NAME --source ... --url ... --username ... --remote ...` |
| Remove profile | `remove-profile NAME` |
| Use as default | `set-default NAME` |
| Profile list/default badges | `list-profiles` |
| Non-secret generated profile view | `show-config [NAME]` |
| Replace password | `set-password NAME` |
| Clear password | `remove-password NAME` |
| Replace TOTP seed | `set-totp NAME` |
| Clear TOTP seed | `remove-totp NAME` |
| Replace remote-log token | `set-remote-log-token NAME` |
| Clear remote-log token | `remove-remote-log-token NAME` |
| Save per-profile routine | `configure-routine --profile NAME ...` |
| Remove routine | `remove-routine NAME` |
| DSM notification policy | `configure-alerts ...` |
| Doctor | `doctor [NAME|--all] [--write-test]` |
| Plan | `plan [NAME|--all] [--allow-delete] [--max-total-delete N]` |
| Run | `run [NAME|--all] [--allow-delete] [--max-total-delete N]` |
| Service/run snapshot | `status` |
| Bounded logs | `logs [LINES]` |
| Package paths | `paths` |

With no profile name, Doctor/Plan/Run uses the selected default. Only explicit `--all` selects every
profile. `--write-test` applies only to Doctor. `--allow-delete` applies only to Plan/Run, and
`--max-total-delete` applies to an all-profile deletion action. Inapplicable or trailing options are
rejected rather than ignored.

## Common recovery sequence

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" paths
sudo -u "$PACKAGE_USER" -- "$MANAGER" list-profiles
sudo -u "$PACKAGE_USER" -- "$MANAGER" show-config personal
sudo -u "$PACKAGE_USER" -- "$MANAGER" status
sudo -u "$PACKAGE_USER" -- "$MANAGER" logs 200
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor personal
sudo -u "$PACKAGE_USER" -- "$MANAGER" plan personal
```

`show-config` is non-secret. Never use direct `cat` on secret files for troubleshooting.

## Machine-readable manager API

`sdsync-dsm api snapshot`, `api logs --lines N`, and `api activity --lines N` are strict JSON
contracts used by the package-user API service and tests. A direct package-user snapshot
deliberately reports:

```json
{
  "capabilities": {
    "mutations": false,
    "secrets": false,
    "write_test": false
  }
}
```

Only the package-user API service returns true capabilities and `private_queue=true` after the
DSM-launched package-UID CGI has authenticated the cookie once and crossed the fixed peer-verified
`0600` socket. The service then validates the strict relay/request, independently resolves the exact
UID/name and administrator membership, recomputes the cookie/optional-token session binding, and
checks policy and package CSRF. Do not call
`ui/api.cgi` from SSH, connect to `api.sock`, forge relay data, or write queue files manually. The
ordinary `0755` `sdsync-dsm-api --consume-job` form is controller-internal and validates its identity
and exact private paths.

The CLI does not require the CGI, socket, or API service. It is the recovery path when the dashboard
cannot launch, but mutations still require the exact package identity and the same private-state and
overlap validation.

## Private package paths

| Purpose | Path |
| --- | --- |
| Core binary | `/var/packages/synology-drive-sync/target/bin/synology-drive-sync` |
| Manager | `/var/packages/synology-drive-sync/target/bin/sdsync-dsm` |
| API service and private job consumer | `/var/packages/synology-drive-sync/target/bin/sdsync-dsm-api` |
| Package-owned DSM CGI relay (`0755`, exact package UID at runtime) | `/var/packages/synology-drive-sync/target/ui/api.cgi` |
| Fixed package-owned API socket (`0000` prepared, same inode `0600` active) | `/var/packages/synology-drive-sync/var/run/api.sock` |
| Dashboard assets | `/var/packages/synology-drive-sync/target/ui/` |
| Generated config | `/var/packages/synology-drive-sync/home/config/config.toml` |
| Profile fragments | `/var/packages/synology-drive-sync/home/config/profiles.d/` |
| Default profile | `/var/packages/synology-drive-sync/home/config/default-profile` |
| Routines | `/var/packages/synology-drive-sync/home/config/routines.d/` |
| Legacy global schedule | `/var/packages/synology-drive-sync/home/config/schedule.conf` |
| Alert policy | `/var/packages/synology-drive-sync/home/config/alerts.conf` |
| Password/TOTP/remote-log-token files | `/var/packages/synology-drive-sync/home/secrets/` |
| Controller/run/routine/health state | `/var/packages/synology-drive-sync/var/state/` |
| PID and overlap locks | `/var/packages/synology-drive-sync/var/run/` |
| API-service PID file | `/var/packages/synology-drive-sync/var/run/api.pid` |
| Private control queue/results/CSRF key | `/var/packages/synology-drive-sync/var/control/` |
| Package logs and Activity | `/var/packages/synology-drive-sync/var/log/` |
| API-service log | `/var/packages/synology-drive-sync/var/log/api.log` |
| DSM package-control log | `/var/log/packages/synology-drive-sync.log` |

DSM must provide the package-private `var` root (through `SYNOPKG_PKGVAR` or the canonical
`/var/packages/synology-drive-sync/var` FHS link). The package fails closed when that root is absent rather than
falling back to a shared or mismatched state path. Use `paths` to observe the actual resolved directories.

Do not edit, chmod, chown, symlink, enqueue, or connect to files in these directories. The manager
uses atomic replacement and validates owner, type, mode, and containment; the CGI and API service
also validate the fixed socket, parent, and kernel peer identities. Manual intervention can make the
control plane fail closed.

## Exit statuses

| Exit | Meaning |
| ---: | --- |
| `0` | Wrapper command succeeded |
| `64` | Invalid command, option, argument, schema, or validated input |
| `66` | Required profile, configuration, or protected credential is absent |
| `69` | Installed executable or required runtime facility is unavailable |
| `73` | Unsafe package path, state, lock, log, queue, or untrusted PID |
| `75` | Another management or Plan/Run operation is active |
| `77` | Command was not run as the DSM package identity |
| `130` / `143` | Interrupted management or terminated Plan/Run operation |

Core planning, transport, authentication, and File Station failures propagate their own nonzero
status. Treat `75` as a bounded retry only after confirming the recorded operation is expected.
Investigate every other nonzero result and obtain a fresh Plan before retrying a mutation.

## Package Center parity

Package Center start/stop controls both the dashboard API service and lifecycle controller. SSH
parity is:

```bash
sudo synopkg status synology-drive-sync
sudo synopkg start synology-drive-sync
sudo synopkg stop synology-drive-sync
```

Starting the package does not enable a schedule. Stopping requests cooperative shutdown of the API
service, controller, and verified active runner and does not silently discard an active job.
