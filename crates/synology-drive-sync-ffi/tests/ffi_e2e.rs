#![allow(dead_code)]

#[path = "../../../tests/support/mod.rs"]
mod support;

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::slice;

use sdsync::{
    SDSYNC_CALLBACK_CANCELLED, SDSYNC_CALLBACK_OK, SDSYNC_CALLBACK_UNAVAILABLE,
    SDSYNC_EVENT_CONTINUE, SDSYNC_PLAN_APPLY, SDSYNC_PLAN_PREVIEW_ONLY, SDSYNC_SECRET_OTP_REJECTED,
    SDSYNC_SECRET_OTP_REQUIRED, SDSYNC_SECRET_PASSWORD, SDSYNC_STATUS_CALLBACK_FAILED,
    SDSYNC_STATUS_CANCELLED, SDSYNC_STATUS_OK, SDSYNC_STATUS_OPERATION_FAILED, SdsyncCallbacksV1,
    SdsyncCancellationV1, SdsyncResultV1, sdsync_cancellation_cancel_v1,
    sdsync_cancellation_free_v1, sdsync_cancellation_new_v1, sdsync_result_bytes_v1,
    sdsync_result_free_v1, sdsync_run_v1,
};
use serde_json::{Value, json};
use support::TestDir;
use support::file_station_mock::MockFileStation;

#[derive(Clone, Copy)]
enum SecretBehavior {
    Valid,
    Unavailable,
    UnavailableSecondPass,
    Cancelled,
    CancelledSecondPass,
    UnknownStatus,
    InvalidLength,
    InvalidUtf8,
    InconsistentLength,
}

struct CallbackContext {
    apply: bool,
    secret_behavior: SecretBehavior,
    cancel_after_authentication: bool,
    cancellation: *mut SdsyncCancellationV1,
    password_queries: usize,
    password_writes: usize,
    otp_requests: usize,
    plan_calls: usize,
    event_calls: usize,
    mutation_events: usize,
}

unsafe extern "C" fn secret_callback(
    user_data: *mut c_void,
    kind: u32,
    buffer: *mut u8,
    capacity: u64,
    written: *mut u64,
) -> i32 {
    // SAFETY: The test keeps one context alive for the complete ABI call.
    let context = unsafe { &mut *user_data.cast::<CallbackContext>() };
    let secret: &[u8] = match kind {
        SDSYNC_SECRET_PASSWORD => {
            if buffer.is_null() {
                context.password_queries += 1;
            } else {
                context.password_writes += 1;
            }
            match context.secret_behavior {
                SecretBehavior::Unavailable => return SDSYNC_CALLBACK_UNAVAILABLE,
                SecretBehavior::UnavailableSecondPass if !buffer.is_null() => {
                    return SDSYNC_CALLBACK_UNAVAILABLE;
                }
                SecretBehavior::Cancelled => return SDSYNC_CALLBACK_CANCELLED,
                SecretBehavior::CancelledSecondPass if !buffer.is_null() => {
                    return SDSYNC_CALLBACK_CANCELLED;
                }
                SecretBehavior::UnknownStatus => return 73,
                SecretBehavior::InvalidLength => {
                    // SAFETY: The ABI contract supplies a valid output pointer.
                    unsafe { *written = 0 };
                    return SDSYNC_CALLBACK_OK;
                }
                SecretBehavior::InvalidUtf8 => &[0xff],
                SecretBehavior::InconsistentLength => {
                    if buffer.is_null() {
                        // SAFETY: The ABI contract supplies a valid output pointer.
                        unsafe { *written = 5 };
                        return SDSYNC_CALLBACK_OK;
                    }
                    assert!(capacity >= 5);
                    // SAFETY: Four bytes fit inside the advertised five-byte buffer.
                    unsafe {
                        ptr::copy_nonoverlapping(b"four".as_ptr(), buffer, 4);
                        *written = 4;
                    }
                    return SDSYNC_CALLBACK_OK;
                }
                SecretBehavior::Valid
                | SecretBehavior::UnavailableSecondPass
                | SecretBehavior::CancelledSecondPass => b"correct horse battery staple",
            }
        }
        SDSYNC_SECRET_OTP_REQUIRED | SDSYNC_SECRET_OTP_REJECTED => {
            context.otp_requests += 1;
            b"654321"
        }
        _ => return 1,
    };
    // SAFETY: The ABI contract supplies a valid output pointer.
    unsafe { *written = secret.len() as u64 };
    if buffer.is_null() {
        return SDSYNC_CALLBACK_OK;
    }
    assert!(capacity >= secret.len() as u64);
    // SAFETY: The callback received at least `secret.len()` writable bytes.
    unsafe { ptr::copy_nonoverlapping(secret.as_ptr(), buffer, secret.len()) };
    SDSYNC_CALLBACK_OK
}

unsafe extern "C" fn plan_callback(user_data: *mut c_void, json: *const u8, json_len: u64) -> u32 {
    // SAFETY: The test keeps one context alive for the complete ABI call.
    let context = unsafe { &mut *user_data.cast::<CallbackContext>() };
    context.plan_calls += 1;
    // SAFETY: The library lends exactly `json_len` immutable bytes for this call.
    let document: Value =
        serde_json::from_slice(unsafe { slice::from_raw_parts(json, json_len as usize) })
            .expect("plan callback JSON");
    assert_eq!(document["schema"], "sdsync.plan.v1");
    assert!(document["plan"]["changes"].as_array().is_some());
    if context.apply {
        SDSYNC_PLAN_APPLY
    } else {
        SDSYNC_PLAN_PREVIEW_ONLY
    }
}

unsafe extern "C" fn event_callback(user_data: *mut c_void, json: *const u8, json_len: u64) -> u32 {
    // SAFETY: The test keeps one context alive for the complete ABI call.
    let context = unsafe { &mut *user_data.cast::<CallbackContext>() };
    context.event_calls += 1;
    // SAFETY: The library lends exactly `json_len` immutable bytes for this call.
    let document: Value =
        serde_json::from_slice(unsafe { slice::from_raw_parts(json, json_len as usize) })
            .expect("event callback JSON");
    assert_eq!(document["schema"], "sdsync.event.v1");
    if document["event"]["kind"] == "mutation" {
        context.mutation_events += 1;
    }
    if context.cancel_after_authentication
        && document["event"]["kind"] == "phase-completed"
        && document["event"]["phase"] == "authentication"
    {
        // SAFETY: The run owns this live handle through callback completion.
        assert_eq!(
            unsafe { sdsync_cancellation_cancel_v1(context.cancellation) },
            SDSYNC_STATUS_OK
        );
    }
    SDSYNC_EVENT_CONTINUE
}

fn run(server: &MockFileStation, source: &TestDir, apply: bool) -> (i32, Value, CallbackContext) {
    run_with_options(server, source, apply, SecretBehavior::Valid, false)
}

fn run_with_options(
    server: &MockFileStation,
    source: &TestDir,
    apply: bool,
    secret_behavior: SecretBehavior,
    cancel_after_authentication: bool,
) -> (i32, Value, CallbackContext) {
    let request = serde_json::to_vec(&json!({
        "schema": "sdsync.request.v1",
        "endpoint": server.base_url(),
        "username": "e2e-user",
        "source": source.path(),
        "remote": "/team/ffi",
        "allow_http": true
    }))
    .expect("request JSON");
    let mut context = CallbackContext {
        apply,
        secret_behavior,
        cancel_after_authentication,
        cancellation: ptr::null_mut(),
        password_queries: 0,
        password_writes: 0,
        otp_requests: 0,
        plan_calls: 0,
        event_calls: 0,
        mutation_events: 0,
    };
    let callbacks = SdsyncCallbacksV1 {
        struct_size: size_of::<SdsyncCallbacksV1>() as u32,
        reserved: 0,
        user_data: (&mut context as *mut CallbackContext).cast(),
        secret: Some(secret_callback),
        plan: Some(plan_callback),
        event: Some(event_callback),
    };
    let mut cancellation = ptr::null_mut();
    if cancel_after_authentication {
        // SAFETY: A valid local output pointer receives one owned handle.
        assert_eq!(
            unsafe { sdsync_cancellation_new_v1(&mut cancellation) },
            SDSYNC_STATUS_OK
        );
        context.cancellation = cancellation;
    }
    let mut result: *mut SdsyncResultV1 = ptr::null_mut();
    // SAFETY: All borrowed inputs and the callback context outlive the call.
    let status = unsafe {
        sdsync_run_v1(
            request.as_ptr(),
            request.len() as u64,
            &callbacks,
            cancellation,
            &mut result,
        )
    };
    assert!(!result.is_null());
    let mut bytes = ptr::null();
    let mut length = 0_u64;
    // SAFETY: `result` is live and both out pointers are valid.
    assert_eq!(
        unsafe { sdsync_result_bytes_v1(result, &mut bytes, &mut length) },
        SDSYNC_STATUS_OK
    );
    // SAFETY: The result handle owns this immutable view through deserialization.
    let document = serde_json::from_slice(unsafe { slice::from_raw_parts(bytes, length as usize) })
        .expect("result JSON");
    // SAFETY: The borrowed result bytes are no longer used.
    unsafe { sdsync_result_free_v1(result) };
    // SAFETY: The run and every callback have returned, so the optional live
    // cancellation handle is no longer borrowed.
    unsafe { sdsync_cancellation_free_v1(cancellation) };
    (status, document, context)
}

#[test]
fn ffi_preview_is_non_mutating_and_secret_is_requested_on_demand() {
    let server = MockFileStation::start();
    let source = TestDir::new("ffi-preview");
    source.write("hello.txt", b"FFI preview");

    let (status, result, context) = run(&server, &source, false);

    assert_eq!(status, SDSYNC_STATUS_OK);
    assert_eq!(result["schema"], "sdsync.ffi-result.v1");
    assert_eq!(result["ok"], true);
    assert_eq!(result["outcome"]["applied"], false);
    assert!(server.file_contents("/team/ffi/hello.txt").is_none());
    assert_eq!(context.password_queries, 1);
    assert_eq!(context.password_writes, 1);
    assert_eq!(context.otp_requests, 0);
    assert_eq!(context.plan_calls, 1);
    assert!(context.event_calls > 0);
    assert_eq!(context.mutation_events, 0);
}

#[test]
fn ffi_apply_executes_and_returns_reconciled_json() {
    let server = MockFileStation::start();
    let source = TestDir::new("ffi-apply");
    source.write("nested/hello.txt", b"FFI applied");

    let (status, result, context) = run(&server, &source, true);

    assert_eq!(status, SDSYNC_STATUS_OK);
    assert_eq!(result["ok"], true);
    assert_eq!(result["outcome"]["applied"], true);
    assert_eq!(result["outcome"]["reconciled"], true);
    assert_eq!(result["outcome"]["execution"]["uploaded"], 1);
    assert_eq!(
        server.file_contents("/team/ffi/nested/hello.txt"),
        Some(b"FFI applied".to_vec())
    );
    assert_eq!(context.plan_calls, 1);
    assert!(context.mutation_events > 0);
}

#[test]
fn secret_callback_protocol_failures_are_distinct_from_valid_absence_and_cancellation() {
    let server = MockFileStation::start();
    let source = TestDir::new("ffi-secret-errors");
    source.write("hello.txt", b"callback classification");

    for behavior in [
        SecretBehavior::UnknownStatus,
        SecretBehavior::InvalidLength,
        SecretBehavior::InvalidUtf8,
        SecretBehavior::InconsistentLength,
    ] {
        let (status, result, _) = run_with_options(&server, &source, false, behavior, false);
        assert_eq!(status, SDSYNC_STATUS_CALLBACK_FAILED);
        assert_eq!(result["ok"], false);
        assert_eq!(result["error"]["code"], "callback-failed");
    }

    let (status, result, _) = run_with_options(
        &server,
        &source,
        false,
        SecretBehavior::UnavailableSecondPass,
        false,
    );
    assert_eq!(status, SDSYNC_STATUS_CALLBACK_FAILED);
    assert_eq!(result["error"]["code"], "callback-failed");
    assert_eq!(
        result["error"]["message"],
        "secret callback became unavailable between query and write passes"
    );

    let (status, result, _) =
        run_with_options(&server, &source, false, SecretBehavior::Unavailable, false);
    assert_eq!(status, SDSYNC_STATUS_OPERATION_FAILED);
    assert_eq!(result["error"]["code"], "credential-unavailable");

    let (status, result, _) =
        run_with_options(&server, &source, false, SecretBehavior::Cancelled, false);
    assert_eq!(status, SDSYNC_STATUS_CANCELLED);
    assert_eq!(result["error"]["code"], "cancelled");

    let (status, result, _) = run_with_options(
        &server,
        &source,
        false,
        SecretBehavior::CancelledSecondPass,
        false,
    );
    assert_eq!(status, SDSYNC_STATUS_CANCELLED);
    assert_eq!(result["error"]["code"], "cancelled");
}

#[test]
fn active_ffi_cancellation_stops_after_authentication_and_logs_out() {
    let server = MockFileStation::start();
    let source = TestDir::new("ffi-active-cancel");
    source.write("hello.txt", b"active cancellation");

    let (status, result, context) =
        run_with_options(&server, &source, true, SecretBehavior::Valid, true);

    assert_eq!(status, SDSYNC_STATUS_CANCELLED);
    assert_eq!(result["error"]["code"], "cancelled");
    assert_eq!(context.password_writes, 1);
    assert_eq!(context.plan_calls, 0);
    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.operation() == "SYNO.API.Auth.logout")
    );
}
