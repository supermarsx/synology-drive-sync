# Compatibility and release selection

The SPK is intentionally architecture-specific and DSM-version-bounded. A CPU instruction-set match
by itself is not enough: the exact model, its supported DSM branch, its official Package Arch, the
reported runtime architecture, and the SPK's embedded ELF must all agree.

## Supported DSM contract

`INFO` declares:

```text
os_min_ver="7.0-40759"
os_max_ver="7.4-99999"
```

The ceiling is deliberate. DSM 7.5 or DSM 8 must not silently install a package that has not been
reviewed against that package framework. Model availability is narrower where Synology stopped a
model earlier or introduced it later. In particular, Evansport `i686` is restricted to DSM 7.0 and
7.1 by the model/toolkit matrix.

Use the interactive [release selector](../release-selector.md). It evaluates the captured official
model catalog and fails closed on unknown models, DSM Enterprise, DSM 6 or older, unsupported
branches, unreviewed future DSM versions, and contradictory model/architecture input.

## Four release SPKs

| Runtime reported by `uname -m` | Official CPU-table Package Arch mapping | SPK `INFO` arch | Release asset | Embedded Rust target |
| --- | --- | --- | --- | --- |
| `x86_64` | Supported DSM 7 x86-64 member platforms resolved by the selector | `x86_64` | `synology-drive-sync-YY.N-x86_64.spk` | `x86_64-unknown-linux-musl` |
| `aarch64` | `armada37xx`, `rtd1296`, `rtd1619b` | `armv8` | `synology-drive-sync-YY.N-armv8.spk` | `aarch64-unknown-linux-musl` |
| `armv7l` or ARMv7-A | `alpine`, `alpine4k`, `armada370`, `armada375`, `armada38x`, `armadaxp`, `comcerto2k`, `monaco` | `armv7 armada370 armada375 armada38x armadaxp comcerto2k monaco` | `synology-drive-sync-YY.N-armv7.spk` | `armv7-unknown-linux-musleabihf` |
| `i686` | `evansport` on DSM 7.0/7.1 | `i686` | `synology-drive-sync-YY.N-i686.spk` | `i686-unknown-linux-musl` |

`armv7l` is a Linux machine name, not a builder argument, `INFO` token, or asset suffix. An accepted
`armv7l` NAS uses the `armv7` release asset. The embedded binary is static little-endian ARMv7-A,
ELF32, EABI5, hard-float; an ARM64 or soft-float executable cannot be made compatible by renaming it.

ARMv5/88f628x and PowerPC devices are intentionally not mapped to a DSM 7 package. Their supported
DSM generations and toolchains require a separate legacy DSM 6 design. `noarch` is also incorrect:
the package contains two matching native ELF executables, the core and the authenticated API helper.

## Collect model and runtime evidence

Run these read-only commands on the NAS that will host the source:

```bash
uname -m
cat /proc/sys/kernel/syno_hw_version
get_key_value /etc.defaults/synoinfo.conf unique
```

In the selector:

1. enter the printed hardware model, not a visually similar retail name;
2. choose DSM and the installed minor branch;
3. enter the complete version/build for DSM 7.0 or 7.4 so the `7.0-40759` minimum or
   `7.4-99999` maximum can be proven (optional for DSM 7.1–7.3);
4. enter the reported architecture exactly;
5. stop if the model, branch, Package Arch, and runtime disagree.

Synology's [platform and `arch` mapping](https://help.synology.com/developer-guide/appendix/platarchs.html)
explains the `INFO` values. Model lifecycle rules and the selector snapshot date are documented under
the release selector's [DSM toolkit intervals](../release-selector.md#dsm-toolkit-intervals).

## Download and verify one exact release

Download the selected `.spk` and `SHA256SUMS` from the same calendar release. Example for ARMv8:

```bash
asset=synology-drive-sync-26.1-armv8.spk
expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print tolower($1) }' SHA256SUMS)
test "$(printf '%s\n' "$expected" | awk 'NF { n++ } END { print n + 0 }')" -eq 1
actual=$(sha256sum "$asset" | awk '{ print tolower($1) }')
test "$actual" = "$expected"
```

Optionally verify GitHub provenance:

```bash
gh attestation verify synology-drive-sync-26.1-armv8.spk \
  --repo supermarsx/synology-drive-sync
```

Do not mix an SPK from one release with a checksum manifest from another. Do not use a release
selector result as evidence that a downloaded file is authentic; selection and verification are
separate gates. See [Release artifacts and verification](../releases.md).

## What package validation proves

The package builder and validator bind `--arch` to ELF class, little-endian encoding, machine type,
program-header layout, static linkage, and executable load segments for both the core and API helper.
The validator also binds the filename, `INFO` version/architecture, DSM floor/ceiling, privileges,
dashboard resources, notification texts, deterministic icons, licenses, archive paths, and modes.

These checks do not prove that Synology accepts the SPK on a particular physical model, that the
package-user service can execute DSM's cookie authenticator, that DSM forwards the browser request
marker as `HTTP_X_SDSYNC_REQUEST=1`, or how the DSM pop-up renders on that release. Record those
separately during
[live-NAS acceptance](troubleshooting.md#live-nas-acceptance). AppLaunch may optionally supply a
`SynoToken` as a session-binding input, but the dashboard does not require one.
