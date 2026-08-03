# Release artifacts and verification

Releases use calendar tags in strict `YY.N` form. `YY` is the two-digit UTC year and `N` is the next positive sequence number for that year, for example `26.1`, `26.2`, then `27.1`.

An automatic release starts only after the repository's exact `CI` workflow completes successfully for a `push` to `main`. That CI run includes a hash-pinned `cargo-audit` 0.22.2 binary which updates the RustSec advisory database and rejects vulnerabilities, yanked crates, unsoundness, and unmaintained warnings. A separate daily workflow reruns the same gate so a new advisory is detected even when `Cargo.lock` has not changed. A maintainer can also dispatch a release manually against `main`, but the workflow refuses to continue unless that exact commit already has a completed, successful `CI` push run. Release jobs check out that approved commit, serialize version allocation, and compile the reported binary version from the allocated tag.

Provenance shows what repository workflow and commit produced an artifact; it does not prove application correctness or live-NAS compatibility. The automated release remains subject to the no-live-NAS caveat in the [README](../README.md).

## Native archives

Six native builds are published. ARM targets use matching hosted ARM runners rather than unreliable cross-compilation.

| OS | Architecture | Release asset |
| --- | --- | --- |
| Linux | x86-64 | `synology-drive-sync-YY.N-linux-x86_64.tar.gz` |
| Linux | ARM64 | `synology-drive-sync-YY.N-linux-aarch64.tar.gz` |
| Windows | x86-64 | `synology-drive-sync-YY.N-windows-x86_64.zip` |
| Windows | ARM64 | `synology-drive-sync-YY.N-windows-aarch64.zip` |
| macOS | Intel x86-64 | `synology-drive-sync-YY.N-macos-x86_64.tar.gz` |
| macOS | Apple silicon ARM64 | `synology-drive-sync-YY.N-macos-aarch64.tar.gz` |

The Linux archives target GNU libc and conservatively require glibc 2.35 or newer. They are built on matching Ubuntu 22.04 x86-64 and ARM64 runners, and the workflow proves that the produced binary references no GLIBC symbol newer than 2.35; its exact minimum may be lower. This lower-risk GNU baseline preserves the host's normal resolver/NSS behavior. The Linux vault backend talks to freedesktop Secret Service over D-Bus using the Rust `zbus` stack; it does not dynamically link `libsecret` or `libdbus`, and the release workflow checks that boundary. A usable vault still requires an active user D-Bus session and an unlocked Secret Service collection.

Every archive has one versioned top-level directory and contains:

- the native executable;
- `LICENSE`, `THIRD_PARTY_LICENSES.html`, `README.md`, and `SECURITY.md`;
- generated Bash, Zsh, Fish, PowerShell, and Elvish completions;
- 15 generated roff manpages: the root page and one page for every top-level and nested subcommand.

The release also publishes:

- `SHA256SUMS`, covering all six archives, both installer scripts, the SBOM, and the notice bundle;
- `synology-drive-sync-YY.N.cdx.json`, a CycloneDX JSON dependency SBOM;
- `THIRD_PARTY_LICENSES.html`, the generated dependency license and attribution bundle;
- `install.sh` and `install.ps1` bootstrap installers.

## Checksum verification

Download `SHA256SUMS` over HTTPS from the same release and require exactly one matching line for the selected asset.

GNU/Linux with only selected assets present:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

macOS example:

```bash
asset=synology-drive-sync-26.1-macos-aarch64.tar.gz
expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print tolower($1) }' SHA256SUMS)
test "$(printf '%s\n' "$expected" | awk 'NF { n++ } END { print n + 0 }')" -eq 1
actual=$(shasum -a 256 "$asset" | awk '{ print tolower($1) }')
test "$actual" = "$expected"
```

Windows PowerShell example:

```powershell
$asset = 'synology-drive-sync-26.1-windows-x86_64.zip'
$matches = @(Get-Content '.\SHA256SUMS' | Where-Object {
    $_ -match "^([0-9A-Fa-f]{64})\s+\*?$([Regex]::Escape($asset))$"
})
if ($matches.Count -ne 1) { throw "Expected exactly one checksum for $asset" }
[void]($matches[0] -match '^([0-9A-Fa-f]{64})')
$expected = $Matches[1]
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $asset).Hash
if ($actual -ne $expected) { throw 'SHA-256 mismatch' }
```

The supplied [installers](installation.md) implement this selection and checksum process automatically, then verify that the extracted binary reports the requested `YY.N` version.

Checksums detect corruption and substitution relative to the manifest, but the manifest is served from the same GitHub Release. Verify the GitHub artifact attestation when publisher provenance matters.

## GitHub artifact attestations

The release workflow creates GitHub-hosted provenance for every file named by `SHA256SUMS`, an attestation for the checksum manifest itself, and a CycloneDX SBOM attestation for the native archives.

With a current GitHub CLI, verify a downloaded archive or installer against this public repository:

```bash
gh attestation verify \
  synology-drive-sync-26.1-linux-x86_64.tar.gz \
  --repo supermarsx/synology-drive-sync

gh attestation verify install.sh \
  --repo supermarsx/synology-drive-sync
```

Inspect the returned statement and confirm the repository, workflow identity, commit, and subject digest match the intended release. GitHub also documents an offline bundle workflow for disconnected environments.

See GitHub's official [artifact-attestation verification guide](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations) for current CLI requirements and trust behavior.

## SBOM

`synology-drive-sync-YY.N.cdx.json` is generated from the locked Rust dependency graph for all supported target platforms with a pinned `cargo-cyclonedx` release. The generator's checkout-local Cargo identity is normalized to the stable `pkg:github/OWNER/REPOSITORY@YY.N` release identity; the root application, its Rust targets, and every affected BOM reference are rewritten and validated against `YY.N`. It is useful for inventory and dependency-vulnerability tooling:

```bash
jq -e '.bomFormat == "CycloneDX" and ((.components | type) == "array")' \
  synology-drive-sync-26.1.cdx.json
```

The release SBOM describes application dependencies; it is not a claim that every target OS component has been scanned. Container builds additionally request BuildKit SBOM and maximum-mode provenance attestations.

`THIRD_PARTY_LICENSES.html` is generated by hash-pinned `cargo-about` 0.9.1 from
the locked graph for all six Linux, Windows, and macOS targets. Generation runs
offline after Cargo fetches checksum-pinned crates, uses explicit accepted-license
policy and known upstream clarifications, and fails on an unreadable or
unclassified license. CI regenerates the file and requires a byte-for-byte match,
so a dependency change cannot silently leave the checked-in notice bundle stale.

## GHCR multi-architecture image

The package name is:

```text
ghcr.io/supermarsx/synology-drive-sync
```

After the first publication, a repository owner must confirm that the package's
GitHub Packages visibility is **Public** if anonymous pulls are intended. Package
visibility is managed separately from repository visibility; until then, use
`docker login ghcr.io` with an appropriately scoped token.

Two native Linux variants are combined into one OCI index:

- `linux/amd64`;
- `linux/arm64`.

Published tags are:

- `YY.N`, the calendar release;
- `latest`, a mutable convenience pointer to the most recently published release.

Pull a calendar tag:

```bash
docker pull ghcr.io/supermarsx/synology-drive-sync:26.1
```

Resolve and pin the digest for reproducible deployment:

```bash
docker pull ghcr.io/supermarsx/synology-drive-sync:26.1
docker image inspect \
  ghcr.io/supermarsx/synology-drive-sync:26.1 \
  --format '{{index .RepoDigests 0}}'
```

Then deploy the returned `ghcr.io/...@sha256:...` reference. GitHub's [Container registry documentation](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry) also recommends digest pinning when the exact image must remain fixed.

Verify the OCI provenance/SBOM attestations against the resolved image:

```bash
gh attestation verify \
  oci://ghcr.io/supermarsx/synology-drive-sync:26.1 \
  --repo supermarsx/synology-drive-sync
```

The final multi-architecture digest receives GitHub provenance and SBOM attestations. Image labels include the source repository, calendar version, UTC build time, and Git commit revision. The runtime image also contains `LICENSE` and `THIRD_PARTY_LICENSES.html` under `/usr/share/licenses/synology-drive-sync/`.

## Release reproducibility boundaries

- `Cargo.lock` and the Rust toolchain version are fixed in the workflow.
- Each archive is built on a matching GitHub-hosted OS/architecture runner; see GitHub's current [hosted-runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
- Linux archives are built against a glibc 2.35 ceiling, verified from the binary's versioned symbol requirements before packaging.
- The binary embeds the calendar release through `SDSYNC_BUILD_VERSION` and is checked with `--version` before packaging.
- Completions and all 15 root/subcommand manpages are generated by the just-built native binary.
- Third-party notices are generated from all supported target graphs with a hash-pinned tool and are reproduced byte-for-byte in CI.
- Archive checksums and attestations are generated only after all six builds and both architecture-specific container builds succeed.
- Native archives are not claimed to be byte-for-byte reproducible across independent runner images.
- Release assets are not separately platform-code-signed or notarized. SHA-256 and GitHub attestations are the provided verification mechanisms.
