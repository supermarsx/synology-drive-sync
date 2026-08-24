# Distribution and compatibility

C SDK archives are published beside the CLI artifacts for every supported native platform and
architecture. Replace `YY.N` with an exact verified calendar tag.

| Platform | Architecture | Release asset |
| --- | --- | --- |
| Windows | x86-64 | `synology-drive-sync-YY.N-c-sdk-windows-x86_64.zip` |
| Windows | ARM64 | `synology-drive-sync-YY.N-c-sdk-windows-aarch64.zip` |
| Linux GNU | x86-64 | `synology-drive-sync-YY.N-c-sdk-linux-x86_64.tar.gz` |
| Linux GNU | ARM64 | `synology-drive-sync-YY.N-c-sdk-linux-aarch64.tar.gz` |
| macOS | Intel x86-64 | `synology-drive-sync-YY.N-c-sdk-macos-x86_64.tar.gz` |
| macOS | Apple silicon ARM64 | `synology-drive-sync-YY.N-c-sdk-macos-aarch64.tar.gz` |

Each archive has one versioned top-level directory containing:

```text
include/sdsync.h
examples/ffi/basic.c
lib/sdsync.dll          # Windows
lib/sdsync.lib          # Windows import library
lib/libsdsync.so        # Linux
lib/libsdsync.dylib     # macOS
LICENSE
THIRD_PARTY_LICENSES.html
```

Select the archive matching both the operating system and process architecture. A 64-bit ARM host
does not make an x86-64 library loadable, and the Linux artifact is a GNU/glibc shared object—not a
musl or DSM SPK library.

## Loader behavior

- **Windows:** link with `sdsync.lib` and make the matching `sdsync.dll` available beside the
  executable or through an application-scoped DLL search path.
- **Linux:** link with `-lsdsync` and set a deployment-controlled RPATH/RUNPATH or loader search path.
  Avoid a mutable global replacement when different applications pin different calendar releases.
- **macOS:** link with `-lsdsync` and give the application an appropriate `@loader_path`/bundle
  layout. These artifacts are not Apple-notarized or platform-code-signed.

The release's `SHA256SUMS` covers the SDK archive. Verify it and, when publisher provenance matters,
the GitHub artifact attestation before extraction. See [release artifacts and verification](../releases.md).

## ABI compatibility policy

The calendar product version and C ABI major are separate. Check both:

- pin an exact `YY.N` release for reproducible behavior;
- compile against the header from that same SDK archive;
- require `sdsync_abi_version_v1() == SDSYNC_ABI_VERSION_V1` before using v1 functions;
- use only symbols declared by the public header, all currently suffixed `_v1`;
- initialize `struct_size` and zero reserved callback-table fields so compatible additive growth can
  be detected safely;
- tolerate documented future result/error fields when decoding versioned JSON.

Existing v1 symbols and meanings are the compatibility boundary. A future incompatible native
contract must use a new ABI constant, symbol suffix, callback layout, and JSON schema version rather
than silently changing v1. Calendar releases may still add plan/event variants or error categories,
so JSON consumers should reject the wrong `schema` but ignore unknown object fields where their
decoder permits it.

Release checks build each library on its matching hosted OS/architecture runner and package the
header and C example from the same approved commit. Static release validation is not a substitute for
testing the library with the deployment's compiler, loader policy, DSM endpoint, proxy/TLS setup, and
secret provider.
