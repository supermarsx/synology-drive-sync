# License and third-party notices

`synology-drive-sync` is distributed under the
[MIT License](https://github.com/supermarsx/synology-drive-sync/blob/main/LICENSE).

The generated
[third-party notices](https://github.com/supermarsx/synology-drive-sync/blob/main/THIRD_PARTY_LICENSES.html)
record the locked Rust release-dependency graph and are shipped with release artifacts. CI regenerates
the notice document from Cargo metadata and refuses a stale tracked copy. DSM SPKs also contain
[AppWindow bundled-code notices](https://github.com/supermarsx/synology-drive-sync/blob/main/packaging/synology/licenses/DSM_UI_THIRD_PARTY_LICENSES.txt)
for the vue-loader and webpack runtime code incorporated into the generated JavaScript. Vue itself is
externalized to DSM and is not bundled; other pnpm packages whose code is not named in that notice
are used only during the build.

Release archives, DSM packages, container images, Rust packages, and dynamic-library artifacts
must keep their notices, SBOM, checksums, and provenance synchronized with the exact source/dependency
set they contain. See [release artifacts and verification](releases.md).
