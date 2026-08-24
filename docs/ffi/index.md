# C ABI, DLL, and shared objects

The repository exposes a small, versioned JSON-over-C ABI through `include/sdsync.h`. It is built as
`sdsync.dll` on Windows, `libsdsync.so` on Linux, and `libsdsync.dylib` on macOS. The ABI wraps the
same safe synchronous `sdk::Engine` workflow as the Rust API; it does not expose Rust layouts or the
individual low-level File Station calls.

> [!NOTE]
> Use a C SDK asset from a release that lists the SDK in its release notes. Older releases do not
> gain DLL/shared-library artifacts retroactively. The Cargo package for the FFI crate is an internal
> workspace package and is not published to crates.io.

## Contract at a glance

- ABI major `1`, reported by `sdsync_abi_version_v1()` and encoded in every symbol suffix;
- one UTF-8 `sdsync.request.v1` JSON document as input;
- optional secret, plan, and event callbacks in a size-versioned callback table;
- an explicit plan callback decision before mutation—without one, the run is preview-only;
- an optional cancellation handle that another thread may signal cooperatively;
- one owned result handle containing `sdsync.ffi-result.v1` JSON on success or failure;
- pointer/length byte views rather than NUL-terminated strings;
- panic containment at the exported run boundary and stable numeric status families.

The normal sequence is:

1. verify `sdsync_abi_version_v1() == SDSYNC_ABI_VERSION_V1`;
2. prepare a request document and a `sdsync_callbacks_v1` table;
3. optionally allocate a cancellation handle;
4. call the blocking `sdsync_run_v1()` function;
5. read the result JSON with `sdsync_result_bytes_v1()`;
6. free the result, then free cancellation only after every run using it has returned.

The plan callback receives a complete immutable plan before remote mutation. Return
`SDSYNC_PLAN_APPLY` only after the caller has authorized that exact plan. Returning
`SDSYNC_PLAN_PREVIEW_ONLY`, or omitting the callback, retains the additive/deletion safety boundary
and produces a non-mutating outcome.

Start with the [function and JSON reference](api.md), then read the
[ownership and callback rules](memory-errors-and-threading.md), the [reference C example](examples.md),
and the [release asset matrix](distribution.md).
