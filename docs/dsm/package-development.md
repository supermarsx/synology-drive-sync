# Build and validate SPKs

The SPK assembler accepts two matching Linux executables:

- `synology-drive-sync`, the core sync engine; and
- `sdsync-dsm-api`, the compiled CGI relay, package-user API service, and private-queue consumer.

Both must be regular, non-symlink, fully static, little-endian ELF files for the selected DSM
architecture. The helper is installed byte-for-byte at ordinary `bin/sdsync-dsm-api` mode `0755`
and at package-owned `ui/api.cgi` mode `0755`. The CGI fails closed unless Webman starts it with its
exact non-root package-owner UID; `defaults.run-as=package` gives the `--serve` process that package
UID but does not document Webman's launch identity. The CGI authenticates once and relays one bounded
request through fixed `var/run/api.sock`; the service binds it as package-owned `0000` before startup
commit, then activates that same inode as `0600`. No outer or inner archive member or installed
executable carries a set-user-ID/set-group-ID bit. The package requests neither a joined web group,
root run-as, nor Linux capabilities.

The complete reviewed `conf/privilege` document is deliberately minimal:

```json
{
  "defaults": {
    "run-as": "package"
  }
}
```

The default covers lifecycle scripts; there is no redundant `ctrl-script` list, `tool` list, or
capability declaration.

## Native DSM AppWindow contract

`INFO` registers `dsmuidir="synology-drive-sync:ui"` and the exact class
`dsmappname="SYNO.SDS.App.SynologyDriveSync.Instance"`. `ui-src/app.config` declares that class as
`type="app"`, sets `appWindow` to the same value, and keeps `allUsers=false`. `config.define` binds
the source bundle to `SynologyDriveSync.js`. The assembler deterministically reproduces the normal
DSM toolkit merge: installed `ui/config` is keyed by `SynologyDriveSync.js`, contains the class
entry plus `depend: []`, and is packaged with regular `ui/SynologyDriveSync.js` and `ui/style.css`
files at mode `0644`.

The bundle uses `SYNO.namespace` and `Vue.extend`; its root is `v-app-instance` containing
`v-app-window`. It renders the dashboard directly rather than embedding a `type=url` page or iframe.
It calls the regular packaged `ui/api.cgi` through the canonical same-origin endpoint
`/webman/3rdparty/synology-drive-sync/api.cgi`.

DSM, not a lifecycle script, owns the
`/usr/syno/synoman/webman/3rdparty/synology-drive-sync` link created from `dsmuidir`. The package must
not create, replace, remove, chmod, or chown that link. Static validation proves that registration,
module config, assets, and API path agree inside the SPK; only the
[live-NAS acceptance](troubleshooting.md#live-nas-acceptance) can prove DSM registered and rendered
the AppWindow and exposed the CGI path on an installed system.

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

## Build the native UI

Use the exact pnpm version pinned by `ui-src/package.json` and the committed lockfile. The assembler
does not install dependencies or build the UI: it checks that the generated files are present,
nonempty, and satisfy the reviewed package contract, but it cannot prove that they are fresh from the
current source. Every local caller must build and compare the generated output before assembly:

```bash
cd packaging/synology/ui-src
pnpm install --frozen-lockfile --ignore-scripts
pnpm run build
cd ../../..
```

This produces only `ui-src/dist/SynologyDriveSync.js` and `ui-src/dist/style.css`. Run
`git diff --exit-code -- packaging/synology/ui-src/dist` from the repository root; a difference means
the committed generated files were stale and must be reviewed. Release and CI jobs pin Node and
pnpm, use the frozen lockfile, rebuild, and reject a generated diff before package validation. The
dedicated CI packaging gate also rebuilds twice and compares bundle hashes for determinism.

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
- module-keyed native AppWindow config, offline Vue JavaScript/CSS, authored SVG mark, and deterministic
  16/24/32/48/64/72/256 PNG icons;
- fixed, preloaded English desktop-alert I18N texts, with no `conf/resource` acquisition worker;
- project license, generated Rust dependency notices, DSM AppWindow bundled-code notices, and musl
  copyright.

Deterministic assembly reduces packaging variance. Independently compiled Rust binaries are not
claimed bit-for-bit reproducible across different compiler/linker/runner images.

## Validator coverage

`validate_spk.py` checks:

- safe outer/inner archive member names, types, modes, ordering, and required files;
- filename/version/`INFO` architecture and DSM-bound consistency;
- exact `conf/privilege` and module-keyed `type=app`/AppWindow config, absence of `conf/resource` and legacy sysnotify mail
  templates, and fixed desktop-alert I18N texts;
- exact `INFO`/application-class identity, bundle/style members, and the canonical
  `/webman/3rdparty/synology-drive-sync/api.cgi` boundary;
- static core/helper ELF identity and equality of helper/CGI bytes;
- no outer or inner archive member has a set-user-ID/set-group-ID bit or is group/world-writable,
  while `conf/privilege` remains the exact root-free package-run-as contract with no joined group;
- authored SVG safe bounds and exact deterministic PNG bytes/dimensions;
- scoped AppWindow styles, offline assets, no iframe/eval/HTML injection or source maps, exact bridge
  action/schema markers, and no secret local-storage path;
- the direct `synodsmnotify -c` contract: the exact native application class plus fixed
  administrator/I18N arguments only, no
  legacy `synonotify` event/custom-variable path, and no dynamic profile, exit, log, or secret data;
- lifecycle scripts, the fixed package-owned `0000`-prepared/`0600`-active socket/service contract,
  exact package-UID peer checks, private FHS behavior,
  icons, exact outer/installed license texts, and installed size.

Source-only validation:

```bash
cd packaging/synology/ui-src
pnpm install --frozen-lockfile --ignore-scripts
pnpm run check
cd ../../..
git diff --exit-code -- packaging/synology/ui-src/dist
python3 packaging/synology/validate_spk.py
python3 -m unittest packaging.synology.test_synology_ui -v
python3 packaging/synology/test_synology_package.py
```

Run the package suite in a Linux/WSL environment that provides its expected shell/process semantics.
Negative tests deliberately tamper with architecture, helper identity, symlinks, permissions,
reserved resources, notifier arguments, UI security markers, icons, and archive fields. They
specifically reject `conf/resource`, legacy sysnotify mail templates, a dynamic notifier operand, an
archived privilege bit, a non-`0755` CGI, a privilege manifest beyond the exact package-run-as
contract, and unsafe socket ownership/mode/peer assumptions. They also reject a wrong native
application class, missing module wrapper or `depend` field, `type=url`, mismatched `appWindow`,
missing bundle/style, and traversal in the API path, so the gates do not merely accept the builder's
happy path.

## Acceptance boundary

Builder/validator success proves the reviewed archive contract, not DSM installation, AppWindow
launch, the official same-origin `SYNO.API.Auth` version 6 token response and package
`X-SYNO-TOKEN` forwarding, DSM's executable-owner CGI runtime identity, fixed-helper `X_OK` probing,
protected `authenticate.cgi` validation/revalidation and execution after a successful probe, bounded
loopback user-service authentication when that probe returns `EACCES` without invoking the validator,
package-owned socket behavior, administrator groups, direct `synodsmnotify` desktop delivery, source
ACLs, reverse proxy, File Station, TOTP, Drive indexing, or sync behavior on a physical model.
Complete [live-NAS acceptance](troubleshooting.md#live-nas-acceptance) before publishing a support
claim.

Official framework references:

- [Package structure](https://help.synology.com/developer-guide/synology_package/introduction.html)
- [Native package app launch](https://help.synology.com/developer-guide/synology_package/package_tgz/launch_app.html)
- [AppWindow UI framework](https://help.synology.com/developer-guide/appendix/ui_framework/application.html)
- [Application authentication](https://help.synology.com/developer-guide/integrate_dsm/web_authentication.html)
- [DSM Login Web API Guide](https://global.download.synology.com/download/Document/Software/DeveloperGuide/Os/DSM/All/enu/DSM_Login_Web_API_Guide_enu.pdf)
- [Privilege configuration](https://help.synology.com/developer-guide/privilege/privilege_config.html)
- [FHS paths](https://help.synology.com/developer-guide/integrate_dsm/fhs.html)
- [Platform and `arch` values](https://help.synology.com/developer-guide/appendix/platarchs.html)
- [Lifecycle scripts](https://help.synology.com/developer-guide/synology_package/scripts.html)
