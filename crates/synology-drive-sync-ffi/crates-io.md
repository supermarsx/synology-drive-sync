# synology-drive-sync-ffi

**The stable C ABI for [`synology-drive-sync`](https://crates.io/crates/synology-drive-sync)**, a
lean one-way file deposit to a Synology NAS over the DSM File Station API. Use it to embed the
engine in a C or C++ program, or in any language that can load a C shared library: Python
`ctypes`, .NET P/Invoke, Go `cgo`, Java Panama and so on.

It is the right choice when the host application is not written in Rust and cannot (or may not)
install Synology Drive Client. If you are writing Rust, depend on
[`synology-drive-sync`](https://docs.rs/synology-drive-sync) directly and use its `sdk::Engine`.

## What the ABI offers

- **One blocking call.** `sdsync_run_v1` scans the local folder, plans, asks your plan callback
  whether to apply, transfers, verifies and logs out.
- **JSON in, JSON out.** The request is one UTF-8 `sdsync.request.v1` document; the outcome or
  failure is one `sdsync.ffi-result.v1` document. No Rust data layouts cross the boundary.
- **Preview by default.** Without a plan callback that returns `SDSYNC_PLAN_APPLY`, the run is
  preview-only and changes nothing. Remote-only entries are kept unless the request carries a
  bounded deletion policy.
- **Secrets stay in your process.** A two-pass callback copies the password (and a one-time code,
  if DSM asks for one) into zeroizing buffers; nothing is read from the environment for you.
- **Cooperative cancellation** from another thread through a cancellation handle.
- **Panics are contained** at the ABI boundary and reported as status `255`.
- **Versioned.** Every symbol ends in `_v1`; a future incompatible contract gets a new suffix.

Eight functions, all declared in the header: `sdsync_abi_version_v1`, `sdsync_build_version_v1`,
`sdsync_cancellation_new_v1`, `sdsync_cancellation_cancel_v1`, `sdsync_cancellation_free_v1`,
`sdsync_run_v1`, `sdsync_result_bytes_v1` and `sdsync_result_free_v1`.

## The header

The C declarations are in [`include/sdsync.h`](https://github.com/supermarsx/synology-drive-sync/blob/main/include/sdsync.h),
which is also shipped inside this package at `include/sdsync.h`. Include it rather than copying
declarations, and compile against the header from the same release as the library.

## Build and link

The library is built as a `cdylib` named `sdsync`: `libsdsync.so` on Linux, `libsdsync.dylib` on
macOS, `sdsync.dll` and `sdsync.lib` on Windows. Unpack this package (for example with `cargo
fetch` and the copy under `~/.cargo/registry/src/`) and build it on its own:

```bash
cargo build --release          # from a checkout of the repository: cargo build -p synology-drive-sync-ffi --profile ffi-release
```

then link with `-lsdsync` and point the loader at the output directory (`-Wl,-rpath,...` on
Linux and macOS, or put `sdsync.dll` beside the executable on Windows). A build with
`panic = "abort"` is rejected on purpose, so a Rust panic cannot abort your process; Cargo's
defaults are fine. Prebuilt, checksummed C SDK archives for Windows, Linux and macOS (the header,
the libraries and a reference example) are attached to each
[GitHub release](https://github.com/supermarsx/synology-drive-sync/releases/latest).

## A minimal C example

This previews one run and prints the result JSON. It reads the password from `DSM_PASSWORD` only
to stay short; a real host should connect the callback to its own secret store.

```c
#include "sdsync.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Two-pass secret callback: first call has buffer == NULL and reports the length. */
static int32_t SDSYNC_CALL provide_secret(
    void *user_data, uint32_t secret_kind,
    uint8_t *buffer, uint64_t capacity, uint64_t *written) {
    (void)user_data;
    const char *value =
        getenv(secret_kind == SDSYNC_SECRET_PASSWORD ? "DSM_PASSWORD" : "DSM_OTP");
    if (value == NULL) return SDSYNC_CALLBACK_UNAVAILABLE;
    const size_t length = strlen(value);
    if (buffer != NULL) {
        if (capacity < (uint64_t)length) return SDSYNC_CALLBACK_UNAVAILABLE;
        memcpy(buffer, value, length);
    }
    *written = (uint64_t)length;
    return SDSYNC_CALLBACK_OK;
}

/* Receives the complete plan before anything is changed. */
static uint32_t SDSYNC_CALL inspect_plan(
    void *user_data, const uint8_t *json, uint64_t json_len) {
    (void)user_data;
    fwrite(json, 1, (size_t)json_len, stderr);
    fputc('\n', stderr);
    return SDSYNC_PLAN_PREVIEW_ONLY; /* return SDSYNC_PLAN_APPLY to perform it */
}

int main(void) {
    if (sdsync_abi_version_v1() != SDSYNC_ABI_VERSION_V1) {
        fputs("incompatible sdsync C ABI version\n", stderr);
        return 2;
    }

    static const char request[] =
        "{\"schema\":\"sdsync.request.v1\","
        "\"endpoint\":\"https://nas.example.com\","
        "\"username\":\"deposit-bot\","
        "\"source\":\"./export\","
        "\"remote\":\"/TeamShare/Project\"}";

    sdsync_callbacks_v1 callbacks = {
        (uint32_t)sizeof(sdsync_callbacks_v1), /* struct_size */
        0,                                     /* reserved: must be zero */
        NULL,                                  /* user_data */
        provide_secret, inspect_plan, NULL     /* secret, plan, event */
    };

    sdsync_result_v1 *result = NULL;
    const int32_t status = sdsync_run_v1(
        (const uint8_t *)request, (uint64_t)(sizeof(request) - 1),
        &callbacks, NULL, &result);

    /* A result handle comes back on every return, failures included: read it, then free it. */
    if (result != NULL) {
        const uint8_t *json = NULL;
        uint64_t json_len = 0;
        if (sdsync_result_bytes_v1(result, &json, &json_len) == SDSYNC_STATUS_OK) {
            fwrite(json, 1, (size_t)json_len, stdout);
            fputc('\n', stdout);
        }
        sdsync_result_free_v1(result);
    }
    return status == SDSYNC_STATUS_OK ? 0 : 1;
}
```

```bash
cc -std=c11 -Wall -Wextra -Iinclude example.c -Ltarget/release -lsdsync \
  -Wl,-rpath,"$PWD/target/release" -o sdsync-example
DSM_PASSWORD='...' ./sdsync-example
```

The repository's reference example,
[`examples/ffi/basic.c`](https://github.com/supermarsx/synology-drive-sync/blob/main/examples/ffi/basic.c),
adds file-based requests, an apply switch and an event callback. The
[C ABI guide](https://supermarsx.github.io/synology-drive-sync/ffi/index.html) documents the
request fields, status codes, ownership and threading rules.

## Safety and guarantees

- **Callbacks run synchronously** on the thread that called `sdsync_run_v1`. They must not retain
  the borrowed pointers they receive, throw, unwind, or free a handle that is active in the run.
- **Free every result exactly once**, and the cancellation handle only after every run using it
  has returned.
- **One direction, additive by default.** The source is authoritative and is opened read-only;
  an edit made on the NAS is never brought back.
- **Honest limits.** Uploads and guarded deletions are **not crash-atomic**, and a synchronisation
  is not a backup. This is young software that has been exercised against few NAS models and DSM
  builds; rehearse against a disposable destination first.

## Links

- Documentation: <https://supermarsx.github.io/synology-drive-sync/ffi/index.html>
- Rust SDK and CLI: <https://crates.io/crates/synology-drive-sync>
- Source and issues: <https://github.com/supermarsx/synology-drive-sync>
- API reference: <https://docs.rs/synology-drive-sync-ffi>

## License and trademarks

MIT licensed; see the
[license](https://github.com/supermarsx/synology-drive-sync/blob/main/license.md).

Synology, DSM, File Station and QuickConnect are trademarks of Synology Inc. This is an
independent open-source project. It is not affiliated with, endorsed by or supported by Synology.
