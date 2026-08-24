# FFI examples

Every C SDK archive contains `examples/ffi/basic.c`, the same fixture maintained in the repository at
[`examples/ffi/basic.c`](https://github.com/supermarsx/synology-drive-sync/blob/main/examples/ffi/basic.c).
It supplies secrets through a two-pass callback, prints plan/event JSON, defaults to preview-only,
reads the result JSON, and frees the result on both success and failure.

## Build from an SDK archive

From the extracted SDK root, compile against its `include` and `lib` directories.

Linux:

```bash
cc -std=c11 -Wall -Wextra -Werror \
  -Iinclude examples/ffi/basic.c -Llib -lsdsync \
  -Wl,-rpath,'$ORIGIN/lib' -o sdsync-basic
```

macOS:

```bash
clang -std=c11 -Wall -Wextra -Werror \
  -Iinclude examples/ffi/basic.c -Llib -lsdsync \
  -Wl,-rpath,@loader_path/lib -o sdsync-basic
```

Windows, from an MSVC Developer Command Prompt:

```batch
cl /std:c11 /W4 /WX /I include examples\ffi\basic.c ^
  /link /LIBPATH:lib sdsync.lib /OUT:sdsync-basic.exe
```

Keep `sdsync.dll` discoverable at runtime—for example, copy it beside the executable or add the
SDK's `lib` directory to the child process's DLL search path. Do not install an unversioned copy into
a global system directory.

## Run a preview

Save a request such as this as `request.json`:

```json
{
  "schema": "sdsync.request.v1",
  "endpoint": "https://files.example.com",
  "username": "mirror-bot",
  "source": "./export",
  "remote": "/TeamShare/Project",
  "exclusions": ["*.tmp"],
  "comparison": "content",
  "jobs": 2
}
```

The example reads `SDSYNC_PASSWORD` and, when DSM requests a second factor, `SDSYNC_OTP` through the
secret callback. Use this environment-variable behavior only as a compact demonstration; a real host
should connect the callback to its own secret store or protected prompt.

```bash
SDSYNC_PASSWORD='retrieve-at-runtime' ./sdsync-basic request.json
```

Plan and event documents go to standard error; the final `sdsync.ffi-result.v1` document goes to
standard output. The example returns a non-zero process exit when the ABI status is non-zero.

## Explicitly apply

The example returns preview-only unless `SDSYNC_APPLY=1` is present. Inspect the emitted plan first,
then authorize the exact request deliberately:

```bash
SDSYNC_PASSWORD='retrieve-at-runtime' SDSYNC_APPLY=1 \
  ./sdsync-basic request.json
```

On PowerShell, set process-scoped variables and remove them after the child exits:

```powershell
$env:SDSYNC_PASSWORD = 'retrieve-at-runtime'
$env:SDSYNC_APPLY = '1'
try {
    .\sdsync-basic.exe .\request.json
} finally {
    Remove-Item Env:SDSYNC_PASSWORD, Env:SDSYNC_APPLY -ErrorAction SilentlyContinue
}
```

Deletion still requires an explicit positive bound in the request document. An apply decision cannot
override request validation, path containment, the empty-source fuse, the deletion maximum, or final
reconciliation.
