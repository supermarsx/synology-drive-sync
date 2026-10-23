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
| New/save profile | `configure-profile --name NAME --source ... --url ... --username ... --remote ... [profile options]` |
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
| Doctor | `doctor [NAME|--all] [--level LEVEL] [--write-test]` |
| Plan | `plan [NAME|--all] [--allow-delete] [--max-total-delete N]` |
| Run | `run [NAME|--all] [--allow-delete] [--max-total-delete N]` |
| Service/run snapshot | `status` |
| Bounded logs | `logs [LINES]` |
| Package paths | `paths` |

With no profile name, Doctor/Plan/Run uses the selected default. Only explicit `--all` selects every
profile. Doctor `LEVEL` is `quick`, `standard` (the default), or `extensive`. `--write-test` applies
only to Doctor at Extensive depth; omitting an explicit level with that legacy invocation promotes
it to Extensive, but new automation should pass both flags. `--allow-delete` applies only to
Plan/Run, and `--max-total-delete` applies to an all-profile deletion action. Inapplicable or
trailing options are rejected rather than ignored. See [Health and Doctor](operations.md#health-and-doctor)
for source behavior, independent target execution, and bounded section evidence.

`configure-profile` is a complete replacement of the named profile's non-secret settings. It covers
the dashboard's target, sync, safety, network, TLS, local-output, and remote-observability fields,
including `--log-format`, `--progress`, and `--output`. DSM keeps the local log path, OS-vault policy,
and credential-file locators fixed. Password, TOTP, and remote-log-token values use the separate
write-only commands above and survive an ordinary profile reconfiguration unless explicitly cleared.
The DSM profile contract accepts `--output human|json|ndjson`, caps `--max-delete` at `2147483647`
for armv7 portability, and caps `--max-rate` at JavaScript's exact-integer maximum
`9007199254740991`.
Routine, legacy-schedule, and all-profile foreground `--max-total-delete` ceilings use the same
`0..2147483647` portable bound.

`configure-alerts` is the one command with a deliberate parity gap. Its five desktop-alert fields
match the dashboard's Notifications tab exactly, but its three DSM system log fields
(`--system-log`, `--system-log-level`, `--system-log-message-id`) are CLI-only and have no dashboard
equivalent, because the message identifier belongs to DSM's own catalogue and must be verified
against a specific DSM build rather than typed into a form. The desktop fields are a full
replacement on every save; the system log fields default to what is already stored, so saving from
the dashboard does not silently disable system logging. See
[DSM system log](operations.md#dsm-system-log).

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

<!-- topology: keep this note accurate as reads move between the manager and the service. -->

The API service reads the inputs for all three itself and assembles the same documents, under a
stricter file contract than the manager's: owner, mode `0600`, link count and size, all checked on the
descriptor it opened rather than on the path. For `api logs` it reads each source backwards from the
newest line and stops at 1 MiB scanned per source, marking a source it had to stop on with
`"scan_limited": true`; it shows every error line and every unlevelled line whatever the category
level, `off` included, and reproduces the manager's redaction. All three remain supported CLI
contracts and remain the oracles differential tests compare against, byte for byte with the
snapshot's generation timestamp as the only normalised field (for `logs`, the in-service document
adds only `scan_limited`), but a dashboard poll no longer reaches the manager for any of them unless
the forced shell lane is engaged. `api logs --source run-stderr` serves the core's error output of the
last runs and is not part of `--source all`.

A mutation result (`sdsync.dsm-result.v1` with `"ok": true`) may carry `"warnings"`, a list of one to
sixteen `{"code", "message"}` entries meaning *the change is saved, but something it depends on did not
complete*. `controller_reload_failed` says the running controller was not told to reload, so the
previous schedule, routines or policy stay in effect until the package service restarts or a later save
reloads it. A manager that prints the older single pair `warning_code` and `warning` is accepted and
normalised into `warnings`, so the dashboard only ever sees the list. The bridge records each entry as
one `mutation_warning` record in `api.log` and shows the dashboard a red *Saved, but it is not fully in
effect* notice with the message. The bridge also captures the queued manager's standard error (16 KiB
kept, the rest counted) and records a non-empty one as `manager_stderr`, rather than discarding it.

`service.state` and `service.pid` are the one part of the snapshot that is not a function of package
files, and the service derives them the same way the manager does rather than from the controller's
own state file — because a controller killed outright leaves `state=running` behind in that file. A
verified identity makes the service `running`, a process that is merely alive makes it `untrusted`,
and a process that is not alive is `stopped` at pid zero. The verified case requires the controller
executable, the PID file, the readiness record and the live private lock to agree on one
pid/start/boot triple.

One behaviour differs, and only in how a refusal is named. A malformed record or an unsafe file is
refused by both, and neither serves it: the manager exits non-zero and the page reports
`manager_exit_status`, while the service reports what it actually found — `package_state_corrupt`
for a record it cannot parse, `config_file_unsafe` for a file whose ownership or mode is not what
the package wrote.

Two of the manager's reading rules surprise people, and the service reproduces both rather than
improving on them. A package state or routine document that exists must carry **every** key the
manager reads from it: the default applies when the whole file is absent, and a key missing from a
file that is present is a corrupt record. A profile fragment is the opposite — a missing key there
takes its default — except for `excludes`, which must be present exactly once.

`api status-rollup` is the other exception. The API service no longer runs the manager for it: the
manager's whole contribution was to list the configured profile names and forward the core's
document unchanged, and the service now composes that document in process from the same library
function the core calls. The command remains a supported CLI contract and remains the oracle a
parity test compares against, but a dashboard poll no longer reaches it. Two differences follow
from the manager no longer being in the path, both of them cases where the manager turned a valid
document into a failed read:

- The manager truncates any API document past 320 KiB and substitutes a truncation marker, which
  the service then rejects as a schema mismatch. The in-process answer has no such ceiling, so a
  rollup over several hundred profiles is served rather than refused.
- The manager collapses a package secrets path to `[secret]` with a pattern that consumes to the
  next space, which makes the surrounding JSON unparseable. The in-process answer does not
  reproduce that.

The manager's neutral-label substitution **is** reproduced: the package's own `home` and `var`
paths, and the physical paths they resolve to, are rewritten to `[package-home]` and
`[package-var]` on both paths. A differential test asserts the two are byte-identical, including
that substitution and including the profile ordering, with the generation timestamp as the one
field allowed to differ.

An operator can send every read back through the manager. See the read-lane switch in
[operations](operations.md).

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
uses atomic replacement and validates owner, type, mode, and containment. The CGI and API service
validate the fixed socket, parent, and kernel peer identities, and the API service additionally
reads package state directly for the reads it answers in process, under a stricter file contract
than the manager's: owner, mode `0600`, link count, and size, all checked on the descriptor it
opened rather than on the path. Manual intervention can make the control plane fail closed, and a
hand-edited file that the manager would have accepted can be refused by the service with a named
code rather than served.

## Exit statuses

| Exit | Meaning |
| ---: | --- |
| `0` | Wrapper command succeeded |
| `64` | Invalid command, option, argument, schema, or validated input |
| `66` | Required profile, configuration, or protected credential is absent |
| `69` | Installed executable or required runtime facility is unavailable |
| `73` | Unsafe package path, state, lock, log, queue, or untrusted PID |
| `75` | Another management or Plan/Run operation is active; a deferral, retried, not a failure |
| `76` | A configuration change that needs the running controller to reload (`configure-routine`, `remove-routine`, `enable` / `set-schedule` with an interval, `configure-performance`) was **saved and audited as succeeded**, but the controller could not be told to reload it. stderr names the reason and PID |
| `77` | Command was not run as the DSM package identity |
| `78` | The package supervisor refused to start the core (binary identity mismatch); the core never ran, and one `supervised core refused: <code>` line is printed |
| `130` / `143` | Interrupted management or terminated Plan/Run operation |

The core's own `3` (partial: finished with transient paths deferred to the next run) is propagated
as is and is recorded as a `partial` outcome, not a failure; `plan` can also exit `3`. Lasting skips
and deferrals exit `0` with a warning. Core planning, transport, authentication, and File Station
failures propagate their own nonzero status. Treat `75` as a bounded retry only after confirming the recorded operation is expected.
Investigate every other nonzero result and obtain a fresh Plan before retrying a mutation.

Exit `76` is not a failed save: do not repeat the command. The reasons are `pid_unreadable` (the PID file
holds no PID), `identity_refused` (the PID is live but not the verified controller), `identity_changed`
(the identity changed between the two checks) and `signal_failed` (`kill -HUP` failed). A missing PID
file, or a dead PID, is simply "controller not running" and exits `0`: the controller reads the saved
files when it starts. The failure leaves `var/state/controller-reload.pending` (package-owned `0600`,
lines `epoch=`, `reason=`, `pid=`, `operation=`), which the dashboard reads for its *Saved, not yet
applied* item, and logs `controller_reload_failed` at `error`. The controller removes the marker when
it starts and each time it processes a reload. Through `sdsync-dsm api <mutation>` the same condition
is exit `0` with a `controller_reload_failed` warning in the result (see above).

Only saves that need a reload can exit `76`: a routine change restarts watchers and polls, an enabled
schedule rebases its interval deadline, and a performance level is applied at reload. The controller
re-reads the alert policy on every event and the schedule file every turn, so `configure-alerts` and
a schedule switched off (`disable`) need none: if the signal cannot be delivered the save still exits
`0`, writes no marker, and logs `controller_reload_skipped` (`warn`, with `needed=false`).

`sdsync-dsm status` prints `service=unknown` when `controller.pid` exists but holds no PID. An action
that names no profile uses the default profile; if `default-profile` exists but names no existing
profile the action is refused with exit `66` and a `default_profile_invalid` controller-log event,
rather than quietly running on the first profile (the dashboard snapshot still shows the first profile
as the default, as before).

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
