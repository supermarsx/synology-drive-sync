# Ownership, errors, and threading

The ABI never transfers a Rust `String`, slice, enum layout, trait object, or allocator contract to
the caller. All text is UTF-8 pointer/length data, and every library allocation has one matching free
function.

## Lifetime table

| Value | Owner | Lifetime |
| --- | --- | --- |
| Request bytes | caller | Readable for the duration of `sdsync_run_v1`; parsed during the call. |
| Callback table and `user_data` | caller | Valid until `sdsync_run_v1` returns; never retained afterwards. |
| Secret output bytes | caller during callback | Copied immediately into library-owned zeroizing storage. |
| Plan/event JSON | library | Borrowed and immutable only for that callback invocation. |
| Build-version bytes | library | Borrowed and immutable until the dynamic library unloads. |
| Result handle | caller after return | Owned until exactly one `sdsync_result_free_v1`. |
| Result JSON view | result handle | Borrowed and immutable until its result is freed. |
| Cancellation handle | caller | Owned until exactly one free after all runs using it have returned. |

Returned views are not NUL-terminated. Do not call `strlen`, write through them, retain callback
views, or free a borrowed pointer. Copy a view if the application needs it beyond the stated lifetime.

## Secrets

Secret acquisition is a two-pass callback so the library can allocate the exact destination. The
second pass is copied into a zeroizing buffer, and SDK diagnostics redact `Secret` values. The caller
still owns its source secret and must protect and clear that storage according to its language/runtime.
Do not put a password, TOTP seed, or code in request JSON, command arguments, logs, or callback error
text.

The callback may return unavailable when no matching secret exists or cancelled when acquisition was
aborted. The library asks for OTP only after DSM challenges authentication, and distinguishes the
first OTP request from a rejected-code retry.

## Errors and panic boundary

Check both the numeric function status and the JSON result. The status is stable for broad branching;
the result carries the versioned structured outcome or a secret-free operator message. Except for a
NULL `out_result`, even validation failures return an owned result that must be freed.

`sdsync_run_v1` catches Rust unwinding at the FFI boundary and maps it to `SDSYNC_STATUS_PANIC` with a
generic diagnostic. No Rust panic is allowed to unwind into C. Foreign callbacks must likewise return
normally—do not throw a C++ exception, long-jump, or unwind through Rust frames. A process abort or
hardware fault cannot be converted into a status code.

## Threads, callbacks, and cancellation

`sdsync_run_v1` is synchronous and callbacks are invoked inline while it is active. Callback code
must return promptly and must not retain borrowed JSON pointers. There is no global callback
registration: a table and its `user_data` belong to that one run.

Cancellation is cooperative. `sdsync_cancellation_cancel_v1` is safe to call repeatedly and from
another thread while a run is active; the operation stops at the next safe cancellation point. The
caller must not free that handle until every concurrent run using it has returned. Never free a
result while another thread is reading its borrowed bytes.

Use a separate result handle per call and externally synchronize ownership/destruction. If the host
language has a moving garbage collector, pin native callback state for the complete call and arrange
deterministic cleanup with a `finally`, `defer`, RAII, or equivalent mechanism.
