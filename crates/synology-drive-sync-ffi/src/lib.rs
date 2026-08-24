//! Stable JSON-over-C ABI for `synology-drive-sync`.
//!
//! The ABI deliberately exchanges versioned UTF-8 JSON instead of Rust data
//! layouts. Secrets are supplied through a caller callback into library-owned
//! zeroizing buffers, and every returned byte buffer has one matching free
//! function.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(panic = "abort")]
compile_error!(
    "synology-drive-sync-ffi requires panic=unwind for ABI containment; build it with `cargo build -p synology-drive-sync-ffi --profile ffi-release`"
);

use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::rc::Rc;
use std::slice;
use std::str;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::sdk::{
    Comparison, DeletionPolicy, Engine, ErrorCode, EventControl, OtpChallenge, PlanDecision,
    PlanSummary, SdkEvent, Secret, SecretProvider, SecretProviderError, SyncRequest,
};
use zeroize::Zeroizing;

/// C ABI major implemented by every exported symbol in this library.
pub const SDSYNC_ABI_VERSION_V1: u32 = 1;

pub const SDSYNC_STATUS_OK: i32 = 0;
pub const SDSYNC_STATUS_INVALID_ARGUMENT: i32 = 1;
pub const SDSYNC_STATUS_CALLBACK_FAILED: i32 = 2;
pub const SDSYNC_STATUS_CANCELLED: i32 = 3;
pub const SDSYNC_STATUS_OPERATION_FAILED: i32 = 4;
pub const SDSYNC_STATUS_PANIC: i32 = 255;

pub const SDSYNC_CALLBACK_OK: i32 = 0;
pub const SDSYNC_CALLBACK_UNAVAILABLE: i32 = 1;
pub const SDSYNC_CALLBACK_CANCELLED: i32 = 2;

pub const SDSYNC_SECRET_PASSWORD: u32 = 1;
pub const SDSYNC_SECRET_OTP_REQUIRED: u32 = 2;
pub const SDSYNC_SECRET_OTP_REJECTED: u32 = 3;

pub const SDSYNC_PLAN_PREVIEW_ONLY: u32 = 0;
pub const SDSYNC_PLAN_APPLY: u32 = 1;
pub const SDSYNC_PLAN_CANCEL: u32 = 2;

pub const SDSYNC_EVENT_CONTINUE: u32 = 0;
pub const SDSYNC_EVENT_CANCEL: u32 = 1;

const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SECRET_BYTES: u64 = 64 * 1024;

/// Secret callback. A query call has `buffer == NULL` and `capacity == 0`;
/// write the required byte count to `written`. A second call supplies exactly
/// that capacity. Bytes must be UTF-8 and are copied immediately.
pub type SdsyncSecretCallbackV1 = unsafe extern "C" fn(
    user_data: *mut c_void,
    secret_kind: u32,
    buffer: *mut u8,
    capacity: u64,
    written: *mut u64,
) -> i32;

/// Plan callback receiving `sdsync.plan.v1` JSON and returning one of the
/// `SDSYNC_PLAN_*` constants.
pub type SdsyncPlanCallbackV1 =
    unsafe extern "C" fn(user_data: *mut c_void, json: *const u8, json_len: u64) -> u32;

/// Event callback receiving `sdsync.event.v1` JSON and returning one of the
/// `SDSYNC_EVENT_*` constants.
pub type SdsyncEventCallbackV1 =
    unsafe extern "C" fn(user_data: *mut c_void, json: *const u8, json_len: u64) -> u32;

/// Size-versioned callback table. Initialize `struct_size` to
/// `sizeof(sdsync_callbacks_v1)`, zero `reserved`, and set unused callbacks to
/// `NULL`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdsyncCallbacksV1 {
    pub struct_size: u32,
    pub reserved: u32,
    pub user_data: *mut c_void,
    pub secret: Option<SdsyncSecretCallbackV1>,
    pub plan: Option<SdsyncPlanCallbackV1>,
    pub event: Option<SdsyncEventCallbackV1>,
}

/// Opaque cancellation handle. It may be cancelled from another thread.
pub struct SdsyncCancellationV1 {
    token: CancellationToken,
}

/// Opaque immutable result containing one UTF-8 `sdsync.ffi-result.v1` JSON
/// document.
pub struct SdsyncResultV1 {
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct Callbacks {
    user_data: *mut c_void,
    secret: Option<SdsyncSecretCallbackV1>,
    plan: Option<SdsyncPlanCallbackV1>,
    event: Option<SdsyncEventCallbackV1>,
}

impl Default for Callbacks {
    fn default() -> Self {
        Self {
            user_data: ptr::null_mut(),
            secret: None,
            plan: None,
            event: None,
        }
    }
}

#[derive(Debug)]
struct FfiFailure {
    status: i32,
    code: &'static str,
    message: String,
}

impl FfiFailure {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: SDSYNC_STATUS_INVALID_ARGUMENT,
            code: "invalid-argument",
            message: message.into(),
        }
    }

    fn callback(message: impl Into<String>) -> Self {
        Self {
            status: SDSYNC_STATUS_CALLBACK_FAILED,
            code: "callback-failed",
            message: message.into(),
        }
    }

    fn panic() -> Self {
        Self {
            status: SDSYNC_STATUS_PANIC,
            code: "panic",
            message: "the Rust library caught an internal panic".to_owned(),
        }
    }
}

#[derive(Serialize)]
struct ResultEnvelope<T: Serialize> {
    schema: &'static str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorEnvelope>,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    code: String,
    message: String,
}

#[derive(Serialize)]
struct PlanEnvelope<'a> {
    schema: &'static str,
    plan: &'a PlanSummary,
}

#[derive(Serialize)]
struct EventEnvelope<'a> {
    schema: &'static str,
    event: &'a SdkEvent,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestDocument {
    schema: String,
    endpoint: String,
    username: String,
    source: String,
    remote: String,
    #[serde(default)]
    allow_http: bool,
    #[serde(default)]
    danger_accept_invalid_certificates: bool,
    ca_certificate: Option<String>,
    connect_timeout_seconds: Option<u64>,
    request_timeout_seconds: Option<u64>,
    retries: Option<u32>,
    max_upload_rate: Option<u64>,
    #[serde(default)]
    exclusions: Vec<String>,
    comparison: Option<RequestComparison>,
    deletion: Option<RequestDeletion>,
    jobs: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RequestComparison {
    Content,
    Metadata,
    SizeOnly,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestDeletion {
    #[serde(default)]
    enabled: bool,
    max_delete: Option<u64>,
    #[serde(default)]
    allow_empty_source: bool,
}

struct CallbackSecrets {
    callbacks: Callbacks,
    callback_failure: Rc<RefCell<Option<FfiFailure>>>,
}

impl CallbackSecrets {
    fn invalid_callback(&self, message: &'static str) -> SecretProviderError {
        let mut failure = self.callback_failure.borrow_mut();
        if failure.is_none() {
            *failure = Some(FfiFailure::callback(message));
        }
        SecretProviderError::Unavailable
    }

    fn read(&self, kind: u32) -> std::result::Result<Option<Secret>, SecretProviderError> {
        let Some(callback) = self.callbacks.secret else {
            return Ok(None);
        };
        let mut required = 0_u64;
        // SAFETY: The callback and user-data pointer are supplied by the caller
        // for the duration of `sdsync_run_v1`; `written` is a valid local out
        // pointer and the query call deliberately supplies no writable buffer.
        let status = unsafe {
            callback(
                self.callbacks.user_data,
                kind,
                ptr::null_mut(),
                0,
                &mut required,
            )
        };
        match status {
            SDSYNC_CALLBACK_OK => {}
            SDSYNC_CALLBACK_UNAVAILABLE => return Ok(None),
            SDSYNC_CALLBACK_CANCELLED => return Err(SecretProviderError::Cancelled),
            _ => {
                return Err(self.invalid_callback("secret callback returned an unknown status"));
            }
        }
        if required == 0 || required > MAX_SECRET_BYTES || required > usize::MAX as u64 {
            return Err(self.invalid_callback("secret callback reported an invalid length"));
        }

        let mut bytes = Zeroizing::new(vec![0_u8; required as usize]);
        let mut written = 0_u64;
        // SAFETY: `bytes` is writable for `required` bytes and remains alive for
        // the call. The callback contract forbids retaining the pointer.
        let status = unsafe {
            callback(
                self.callbacks.user_data,
                kind,
                bytes.as_mut_ptr(),
                required,
                &mut written,
            )
        };
        match status {
            SDSYNC_CALLBACK_OK => {}
            SDSYNC_CALLBACK_UNAVAILABLE => {
                return Err(self.invalid_callback(
                    "secret callback became unavailable between query and write passes",
                ));
            }
            SDSYNC_CALLBACK_CANCELLED => return Err(SecretProviderError::Cancelled),
            _ => {
                return Err(self.invalid_callback("secret callback returned an unknown status"));
            }
        }
        if written != required {
            return Err(
                self.invalid_callback("secret callback changed its reported length between passes")
            );
        }
        let text = str::from_utf8(&bytes[..written as usize])
            .map_err(|_| self.invalid_callback("secret callback returned invalid UTF-8"))?
            .to_owned();
        Ok(Some(Secret::new(text)))
    }
}

impl SecretProvider for CallbackSecrets {
    fn password(&mut self) -> std::result::Result<Secret, SecretProviderError> {
        self.read(SDSYNC_SECRET_PASSWORD)?
            .ok_or(SecretProviderError::Unavailable)
    }

    fn otp(
        &mut self,
        challenge: OtpChallenge,
    ) -> std::result::Result<Option<Secret>, SecretProviderError> {
        let kind = match challenge {
            OtpChallenge::Required => SDSYNC_SECRET_OTP_REQUIRED,
            OtpChallenge::Rejected => SDSYNC_SECRET_OTP_REJECTED,
        };
        self.read(kind)
    }
}

/// Return the implemented C ABI major.
#[unsafe(no_mangle)]
pub extern "C" fn sdsync_abi_version_v1() -> u32 {
    SDSYNC_ABI_VERSION_V1
}

/// Return a borrowed pointer/length view of the static calendar build version.
/// The pointer remains valid for the lifetime of the loaded library and must not
/// be freed.
///
/// # Safety
///
/// `data` and `length` must each point to writable storage for their respective
/// output values for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdsync_build_version_v1(data: *mut *const u8, length: *mut u64) -> i32 {
    if data.is_null() || length.is_null() {
        return SDSYNC_STATUS_INVALID_ARGUMENT;
    }
    let version = synology_drive_sync::sdk::build_version().as_bytes();
    // SAFETY: Both out pointers were checked non-null and are caller-provided
    // writable locations for the duration of this call.
    unsafe {
        *data = version.as_ptr();
        *length = version.len() as u64;
    }
    SDSYNC_STATUS_OK
}

/// Allocate a new cancellation handle.
///
/// # Safety
///
/// `out` must point to writable storage for one handle pointer. On success the
/// caller owns that handle and must eventually free it exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdsync_cancellation_new_v1(out: *mut *mut SdsyncCancellationV1) -> i32 {
    if out.is_null() {
        return SDSYNC_STATUS_INVALID_ARGUMENT;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        Box::into_raw(Box::new(SdsyncCancellationV1 {
            token: CancellationToken::default(),
        }))
    }));
    match result {
        Ok(handle) => {
            // SAFETY: `out` is a checked caller-provided writable pointer.
            unsafe { *out = handle };
            SDSYNC_STATUS_OK
        }
        Err(_) => {
            // SAFETY: `out` is a checked caller-provided writable pointer.
            unsafe { *out = ptr::null_mut() };
            SDSYNC_STATUS_PANIC
        }
    }
}

/// Cooperatively cancel a handle. Safe to call repeatedly and from another
/// thread while `sdsync_run_v1` is active.
///
/// # Safety
///
/// `cancellation` must be a live handle allocated by this library and must not
/// be freed until this call and every concurrent run using it have returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdsync_cancellation_cancel_v1(
    cancellation: *const SdsyncCancellationV1,
) -> i32 {
    if cancellation.is_null() {
        return SDSYNC_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: The caller guarantees a live handle allocated by this library.
    unsafe { (*cancellation).token.cancel() };
    SDSYNC_STATUS_OK
}

/// Free a cancellation handle. `NULL` is accepted. The caller must ensure no
/// run still uses the handle and must not free it twice.
///
/// # Safety
///
/// A non-null pointer must be a live handle returned by
/// `sdsync_cancellation_new_v1` and ownership must be transferred exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdsync_cancellation_free_v1(cancellation: *mut SdsyncCancellationV1) {
    if !cancellation.is_null() {
        // SAFETY: The caller transfers the unique handle returned by
        // `sdsync_cancellation_new_v1` exactly once.
        drop(unsafe { Box::from_raw(cancellation) });
    }
}

/// Execute one plan/apply operation from a `sdsync.request.v1` UTF-8 JSON
/// document. A missing plan callback fails closed to preview-only behavior.
///
/// On every return except an invalid `out_result` pointer, `*out_result` owns a
/// JSON result handle that must be freed with `sdsync_result_free_v1`.
///
/// # Safety
///
/// `request` must remain readable for `request_len` bytes. Every supplied
/// callback table, callback user-data value, and cancellation handle must
/// remain valid until the call returns. `out_result` must point to writable
/// storage for one result pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdsync_run_v1(
    request: *const u8,
    request_len: u64,
    callbacks: *const SdsyncCallbacksV1,
    cancellation: *const SdsyncCancellationV1,
    out_result: *mut *mut SdsyncResultV1,
) -> i32 {
    if out_result.is_null() {
        return SDSYNC_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: `out_result` was checked non-null and is caller-writable.
    unsafe { *out_result = ptr::null_mut() };

    let (status, bytes) = contain_run(|| {
        // SAFETY: Raw ABI validation and copying are contained in this helper;
        // the caller must provide readable memory for every non-null input.
        unsafe { run_inner(request, request_len, callbacks, cancellation) }
    });
    let handle = Box::into_raw(Box::new(SdsyncResultV1 { bytes }));
    // SAFETY: `out_result` was checked and the newly allocated handle is
    // transferred to the caller.
    unsafe { *out_result = handle };
    status
}

fn contain_run<F>(operation: F) -> (i32, Vec<u8>)
where
    F: FnOnce() -> std::result::Result<synology_drive_sync::sdk::SyncOutcome, FfiFailure>,
{
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(outcome)) => (
            SDSYNC_STATUS_OK,
            serialize_success(outcome).unwrap_or_else(serialization_fallback),
        ),
        Ok(Err(failure)) => {
            let status = failure.status;
            (status, serialize_failure(&failure))
        }
        Err(_) => {
            let failure = FfiFailure::panic();
            (failure.status, serialize_failure(&failure))
        }
    }
}

/// Borrow the UTF-8 JSON bytes owned by a result handle. The view remains valid
/// until `sdsync_result_free_v1` is called and must not be modified or freed.
///
/// # Safety
///
/// `result` must be a live library-owned result. `data` and `length` must point
/// to writable storage, and the caller must not use the returned view after
/// freeing `result`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdsync_result_bytes_v1(
    result: *const SdsyncResultV1,
    data: *mut *const u8,
    length: *mut u64,
) -> i32 {
    if result.is_null() || data.is_null() || length.is_null() {
        return SDSYNC_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: The caller guarantees a live result handle and writable out
    // pointers for this call.
    unsafe {
        *data = (*result).bytes.as_ptr();
        *length = (*result).bytes.len() as u64;
    }
    SDSYNC_STATUS_OK
}

/// Free a result handle. `NULL` is accepted. A handle must be freed exactly
/// once and no borrowed byte view may be used afterwards.
///
/// # Safety
///
/// A non-null pointer must be a live result returned by `sdsync_run_v1`, and
/// ownership must be transferred exactly once after all borrowed views expire.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdsync_result_free_v1(result: *mut SdsyncResultV1) {
    if !result.is_null() {
        // SAFETY: The caller transfers one live library-owned handle exactly
        // once.
        drop(unsafe { Box::from_raw(result) });
    }
}

unsafe fn run_inner(
    request: *const u8,
    request_len: u64,
    callbacks: *const SdsyncCallbacksV1,
    cancellation: *const SdsyncCancellationV1,
) -> std::result::Result<synology_drive_sync::sdk::SyncOutcome, FfiFailure> {
    if request.is_null() || request_len == 0 {
        return Err(FfiFailure::invalid(
            "request pointer must be non-null and length must be positive",
        ));
    }
    if request_len > MAX_REQUEST_BYTES || request_len > usize::MAX as u64 {
        return Err(FfiFailure::invalid("request document is too large"));
    }
    // SAFETY: The caller promises `request` is readable for `request_len`
    // bytes for the duration of the call; bounds were checked above.
    let request_bytes = unsafe { slice::from_raw_parts(request, request_len as usize) };
    let request_text = str::from_utf8(request_bytes)
        .map_err(|_| FfiFailure::invalid("request document must be valid UTF-8"))?;
    let document: RequestDocument = serde_json::from_str(request_text).map_err(|error| {
        let category = match error.classify() {
            serde_json::error::Category::Io => "I/O",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "schema",
            serde_json::error::Category::Eof => "unexpected EOF",
        };
        FfiFailure::invalid(format!(
            "invalid request JSON ({category} at line {}, column {})",
            error.line(),
            error.column()
        ))
    })?;
    let request = build_request(document)?;
    // SAFETY: Callback table validation copies the complete v1 prefix only
    // after checking its advertised byte size.
    let callbacks = unsafe { copy_callbacks(callbacks)? };
    let token = if cancellation.is_null() {
        CancellationToken::default()
    } else {
        // SAFETY: The caller guarantees a live library-created cancellation
        // handle for the duration of this call.
        unsafe { (*cancellation).token.clone() }
    };

    let callback_failure = Rc::new(RefCell::new(None::<FfiFailure>));
    let plan_failure = Rc::clone(&callback_failure);
    let event_failure = Rc::clone(&callback_failure);
    let plan_token = token.clone();
    let event_token = token.clone();
    let mut secrets = CallbackSecrets {
        callbacks,
        callback_failure: Rc::clone(&callback_failure),
    };
    let outcome = Engine.run(
        &request,
        &mut secrets,
        &token,
        move |plan| match invoke_plan(callbacks, plan) {
            Ok(PlanDecision::Apply) => PlanDecision::Apply,
            Ok(PlanDecision::PreviewOnly) => PlanDecision::PreviewOnly,
            Err(failure) => {
                *plan_failure.borrow_mut() = Some(failure);
                plan_token.cancel();
                PlanDecision::PreviewOnly
            }
        },
        move |event| match invoke_event(callbacks, event) {
            Ok(control) => control,
            Err(failure) => {
                *event_failure.borrow_mut() = Some(failure);
                event_token.cancel();
                EventControl::Cancel
            }
        },
    );
    if let Some(failure) = callback_failure.borrow_mut().take() {
        return Err(failure);
    }
    outcome.map_err(|error| {
        let status = if error.code() == ErrorCode::Cancelled {
            SDSYNC_STATUS_CANCELLED
        } else {
            SDSYNC_STATUS_OPERATION_FAILED
        };
        FfiFailure {
            status,
            code: error_code_name(error.code()),
            message: error.message().to_owned(),
        }
    })
}

unsafe fn copy_callbacks(
    callbacks: *const SdsyncCallbacksV1,
) -> std::result::Result<Callbacks, FfiFailure> {
    if callbacks.is_null() {
        return Ok(Callbacks::default());
    }
    // SAFETY: The caller promises the pointer is readable for at least the
    // advertised first u32. Unaligned input is tolerated.
    let advertised = unsafe { ptr::read_unaligned(callbacks.cast::<u32>()) };
    if advertised < size_of::<SdsyncCallbacksV1>() as u32 {
        return Err(FfiFailure::invalid(
            "callback table is smaller than the v1 layout",
        ));
    }
    // SAFETY: The checked advertised size covers the complete v1 prefix and
    // the caller keeps it readable for this call.
    let table = unsafe { ptr::read_unaligned(callbacks) };
    if table.reserved != 0 {
        return Err(FfiFailure::invalid(
            "callback table reserved field must be zero",
        ));
    }
    Ok(Callbacks {
        user_data: table.user_data,
        secret: table.secret,
        plan: table.plan,
        event: table.event,
    })
}

fn build_request(document: RequestDocument) -> std::result::Result<SyncRequest, FfiFailure> {
    if document.schema != "sdsync.request.v1" {
        return Err(FfiFailure::invalid(
            "request schema must be exactly sdsync.request.v1",
        ));
    }
    let mut builder = SyncRequest::builder(
        document.endpoint,
        document.username,
        document.source,
        document.remote,
    )
    .allow_http(document.allow_http)
    .danger_accept_invalid_certificates(document.danger_accept_invalid_certificates);
    if let Some(path) = document.ca_certificate {
        builder = builder.ca_certificate(path);
    }
    if let Some(seconds) = document.connect_timeout_seconds {
        builder = builder.connect_timeout(Duration::from_secs(seconds));
    }
    if let Some(seconds) = document.request_timeout_seconds {
        builder = builder.request_timeout(Duration::from_secs(seconds));
    }
    if let Some(retries) = document.retries {
        builder = builder.retries(retries);
    }
    if let Some(rate) = document.max_upload_rate {
        builder = builder.max_upload_rate(rate);
    }
    for pattern in document.exclusions {
        builder = builder.exclude(pattern);
    }
    if let Some(comparison) = document.comparison {
        builder = builder.comparison(match comparison {
            RequestComparison::Content => Comparison::Content,
            RequestComparison::Metadata => Comparison::Metadata,
            RequestComparison::SizeOnly => Comparison::SizeOnly,
        });
    }
    if let Some(deletion) = document.deletion {
        if deletion.enabled {
            let maximum = deletion
                .max_delete
                .ok_or_else(|| FfiFailure::invalid("enabled deletion requires max_delete"))?;
            let maximum = usize::try_from(maximum)
                .map_err(|_| FfiFailure::invalid("max_delete exceeds this platform"))?;
            let mut policy = DeletionPolicy::bounded(maximum)
                .map_err(|error| FfiFailure::invalid(error.message()))?;
            if deletion.allow_empty_source {
                policy = policy.allow_empty_source();
            }
            builder = builder.deletion(policy);
        } else if deletion.max_delete.is_some() || deletion.allow_empty_source {
            return Err(FfiFailure::invalid(
                "disabled deletion cannot set max_delete or allow_empty_source",
            ));
        }
    }
    if let Some(jobs) = document.jobs {
        builder = builder.jobs(jobs as usize);
    }
    builder
        .build()
        .map_err(|error| FfiFailure::invalid(error.message()))
}

fn invoke_plan(
    callbacks: Callbacks,
    plan: &PlanSummary,
) -> std::result::Result<PlanDecision, FfiFailure> {
    let Some(callback) = callbacks.plan else {
        return Ok(PlanDecision::PreviewOnly);
    };
    let json = serde_json::to_vec(&PlanEnvelope {
        schema: "sdsync.plan.v1",
        plan,
    })
    .map_err(|_| FfiFailure::callback("failed to serialize plan callback JSON"))?;
    // SAFETY: The byte view remains live and immutable for the callback; the
    // callback contract forbids retaining it.
    let decision = unsafe { callback(callbacks.user_data, json.as_ptr(), json.len() as u64) };
    match decision {
        SDSYNC_PLAN_PREVIEW_ONLY => Ok(PlanDecision::PreviewOnly),
        SDSYNC_PLAN_APPLY => Ok(PlanDecision::Apply),
        SDSYNC_PLAN_CANCEL => Err(FfiFailure {
            status: SDSYNC_STATUS_CANCELLED,
            code: "cancelled",
            message: "plan callback cancelled the operation".to_owned(),
        }),
        _ => Err(FfiFailure::callback(
            "plan callback returned an unknown decision",
        )),
    }
}

fn invoke_event(
    callbacks: Callbacks,
    event: &SdkEvent,
) -> std::result::Result<EventControl, FfiFailure> {
    let Some(callback) = callbacks.event else {
        return Ok(EventControl::Continue);
    };
    let json = serde_json::to_vec(&EventEnvelope {
        schema: "sdsync.event.v1",
        event,
    })
    .map_err(|_| FfiFailure::callback("failed to serialize event callback JSON"))?;
    // SAFETY: The byte view remains live and immutable for the callback; the
    // callback contract forbids retaining it.
    let control = unsafe { callback(callbacks.user_data, json.as_ptr(), json.len() as u64) };
    match control {
        SDSYNC_EVENT_CONTINUE => Ok(EventControl::Continue),
        SDSYNC_EVENT_CANCEL => Ok(EventControl::Cancel),
        _ => Err(FfiFailure::callback(
            "event callback returned an unknown control value",
        )),
    }
}

fn serialize_success(
    outcome: synology_drive_sync::sdk::SyncOutcome,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&ResultEnvelope {
        schema: "sdsync.ffi-result.v1",
        ok: true,
        outcome: Some(outcome),
        error: None,
    })
}

fn serialize_failure(failure: &FfiFailure) -> Vec<u8> {
    serde_json::to_vec(&ResultEnvelope::<serde_json::Value> {
        schema: "sdsync.ffi-result.v1",
        ok: false,
        outcome: None,
        error: Some(ErrorEnvelope {
            code: failure.code.to_owned(),
            message: failure.message.clone(),
        }),
    })
    .unwrap_or_else(serialization_fallback)
}

fn serialization_fallback(_: serde_json::Error) -> Vec<u8> {
    br#"{"schema":"sdsync.ffi-result.v1","ok":false,"error":{"code":"internal","message":"result serialization failed"}}"#.to_vec()
}

fn error_code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest => "invalid-request",
        ErrorCode::CredentialUnavailable => "credential-unavailable",
        ErrorCode::OtpRequired => "otp-required",
        ErrorCode::Authentication => "authentication",
        ErrorCode::LocalFilesystem => "local-filesystem",
        ErrorCode::Network => "network",
        ErrorCode::Remote => "remote",
        ErrorCode::Safety => "safety",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::Reconciliation => "reconciliation",
        ErrorCode::Internal => "internal",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    unsafe fn result_json(handle: *mut SdsyncResultV1) -> String {
        let mut data = ptr::null();
        let mut length = 0_u64;
        // SAFETY: Tests pass one live result handle and valid local out pointers.
        assert_eq!(
            unsafe { sdsync_result_bytes_v1(handle, &mut data, &mut length) },
            SDSYNC_STATUS_OK
        );
        // SAFETY: The result API returned a live view for exactly `length`
        // bytes, valid until the handle is freed below.
        let text = unsafe { slice::from_raw_parts(data, length as usize) };
        String::from_utf8(text.to_vec()).expect("result is UTF-8")
    }

    #[test]
    fn abi_and_build_version_are_borrowed_and_stable() {
        assert_eq!(sdsync_abi_version_v1(), 1);
        let mut data = ptr::null();
        let mut length = 0_u64;
        // SAFETY: Valid local out pointers are supplied.
        assert_eq!(
            unsafe { sdsync_build_version_v1(&mut data, &mut length) },
            SDSYNC_STATUS_OK
        );
        // SAFETY: The function returns static bytes for the loaded library.
        let version = unsafe { slice::from_raw_parts(data, length as usize) };
        assert_eq!(
            version,
            synology_drive_sync::sdk::build_version().as_bytes()
        );
        // SAFETY: Null out pointers are intentionally tested.
        assert_eq!(
            unsafe { sdsync_build_version_v1(ptr::null_mut(), &mut length) },
            SDSYNC_STATUS_INVALID_ARGUMENT
        );
    }

    #[test]
    fn invalid_utf8_returns_owned_redacted_json() {
        let request = [0xff_u8];
        let mut result = ptr::null_mut();
        // SAFETY: The request and out pointer are valid for this call.
        let status = unsafe {
            sdsync_run_v1(
                request.as_ptr(),
                request.len() as u64,
                ptr::null(),
                ptr::null(),
                &mut result,
            )
        };
        assert_eq!(status, SDSYNC_STATUS_INVALID_ARGUMENT);
        assert!(!result.is_null());
        // SAFETY: `result` is live and uniquely owned by this test.
        let json = unsafe { result_json(result) };
        assert!(json.contains("request document must be valid UTF-8"));
        assert!(!json.contains("password"));
        // SAFETY: Transfer the live handle exactly once.
        unsafe { sdsync_result_free_v1(result) };
        // SAFETY: Null frees are explicitly supported.
        unsafe { sdsync_result_free_v1(ptr::null_mut()) };
    }

    #[test]
    fn null_arguments_fail_without_dereferencing() {
        let mut result = ptr::null_mut();
        // SAFETY: Null inputs are intentionally exercised and must be rejected.
        assert_eq!(
            unsafe { sdsync_run_v1(ptr::null(), 1, ptr::null(), ptr::null(), &mut result) },
            SDSYNC_STATUS_INVALID_ARGUMENT
        );
        assert!(!result.is_null());
        // SAFETY: Free the result returned for the rejected call.
        unsafe { sdsync_result_free_v1(result) };
        // SAFETY: A null out-result must be rejected before other pointers.
        assert_eq!(
            unsafe { sdsync_run_v1(ptr::null(), 0, ptr::null(), ptr::null(), ptr::null_mut()) },
            SDSYNC_STATUS_INVALID_ARGUMENT
        );
    }

    #[test]
    fn pre_cancelled_request_never_needs_network_or_secrets() {
        let request = br#"{
            "schema":"sdsync.request.v1",
            "endpoint":"https://files.example.invalid",
            "username":"user",
            "source":".",
            "remote":"/home/Drive/backup"
        }"#;
        let mut cancellation = ptr::null_mut();
        // SAFETY: Valid local out pointer.
        assert_eq!(
            unsafe { sdsync_cancellation_new_v1(&mut cancellation) },
            SDSYNC_STATUS_OK
        );
        // SAFETY: The handle is live.
        assert_eq!(
            unsafe { sdsync_cancellation_cancel_v1(cancellation) },
            SDSYNC_STATUS_OK
        );
        let mut result = ptr::null_mut();
        // SAFETY: Inputs and handle remain live through the call.
        let status = unsafe {
            sdsync_run_v1(
                request.as_ptr(),
                request.len() as u64,
                ptr::null(),
                cancellation,
                &mut result,
            )
        };
        assert_eq!(status, SDSYNC_STATUS_CANCELLED);
        // SAFETY: Result is live until freed.
        let json = unsafe { result_json(result) };
        assert!(json.contains("\"code\":\"cancelled\""));
        // SAFETY: Each live handle is freed exactly once after the run ends.
        unsafe {
            sdsync_result_free_v1(result);
            sdsync_cancellation_free_v1(cancellation);
        }
    }

    #[test]
    fn production_job_and_retry_limits_are_invalid_ffi_requests() {
        for (field, expected_message) in [
            (r#""jobs":17"#, "jobs must be between 1 and 16"),
            (r#""retries":6"#, "retries must be between 0 and 5"),
        ] {
            let request = format!(
                r#"{{
                    "schema":"sdsync.request.v1",
                    "endpoint":"https://files.example.invalid",
                    "username":"user",
                    "source":".",
                    "remote":"/home/Drive/backup",
                    {field}
                }}"#
            );
            let mut result = ptr::null_mut();
            // SAFETY: The UTF-8 request and out pointer remain valid for this call.
            let status = unsafe {
                sdsync_run_v1(
                    request.as_ptr(),
                    request.len() as u64,
                    ptr::null(),
                    ptr::null(),
                    &mut result,
                )
            };
            assert_eq!(status, SDSYNC_STATUS_INVALID_ARGUMENT);
            assert!(!result.is_null());
            // SAFETY: `result` is live and uniquely owned by this iteration.
            let json = unsafe { result_json(result) };
            assert!(json.contains(r#""code":"invalid-argument""#));
            assert!(json.contains(expected_message));
            // SAFETY: Transfer the live handle exactly once.
            unsafe { sdsync_result_free_v1(result) };
        }
    }

    unsafe extern "C" fn secret_callback(
        user_data: *mut c_void,
        _kind: u32,
        buffer: *mut u8,
        capacity: u64,
        written: *mut u64,
    ) -> i32 {
        let secret = b"ffi-super-secret";
        if buffer.is_null() {
            // SAFETY: The callback contract gives a valid written pointer.
            unsafe { *written = secret.len() as u64 };
            return SDSYNC_CALLBACK_OK;
        }
        if capacity < secret.len() as u64 {
            return SDSYNC_CALLBACK_UNAVAILABLE;
        }
        // SAFETY: The caller provided writable capacity and a valid out pointer.
        unsafe {
            ptr::copy_nonoverlapping(secret.as_ptr(), buffer, secret.len());
            *written = secret.len() as u64;
            *(user_data.cast::<u32>()) += 1;
        }
        SDSYNC_CALLBACK_OK
    }

    #[test]
    fn secret_callback_uses_two_pass_owned_zeroizing_buffer() {
        let mut writes = 0_u32;
        let callbacks = Callbacks {
            user_data: (&mut writes as *mut u32).cast(),
            secret: Some(secret_callback),
            plan: None,
            event: None,
        };
        let callback_failure = Rc::new(RefCell::new(None));
        let secrets = CallbackSecrets {
            callbacks,
            callback_failure: Rc::clone(&callback_failure),
        };
        let value = secrets
            .read(SDSYNC_SECRET_PASSWORD)
            .expect("callback succeeds")
            .expect("secret exists");
        assert_eq!(writes, 1);
        assert!(callback_failure.borrow().is_none());
        assert_eq!(format!("{value:?}"), "Secret([REDACTED])");
        assert!(!format!("{value:?}").contains("ffi-super-secret"));
    }

    #[test]
    fn too_small_callback_table_is_rejected() {
        let table = SdsyncCallbacksV1 {
            struct_size: (mem::size_of::<SdsyncCallbacksV1>() - 1) as u32,
            reserved: 0,
            user_data: ptr::null_mut(),
            secret: None,
            plan: None,
            event: None,
        };
        // SAFETY: The table itself is readable; only its advertised size is bad.
        let error = unsafe { copy_callbacks(&table) }.expect_err("small table");
        assert_eq!(error.status, SDSYNC_STATUS_INVALID_ARGUMENT);
    }

    #[test]
    fn panic_guard_maps_to_a_stable_failure_document() {
        struct SensitivePanic(&'static str);
        let (status, bytes) = contain_run(|| {
            let payload = SensitivePanic("sensitive panic detail");
            assert_eq!(payload.0, "sensitive panic detail");
            std::panic::panic_any(payload);
        });
        let json = String::from_utf8(bytes).expect("UTF-8 JSON");
        assert_eq!(status, SDSYNC_STATUS_PANIC);
        assert!(json.contains("caught an internal panic"));
        assert!(!json.contains("sensitive panic detail"));
    }
}
