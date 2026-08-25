# Build and validate SPKs

The SPK assembler accepts two matching Linux executables:

- `synology-drive-sync`, the core sync engine; and
- `sdsync-dsm-api`, the compiled CGI relay, package-user API service, and private-queue consumer.

Both must be regular, non-symlink, fully static, little-endian ELF files for the selected DSM
architecture. The helper is installed byte-for-byte at ordinary `bin/sdsync-dsm-api` mode `0755`
and at `ui/api.cgi` mode `0755`. The CGI executes as DSM `http` and relays one bounded request to the
package-user `--serve` process through the fixed `ui/api.sock`, owned by the package user, grouped to
`http`, and mode `0660`. No outer or inner archive member or installed executable carries a
set-user-ID/set-group-ID bit. The package requests neither root run-as nor Linux capabilities.

The complete reviewed `conf/privilege` document is deliberately minimal:

```json
{
  "defaults": {
    "run-as": "package"
  },
  "join-groupname": "http"
}
```

The default covers lifecycle scripts; there is no redundant `ctrl-script` list, `tool` list, or
capability declaration.

## Architecture contracts

| Builder `--arch` | Rust target | Required ELF |
| --- | --- | --- |
| `x86_64` | `x86_64-unknown-linux-musl` | ELF64, `EM_X86_64`, little-endian, static |
| `i686` | `i686-unknown-linux-musl` | ELF32, `EM_386`, little-endian, static |
| `armv7` | `armv7-unknown-linux-musleabihf` | ELF32, `EM_ARM`, little-endian, EABI5 hard-float, static |
| `armv8` | `aarch64-unknown-linux-musl` | ELF64, `EM_AARCH64`, little-endian, static |

A matching filename is not sufficient. The builder rejects the wrong class, endianness, machine,
ARM EABI/float ABI, malformed program headers, missing executable load segment, ELF interpreter, or
`DT_NEEDED` dependency.

## Native x86-64 example

Build both binaries with Rust 1.88 or newer and a matching musl linker:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked \
  --package synology-drive-sync \
  --bins \
  --target x86_64-unknown-linux-musl

bash packaging/synology/build-spk.sh \
  --binary target/x86_64-unknown-linux-musl/release/synology-drive-sync \
  --api-binary target/x86_64-unknown-linux-musl/release/sdsync-dsm-api \
  --arch x86_64 \
  --version 26.1 \
  --output dist

python3 packaging/synology/validate_spk.py \
  --binary target/x86_64-unknown-linux-musl/release/synology-drive-sync \
  --api-binary target/x86_64-unknown-linux-musl/release/sdsync-dsm-api \
  --arch x86_64 \
  dist/synology-drive-sync-26.1-x86_64.spk
```

The target directory/path can vary with Cargo configuration; supply the actual files rather than
copying a path blindly.

## ARMv7 cross-build example

The supported release/local cross-build path uses Rust 1.88.0, Zig 0.16.0, and
`cargo-zigbuild` 0.23.2:

```bash
rustup toolchain install 1.88.0 --profile minimal \
  --target armv7-unknown-linux-musleabihf
cargo install --locked cargo-zigbuild --version 0.23.2

cargo +1.88.0 zigbuild \
  --release --locked \
  --package synology-drive-sync \
  --bins \
  --target armv7-unknown-linux-musleabihf

bash packaging/synology/build-spk.sh \
  --binary target/armv7-unknown-linux-musleabihf/release/synology-drive-sync \
  --api-binary target/armv7-unknown-linux-musleabihf/release/sdsync-dsm-api \
  --arch armv7 \
  --version 26.1 \
  --output dist
```

Do not use plain `cargo build` on an x86 host without a compatible ARM musl cross-linker. Release CI
also executes both resulting target binaries under the pinned architecture emulator before accepting
the SPK; static archive validation alone is not an execution proof.

Use `cargo zigbuild --bins` with the corresponding target for `i686`; use a native matching musl
toolchain or reviewed cross-toolchain for ARM64.

## Reproducible package assembly

Artifacts are named `synology-drive-sync-VERSION-ARCH.spk`; a leading `v` is removed. A semantic
version such as `0.1.0` becomes DSM version `0.1.0-1` in `INFO`.

`SOURCE_DATE_EPOCH` controls tar member timestamps and the inner gzip header. The assembler
normalizes member order, ownership, group, modes, and archive metadata. It includes:

- architecture-bound `INFO` with DSM `7.0-40759` through `7.4-99999`;
- core, manager, controller, runner, compiled helper/CGI, lifecycle scripts, and privilege policy;
- DSM desktop app config, offline HTML/CSS/JavaScript, authored SVG mark, and deterministic
  16/24/32/48/64/72/256 PNG icons;
- fixed Notification Center resources and English texts;
- project license, generated notices, and musl copyright.

Deterministic assembly reduces packaging variance. Independently compiled Rust binaries are not
claimed bit-for-bit reproducible across different compiler/linker/runner images.

## Validator coverage

`validate_spk.py` checks:

- safe outer/inner archive member names, types, modes, ordering, and required files;
- filename/version/`INFO` architecture and DSM-bound consistency;
- exact `conf/privilege`, `conf/resource`, application config, notification category/events/texts;
- static core/helper ELF identity and equality of helper/CGI bytes;
- no outer or inner archive member has a set-user-ID/set-group-ID bit or is group/world-writable,
  while `conf/privilege` remains the exact root-free package/`http` contract;
- authored SVG safe bounds and exact deterministic PNG bytes/dimensions;
- CSP, no-referrer ordering, offline assets, no inline handlers/eval/HTML injection, exact bridge
  action/schema markers, and no secret local-storage path;
- lifecycle scripts, the fixed `package:http` `0660` socket/service contract, private FHS behavior,
  icons, license texts, and installed size.

Source-only validation:

```bash
python3 packaging/synology/validate_spk.py
node --check packaging/synology/package/ui/app.js
python3 -m unittest packaging.synology.test_synology_ui -v
python3 packaging/synology/test_synology_package.py
```

Run the package suite in a Linux/WSL environment that provides its expected shell/process semantics.
Negative tests deliberately tamper with architecture, helper identity, symlinks, permissions,
resources, UI security markers, icons, and archive fields. They specifically reject an archived
privilege bit, a non-`0755` CGI, a privilege manifest beyond the exact two-key contract, and unsafe
socket ownership/mode/peer assumptions, so the gates do not merely accept the builder's happy path.

## Acceptance boundary

Builder/validator success proves the reviewed archive contract, not DSM installation, web launch,
the `http` CGI identity, package-user `authenticate.cgi` execution, socket group behavior,
administrator groups, Notification Center, source ACLs, reverse proxy, File Station, TOTP, Drive
indexing, or sync behavior on a physical model. Complete
[live-NAS acceptance](troubleshooting.md#live-nas-acceptance) before publishing a support claim.

Official framework references:

- [Package structure](https://help.synology.com/developer-guide/synology_package/introduction.html)
- [Desktop application integration](https://help.synology.com/developer-guide/integrate_dsm/desktopapp.html)
- [Privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html)
- [FHS paths](https://help.synology.com/developer-guide/integrate_dsm/fhs.html)
- [Platform and `arch` values](https://help.synology.com/developer-guide/appendix/platarchs.html)
- [Lifecycle scripts](https://help.synology.com/developer-guide/synology_package/scripts.html)
