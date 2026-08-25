# Disposable live-NAS acceptance and recovery

The automated suite does not log in to a NAS. Production approval therefore requires a disposable end-to-end exercise against the exact DSM version, File Station package, reverse proxy, account policy, scheduler identity, and Synology Drive indexing configuration that will be used in service. For the DSM SPK, this must be a real two-NAS test: install the package on the source NAS and target the second NAS through its production-equivalent File Station URL.

> [!WARNING]
> Use a new destination containing no valuable data. Keep `--delete` disabled until the deletion-isolation step, use an intentionally small deletion cap there, and never use the shared-folder root. The local source is authoritative: additive sync can overwrite changed remote files even without deletion.

## 1. Record prerequisites and recovery ownership

Record the release version or container digest, DSM and File Station versions, public reverse-proxy URL, logical File Station destination, scheduler identity, and the person responsible for alerts and restore. Use a dedicated non-administrator DSM account with File Station permission and write access only where required. Pause every other Drive client, File Station session, and automation that can write the disposable destination during acceptance; production mirror runs also require an operational single-writer window because File Station has no atomic compare-and-delete request.

Create two sibling logical paths in an enabled Synology Drive Team Folder or the test account's Drive-backed home:

- `/team-folder/sdsync-acceptance-UNIQUE`, the disposable sync destination;
- `/team-folder/sdsync-acceptance-canary-UNIQUE`, an out-of-scope canary that must never change.

For a user-home deployment, substitute `/home/Drive/sdsync-acceptance-UNIQUE` and
`/home/Drive/sdsync-acceptance-canary-UNIQUE` consistently. User Home service, the target account's
home, and its Drive directory must already exist. For a Team Folder, the shared-folder root must
already exist and be enabled in Drive. The sync may create a chosen missing subdirectory below an
existing writable share; it never provisions either DSM root or its ACL.

Enable and verify at least one independent recovery layer before continuing: Synology Drive version history, a shared-folder snapshot, or a separate backup. Perform a small manual restore from it now. A recycle bin helps with deletion but is not sufficient proof that an interrupted overwrite can be recovered.

## 2. Build a deterministic local corpus

Use a quiescent, exclusively owned local directory containing:

- a small UTF-8 text file;
- a binary file large enough to exercise the real proxy upload path;
- a nested file and an empty directory;
- a file that will be changed between two syncs.
- `move-from/server-copy.bin`, whose bytes are unique in the corpus, for the cross-parent server-copy exercise.

Record content hashes outside the source tree. On Unix:

```bash
(cd ./acceptance-source && find . -type f -print0 \
  | sort -z \
  | xargs -0 sha256sum) > ./acceptance-source.sha256
```

On Windows PowerShell:

```powershell
$sourceRoot = (Resolve-Path '.\acceptance-source').Path
@(Get-ChildItem -LiteralPath $sourceRoot -File -Recurse | ForEach-Object {
    [pscustomobject]@{
        RelativePath = [IO.Path]::GetRelativePath($sourceRoot, $_.FullName)
        Hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
    }
} | Sort-Object RelativePath) |
  ConvertTo-Json | Set-Content -Encoding utf8 '.\acceptance-source.sha256.json'
```

Do not place either manifest inside the source directory.

Exercise the production scanner locally before any DSM request:

```bash
synology-drive-sync doctor source ./acceptance-source \
  --hash --output json > ./acceptance-source-doctor.json
```

Require a successful, stable full-file read and review the reported counts. The diagnostic's MD5
hashing supports File Station compatibility and accidental-corruption checks; it does not replace
the independently retained SHA-256 manifest.

## 3. Prove reverse-proxy routing before authentication

The base URL must be the one public HTTPS origin used for every request. If it contains a prefix, confirm that the proxy rewrites that prefix's `/webapi/*` path to File Station rather than only serving the browser UI.

```bash
synology-drive-sync doctor \
  --url https://files.example.com \
  --routing-only
```

Acceptance requires valid TLS, JSON API discovery rather than HTML or a redirect, and no fallback to a private NAS address or DSM management port. Confirm the proxy request-body limit and send/read timeouts exceed the largest test upload.

## 4. Prove password and optional TOTP authentication

Enroll the password under the same OS identity that will run the scheduler:

```bash
synology-drive-sync credentials set-password \
  --url https://files.example.com \
  --username acceptance-bot
```

If the DSM account uses authenticator-app TOTP, import its existing manual Base32 key or original `otpauth://` URI:

```bash
synology-drive-sync credentials set-totp \
  --url https://files.example.com \
  --username acceptance-bot
```

Then authenticate and inspect the disposable destination without mutation:

```bash
synology-drive-sync doctor \
  --url https://files.example.com \
  --username acceptance-bot \
  target /team-folder/sdsync-acceptance-UNIQUE
```

Run this once interactively and once as the exact scheduler identity/logon mode. Require the non-mutating File Station permission probe to succeed for the exact existing destination, or for the first missing component under its nearest existing ancestor. For TOTP, synchronize the client and NAS clocks and prove a challenge succeeds without placing a seed or current code in arguments, profiles, unit files, task XML, or logs. DSM Secure SignIn approval and hardware/security-key challenges are not supported; failure here requires an app-compatible TOTP account rather than a plaintext workaround.

After confirming that the disposable destination itself already exists, opt into the live write
probe:

```bash
synology-drive-sync doctor \
  --url https://files.example.com \
  --username acceptance-bot \
  target /team-folder/sdsync-acceptance-UNIQUE \
  --write-test --output json > ./acceptance-write-test.json
```

This command deliberately creates a uniquely named child, uploads and verifies known content,
optionally exercises a server-side copy, and removes only its own probe artifacts. Require every
reported stage and cleanup to succeed. A crash, lost connection, NAS failure, or failed cleanup can
leave the reported probe path behind; inspect that exact path and remove it manually before retrying.
Do not turn this disposable mutation into a routine production health check.

## 5. Plan and perform an additive sync

Capture a machine-readable plan and require zero mutations during review:

```bash
synology-drive-sync plan ./acceptance-source \
  /team-folder/sdsync-acceptance-UNIQUE \
  --url https://files.example.com \
  --username acceptance-bot \
  --output json > ./acceptance-plan.json
```

Confirm every path is under the disposable destination and that `deletions` is zero. Then run the same invocation as `sync` without `--delete`. A second unchanged `plan --exit-code` must exit `0`. Change the designated local file, plan again, sync it, and verify the resulting plan is empty.

Next prove the non-destructive server-copy optimization from that clean baseline. Move the unique file to a different parent while preserving its basename:

```bash
mkdir -p ./acceptance-source/move-to
mv ./acceptance-source/move-from/server-copy.bin \
  ./acceptance-source/move-to/server-copy.bin
synology-drive-sync plan ./acceptance-source \
  /team-folder/sdsync-acceptance-UNIQUE \
  --url https://files.example.com \
  --username acceptance-bot \
  --output json > ./acceptance-copy-plan.json
jq -e '
  .plan.summary.server_copies == 1 and
  .plan.summary.uploads == 0 and
  .plan.summary.upload_bytes == 0 and
  (.plan.actions.copies | length) == 1
' ./acceptance-copy-plan.json
```

PowerShell can assert the same contract with:

```powershell
$copyPlan = Get-Content '.\acceptance-copy-plan.json' -Raw | ConvertFrom-Json
$summary = $copyPlan.plan.summary
if ($summary.server_copies -ne 1 -or $summary.uploads -ne 0 -or
    $summary.upload_bytes -ne 0 -or $copyPlan.plan.actions.copies.Count -ne 1) {
    throw 'Expected exactly one server copy with no upload bytes'
}
```

Inspect the copy action's source and destination, then execute it with JSON output:

```bash
synology-drive-sync sync ./acceptance-source \
  /team-folder/sdsync-acceptance-UNIQUE \
  --url https://files.example.com \
  --username acceptance-bot \
  --output json > ./acceptance-copy-sync.json
jq -e '
  .result.server_copied == 1 and
  .result.uploaded == 0 and
  .result.upload_bytes == 0
' ./acceptance-copy-sync.json
```

Require exactly one completed server copy, zero uploads, and zero uploaded bytes. Because this is additive, the original remote file remains; verify both copies have the expected bytes, then remove only the stale original through File Station before the later deletion-isolation exercise. Require another empty plan and regenerate the SHA-256 manifest from section 2 so it describes the final local paths.

This optimization requires a unique byte-identical remote candidate, different source and destination parents, the same final basename, and File Station server-copy support. A basename-changing rename or a same-parent move intentionally falls back to a verified upload; that fallback does not satisfy this server-copy acceptance assertion.

## 6. Verify bytes and Drive indexing externally

The default `content` comparison requires matching size, File Station MD5, and second-resolution file mtime, and verifies the remote MD5 again after upload. That is useful transport/content-correspondence evidence, but MD5 is not a collision-resistant security proof or an independent recovery check. Explicit `metadata` and `size-only` performance modes provide weaker change detection and do not establish content equality.

For production acceptance, independently download the completed destination through File Station, or wait for a separate Synology Drive client to finish downloading it, and compare every file against the recorded SHA-256 manifest. On Unix, place it at `./acceptance-download` and run:

```bash
(cd ./acceptance-download && sha256sum --check ../acceptance-source.sha256)
```

On PowerShell:

```powershell
$expected = @(Get-Content '.\acceptance-source.sha256.json' -Raw | ConvertFrom-Json)
$downloadRoot = (Resolve-Path '.\acceptance-download').Path
$actual = @(Get-ChildItem -LiteralPath $downloadRoot -File -Recurse | ForEach-Object {
    [pscustomobject]@{
        RelativePath = [IO.Path]::GetRelativePath($downloadRoot, $_.FullName)
        Hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
    }
} | Sort-Object RelativePath)
$difference = Compare-Object $expected $actual -Property RelativePath, Hash
if ($difference) { $difference; throw 'Downloaded bytes differ from the source manifest' }
```

File count, hierarchy, empty-directory presence, sizes, and modification times should also match the documented scope.

Separately open Synology Drive or use a Drive client and confirm the File Station writes become visible under the enabled Team Folder or My Drive location. Record indexing latency. A successful File Station sync alone does not prove Drive indexing or client visibility.

## 7. Exercise one bounded whole-file retry

Keep deletion disabled. Using a controlled test proxy or firewall rule, make exactly one large-file upload attempt return a retryable failure such as HTTP `503`, then restore the route before the configured attempts are exhausted. Do not redirect credentials or logging headers to another origin.

Acceptance requires a `retry.scheduled` diagnostic, final success, and an externally matching SHA-256 hash. There is no partial/resumable upload: content mode may recognize that the failed HTTP response nevertheless left the exact completed destination, but otherwise the next attempt must restart the complete file. Measure total wall-clock time for the complete worst-case acceptance workload, including local scanning and hashing, remote inventory, all planned operations, the exercised retry, final reconciliation, and shutdown. Size the scheduler's outer whole-workload limit above that measurement with explicit headroom; `timeout * (retries + 1)` covers only one operation and is not a sufficient scheduler budget. If all attempts fail, the job must return nonzero and the configured alert must fire; inspect a fresh `plan` before rerunning.

## 8. Prove deletion containment in the disposable root

Create one remote-only test file inside the disposable destination and a different canary file in the sibling canary path. Review an exact destructive plan with a cap of one:

```bash
synology-drive-sync plan ./acceptance-source \
  /team-folder/sdsync-acceptance-UNIQUE \
  --url https://files.example.com \
  --username acceptance-bot \
  --delete --max-delete 1 --output json > ./acceptance-delete-plan.json
```

The plan must name only the one in-root remote-only file. It must not name the sibling canary, an excluded path, a managed path, or the destination itself. Keep every other remote writer paused. Only after human review, repeat the same invocation as `sync`. The client must fail closed if its planned remote snapshot changes before deletion, but this check is not an atomic server-side compare-and-delete. Confirm the in-root file was removed, the sibling canary is unchanged, and the recovery layer can restore the deleted file. Restore it, verify its bytes, and disable deletion again.

## 9. Prove failure alerting and recovery

Run the scheduler once with a deliberately invalid disposable URL or username. It must return nonzero, retain bounded diagnostics, and deliver an alert to a monitored destination. Restore the valid configuration and require `doctor` plus a fresh additive `plan` before rerunning.

For a real overwrite, deletion, corruption, or unexpected plan:

1. stop the timer/task/job and keep `--delete` disabled;
2. preserve the plan, application log, scheduler result, release identity, and relevant redacted proxy diagnostics;
3. determine whether the authoritative local source is safe—restoring only the remote copy and immediately rerunning can overwrite it again;
4. restore from Drive version history, snapshot, or the independent backup;
5. verify restored bytes externally with SHA-256;
6. correct or isolate the local source, run authenticated `doctor`, and review a new additive plan before resuming the schedule.

For a multi-profile deployment, complete the source, target, write-probe, sync, recovery, and alert
checks for every profile independently. Then capture one additive
`plan --profiles ... --max-total-delete N --output json` (or the corresponding `--all-profiles`)
result, verify deterministic profile-name order, confirm that every configured root is distinct and
non-nested even across URL aliases, and review each deletion count plus the aggregate count. Do not
enable a scheduled batch until that complete preflight passes under the scheduler identity.
Require `all_targets_preflighted_before_mutation: true`. Retain the later sync aggregate as well;
compare each `preflight_plan` with its fresh `execution_plan`, review `mutation_authorized`, and
reconcile `preflight_deletions` against `execution_reserved_deletions`. Any `partial` job or
aggregate status, aggregate-cap exit `1`, or unexplained drift fails acceptance.

## 10. Prove DSM package installation and lifecycle

This section is mandatory when the executable is deployed as an SPK. The generic mock suite and
static SPK validator are not substitutes.

Record the source NAS model, CPU architecture, Package Arch, reported `uname -m`, exact DSM version
and build, selected `x86_64`, `armv8`, `armv7`, or `i686` SPK filename, SHA-256, GitHub attestation
result, and the Package Center warning shown for a non-Synology package. For ARMv7, also record the
compatible INFO token (`armv7`, `armada370`, `armada375`, `armada38x`, `armadaxp`, `comcerto2k`, or
`monaco`); for Evansport `i686`, prove the NAS remains on the supported DSM 7.0/7.1 line. Resolve the
asset with the [release selector](release-selector.md) and fail acceptance if its model, DSM, and
runtime inputs conflict. Verify that an SPK for the wrong architecture is not used. Install through
**Package Center > Manual Install** and confirm scheduling is disabled before entering any target
credentials.

On the source NAS, grant the actual package **System internal user** read-only permission to one
disposable source share and no access to unrelated shares. DSM may collision-rename its NSS
username; resolve `$PACKAGE_USER` through the
[canonical package-identity discovery](dsm/cli-parity.md#discover-the-actual-package-identity).
Confirm the physical source path is not a symlink and run every check as that identity:

```bash
MANAGER=/var/packages/synology-drive-sync/target/bin/sdsync-dsm
sudo -u "$PACKAGE_USER" -- "$MANAGER" paths
sudo -u "$PACKAGE_USER" -- "$MANAGER" configure-profile \
  --name acceptance-nas-b \
  --source '/volume1/sdsync-acceptance-source' \
  --url 'https://files-b.example.com' \
  --username 'acceptance-bot' \
  --remote '/home/Drive/sdsync-acceptance-UNIQUE' \
  --default
sudo -u "$PACKAGE_USER" -- "$MANAGER" set-password acceptance-nas-b
sudo -u "$PACKAGE_USER" -- "$MANAGER" set-totp acceptance-nas-b # when required
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor acceptance-nas-b
sudo -u "$PACKAGE_USER" -- "$MANAGER" plan acceptance-nas-b
sudo -u "$PACKAGE_USER" -- "$MANAGER" run acceptance-nas-b
```

Repeat with a Team Folder or shared-folder profile if that is the production destination type. Test
the arbitrary-folder and provisioning boundary explicitly: choose a new nested destination beneath
an existing writable share, leave the nested destination absent, require `doctor` to report the
nearest-existing-parent permission evidence, require `plan` to contain only the expected folder and
payload creation, then run and confirm the whole descendant hierarchy appears. Separately prove
that a nonexistent top-level shared-folder path fails closed rather than creating a DSM share, and
that a trailing slash, dot segment, Drive-incompatible name such as `~temporary`, or case-varied
DSM-managed component such as `@EADIR`/`@APPDATA` is rejected before any target request can mutate
data. Include a case-only local/remote directory conflict and a path whose selected prefix plus
relative name exceeds the Drive portability limit; both must fail during planning.

Add a second profile with a different target URL or destination and distinct protected credentials.
Require these commands to preflight every target and run in deterministic profile-name order:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" doctor --all
sudo -u "$PACKAGE_USER" -- "$MANAGER" plan --all
sudo -u "$PACKAGE_USER" -- "$MANAGER" run --all
```

When TOTP is used, prove a scheduled, non-interactive challenge with synchronized clocks and the
package-owned seed file; do not accept an interactive current-code prompt as unattended evidence.
When the URL has a reverse-proxy prefix, prove every discovery, login, inventory, upload, verify,
copy, and delete-probe request remains under that prefix. Exercise a file larger than the deployed
proxy's request-body threshold and measure the full run time. These combinations must be tested live
because they are not established by the mock suite.

Exercise the built-in controller with deletion disabled:

```bash
sudo -u "$PACKAGE_USER" -- "$MANAGER" enable --interval 60
sudo synopkg start synology-drive-sync
sudo -u "$PACKAGE_USER" -- "$MANAGER" status
sudo -u "$PACKAGE_USER" -- "$MANAGER" logs 200
```

Require no immediate sync at enable time, one run after the interval, rejection of an overlapping
manual run, a recorded successful exit state, bounded logs, and an externally delivered alert for a
later deliberately failed scheduled run. Reboot the source NAS and require the package controller,
schedule, package identity, profile, credentials, and source ACL to recover without widening
permissions. Stop the package during a large disposable transfer and verify cooperative cancellation
and a clean subsequent plan; a timed-out stop must not force-kill the worker.

Deletion acceptance requires both a profile configured with `--delete --max-delete N` and manager
execution/scheduling with `--allow-delete`; prove that omitting either opt-in suppresses deletion.
Then perform the existing one-file containment/canary test with both a small per-profile cap and
`--max-total-delete`. Disable deletion again immediately afterward.

Test an upgrade from the previous verified compatible SPK to the candidate. Stop the package first,
then require Package Center to retain private profiles, password/TOTP material, schedule, state, and
logs; require upgrade-time config validation; and rerun `doctor` plus an additive `plan` before
resuming. A package rollback does not revert completed File Station writes.

Finally, on a disposable package installation, disable and stop the package, uninstall it, and
verify that `/var/packages/synology-drive-sync/home` private configuration/secrets and package
`var` state/logs are removed while the complete local source, remote target, sibling canary,
snapshots, and DSM ACL configuration remain unchanged. Export the non-secret config and audit logs
before this test. Package-owned credentials removed by uninstall are intentionally unrecoverable.

The package reads the source NAS filesystem and uploads the bytes to the target NAS. Do not record a
claim of a direct NAS-to-NAS File Station server-copy primitive: server-side copy optimization is
limited to matching content already present within the one target NAS.

## Acceptance record

Production approval should retain evidence for all of the following:

- exact release or digest and successful artifact verification;
- local source, routing-only, authenticated target, and disposable write-probe results;
- password and, when applicable, TOTP under the scheduler identity;
- reviewed additive plans and externally verified file hashes;
- one unique cross-parent, same-basename move planned as exactly one server copy with zero upload bytes;
- Synology Drive indexing visibility;
- successful bounded retry and failed-job alert delivery;
- deletion containment with the sibling canary preserved;
- an enforced single-writer window for every destructive run;
- successful restore with verified bytes;
- deletion still disabled unless its operational need and cap were separately approved;
- for SPK deployments, source-share ACL isolation, arbitrary nested target creation, Package Center
  install/upgrade/start/stop/uninstall evidence, reboot persistence, and two-NAS TOTP/proxy results.

Repeat the relevant steps after DSM/File Station upgrades, reverse-proxy changes, authentication-policy changes, scheduler identity changes, or a major application upgrade.
