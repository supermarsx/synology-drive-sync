//! Security boundary for the DSM dashboard CGI and its private controller queue.
//!
//! This module intentionally belongs only to the dedicated `sdsync-dsm-api`
//! binary.  It is not exported by the library and is never selected through
//! the main CLI's `argv[0]` or command dispatch.

#![cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        unused_imports,
        unused_mut,
        unused_variables,
        unreachable_code
    )
)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(target_os = "linux")]
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
#[cfg(target_os = "linux")]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
#[cfg(target_os = "linux")]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

#[cfg(target_os = "linux")]
use synology_drive_sync::Error as SyncError;
#[cfg(target_os = "linux")]
use synology_drive_sync::api::{ApiClient, ClientOptions};
use synology_drive_sync::vault::{generate_totp, parse_totp_secret};

const PACKAGE_ROOT: &str = "/var/packages/synology-drive-sync/target";
const PACKAGE_HOME: &str = "/var/packages/synology-drive-sync/home";
const PACKAGE_VAR: &str = "/var/packages/synology-drive-sync/var";
const MANAGER_PATH: &str = "/var/packages/synology-drive-sync/target/bin/sdsync-dsm";
const BINARY_PATH: &str = "/var/packages/synology-drive-sync/target/bin/synology-drive-sync";
const CONTROLLER_PATH: &str = "/var/packages/synology-drive-sync/target/libexec/sdsync-controller";
const AUTHENTICATE_PATH: &str = "/usr/syno/synoman/webman/modules/authenticate.cgi";
const DSM_USER_SERVICE_PATH: &str = "/webapi/entry.cgi";
const DSM_USER_SERVICE_API: &str = "SYNO.Core.Desktop.Initdata";
const CONTROL_ROOT: &str = "/var/packages/synology-drive-sync/var/control";
const REQUESTS_DIR: &str = "/var/packages/synology-drive-sync/var/control/requests";
const PROCESSING_DIR: &str = "/var/packages/synology-drive-sync/var/control/processing";
const RESPONSES_DIR: &str = "/var/packages/synology-drive-sync/var/control/responses";
const STAGING_DIR: &str = "/var/packages/synology-drive-sync/var/control/staging";
const CSRF_KEY_PATH: &str = "/var/packages/synology-drive-sync/var/control/csrf.key";
const SECURITY_POLICY_PATH: &str = "/var/packages/synology-drive-sync/home/config/security.conf";
const PROFILE_SECRET_ROOT: &str = "/var/packages/synology-drive-sync/home/secrets";
const ENQUEUE_LOCK_PATH: &str = "/var/packages/synology-drive-sync/var/control/enqueue.lock";
const ENQUEUE_SEQUENCE_PATH: &str =
    "/var/packages/synology-drive-sync/var/control/enqueue.sequence";
const AUDIT_OUTBOX_DIR: &str = "/var/packages/synology-drive-sync/var/state/audit-outbox";
const AUDIT_OUTBOX_LOCK_PATH: &str = "/var/packages/synology-drive-sync/var/run/audit-outbox.flock";
const LOG_ROOT: &str = "/var/packages/synology-drive-sync/var/log";
const AUDIT_LOG_PATH: &str = "/var/packages/synology-drive-sync/var/log/audit.log";
const ACTIVITY_LOG_PATH: &str = "/var/packages/synology-drive-sync/var/log/activity.log";
const API_LOG_PATH: &str = "/var/packages/synology-drive-sync/var/log/api.log";
const CGI_FAILURE_STATE_PATH: &str = "/var/packages/synology-drive-sync/var/run/cgi-failure.state";
const API_SOCKET_PATH: &str = "/var/packages/synology-drive-sync/var/run/api.sock";
const API_PID_PATH: &str = "/var/packages/synology-drive-sync/var/run/api.pid";
const API_BOUND_PATH: &str = "/var/packages/synology-drive-sync/var/run/api.bound";
const API_READY_PATH: &str = "/var/packages/synology-drive-sync/var/run/api.ready";
const CONTROLLER_START_PATH: &str = "/var/packages/synology-drive-sync/var/run/controller.starting";
const PACKAGE_TRANSITION_PATH: &str =
    "/var/packages/synology-drive-sync/var/run/package.transition";
const SERVICE_CLOSED_PATH: &str = "/var/packages/synology-drive-sync/var/run/service.closed";
const FAILED_API_CHILD_PATH: &str = "/var/packages/synology-drive-sync/var/run/failed-start.api";
const FAILED_CONTROLLER_CHILD_PATH: &str =
    "/var/packages/synology-drive-sync/var/run/failed-start.controller";
const ADMINISTRATORS_GROUP: &str = "administrators";
const RELAY_SCHEMA: &str = "sdsync.dsm-relay.v1";
const CGI_ORIGIN_VARIABLES: &[&str] = &[
    "REQUEST_METHOD",
    "GATEWAY_INTERFACE",
    "QUERY_STRING",
    "CONTENT_LENGTH",
    "CONTENT_TYPE",
    "HTTP_COOKIE",
    "HTTP_X_SDSYNC_REQUEST",
    "HTTP_X_SYNO_TOKEN",
    "HTTP_X_SDSYNC_CSRF",
    "HTTP_TRANSFER_ENCODING",
    "HTTP_HOST",
    "REMOTE_ADDR",
    "REMOTE_PORT",
    "REQUEST_SCHEME",
    "SERVER_ADDR",
    "SERVER_NAME",
    "SERVER_PORT",
    "SERVER_PROTOCOL",
    "HTTPS",
    "SCRIPT_NAME",
    "SCRIPT_FILENAME",
    "DOCUMENT_ROOT",
    "SCGI",
    "SOCKET",
];
const CORE_CLI_ENVIRONMENT_VARIABLES: &[&str] = &[
    "SDSYNC_CONFIG",
    "SDSYNC_PROFILE",
    "SDSYNC_MAX_TOTAL_DELETE",
    "SDSYNC_URL",
    "SDSYNC_USERNAME",
    "SDSYNC_PASSWORD",
    "SDSYNC_OTP",
    "SDSYNC_REMOTE_LOG_TOKEN",
    "SDSYNC_PASSWORD_STDIN",
    "SDSYNC_PASSWORD_FILE",
    "SDSYNC_TOTP_SECRET_FILE",
    "SDSYNC_NO_VAULT",
    "SDSYNC_COMPARE",
    "SDSYNC_JOBS",
    "SDSYNC_DELETE",
    "SDSYNC_ALLOW_EMPTY_SOURCE",
    "SDSYNC_MAX_DELETE",
    "SDSYNC_RETRIES",
    "SDSYNC_TIMEOUT",
    "SDSYNC_MAX_RATE",
    "SDSYNC_CONNECT_TIMEOUT",
    "SDSYNC_CA_CERTIFICATE",
    "SDSYNC_ALLOW_HTTP",
    "SDSYNC_DANGER_ACCEPT_INVALID_CERTS",
    "SDSYNC_QUIET",
    "SDSYNC_LOG_LEVEL",
    "SDSYNC_LOG_FORMAT",
    "SDSYNC_LOG_FILE",
    "SDSYNC_REMOTE_LOG_URL",
    "SDSYNC_REMOTE_LOG_TOKEN_FILE",
    "SDSYNC_REMOTE_LOG_TOKEN_ENV",
    "SDSYNC_REMOTE_LOG_MODE",
    "SDSYNC_PROGRESS",
    "SDSYNC_OUTPUT",
    "SDSYNC_REMOTE",
];

const MAX_QUERY_BYTES: usize = 4 * 1024;
const MAX_COOKIE_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 1024;
const MAX_CSRF_BYTES: usize = 256;
const MAX_POST_BODY_BYTES: usize = 64 * 1024;
const MAX_JOB_BYTES: usize = 64 * 1024;
const MAX_AUDIT_OUTBOX_BYTES: usize = 4 * 1024;
const MAX_MANAGER_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_AUTHENTICATED_USERNAME_BYTES: usize = 256;
const MAX_AUTH_OUTPUT_BYTES: usize = MAX_AUTHENTICATED_USERNAME_BYTES + 2;
const MAX_DSM_USER_SERVICE_OUTPUT_BYTES: usize = MAX_MANAGER_OUTPUT_BYTES;
const MAX_SECRET_BYTES: usize = 4096;
const MAX_CONNECTION_SECRET_BYTES: usize = (MAX_SECRET_BYTES * 2) + 32;
const CONNECTION_PROOF_LIFETIME_SECONDS: u64 = 5 * 60;
const MAX_DSM_DELETE_BOUND: u64 = 2_147_483_647;
const MAX_JOB_AGE_SECONDS: u64 = 24 * 60 * 60;
const RESULT_RETENTION_SECONDS: u64 = 60 * 60;
const MAX_OUTSTANDING_JOBS: usize = 256;
const CSRF_LIFETIME_SECONDS: u64 = 5 * 60;
const CLOCK_SKEW_SECONDS: u64 = 30;
const SERVER_JOB_ID_BYTES: usize = 24;
const MAX_RELAY_REQUEST_BYTES: usize = 256 * 1024;
const MAX_RELAY_RESPONSE_BYTES: usize = MAX_MANAGER_OUTPUT_BYTES + 2;
#[cfg(target_os = "linux")]
const RELAY_IO_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "linux")]
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const CGI_SERVICE_CONNECT_WINDOW: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const CGI_SERVICE_RETRY_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(target_os = "linux")]
const AUTH_HELPER_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const READ_MANAGER_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(target_os = "linux")]
const CONTROLLER_WAKE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const INTERACTIVE_AUTH_TEST_PROBE_TIMEOUT: Duration = Duration::from_secs(12);
#[cfg(target_os = "linux")]
const INTERACTIVE_AUTH_TEST_LOGOUT_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(target_os = "linux")]
const INTERACTIVE_REMOTE_BROWSE_PROBE_TIMEOUT: Duration = Duration::from_secs(27);
#[cfg(target_os = "linux")]
const INTERACTIVE_REMOTE_BROWSE_LOGOUT_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(target_os = "linux")]
const MAX_HELPER_STDERR_BYTES: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const API_WORKER_COUNT: usize = 4;
#[cfg(target_os = "linux")]
const API_QUEUE_CAPACITY: usize = 16;

type HmacSha256 = Hmac<Sha256>;
type BridgeResult<T> = Result<T, BridgeError>;
type DecodedConnectionSecrets = (Option<Zeroizing<Vec<u8>>>, Option<Zeroizing<Vec<u8>>>);
type ResolvedConnectionSecrets = (Zeroizing<Vec<u8>>, Option<Zeroizing<Vec<u8>>>);

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct ControlPaths<'a> {
    root: &'a Path,
    requests: &'a Path,
    processing: &'a Path,
    responses: &'a Path,
    staging: &'a Path,
    csrf_key: &'a Path,
    enqueue_lock: &'a Path,
    enqueue_sequence: &'a Path,
    audit_outbox_directory: &'a Path,
    audit_outbox_lock: &'a Path,
    package_transition: &'a Path,
    service_closed: &'a Path,
}

#[cfg(target_os = "linux")]
struct EnqueueRequest<'a> {
    package_uid: u32,
    client_request_id: &'a str,
    requested_by: &'a str,
    requested_uid: u32,
    session_binding: &'a [u8; 32],
    audit_transaction: &'a str,
    request_fingerprint: &'a str,
    issued_at_epoch: u64,
    mutation: &'a Mutation,
    secret: Option<&'a [u8]>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct AuditOutboxPaths<'a> {
    directory: &'a Path,
    lock: &'a Path,
    requests: Option<&'a Path>,
    processing: Option<&'a Path>,
    responses: Option<&'a Path>,
}

#[cfg(target_os = "linux")]
impl AuditOutboxPaths<'static> {
    fn production() -> Self {
        Self {
            directory: Path::new(AUDIT_OUTBOX_DIR),
            lock: Path::new(AUDIT_OUTBOX_LOCK_PATH),
            requests: Some(Path::new(REQUESTS_DIR)),
            processing: Some(Path::new(PROCESSING_DIR)),
            responses: Some(Path::new(RESPONSES_DIR)),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuditOutboxPhase {
    Prepared,
    Publishing,
    Queued,
    Executing,
    Succeeded,
    Failed,
    OutcomeUnknown,
}

impl AuditOutboxPhase {
    fn terminal_state(self) -> Option<&'static str> {
        match self {
            Self::Succeeded => Some("succeeded"),
            Self::Failed => Some("failed"),
            Self::OutcomeUnknown => Some("outcome_unknown"),
            Self::Prepared | Self::Publishing | Self::Queued | Self::Executing => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditOutboxRecord {
    schema: String,
    transaction: String,
    operation: String,
    profile: String,
    actor: String,
    actor_uid: u32,
    origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_request_id: Option<String>,
    job_id: Option<String>,
    owner_pid: u32,
    owner_start: u64,
    owner_boot: String,
    phase: AuditOutboxPhase,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditLogRecord<'a> {
    epoch: u64,
    level: &'a str,
    configured_level: &'a str,
    subject_level: &'a str,
    mandatory: bool,
    category: &'a str,
    subject_category: &'a str,
    operation: &'a str,
    state: &'a str,
    transaction: &'a str,
    origin: &'a str,
    actor: &'a str,
    actor_uid: Option<u32>,
    profile: &'a str,
    #[serde(default)]
    client_request_id: Option<&'a str>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Eq, PartialEq)]
enum EnqueueOutcome {
    Existing(String),
    Published {
        job_id: String,
        durability_uncertain: bool,
    },
}

#[cfg(target_os = "linux")]
#[derive(Debug, Eq, PartialEq)]
enum SessionRequestStatus {
    Pending { job_id: String, operation: String },
    Complete { job_id: String, operation: String },
}

#[cfg(target_os = "linux")]
impl EnqueueOutcome {
    fn job_id(&self) -> &str {
        match self {
            Self::Existing(job_id) | Self::Published { job_id, .. } => job_id,
        }
    }

    fn response_state(&self) -> &'static str {
        "queued"
    }

    fn replayed(&self) -> bool {
        matches!(self, Self::Existing(_))
    }

    fn durability_warning(&self) -> bool {
        matches!(
            self,
            Self::Published {
                durability_uncertain: true,
                ..
            }
        )
    }

    fn should_wake_controller(&self) -> bool {
        // An Existing result can be an exact replay after the original 202
        // acknowledgement or advisory wake was lost. USR2 is idempotent and
        // does not alter queue state, so replay assists pending work too; an
        // already-complete job only causes a harmless extra controller turn.
        matches!(self, Self::Existing(_) | Self::Published { .. })
    }
}

#[cfg(target_os = "linux")]
impl ControlPaths<'static> {
    fn production() -> Self {
        Self {
            root: Path::new(CONTROL_ROOT),
            requests: Path::new(REQUESTS_DIR),
            processing: Path::new(PROCESSING_DIR),
            responses: Path::new(RESPONSES_DIR),
            staging: Path::new(STAGING_DIR),
            csrf_key: Path::new(CSRF_KEY_PATH),
            enqueue_lock: Path::new(ENQUEUE_LOCK_PATH),
            enqueue_sequence: Path::new(ENQUEUE_SEQUENCE_PATH),
            audit_outbox_directory: Path::new(AUDIT_OUTBOX_DIR),
            audit_outbox_lock: Path::new(AUDIT_OUTBOX_LOCK_PATH),
            package_transition: Path::new(PACKAGE_TRANSITION_PATH),
            service_closed: Path::new(SERVICE_CLOSED_PATH),
        }
    }
}

#[cfg(target_os = "linux")]
impl ControlPaths<'_> {
    fn audit_outbox(&self) -> AuditOutboxPaths<'_> {
        AuditOutboxPaths {
            directory: self.audit_outbox_directory,
            lock: self.audit_outbox_lock,
            requests: Some(self.requests),
            processing: Some(self.processing),
            responses: Some(self.responses),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    BadRequest,
    Unauthorized,
    Forbidden,
    CsrfRejected,
    MethodNotAllowed,
    UnsupportedMediaType,
    PayloadTooLarge,
    Conflict,
    UnsafeRuntime,
    Unavailable,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BridgeError {
    kind: ErrorKind,
}

impl BridgeError {
    const fn new(kind: ErrorKind) -> Self {
        Self { kind }
    }

    const fn bad_request() -> Self {
        Self::new(ErrorKind::BadRequest)
    }

    const fn unsafe_runtime() -> Self {
        Self::new(ErrorKind::UnsafeRuntime)
    }

    const fn internal() -> Self {
        Self::new(ErrorKind::Internal)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CgiFailureStage {
    Request,
    Identity,
    Authentication,
    Runtime,
    BridgeConnect,
    BridgeIo,
    BridgeProtocol,
    ServiceRequest,
}

impl CgiFailureStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Identity => "cgi_identity",
            Self::Authentication => "dsm_authentication",
            Self::Runtime => "cgi_runtime",
            Self::BridgeConnect => "bridge_connect",
            Self::BridgeIo => "bridge_io",
            Self::BridgeProtocol => "bridge_protocol",
            Self::ServiceRequest => "service_request",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CgiFailure {
    error: BridgeError,
    stage: CgiFailureStage,
    code: Option<&'static str>,
}

impl CgiFailure {
    const fn new(stage: CgiFailureStage, error: BridgeError) -> Self {
        Self {
            error,
            stage,
            code: None,
        }
    }

    const fn coded(stage: CgiFailureStage, error: BridgeError, code: &'static str) -> Self {
        Self {
            error,
            stage,
            code: Some(code),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IdentityState {
    real_uid: u32,
    effective_uid: u32,
    executable_uid: u32,
    executable_mode: u32,
}

#[derive(Clone)]
struct CgiEnvironment {
    method: String,
    content_length: Option<String>,
    content_type: Option<String>,
    query: Zeroizing<String>,
    cookie: Zeroizing<String>,
    request_marker: Option<String>,
    synology_token_header: Option<Zeroizing<String>>,
    csrf_header: Option<Zeroizing<String>>,
    remote_address: Option<String>,
    server_address: Option<String>,
    server_name: Option<String>,
    server_port: Option<String>,
    https: Option<String>,
    transfer_encoding: Option<String>,
    native_authentication_context: NativeAuthenticationContext,
}

#[derive(Clone, Default)]
struct NativeAuthenticationContext {
    gateway_interface: Option<String>,
    http_host: Option<String>,
    remote_port: Option<String>,
    request_scheme: Option<String>,
    server_protocol: Option<String>,
    script_name: Option<String>,
    script_filename: Option<String>,
    document_root: Option<String>,
    scgi: Option<String>,
    socket: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReadAction {
    Csrf,
    Snapshot,
    SourceDirectories { parent: String },
    SourcePath { path: String },
    Logs { lines: u16, source: LogSource },
    Activity { lines: u16 },
    Result { job_id: String },
    RequestStatus { request_id: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogSource {
    All,
    Api,
    Controller,
    Scheduler,
    Sync,
    Audit,
}

impl LogSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Api => "api",
            Self::Controller => "controller",
            Self::Scheduler => "scheduler",
            Self::Sync => "sync",
            Self::Audit => "audit",
        }
    }
}

enum ValidatedHttpRequest {
    Get {
        action: ReadAction,
        authentication: AuthenticationInputs,
    },
    Post {
        content_length: usize,
        csrf_token: Zeroizing<String>,
        authentication: AuthenticationInputs,
    },
}

struct AuthenticationInputs {
    cookie: Zeroizing<String>,
    synology_token: Option<Zeroizing<String>>,
    remote_address: Option<String>,
    server_address: Option<String>,
    server_name: Option<String>,
    server_port: Option<String>,
    https: Option<String>,
    native_context: NativeAuthenticationContext,
}

#[derive(Serialize)]
struct RelayRequestRef<'a> {
    schema: &'static str,
    method: &'a str,
    content_length: Option<&'a str>,
    content_type: Option<&'a str>,
    query: &'a str,
    cookie: &'a str,
    request_marker: Option<&'a str>,
    synology_token_header: Option<&'a str>,
    csrf_header: Option<&'a str>,
    remote_address: Option<&'a str>,
    server_address: Option<&'a str>,
    server_name: Option<&'a str>,
    server_port: Option<&'a str>,
    https: Option<&'a str>,
    transfer_encoding: Option<&'a str>,
    authenticated_username: &'a str,
    authenticated_uid: u32,
    session_binding: &'a str,
    body: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayRequest {
    schema: String,
    method: String,
    content_length: Option<String>,
    content_type: Option<String>,
    query: String,
    cookie: String,
    request_marker: Option<String>,
    synology_token_header: Option<String>,
    csrf_header: Option<String>,
    remote_address: Option<String>,
    server_address: Option<String>,
    server_name: Option<String>,
    server_port: Option<String>,
    https: Option<String>,
    transfer_encoding: Option<String>,
    authenticated_username: String,
    authenticated_uid: u32,
    session_binding: String,
    body: Option<String>,
}

impl RelayRequest {
    fn environment(&self) -> CgiEnvironment {
        CgiEnvironment {
            method: self.method.clone(),
            content_length: self.content_length.clone(),
            content_type: self.content_type.clone(),
            query: Zeroizing::new(self.query.clone()),
            cookie: Zeroizing::new(self.cookie.clone()),
            request_marker: self.request_marker.clone(),
            synology_token_header: self
                .synology_token_header
                .as_ref()
                .map(|value| Zeroizing::new(value.clone())),
            csrf_header: self
                .csrf_header
                .as_ref()
                .map(|value| Zeroizing::new(value.clone())),
            remote_address: self.remote_address.clone(),
            server_address: self.server_address.clone(),
            server_name: self.server_name.clone(),
            server_port: self.server_port.clone(),
            https: self.https.clone(),
            transfer_encoding: self.transfer_encoding.clone(),
            // DSM-native authentication context is needed only by the
            // pre-relay authenticate.cgi invocation. It is deliberately not
            // serialized across the package-private relay after authentication.
            native_authentication_context: NativeAuthenticationContext::default(),
        }
    }

    fn validate_fields(&self) -> BridgeResult<()> {
        validate_environment_value(&self.method, 16)?;
        validate_optional_environment_value(self.content_length.as_deref(), 32)?;
        validate_optional_environment_value(self.content_type.as_deref(), 128)?;
        validate_environment_value(&self.query, MAX_QUERY_BYTES)?;
        validate_environment_value(&self.cookie, MAX_COOKIE_BYTES)?;
        validate_optional_environment_value(self.request_marker.as_deref(), 8)?;
        validate_optional_environment_value(
            self.synology_token_header.as_deref(),
            MAX_TOKEN_BYTES,
        )?;
        validate_optional_environment_value(self.csrf_header.as_deref(), MAX_CSRF_BYTES)?;
        validate_optional_environment_value(self.remote_address.as_deref(), 128)?;
        validate_optional_environment_value(self.server_address.as_deref(), 128)?;
        validate_optional_environment_value(self.server_name.as_deref(), 255)?;
        validate_optional_environment_value(self.server_port.as_deref(), 8)?;
        validate_optional_environment_value(self.https.as_deref(), 16)?;
        validate_optional_environment_value(self.transfer_encoding.as_deref(), 64)?;
        if !valid_authenticated_username(&self.authenticated_username)
            || self.authenticated_uid == 0
            || hex_decode_exact::<32>(&self.session_binding).is_none()
        {
            return Err(BridgeError::bad_request());
        }
        if self
            .body
            .as_ref()
            .is_some_and(|body| body.len() > MAX_POST_BODY_BYTES)
        {
            return Err(BridgeError::new(ErrorKind::PayloadTooLarge));
        }
        Ok(())
    }
}

impl Drop for RelayRequest {
    fn drop(&mut self) {
        self.schema.zeroize();
        self.method.zeroize();
        self.content_length.zeroize();
        self.content_type.zeroize();
        self.query.zeroize();
        self.cookie.zeroize();
        self.request_marker.zeroize();
        self.synology_token_header.zeroize();
        self.csrf_header.zeroize();
        self.remote_address.zeroize();
        self.server_address.zeroize();
        self.server_name.zeroize();
        self.server_port.zeroize();
        self.https.zeroize();
        self.transfer_encoding.zeroize();
        self.authenticated_username.zeroize();
        self.session_binding.zeroize();
        self.body.zeroize();
    }
}

impl fmt::Debug for RelayRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RelayRequest")
            .field("schema", &self.schema)
            .field("method", &self.method)
            .field("content_length", &self.content_length)
            .field("query", &"[redacted]")
            .field("cookie", &"[redacted]")
            .field("synology_token_header", &"[redacted]")
            .field("csrf_header", &"[redacted]")
            .field("body", &"[redacted]")
            .finish()
    }
}

fn encode_relay_request(
    environment: &CgiEnvironment,
    body: Option<&[u8]>,
    session: &AuthenticatedSession,
) -> BridgeResult<Zeroizing<Vec<u8>>> {
    let body = body
        .map(|bytes| std::str::from_utf8(bytes).map_err(|_| BridgeError::bad_request()))
        .transpose()?;
    let session_binding = hex_encode(&session.binding);
    let request = RelayRequestRef {
        schema: RELAY_SCHEMA,
        method: &environment.method,
        content_length: environment.content_length.as_deref(),
        content_type: environment.content_type.as_deref(),
        query: &environment.query,
        cookie: &environment.cookie,
        request_marker: environment.request_marker.as_deref(),
        synology_token_header: environment
            .synology_token_header
            .as_ref()
            .map(|value| value.as_str()),
        csrf_header: environment.csrf_header.as_ref().map(|value| value.as_str()),
        remote_address: environment.remote_address.as_deref(),
        server_address: environment.server_address.as_deref(),
        server_name: environment.server_name.as_deref(),
        server_port: environment.server_port.as_deref(),
        https: environment.https.as_deref(),
        transfer_encoding: environment.transfer_encoding.as_deref(),
        authenticated_username: &session.username,
        authenticated_uid: session.uid,
        session_binding: &session_binding,
        body,
    };
    let encoded = serde_json::to_vec(&request).map_err(|_| BridgeError::internal())?;
    if encoded.is_empty() || encoded.len() > MAX_RELAY_REQUEST_BYTES {
        return Err(BridgeError::new(ErrorKind::PayloadTooLarge));
    }
    Ok(Zeroizing::new(encoded))
}

fn decode_relay_request(encoded: &[u8]) -> BridgeResult<RelayRequest> {
    if encoded.is_empty() || encoded.len() > MAX_RELAY_REQUEST_BYTES {
        return Err(BridgeError::new(ErrorKind::PayloadTooLarge));
    }
    let request =
        serde_json::from_slice::<RelayRequest>(encoded).map_err(|_| BridgeError::bad_request())?;
    if request.schema != RELAY_SCHEMA {
        return Err(BridgeError::bad_request());
    }
    request.validate_fields()?;
    Ok(request)
}

fn validate_relay_http_request(
    relay: &RelayRequest,
) -> BridgeResult<(ValidatedHttpRequest, Option<&[u8]>)> {
    let request = validate_http_request(relay.environment())?;
    let body = match (&request, relay.body.as_deref()) {
        (ValidatedHttpRequest::Get { .. }, None) => None,
        (ValidatedHttpRequest::Post { content_length, .. }, Some(body))
            if body.len() == *content_length =>
        {
            Some(body.as_bytes())
        }
        _ => return Err(BridgeError::bad_request()),
    };
    Ok((request, body))
}

fn validate_relay_authenticated_session(
    relay: &RelayRequest,
    authentication: &AuthenticationInputs,
    independently_resolved_uid: u32,
) -> BridgeResult<AuthenticatedSession> {
    let binding = hex_decode_exact::<32>(&relay.session_binding)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unauthorized))?;
    if independently_resolved_uid == 0 || relay.authenticated_uid != independently_resolved_uid {
        return Err(BridgeError::new(ErrorKind::Unauthorized));
    }
    let expected_binding = session_binding(
        &relay.authenticated_username,
        independently_resolved_uid,
        &authentication.cookie,
        authentication
            .synology_token
            .as_ref()
            .map(|value| value.as_str()),
    )?;
    if !session_binding_matches(&binding, &expected_binding) {
        return Err(BridgeError::new(ErrorKind::Unauthorized));
    }
    Ok(AuthenticatedSession {
        username: relay.authenticated_username.clone(),
        uid: independently_resolved_uid,
        binding: expected_binding,
    })
}

struct AuthenticatedSession {
    username: String,
    uid: u32,
    binding: [u8; 32],
}

impl Drop for AuthenticatedSession {
    fn drop(&mut self) {
        self.binding.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMutationRequest<'a> {
    schema: &'a str,
    request_id: &'a str,
    operation: &'a str,
    #[serde(borrow)]
    arguments: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob<'a> {
    schema: &'a str,
    request_id: &'a str,
    client_request_id: &'a str,
    requested_by: &'a str,
    requested_uid: u32,
    session_binding: &'a str,
    audit_transaction: &'a str,
    request_fingerprint: &'a str,
    issued_at_epoch: u64,
    operation: &'a str,
    #[serde(borrow)]
    arguments: &'a RawValue,
}

struct ParsedJob {
    request_id: String,
    client_request_id: String,
    requested_by: String,
    requested_uid: u32,
    session_binding: [u8; 32],
    audit_transaction: String,
    request_fingerprint: String,
    issued_at_epoch: u64,
    mutation: Mutation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueuedJobClass {
    Connection,
    Concurrent,
    Serialized,
}

impl QueuedJobClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Concurrent => "concurrent",
            Self::Serialized => "serialized",
        }
    }
}

impl Drop for ParsedJob {
    fn drop(&mut self) {
        self.session_binding.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQueuedResponse<'a> {
    schema: &'a str,
    job_id: &'a str,
    operation: Option<&'a str>,
    client_request_id: &'a str,
    requested_by: &'a str,
    requested_uid: u32,
    session_binding: &'a str,
    request_fingerprint: &'a str,
    audit_transaction: &'a str,
    audit_pending: bool,
    audit_terminal_state: &'a str,
    issued_at_epoch: u64,
    completed_at_epoch: u64,
    #[serde(borrow)]
    result: &'a RawValue,
}

struct ParsedQueuedResponse {
    operation: Option<String>,
    client_request_id: String,
    requested_by: String,
    requested_uid: u32,
    session_binding: [u8; 32],
    request_fingerprint: String,
    audit_transaction: String,
    audit_pending: bool,
    audit_terminal_state: String,
    completed_at_epoch: u64,
    result: Value,
}

impl Drop for ParsedQueuedResponse {
    fn drop(&mut self) {
        self.session_binding.zeroize();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigureProfileArgs {
    name: String,
    source: String,
    url: String,
    username: String,
    remote: String,
    compare: CompareMode,
    jobs: u8,
    delete: bool,
    max_delete: u64,
    allow_http: bool,
    allow_empty_source: bool,
    excludes: Vec<String>,
    retries: u8,
    timeout_seconds: u32,
    connect_timeout_seconds: u32,
    max_rate_bytes_per_second: Option<u64>,
    ca_certificate: Option<String>,
    danger_accept_invalid_certs: bool,
    verbosity: u8,
    quiet: bool,
    log_level: LogLevel,
    log_format: LogFormat,
    progress: ProgressMode,
    output: OutputFormat,
    remote_log_url: Option<String>,
    remote_log_mode: RemoteLogMode,
    make_default: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CompareMode {
    Content,
    Metadata,
    SizeOnly,
}

impl CompareMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Metadata => "metadata",
            Self::SizeOnly => "size-only",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Off,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum LogFormat {
    Human,
    Json,
}

impl LogFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Json => "json",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum ProgressMode {
    Auto,
    Always,
    Never,
}

impl ProgressMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum OutputFormat {
    Human,
    Json,
    Ndjson,
}

impl OutputFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Json => "json",
            Self::Ndjson => "ndjson",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum RemoteLogMode {
    BestEffort,
    Required,
}

impl RemoteLogMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::BestEffort => "best-effort",
            Self::Required => "required",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretRequestArgs {
    profile: String,
    kind: SecretKind,
    mode: SecretMode,
    value: Option<SecretString>,
}

struct SecretString(Zeroizing<String>);

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|value| Self(Zeroizing::new(value)))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum CredentialSource {
    Stored,
    Provided,
    None,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectionRequestArgs {
    #[serde(default)]
    profile: Option<String>,
    url: String,
    username: String,
    allow_http: bool,
    danger_accept_invalid_certs: bool,
    #[serde(default)]
    ca_certificate: Option<String>,
    connect_timeout_seconds: u32,
    timeout_seconds: u32,
    retries: u8,
    password_source: CredentialSource,
    #[serde(default)]
    password: Option<SecretString>,
    totp_source: CredentialSource,
    #[serde(default)]
    totp: Option<SecretString>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConnectionJobArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    url: String,
    username: String,
    allow_http: bool,
    danger_accept_invalid_certs: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ca_certificate: Option<String>,
    connect_timeout_seconds: u32,
    timeout_seconds: u32,
    retries: u8,
    password_source: CredentialSource,
    totp_source: CredentialSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowseRemoteRequestArgs {
    #[serde(flatten)]
    connection: ConnectionRequestArgs,
    parent: String,
    connection_proof: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BrowseRemoteJobArgs {
    #[serde(flatten)]
    connection: ConnectionJobArgs,
    parent: String,
    connection_proof: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SecretJobArgs {
    profile: String,
    kind: SecretKind,
    mode: SecretMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum SecretKind {
    Password,
    Totp,
    RemoteLogToken,
}

impl SecretKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Totp => "totp",
            Self::RemoteLogToken => "remote-log-token",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum SecretMode {
    Replace,
    Clear,
}

impl SecretMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Clear => "clear",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScheduleArgs {
    enabled: bool,
    interval_seconds: u32,
    allow_delete: bool,
    max_total_delete: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RoutineArgs {
    profile: String,
    enabled: bool,
    action: RoutineAction,
    mode: RoutineMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interval_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    weekdays: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time_window_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time_window_end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    debounce_seconds: Option<u32>,
    retry_count: u8,
    retry_backoff_seconds: u32,
    retry_exponential: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    poll_seconds: Option<u32>,
    allow_delete: bool,
    max_total_delete: u64,
    depends_on: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RoutineAction {
    Plan,
    Sync,
}

impl RoutineAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Sync => "sync",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RoutineMode {
    Interval,
    Daily,
    Realtime,
}

impl RoutineMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Interval => "interval",
            Self::Daily => "daily",
            Self::Realtime => "realtime",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AlertPolicyArgs {
    enabled: bool,
    on_success: bool,
    on_failure: bool,
    failure_threshold: u8,
    cooldown_seconds: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum PolicyLogLevel {
    Off,
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl PolicyLogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    fn allows(self, event: Self) -> bool {
        fn rank(level: PolicyLogLevel) -> Option<u8> {
            match level {
                PolicyLogLevel::Off => None,
                PolicyLogLevel::Trace => Some(0),
                PolicyLogLevel::Debug => Some(1),
                PolicyLogLevel::Info => Some(2),
                PolicyLogLevel::Warn => Some(3),
                PolicyLogLevel::Error => Some(4),
            }
        }
        match (rank(self), rank(event)) {
            (Some(threshold), Some(severity)) => severity >= threshold,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct SecurityPolicyArgs {
    require_https: bool,
    allow_interface_changes: bool,
    allow_profile_changes: bool,
    allow_secret_changes: bool,
    allow_routine_changes: bool,
    allow_notification_changes: bool,
    allow_operational_actions: bool,
    allow_http_targets: bool,
    allow_invalid_tls: bool,
    allow_destructive_sync: bool,
    allow_doctor_write_test: bool,
    allow_remote_logging: bool,
    allow_empty_source: bool,
    csrf_lifetime_seconds: u64,
    result_retention_seconds: u64,
    max_outstanding_jobs: usize,
    audit_log_level: PolicyLogLevel,
    bridge_log_level: PolicyLogLevel,
    authentication_log_level: PolicyLogLevel,
    security_log_level: PolicyLogLevel,
    configuration_log_level: PolicyLogLevel,
    secrets_log_level: PolicyLogLevel,
    routines_log_level: PolicyLogLevel,
    operations_log_level: PolicyLogLevel,
    notifications_log_level: PolicyLogLevel,
    sync_log_level: PolicyLogLevel,
    controller_log_level: PolicyLogLevel,
    scheduler_log_level: PolicyLogLevel,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum ClientEventKind {
    InterfaceSettings,
    SessionNotifications,
}

impl ClientEventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::InterfaceSettings => "interface-settings",
            Self::SessionNotifications => "session-notifications",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ClientEventArgs {
    event: ClientEventKind,
}

impl Default for SecurityPolicyArgs {
    fn default() -> Self {
        Self {
            require_https: false,
            allow_interface_changes: true,
            allow_profile_changes: true,
            allow_secret_changes: true,
            allow_routine_changes: true,
            allow_notification_changes: true,
            allow_operational_actions: true,
            allow_http_targets: true,
            allow_invalid_tls: true,
            allow_destructive_sync: true,
            allow_doctor_write_test: true,
            allow_remote_logging: true,
            allow_empty_source: true,
            csrf_lifetime_seconds: CSRF_LIFETIME_SECONDS,
            result_retention_seconds: RESULT_RETENTION_SECONDS,
            max_outstanding_jobs: MAX_OUTSTANDING_JOBS,
            audit_log_level: PolicyLogLevel::Info,
            bridge_log_level: PolicyLogLevel::Info,
            authentication_log_level: PolicyLogLevel::Warn,
            security_log_level: PolicyLogLevel::Warn,
            configuration_log_level: PolicyLogLevel::Info,
            secrets_log_level: PolicyLogLevel::Info,
            routines_log_level: PolicyLogLevel::Info,
            operations_log_level: PolicyLogLevel::Info,
            notifications_log_level: PolicyLogLevel::Warn,
            sync_log_level: PolicyLogLevel::Info,
            controller_log_level: PolicyLogLevel::Info,
            scheduler_log_level: PolicyLogLevel::Info,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OperationalActionArgs {
    kind: OperationalActionKind,
    scope: String,
    level: Option<OperationalDoctorLevel>,
    write_test: Option<bool>,
    allow_delete: Option<bool>,
    max_total_delete: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum OperationalDoctorLevel {
    Quick,
    Standard,
    Extensive,
}

impl OperationalDoctorLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Standard => "standard",
            Self::Extensive => "extensive",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum OperationalActionKind {
    Doctor,
    Plan,
    Run,
}

impl OperationalActionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Doctor => "doctor",
            Self::Plan => "plan",
            Self::Run => "run",
        }
    }
}

#[derive(Clone, Debug)]
enum Mutation {
    ConfigureProfile(ConfigureProfileArgs),
    RemoveProfile(NameArgs),
    SetDefault(NameArgs),
    SetSecret(SecretJobArgs),
    TestProfileAuth(ConnectionJobArgs),
    BrowseRemote(BrowseRemoteJobArgs),
    Schedule(ScheduleArgs),
    Routine(RoutineArgs),
    RemoveRoutine(NameArgs),
    AlertPolicy(AlertPolicyArgs),
    SecurityPolicy(SecurityPolicyArgs),
    ClientEvent(ClientEventArgs),
    Action(OperationalActionArgs),
}

struct ParsedMutation {
    request_id: String,
    mutation: Mutation,
    secret: Option<Zeroizing<Vec<u8>>>,
}

impl fmt::Debug for ValidatedHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Get { action, .. } => formatter
                .debug_struct("ValidatedHttpRequest::Get")
                .field("action", action)
                .field("authentication", &"[redacted]")
                .finish(),
            Self::Post { content_length, .. } => formatter
                .debug_struct("ValidatedHttpRequest::Post")
                .field("content_length", content_length)
                .field("csrf_token", &"[redacted]")
                .field("authentication", &"[redacted]")
                .finish(),
        }
    }
}

impl Mutation {
    fn operation_id(&self) -> &'static str {
        match self {
            Self::ConfigureProfile(_) => "configure-profile",
            Self::RemoveProfile(_) => "remove-profile",
            Self::SetDefault(_) => "set-default",
            Self::SetSecret(_) => "set-secret",
            Self::TestProfileAuth(_) => "test-profile-auth",
            Self::BrowseRemote(_) => "browse-remote",
            Self::Schedule(_) => "schedule",
            Self::Routine(_) => "routine",
            Self::RemoveRoutine(_) => "remove-routine",
            Self::AlertPolicy(_) => "alert-policy",
            Self::SecurityPolicy(_) => "security-policy",
            Self::ClientEvent(_) => "client-event",
            Self::Action(_) => "action",
        }
    }

    fn arguments_value(&self) -> BridgeResult<Value> {
        let result = match self {
            Self::ConfigureProfile(value) => serde_json::to_value(value),
            Self::RemoveProfile(value) | Self::SetDefault(value) | Self::RemoveRoutine(value) => {
                serde_json::to_value(value)
            }
            Self::SetSecret(value) => serde_json::to_value(value),
            Self::TestProfileAuth(value) => serde_json::to_value(value),
            Self::BrowseRemote(value) => serde_json::to_value(value),
            Self::Schedule(value) => serde_json::to_value(value),
            Self::Routine(value) => serde_json::to_value(value),
            Self::AlertPolicy(value) => serde_json::to_value(value),
            Self::SecurityPolicy(value) => serde_json::to_value(value),
            Self::ClientEvent(value) => serde_json::to_value(value),
            Self::Action(value) => serde_json::to_value(value),
        };
        result.map_err(|_| BridgeError::internal())
    }
}

fn process_environment() -> BridgeResult<CgiEnvironment> {
    fn required(name: &str, maximum: usize) -> BridgeResult<String> {
        let value = std::env::var(name).map_err(|_| BridgeError::bad_request())?;
        validate_environment_value(&value, maximum)?;
        Ok(value)
    }

    fn optional(name: &str, maximum: usize) -> BridgeResult<Option<String>> {
        match std::env::var(name) {
            Ok(value) => {
                validate_environment_value(&value, maximum)?;
                Ok(Some(value))
            }
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(BridgeError::bad_request()),
        }
    }

    Ok(CgiEnvironment {
        method: required("REQUEST_METHOD", 16)?,
        content_length: optional("CONTENT_LENGTH", 32)?,
        content_type: optional("CONTENT_TYPE", 128)?,
        query: Zeroizing::new(optional("QUERY_STRING", MAX_QUERY_BYTES)?.unwrap_or_default()),
        // Synology's CGI wrapper may omit an unauthenticated Cookie header
        // entirely. Preserve that as an empty authentication input so the
        // request is classified as unauthenticated (401), not malformed
        // transport metadata (400).
        cookie: Zeroizing::new(optional("HTTP_COOKIE", MAX_COOKIE_BYTES)?.unwrap_or_default()),
        request_marker: optional("HTTP_X_SDSYNC_REQUEST", 8)?,
        synology_token_header: optional("HTTP_X_SYNO_TOKEN", MAX_TOKEN_BYTES)?.map(Zeroizing::new),
        csrf_header: optional("HTTP_X_SDSYNC_CSRF", MAX_CSRF_BYTES)?.map(Zeroizing::new),
        remote_address: optional("REMOTE_ADDR", 128)?,
        server_address: optional("SERVER_ADDR", 128)?,
        server_name: optional("SERVER_NAME", 255)?,
        server_port: optional("SERVER_PORT", 8)?,
        https: optional("HTTPS", 16)?,
        transfer_encoding: optional("HTTP_TRANSFER_ENCODING", 64)?,
        native_authentication_context: NativeAuthenticationContext {
            gateway_interface: optional("GATEWAY_INTERFACE", 64)?,
            http_host: optional("HTTP_HOST", 512)?,
            remote_port: optional("REMOTE_PORT", 8)?,
            request_scheme: optional("REQUEST_SCHEME", 16)?,
            server_protocol: optional("SERVER_PROTOCOL", 32)?,
            script_name: optional("SCRIPT_NAME", MAX_QUERY_BYTES)?,
            script_filename: optional("SCRIPT_FILENAME", MAX_QUERY_BYTES)?,
            document_root: optional("DOCUMENT_ROOT", MAX_QUERY_BYTES)?,
            scgi: optional("SCGI", 64)?,
            socket: optional("SOCKET", MAX_QUERY_BYTES)?,
        },
    })
}

fn validate_environment_value(value: &str, maximum: usize) -> BridgeResult<()> {
    if value.len() > maximum || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_optional_environment_value(value: Option<&str>, maximum: usize) -> BridgeResult<()> {
    value.map_or(Ok(()), |value| validate_environment_value(value, maximum))
}

fn validate_http_request(mut environment: CgiEnvironment) -> BridgeResult<ValidatedHttpRequest> {
    // DSM's Webman/FastCGI boundary can materialize absent request metadata as
    // present-but-empty CGI variables. Empty values are semantically absent;
    // non-empty transfer encodings and GET entity metadata remain rejected.
    if environment
        .transfer_encoding
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return Err(BridgeError::bad_request());
    }
    if environment.request_marker.as_deref() != Some("1") {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    // Synology's authenticated DSM session is the unique `id` cookie. Reject
    // absent or ambiguous identities before invoking either authentication
    // path; ancillary browser/proxy cookies are not session credentials.
    dsm_session_cookie_id(&environment.cookie)?;

    let mut query = parse_urlencoded(&environment.query)?;
    let query_token = query.remove("SynoToken").map(Zeroizing::new);
    let synology_token =
        choose_synology_token(environment.synology_token_header.take(), query_token)?;

    let authentication = AuthenticationInputs {
        cookie: environment.cookie,
        synology_token,
        remote_address: environment.remote_address,
        server_address: environment.server_address,
        server_name: environment.server_name,
        server_port: environment.server_port,
        https: environment.https,
        native_context: environment.native_authentication_context,
    };

    match environment.method.as_str() {
        "GET" => {
            if environment
                .content_length
                .as_deref()
                .is_some_and(|value| !value.is_empty() && value != "0")
                || environment
                    .content_type
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                || environment
                    .csrf_header
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
            {
                return Err(BridgeError::bad_request());
            }
            let action = parse_read_action(query)?;
            Ok(ValidatedHttpRequest::Get {
                action,
                authentication,
            })
        }
        "POST" => {
            if !query.is_empty() {
                return Err(BridgeError::bad_request());
            }
            let content_length = parse_content_length(environment.content_length.as_deref())?;
            validate_json_content_type(environment.content_type.as_deref())?;
            let csrf_token = environment
                .csrf_header
                .take()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| BridgeError::new(ErrorKind::Forbidden))?;
            Ok(ValidatedHttpRequest::Post {
                content_length,
                csrf_token,
                authentication,
            })
        }
        _ => Err(BridgeError::new(ErrorKind::MethodNotAllowed)),
    }
}

fn choose_synology_token(
    header: Option<Zeroizing<String>>,
    query: Option<Zeroizing<String>>,
) -> BridgeResult<Option<Zeroizing<String>>> {
    let selected = match (header, query) {
        (Some(header), Some(query)) => {
            if !constant_time_equal(header.as_bytes(), query.as_bytes()) {
                return Err(BridgeError::new(ErrorKind::Forbidden));
            }
            header
        }
        (Some(header), None) => header,
        (None, Some(query)) => query,
        (None, None) => return Ok(None),
    };
    if selected.is_empty()
        || selected.len() > MAX_TOKEN_BYTES
        || selected
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    Ok(Some(selected))
}

fn parse_content_length(value: Option<&str>) -> BridgeResult<usize> {
    let value = value.ok_or_else(BridgeError::bad_request)?;
    if value.is_empty()
        || value.len() > 10
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(BridgeError::bad_request());
    }
    let parsed = value
        .parse::<usize>()
        .map_err(|_| BridgeError::bad_request())?;
    if parsed == 0 {
        return Err(BridgeError::bad_request());
    }
    if parsed > MAX_POST_BODY_BYTES {
        return Err(BridgeError::new(ErrorKind::PayloadTooLarge));
    }
    Ok(parsed)
}

fn validate_json_content_type(value: Option<&str>) -> BridgeResult<()> {
    let value = value.ok_or_else(|| BridgeError::new(ErrorKind::UnsupportedMediaType))?;
    let normalized = value.to_ascii_lowercase();
    let valid = matches!(
        normalized.as_str(),
        "application/json" | "application/json; charset=utf-8" | "application/json;charset=utf-8"
    );
    if !valid {
        return Err(BridgeError::new(ErrorKind::UnsupportedMediaType));
    }
    Ok(())
}

fn parse_urlencoded(value: &str) -> BridgeResult<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    if value.is_empty() {
        return Ok(result);
    }
    for component in value.split('&') {
        if component.is_empty() {
            return Err(BridgeError::bad_request());
        }
        let (raw_key, raw_value) = component
            .split_once('=')
            .ok_or_else(BridgeError::bad_request)?;
        let key = percent_decode(raw_key)?;
        let value = percent_decode(raw_value)?;
        if key.is_empty() || result.insert(key, value).is_some() {
            return Err(BridgeError::bad_request());
        }
    }
    Ok(result)
}

fn percent_decode(value: &str) -> BridgeResult<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(BridgeError::bad_request());
                }
                let high = hex_nibble(bytes[index + 1]).ok_or_else(BridgeError::bad_request)?;
                let low = hex_nibble(bytes[index + 2]).ok_or_else(BridgeError::bad_request)?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    if decoded.contains(&0) {
        return Err(BridgeError::bad_request());
    }
    String::from_utf8(decoded).map_err(|_| BridgeError::bad_request())
}

fn parse_read_action(mut query: BTreeMap<String, String>) -> BridgeResult<ReadAction> {
    let action = query
        .remove("action")
        .ok_or_else(BridgeError::bad_request)?;
    let parsed = match action.as_str() {
        "csrf" => {
            require_empty_query(&query)?;
            ReadAction::Csrf
        }
        "snapshot" => {
            require_empty_query(&query)?;
            ReadAction::Snapshot
        }
        "source-directories" => {
            let parent = query
                .remove("parent")
                .ok_or_else(BridgeError::bad_request)?;
            validate_source_browser_path(&parent)?;
            require_empty_query(&query)?;
            ReadAction::SourceDirectories { parent }
        }
        "source-path" => {
            let path = query.remove("path").ok_or_else(BridgeError::bad_request)?;
            validate_source_path(&path)?;
            require_empty_query(&query)?;
            ReadAction::SourcePath { path }
        }
        "logs" => {
            let lines = parse_lines(query.remove("lines"))?;
            let source = match query.remove("source").as_deref().unwrap_or("all") {
                "all" => LogSource::All,
                "api" => LogSource::Api,
                "controller" => LogSource::Controller,
                "scheduler" => LogSource::Scheduler,
                "sync" => LogSource::Sync,
                "audit" => LogSource::Audit,
                _ => return Err(BridgeError::bad_request()),
            };
            require_empty_query(&query)?;
            ReadAction::Logs { lines, source }
        }
        "activity" => {
            let lines = parse_lines(query.remove("lines"))?;
            require_empty_query(&query)?;
            ReadAction::Activity { lines }
        }
        "result" => {
            let job_id = query
                .remove("job_id")
                .filter(|value| valid_server_job_id(value))
                .ok_or_else(BridgeError::bad_request)?;
            require_empty_query(&query)?;
            ReadAction::Result { job_id }
        }
        "request-status" => {
            let request_id = query
                .remove("request_id")
                .filter(|value| valid_client_request_id(value))
                .ok_or_else(BridgeError::bad_request)?;
            require_empty_query(&query)?;
            ReadAction::RequestStatus { request_id }
        }
        _ => return Err(BridgeError::bad_request()),
    };
    Ok(parsed)
}

fn parse_lines(value: Option<String>) -> BridgeResult<u16> {
    let value = value.unwrap_or_else(|| "100".to_owned());
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(BridgeError::bad_request());
    }
    let lines = value
        .parse::<u16>()
        .map_err(|_| BridgeError::bad_request())?;
    if !(1..=1000).contains(&lines) {
        return Err(BridgeError::bad_request());
    }
    Ok(lines)
}

fn require_empty_query(query: &BTreeMap<String, String>) -> BridgeResult<()> {
    if query.is_empty() {
        Ok(())
    } else {
        Err(BridgeError::bad_request())
    }
}

fn is_positive_decimal_suffix(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('0') && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_dsm_volume_name(value: &str) -> bool {
    value
        .strip_prefix("volumeUSB")
        .or_else(|| value.strip_prefix("volumeSATA"))
        .or_else(|| value.strip_prefix("volume"))
        .is_some_and(is_positive_decimal_suffix)
}

fn is_dsm_managed_source_name(value: &str) -> bool {
    [
        "#recycle",
        "#snapshot",
        "@eaDir",
        "@tmp",
        "@sharebin",
        "@apphome",
        "@appdata",
        "@appstore",
        "@apptemp",
        "@appconf",
        ".SynologyWorkingDirectory",
    ]
    .iter()
    .any(|managed| value.eq_ignore_ascii_case(managed))
}

fn validate_source_browser_path(value: &str) -> BridgeResult<()> {
    if value == "/" {
        return Ok(());
    }
    if value.is_empty()
        || value.len() > 4096
        || !value.starts_with("/volume")
        || value.ends_with('/')
        || value.contains("//")
        || value.contains('\\')
        || value.contains('"')
        || value.chars().any(char::is_control)
    {
        return Err(BridgeError::bad_request());
    }
    let components: Vec<_> = value[1..].split('/').collect();
    if components.is_empty()
        || !is_dsm_volume_name(components[0])
        || components.iter().any(|component| {
            component.is_empty()
                || matches!(*component, "." | "..")
                || is_dsm_managed_source_name(component)
        })
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_source_path(value: &str) -> BridgeResult<()> {
    validate_bounded_text(value, 4096, false)?;
    if value == "/"
        || !value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.contains('\\')
        || value.contains('"')
        || contains_dot_segment(value)
    {
        return Err(BridgeError::bad_request());
    }
    let mut components = value[1..].split('/');
    if !components.next().is_some_and(is_dsm_volume_name)
        || components.any(is_dsm_managed_source_name)
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn source_browser_parent(value: &str) -> Option<String> {
    if value == "/" {
        return None;
    }
    let parent = value
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    Some(if parent.is_empty() { "/" } else { parent }.to_owned())
}

#[cfg(target_os = "linux")]
fn package_identity_can_read_and_traverse(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // The service's real and effective identities are the package account.
    // `access` therefore checks the exact R_OK/X_OK pair used by the manager's
    // authoritative save-time source validation.
    unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::X_OK) == 0 }
}

#[cfg(target_os = "linux")]
fn source_directories_document(system_root: &Path, parent: &str) -> BridgeResult<Vec<u8>> {
    const MAX_DIRECTORY_RESULTS: usize = 500;
    const MAX_SCANNED_ENTRIES: usize = 4096;

    validate_source_browser_path(parent)?;
    let system_root = fs::canonicalize(system_root).map_err(|_| BridgeError::unsafe_runtime())?;
    let physical_parent = if parent == "/" {
        system_root.clone()
    } else {
        system_root.join(parent.trim_start_matches('/'))
    };
    let metadata = fs::symlink_metadata(&physical_parent)
        .map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let canonical_parent =
        fs::canonicalize(&physical_parent).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    if canonical_parent != physical_parent || !canonical_parent.starts_with(&system_root) {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    if !package_identity_can_read_and_traverse(&canonical_parent) {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }

    let mut directories = Vec::new();
    let mut truncated = false;
    let entries =
        fs::read_dir(&canonical_parent).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SCANNED_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            // A child may disappear or become unreadable while the directory
            // is being enumerated. The already-validated parent remains safe;
            // omit only the unusable child from this bounded snapshot.
            continue;
        };
        let Ok(name) = entry.file_name().into_string() else {
            // DSM paths exposed to the JSON UI must be UTF-8. A non-UTF-8
            // sibling must not make otherwise usable folders unselectable.
            continue;
        };
        if (parent == "/" && !is_dsm_volume_name(&name))
            || (parent != "/"
                && (name.is_empty()
                    || name.contains('"')
                    || name.contains('\\')
                    || name.chars().any(char::is_control)
                    || is_dsm_managed_source_name(&name)))
        {
            continue;
        }
        let entry_path = entry.path();
        let Ok(entry_metadata) = fs::symlink_metadata(&entry_path) else {
            continue;
        };
        if !entry_metadata.file_type().is_dir() || entry_metadata.file_type().is_symlink() {
            continue;
        }
        let canonical_entry = match fs::canonicalize(&entry_path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if canonical_entry != entry_path
            || !canonical_entry.starts_with(&system_root)
            || !package_identity_can_read_and_traverse(&canonical_entry)
            || fs::read_dir(&canonical_entry).is_err()
        {
            continue;
        }
        let logical_path = if parent == "/" {
            format!("/{name}")
        } else {
            format!("{parent}/{name}")
        };
        validate_source_browser_path(&logical_path)?;
        directories.push(json!({ "name": name, "path": logical_path }));
        if directories.len() > MAX_DIRECTORY_RESULTS {
            truncated = true;
            break;
        }
    }
    directories.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    directories.truncate(MAX_DIRECTORY_RESULTS);
    serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-source-directories.v1",
        "current": parent,
        "parent": source_browser_parent(parent),
        "directories": directories,
        "truncated": truncated,
    }))
    .map_err(|_| BridgeError::internal())
}

#[cfg(target_os = "linux")]
fn source_path_document(system_root: &Path, path: &str) -> BridgeResult<Vec<u8>> {
    validate_source_path(path)?;
    let system_root = fs::canonicalize(system_root).map_err(|_| BridgeError::unsafe_runtime())?;
    let physical_path = system_root.join(path.trim_start_matches('/'));
    let metadata =
        fs::symlink_metadata(&physical_path).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let canonical_path =
        fs::canonicalize(&physical_path).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    if canonical_path != physical_path
        || !canonical_path.starts_with(&system_root)
        || !package_identity_can_read_and_traverse(&canonical_path)
        || fs::read_dir(&canonical_path).is_err()
    {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-source-path.v1",
        "path": path,
        "valid": true,
    }))
    .map_err(|_| BridgeError::internal())
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn session_binding_matches(stored: &[u8; 32], current: &[u8; 32]) -> bool {
    constant_time_equal(stored, current)
}

fn read_exact_body(
    reader: &mut dyn Read,
    content_length: usize,
) -> BridgeResult<Zeroizing<Vec<u8>>> {
    if content_length == 0 || content_length > MAX_POST_BODY_BYTES {
        return Err(BridgeError::new(ErrorKind::PayloadTooLarge));
    }
    let mut body = Zeroizing::new(Vec::with_capacity(content_length));
    let mut limited = reader.take((content_length + 1) as u64);
    limited
        .read_to_end(&mut body)
        .map_err(|_| BridgeError::bad_request())?;
    if body.len() != content_length {
        return Err(BridgeError::bad_request());
    }
    Ok(body)
}

fn parse_mutation_request(body: &[u8]) -> BridgeResult<ParsedMutation> {
    let request: RawMutationRequest<'_> =
        serde_json::from_slice(body).map_err(|_| BridgeError::bad_request())?;
    if request.schema != "sdsync.dsm-request.v1" || !valid_client_request_id(request.request_id) {
        return Err(BridgeError::bad_request());
    }

    let (mutation, secret) = match request.operation {
        "configure-profile" => {
            let arguments: ConfigureProfileArgs = parse_arguments(request.arguments)?;
            validate_configure_profile(&arguments)?;
            (Mutation::ConfigureProfile(arguments), None)
        }
        "remove-profile" => {
            let arguments: NameArgs = parse_arguments(request.arguments)?;
            validate_existing_name(&arguments.name)?;
            (Mutation::RemoveProfile(arguments), None)
        }
        "set-default" => {
            let arguments: NameArgs = parse_arguments(request.arguments)?;
            validate_existing_name(&arguments.name)?;
            (Mutation::SetDefault(arguments), None)
        }
        "set-secret" => {
            let mut arguments: SecretRequestArgs = parse_arguments(request.arguments)?;
            validate_existing_name(&arguments.profile)?;
            let secret = match arguments.mode {
                SecretMode::Replace => {
                    let value = arguments
                        .value
                        .take()
                        .ok_or_else(BridgeError::bad_request)?;
                    validate_secret(&value.0)?;
                    let bytes = Zeroizing::new(value.0.as_bytes().to_vec());
                    Some(bytes)
                }
                SecretMode::Clear => {
                    if arguments.value.is_some() {
                        return Err(BridgeError::bad_request());
                    }
                    None
                }
            };
            (
                Mutation::SetSecret(SecretJobArgs {
                    profile: arguments.profile,
                    kind: arguments.kind,
                    mode: arguments.mode,
                }),
                secret,
            )
        }
        "test-profile-auth" => {
            let arguments: ConnectionRequestArgs = parse_arguments(request.arguments)?;
            let (arguments, secret) = parse_connection_request(arguments)?;
            (Mutation::TestProfileAuth(arguments), secret)
        }
        "browse-remote" => {
            let arguments: BrowseRemoteRequestArgs = parse_arguments(request.arguments)?;
            validate_remote_browser_parent(&arguments.parent)?;
            if !valid_connection_proof_syntax(&arguments.connection_proof) {
                return Err(BridgeError::bad_request());
            }
            let (connection, secret) = parse_connection_request(arguments.connection)?;
            (
                Mutation::BrowseRemote(BrowseRemoteJobArgs {
                    connection,
                    parent: arguments.parent,
                    connection_proof: arguments.connection_proof,
                }),
                secret,
            )
        }
        "schedule" => {
            let arguments: ScheduleArgs = parse_arguments(request.arguments)?;
            validate_schedule(&arguments)?;
            (Mutation::Schedule(arguments), None)
        }
        "routine" => {
            let arguments: RoutineArgs = parse_arguments(request.arguments)?;
            validate_routine(&arguments)?;
            (Mutation::Routine(arguments), None)
        }
        "remove-routine" => {
            let arguments: NameArgs = parse_arguments(request.arguments)?;
            validate_existing_name(&arguments.name)?;
            (Mutation::RemoveRoutine(arguments), None)
        }
        "alert-policy" => {
            let arguments: AlertPolicyArgs = parse_arguments(request.arguments)?;
            validate_alert_policy(&arguments)?;
            (Mutation::AlertPolicy(arguments), None)
        }
        "security-policy" => {
            let arguments: SecurityPolicyArgs = parse_arguments(request.arguments)?;
            validate_security_policy(&arguments)?;
            (Mutation::SecurityPolicy(arguments), None)
        }
        "client-event" => {
            let arguments: ClientEventArgs = parse_arguments(request.arguments)?;
            (Mutation::ClientEvent(arguments), None)
        }
        "action" => {
            let arguments: OperationalActionArgs = parse_arguments(request.arguments)?;
            validate_operational_action(&arguments)?;
            (Mutation::Action(arguments), None)
        }
        _ => return Err(BridgeError::bad_request()),
    };

    Ok(ParsedMutation {
        request_id: request.request_id.to_owned(),
        mutation,
        secret,
    })
}

fn parse_arguments<T>(raw: &RawValue) -> BridgeResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(raw.get()).map_err(|_| BridgeError::bad_request())
}

fn parse_job(body: &[u8]) -> BridgeResult<ParsedJob> {
    let job: RawJob<'_> = serde_json::from_slice(body).map_err(|_| BridgeError::bad_request())?;
    if job.schema != "sdsync.dsm-job.v1"
        || !valid_server_job_id(job.request_id)
        || !valid_client_request_id(job.client_request_id)
        || !valid_authenticated_username(job.requested_by)
        || job.requested_uid == 0
        || !valid_server_job_id(job.audit_transaction)
        || !valid_request_fingerprint(job.request_fingerprint)
    {
        return Err(BridgeError::bad_request());
    }
    let session_binding =
        hex_decode_exact::<32>(job.session_binding).ok_or_else(BridgeError::bad_request)?;

    let mutation = match job.operation {
        "configure-profile" => {
            let value: ConfigureProfileArgs = parse_arguments(job.arguments)?;
            validate_configure_profile(&value)?;
            Mutation::ConfigureProfile(value)
        }
        "remove-profile" => {
            let value: NameArgs = parse_arguments(job.arguments)?;
            validate_existing_name(&value.name)?;
            Mutation::RemoveProfile(value)
        }
        "set-default" => {
            let value: NameArgs = parse_arguments(job.arguments)?;
            validate_existing_name(&value.name)?;
            Mutation::SetDefault(value)
        }
        "set-secret" => {
            let value: SecretJobArgs = parse_arguments(job.arguments)?;
            validate_existing_name(&value.profile)?;
            Mutation::SetSecret(value)
        }
        "test-profile-auth" => {
            let value: ConnectionJobArgs = parse_arguments(job.arguments)?;
            validate_connection_job(&value)?;
            Mutation::TestProfileAuth(value)
        }
        "browse-remote" => {
            let value: BrowseRemoteJobArgs = parse_arguments(job.arguments)?;
            validate_connection_job(&value.connection)?;
            validate_remote_browser_parent(&value.parent)?;
            if !valid_connection_proof_syntax(&value.connection_proof) {
                return Err(BridgeError::bad_request());
            }
            Mutation::BrowseRemote(value)
        }
        "schedule" => {
            let value: ScheduleArgs = parse_arguments(job.arguments)?;
            validate_schedule(&value)?;
            Mutation::Schedule(value)
        }
        "routine" => {
            let value: RoutineArgs = parse_arguments(job.arguments)?;
            validate_routine(&value)?;
            Mutation::Routine(value)
        }
        "remove-routine" => {
            let value: NameArgs = parse_arguments(job.arguments)?;
            validate_existing_name(&value.name)?;
            Mutation::RemoveRoutine(value)
        }
        "alert-policy" => {
            let value: AlertPolicyArgs = parse_arguments(job.arguments)?;
            validate_alert_policy(&value)?;
            Mutation::AlertPolicy(value)
        }
        "security-policy" => {
            let value: SecurityPolicyArgs = parse_arguments(job.arguments)?;
            validate_security_policy(&value)?;
            Mutation::SecurityPolicy(value)
        }
        "client-event" => {
            let value: ClientEventArgs = parse_arguments(job.arguments)?;
            Mutation::ClientEvent(value)
        }
        "action" => {
            let value: OperationalActionArgs = parse_arguments(job.arguments)?;
            validate_operational_action(&value)?;
            Mutation::Action(value)
        }
        _ => return Err(BridgeError::bad_request()),
    };
    Ok(ParsedJob {
        request_id: job.request_id.to_owned(),
        client_request_id: job.client_request_id.to_owned(),
        requested_by: job.requested_by.to_owned(),
        requested_uid: job.requested_uid,
        session_binding,
        audit_transaction: job.audit_transaction.to_owned(),
        request_fingerprint: job.request_fingerprint.to_owned(),
        issued_at_epoch: job.issued_at_epoch,
        mutation,
    })
}

fn queued_job_class(job: &ParsedJob) -> QueuedJobClass {
    match &job.mutation {
        Mutation::TestProfileAuth(_) | Mutation::BrowseRemote(_) => QueuedJobClass::Connection,
        // Operational actions may be long-running, but they do not commit
        // profile, credential, policy, or scheduler state. A bounded
        // connection probe can therefore run beside them without observing a
        // partially committed configuration mutation.
        Mutation::Action(_) => QueuedJobClass::Concurrent,
        _ => QueuedJobClass::Serialized,
    }
}

fn valid_client_request_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_server_job_id(value: &str) -> bool {
    value.len() == SERVER_JOB_ID_BYTES * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_request_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_job_freshness(issued_at_epoch: u64, now: u64) -> BridgeResult<()> {
    if issued_at_epoch > now.saturating_add(CLOCK_SKEW_SECONDS)
        || now.saturating_sub(issued_at_epoch) > MAX_JOB_AGE_SECONDS
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_name(value: &str) -> BridgeResult<()> {
    if value.is_empty()
        || value.len() > 64
        || value == "all"
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_existing_name(value: &str) -> BridgeResult<()> {
    // Released package revisions accepted filesystem-safe profile identifiers
    // up to the platform component limit. Keep that compatibility for reads,
    // actions, secrets, and removal while retaining the tighter 64-byte limit
    // for newly configured profiles.
    if value.is_empty()
        || value.len() > 255
        || value == "all"
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_bounded_text(value: &str, maximum: usize, empty_allowed: bool) -> BridgeResult<()> {
    if (!empty_allowed && value.is_empty())
        || value.len() > maximum
        || value
            .chars()
            .any(|character| character == '\0' || character.is_control())
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_secret(value: &str) -> BridgeResult<()> {
    if value.is_empty()
        || value.len() > MAX_SECRET_BYTES
        || value
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_connection_job(value: &ConnectionJobArgs) -> BridgeResult<()> {
    if let Some(profile) = &value.profile {
        validate_existing_name(profile)?;
    }
    validate_bounded_text(&value.url, 2048, false)?;
    if !(value.url.starts_with("https://")
        || (value.allow_http && value.url.starts_with("http://")))
    {
        return Err(BridgeError::bad_request());
    }
    validate_bounded_text(&value.username, 256, false)?;
    if value.connect_timeout_seconds == 0
        || value.connect_timeout_seconds > 600
        || value.timeout_seconds == 0
        || value.timeout_seconds > 86_400
        || value.retries > 5
        || value.password_source == CredentialSource::None
        || (matches!(value.password_source, CredentialSource::Stored) && value.profile.is_none())
        || (matches!(value.totp_source, CredentialSource::Stored) && value.profile.is_none())
    {
        return Err(BridgeError::bad_request());
    }
    if let Some(certificate) = &value.ca_certificate {
        validate_bounded_text(certificate, 4096, false)?;
        if !certificate.starts_with('/') || contains_dot_segment(certificate) {
            return Err(BridgeError::bad_request());
        }
    }
    Ok(())
}

fn validate_remote_browser_parent(value: &str) -> BridgeResult<()> {
    if value == "/" {
        return Ok(());
    }
    validate_bounded_text(value, 247, false)?;
    if !value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.contains('\\')
        || contains_dot_segment(value)
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn valid_connection_proof_syntax(value: &str) -> bool {
    let components: Vec<_> = value.split('.').collect();
    components.len() == 4
        && components[0] == "v1"
        && parse_canonical_u64(components[1]).is_ok()
        && components[2].len() == 64
        && components[2].bytes().all(|byte| byte.is_ascii_hexdigit())
        && components[3].len() == 64
        && components[3].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn encode_connection_secret_envelope(
    password: Option<SecretString>,
    totp: Option<SecretString>,
) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
    const MAGIC: &[u8] = b"sdsync-connection-secrets-v1\0";
    if password.is_none() && totp.is_none() {
        return Ok(None);
    }
    let mut encoded = Zeroizing::new(Vec::with_capacity(MAX_CONNECTION_SECRET_BYTES));
    encoded.extend_from_slice(MAGIC);
    for value in [password, totp] {
        let bytes = value
            .as_ref()
            .map(|value| value.0.as_bytes())
            .unwrap_or_default();
        let length = u32::try_from(bytes.len()).map_err(|_| BridgeError::bad_request())?;
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(bytes);
    }
    if encoded.len() > MAX_CONNECTION_SECRET_BYTES {
        return Err(BridgeError::new(ErrorKind::PayloadTooLarge));
    }
    Ok(Some(encoded))
}

fn decode_connection_secret_envelope(
    encoded: Option<Zeroizing<Vec<u8>>>,
) -> BridgeResult<DecodedConnectionSecrets> {
    const MAGIC: &[u8] = b"sdsync-connection-secrets-v1\0";
    let Some(encoded) = encoded else {
        return Ok((None, None));
    };
    if encoded.len() > MAX_CONNECTION_SECRET_BYTES || !encoded.starts_with(MAGIC) {
        return Err(BridgeError::bad_request());
    }
    let mut cursor = MAGIC.len();
    let mut values = Vec::with_capacity(2);
    for _ in 0..2 {
        let length_bytes: [u8; 4] = encoded
            .get(cursor..cursor + 4)
            .and_then(|value| value.try_into().ok())
            .ok_or_else(BridgeError::bad_request)?;
        cursor += 4;
        let length = u32::from_be_bytes(length_bytes) as usize;
        let value = encoded
            .get(cursor..cursor + length)
            .ok_or_else(BridgeError::bad_request)?;
        cursor += length;
        values.push((!value.is_empty()).then(|| Zeroizing::new(value.to_vec())));
    }
    if cursor != encoded.len() {
        return Err(BridgeError::bad_request());
    }
    let totp = values.pop().ok_or_else(BridgeError::internal)?;
    let password = values.pop().ok_or_else(BridgeError::internal)?;
    Ok((password, totp))
}

fn parse_connection_request(
    mut value: ConnectionRequestArgs,
) -> BridgeResult<(ConnectionJobArgs, Option<Zeroizing<Vec<u8>>>)> {
    let password = match value.password_source {
        CredentialSource::Provided => {
            let secret = value.password.take().ok_or_else(BridgeError::bad_request)?;
            validate_secret(&secret.0)?;
            Some(secret)
        }
        CredentialSource::Stored | CredentialSource::None => {
            if value.password.is_some() {
                return Err(BridgeError::bad_request());
            }
            None
        }
    };
    let totp = match value.totp_source {
        CredentialSource::Provided => {
            let secret = value.totp.take().ok_or_else(BridgeError::bad_request)?;
            validate_secret(&secret.0)?;
            // Parse now as well as at execution so malformed provisioning
            // material never enters the private queue.
            parse_totp_secret(&secret.0).map_err(|_| BridgeError::bad_request())?;
            Some(secret)
        }
        CredentialSource::Stored | CredentialSource::None => {
            if value.totp.is_some() {
                return Err(BridgeError::bad_request());
            }
            None
        }
    };
    let job = ConnectionJobArgs {
        profile: value.profile,
        url: value.url,
        username: value.username,
        allow_http: value.allow_http,
        danger_accept_invalid_certs: value.danger_accept_invalid_certs,
        ca_certificate: value.ca_certificate,
        connect_timeout_seconds: value.connect_timeout_seconds,
        timeout_seconds: value.timeout_seconds,
        retries: value.retries,
        password_source: value.password_source,
        totp_source: value.totp_source,
    };
    validate_connection_job(&job)?;
    let envelope = encode_connection_secret_envelope(password, totp)?;
    Ok((job, envelope))
}

fn validate_configure_profile(value: &ConfigureProfileArgs) -> BridgeResult<()> {
    const MAX_DSM_RATE_BYTES_PER_SECOND: u64 = 9_007_199_254_740_991;

    validate_name(&value.name)?;
    validate_bounded_text(&value.source, 4096, false)?;
    if validate_source_path(&value.source).is_err() {
        return Err(BridgeError::bad_request());
    }
    validate_bounded_text(&value.url, 2048, false)?;
    if !(value.url.starts_with("https://")
        || (value.allow_http && value.url.starts_with("http://")))
    {
        return Err(BridgeError::bad_request());
    }
    validate_bounded_text(&value.username, 256, false)?;
    validate_bounded_text(&value.remote, 247, false)?;
    if !value.remote.starts_with('/')
        || value.remote == "/"
        || value.remote.ends_with('/')
        || value.remote.contains("//")
        || contains_dot_segment(&value.remote)
    {
        return Err(BridgeError::bad_request());
    }
    if !(1..=16).contains(&value.jobs)
        || value.max_delete > MAX_DSM_DELETE_BOUND
        || value.retries > 5
        || value.timeout_seconds == 0
        || value.timeout_seconds > 86_400
        || value.connect_timeout_seconds == 0
        || value.connect_timeout_seconds > 600
        || value
            .max_rate_bytes_per_second
            .is_some_and(|rate| rate == 0 || rate > MAX_DSM_RATE_BYTES_PER_SECOND)
        || value.verbosity > 2
        || (value.allow_empty_source && !value.delete)
        || value.excludes.len() > 64
    {
        return Err(BridgeError::bad_request());
    }
    for exclude in &value.excludes {
        validate_bounded_text(exclude, 512, false)?;
    }
    if let Some(certificate) = &value.ca_certificate {
        validate_bounded_text(certificate, 4096, false)?;
        if !certificate.starts_with('/') || contains_dot_segment(certificate) {
            return Err(BridgeError::bad_request());
        }
    }
    if let Some(remote_log_url) = &value.remote_log_url {
        validate_bounded_text(remote_log_url, 2048, false)?;
        if !remote_log_url.starts_with("https://") {
            return Err(BridgeError::bad_request());
        }
    }
    if matches!(value.remote_log_mode, RemoteLogMode::Required) && value.remote_log_url.is_none() {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn contains_dot_segment(value: &str) -> bool {
    value
        .split('/')
        .any(|component| matches!(component, "." | ".."))
}

fn validate_schedule(value: &ScheduleArgs) -> BridgeResult<()> {
    if !(60..=2_592_000).contains(&value.interval_seconds)
        || value.max_total_delete > MAX_DSM_DELETE_BOUND
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_routine(value: &RoutineArgs) -> BridgeResult<()> {
    validate_existing_name(&value.profile)?;
    if value.retry_count > 5
        || !(10..=300).contains(&value.retry_backoff_seconds)
        || value.max_total_delete > MAX_DSM_DELETE_BOUND
        || value.depends_on.len() > 64
    {
        return Err(BridgeError::bad_request());
    }
    match value.mode {
        RoutineMode::Interval => {
            if !value
                .interval_seconds
                .is_some_and(|seconds| (60..=2_592_000).contains(&seconds))
                || value.weekdays.is_some()
                || value.time_window_start.is_some()
                || value.time_window_end.is_some()
                || value.debounce_seconds.is_some()
                || value.poll_seconds.is_some()
            {
                return Err(BridgeError::bad_request());
            }
        }
        RoutineMode::Daily => {
            let (Some(weekdays), Some(window_start), Some(window_end)) = (
                value.weekdays.as_ref(),
                value.time_window_start.as_deref(),
                value.time_window_end.as_deref(),
            ) else {
                return Err(BridgeError::bad_request());
            };
            if value.interval_seconds.is_some()
                || value.debounce_seconds.is_some()
                || value.poll_seconds.is_some()
                || weekdays.is_empty()
                || weekdays.len() > 7
                || !valid_clock_time(window_start)
                || !valid_clock_time(window_end)
            {
                return Err(BridgeError::bad_request());
            }
            let mut unique_weekdays = BTreeSet::new();
            if weekdays
                .iter()
                .any(|weekday| !(1..=7).contains(weekday) || !unique_weekdays.insert(*weekday))
            {
                return Err(BridgeError::bad_request());
            }
        }
        RoutineMode::Realtime => {
            if !value
                .debounce_seconds
                .is_some_and(|seconds| (1..=3600).contains(&seconds))
                || !value
                    .poll_seconds
                    .is_some_and(|seconds| (5..=3600).contains(&seconds))
                || value.interval_seconds.is_some()
                || value.weekdays.is_some()
                || value.time_window_start.is_some()
                || value.time_window_end.is_some()
            {
                return Err(BridgeError::bad_request());
            }
        }
    }
    let mut dependencies = BTreeSet::new();
    for dependency in &value.depends_on {
        validate_existing_name(dependency)?;
        if dependency == &value.profile || !dependencies.insert(dependency) {
            return Err(BridgeError::bad_request());
        }
    }
    Ok(())
}

fn valid_clock_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || !bytes[0].is_ascii_digit()
        || !bytes[1].is_ascii_digit()
        || !bytes[3].is_ascii_digit()
        || !bytes[4].is_ascii_digit()
    {
        return false;
    }
    let hour = (bytes[0] - b'0') * 10 + bytes[1] - b'0';
    let minute = (bytes[3] - b'0') * 10 + bytes[4] - b'0';
    hour <= 23 && minute <= 59
}

fn validate_alert_policy(value: &AlertPolicyArgs) -> BridgeResult<()> {
    if !(1..=100).contains(&value.failure_threshold)
        || !(60..=2_592_000).contains(&value.cooldown_seconds)
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_security_policy(value: &SecurityPolicyArgs) -> BridgeResult<()> {
    if !(60..=900).contains(&value.csrf_lifetime_seconds)
        || !(300..=86_400).contains(&value.result_retention_seconds)
        || !(1..=MAX_OUTSTANDING_JOBS).contains(&value.max_outstanding_jobs)
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn policy_level_for_category(
    policy: &SecurityPolicyArgs,
    category: &str,
) -> Option<PolicyLogLevel> {
    match category {
        "audit" => Some(policy.audit_log_level),
        "bridge" => Some(policy.bridge_log_level),
        "authentication" => Some(policy.authentication_log_level),
        "security" => Some(policy.security_log_level),
        "configuration" => Some(policy.configuration_log_level),
        "secrets" => Some(policy.secrets_log_level),
        "routines" => Some(policy.routines_log_level),
        "operations" => Some(policy.operations_log_level),
        "notifications" => Some(policy.notifications_log_level),
        "sync" => Some(policy.sync_log_level),
        "controller" => Some(policy.controller_log_level),
        "scheduler" => Some(policy.scheduler_log_level),
        _ => None,
    }
}

fn policy_level_for_log_source(
    policy: &SecurityPolicyArgs,
    source: &str,
) -> Option<PolicyLogLevel> {
    match source {
        "audit" => Some(policy.audit_log_level),
        "api" => Some(policy.bridge_log_level),
        "controller" => Some(policy.controller_log_level),
        "scheduler" => Some(policy.scheduler_log_level),
        "sync" => Some(policy.sync_log_level),
        _ => None,
    }
}

fn event_visible_at_threshold(
    policy: &SecurityPolicyArgs,
    category: &str,
    level: &str,
    mandatory: bool,
) -> bool {
    if mandatory && category == "audit" {
        return true;
    }
    let Some(threshold) = policy_level_for_category(policy, category) else {
        return false;
    };
    let Some(event_level) = PolicyLogLevel::parse(level) else {
        return false;
    };
    threshold.allows(event_level)
}

fn log_line_visible_at_threshold(policy: &SecurityPolicyArgs, source: &str, line: &str) -> bool {
    if source == "audit" {
        // The audit file contains only the non-disableable minimal records.
        return true;
    }
    let parsed = serde_json::from_str::<Value>(line).ok();
    let threshold = if source == "api" {
        match parsed
            .as_ref()
            .and_then(|value| value.get("category"))
            .and_then(Value::as_str)
        {
            Some(category) => policy_level_for_category(policy, category),
            // Opaque legacy API-server output is bridge-owned.
            None => Some(policy.bridge_log_level),
        }
    } else {
        policy_level_for_log_source(policy, source)
    };
    let Some(threshold) = threshold else {
        return false;
    };
    let event_level = parsed
        .and_then(|value| {
            value
                .get("level")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .and_then(|value| PolicyLogLevel::parse(&value))
        // Opaque legacy/core output has no trustworthy severity contract.
        // Treat it deterministically as Info; package-owned controller and
        // scheduler lifecycle records are structured at the writer.
        .unwrap_or(PolicyLogLevel::Info);
    threshold.allows(event_level)
}

fn cgi_failure_category(stage: &str) -> Option<&'static str> {
    match stage {
        "request" | "bridge_connect" | "bridge_io" | "bridge_protocol" => Some("bridge"),
        "cgi_identity" | "cgi_runtime" => Some("security"),
        "dsm_authentication" => Some("authentication"),
        _ => None,
    }
}

fn parse_security_policy_file(bytes: &[u8]) -> BridgeResult<SecurityPolicyArgs> {
    // The persisted policy is a canonical LF-terminated document. `str::lines`
    // otherwise normalizes CRLF by stripping CR, while the POSIX shell parser
    // deliberately treats that CR as an invalid value and fails closed.
    if !bytes.ends_with(b"\n") || bytes.contains(&b'\r') {
        return Err(BridgeError::unsafe_runtime());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| BridgeError::unsafe_runtime())?;
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() || line.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(BridgeError::unsafe_runtime());
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(BridgeError::unsafe_runtime)?;
        if key.is_empty()
            || value.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            || fields.insert(key, value).is_some()
        {
            return Err(BridgeError::unsafe_runtime());
        }
    }

    fn take_bool(fields: &mut BTreeMap<&str, &str>, key: &str) -> BridgeResult<bool> {
        match fields.remove(key) {
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            _ => Err(BridgeError::unsafe_runtime()),
        }
    }
    fn take_u64(fields: &mut BTreeMap<&str, &str>, key: &str) -> BridgeResult<u64> {
        let value = fields.remove(key).ok_or_else(BridgeError::unsafe_runtime)?;
        parse_canonical_u64(value).map_err(|_| BridgeError::unsafe_runtime())
    }
    fn take_level(fields: &mut BTreeMap<&str, &str>, key: &str) -> BridgeResult<PolicyLogLevel> {
        match fields.remove(key) {
            Some("off") => Ok(PolicyLogLevel::Off),
            Some("trace") => Ok(PolicyLogLevel::Trace),
            Some("debug") => Ok(PolicyLogLevel::Debug),
            Some("info") => Ok(PolicyLogLevel::Info),
            Some("warn") => Ok(PolicyLogLevel::Warn),
            Some("error") => Ok(PolicyLogLevel::Error),
            _ => Err(BridgeError::unsafe_runtime()),
        }
    }

    if take_u64(&mut fields, "policy_version")? != 1 {
        return Err(BridgeError::unsafe_runtime());
    }

    let value = SecurityPolicyArgs {
        require_https: take_bool(&mut fields, "require_https")?,
        allow_interface_changes: take_bool(&mut fields, "allow_interface_changes")?,
        allow_profile_changes: take_bool(&mut fields, "allow_profile_changes")?,
        allow_secret_changes: take_bool(&mut fields, "allow_secret_changes")?,
        allow_routine_changes: take_bool(&mut fields, "allow_routine_changes")?,
        allow_notification_changes: take_bool(&mut fields, "allow_notification_changes")?,
        allow_operational_actions: take_bool(&mut fields, "allow_operational_actions")?,
        allow_http_targets: take_bool(&mut fields, "allow_http_targets")?,
        allow_invalid_tls: take_bool(&mut fields, "allow_invalid_tls")?,
        allow_destructive_sync: take_bool(&mut fields, "allow_destructive_sync")?,
        allow_doctor_write_test: take_bool(&mut fields, "allow_doctor_write_test")?,
        allow_remote_logging: take_bool(&mut fields, "allow_remote_logging")?,
        allow_empty_source: take_bool(&mut fields, "allow_empty_source")?,
        csrf_lifetime_seconds: take_u64(&mut fields, "csrf_lifetime_seconds")?,
        result_retention_seconds: take_u64(&mut fields, "result_retention_seconds")?,
        max_outstanding_jobs: take_u64(&mut fields, "max_outstanding_jobs")?
            .try_into()
            .map_err(|_| BridgeError::unsafe_runtime())?,
        audit_log_level: take_level(&mut fields, "audit_log_level")?,
        bridge_log_level: take_level(&mut fields, "bridge_log_level")?,
        authentication_log_level: take_level(&mut fields, "authentication_log_level")?,
        security_log_level: take_level(&mut fields, "security_log_level")?,
        configuration_log_level: take_level(&mut fields, "configuration_log_level")?,
        secrets_log_level: take_level(&mut fields, "secrets_log_level")?,
        routines_log_level: take_level(&mut fields, "routines_log_level")?,
        operations_log_level: take_level(&mut fields, "operations_log_level")?,
        notifications_log_level: take_level(&mut fields, "notifications_log_level")?,
        sync_log_level: take_level(&mut fields, "sync_log_level")?,
        controller_log_level: take_level(&mut fields, "controller_log_level")?,
        scheduler_log_level: take_level(&mut fields, "scheduler_log_level")?,
    };
    if !fields.is_empty() {
        return Err(BridgeError::unsafe_runtime());
    }
    validate_security_policy(&value).map_err(|_| BridgeError::unsafe_runtime())?;
    Ok(value)
}

fn validate_mutation_against_security_policy(
    mutation: &Mutation,
    policy: &SecurityPolicyArgs,
) -> BridgeResult<()> {
    let allowed = match mutation {
        Mutation::ConfigureProfile(value) => {
            policy.allow_profile_changes
                && (policy.allow_http_targets || !value.allow_http)
                && (policy.allow_invalid_tls || !value.danger_accept_invalid_certs)
                && (policy.allow_destructive_sync || !value.delete)
                && (policy.allow_remote_logging || value.remote_log_url.is_none())
                && (policy.allow_empty_source || !value.allow_empty_source)
        }
        Mutation::RemoveProfile(_) | Mutation::SetDefault(_) => policy.allow_profile_changes,
        Mutation::SetSecret(value) => {
            policy.allow_secret_changes
                && (policy.allow_remote_logging
                    || value.kind != SecretKind::RemoteLogToken
                    || value.mode == SecretMode::Clear)
        }
        Mutation::TestProfileAuth(value)
        | Mutation::BrowseRemote(BrowseRemoteJobArgs {
            connection: value, ..
        }) => {
            policy.allow_operational_actions
                && (policy.allow_http_targets || !value.allow_http)
                && (policy.allow_invalid_tls || !value.danger_accept_invalid_certs)
        }
        Mutation::Schedule(value) => {
            policy.allow_routine_changes && (policy.allow_destructive_sync || !value.allow_delete)
        }
        Mutation::Routine(value) => {
            policy.allow_routine_changes && (policy.allow_destructive_sync || !value.allow_delete)
        }
        Mutation::RemoveRoutine(_) => policy.allow_routine_changes,
        Mutation::AlertPolicy(_) => policy.allow_notification_changes,
        Mutation::SecurityPolicy(_) => true,
        Mutation::ClientEvent(value) => match value.event {
            ClientEventKind::InterfaceSettings => policy.allow_interface_changes,
            ClientEventKind::SessionNotifications => policy.allow_notification_changes,
        },
        Mutation::Action(value) => {
            policy.allow_operational_actions
                && (policy.allow_doctor_write_test || value.write_test != Some(true))
                && (policy.allow_destructive_sync || value.allow_delete != Some(true))
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(BridgeError::new(ErrorKind::Forbidden))
    }
}

fn validate_operational_action(value: &OperationalActionArgs) -> BridgeResult<()> {
    if value.scope != "all" {
        validate_existing_name(&value.scope)?;
    }
    match value.kind {
        OperationalActionKind::Doctor => {
            if value.allow_delete.is_some() || value.max_total_delete.is_some() {
                return Err(BridgeError::bad_request());
            }
            if value.write_test == Some(true)
                && matches!(
                    value.level,
                    Some(OperationalDoctorLevel::Quick | OperationalDoctorLevel::Standard)
                )
            {
                return Err(BridgeError::bad_request());
            }
        }
        OperationalActionKind::Plan | OperationalActionKind::Run => {
            if value.level.is_some() || value.write_test.is_some() {
                return Err(BridgeError::bad_request());
            }
            let _allow_delete = value.allow_delete.ok_or_else(BridgeError::bad_request)?;
            if value.scope == "all" {
                if value
                    .max_total_delete
                    .is_none_or(|maximum| maximum > MAX_DSM_DELETE_BOUND)
                {
                    return Err(BridgeError::bad_request());
                }
            } else if value.max_total_delete.is_some() {
                return Err(BridgeError::bad_request());
            }
        }
    }
    Ok(())
}

fn valid_authenticated_username(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_AUTHENTICATED_USERNAME_BYTES
        && value.chars().all(|character| {
            !character.is_control()
                && character != '|'
                && !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                        | '\u{fff9}'..='\u{fffb}'
                )
        })
}

fn validate_cgi_identity(state: &IdentityState) -> BridgeResult<u32> {
    let package_uid = state.executable_uid;
    let regular_file = state.executable_mode & 0o170_000 == 0o100_000;
    if package_uid == 0
        || state.real_uid != package_uid
        || state.effective_uid != package_uid
        || !regular_file
        || state.executable_mode & 0o7777 != 0o755
    {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(package_uid)
}

fn validate_package_identity(state: &IdentityState) -> BridgeResult<u32> {
    let package_uid = state.executable_uid;
    let regular_file = state.executable_mode & 0o170_000 == 0o100_000;
    if package_uid == 0
        || state.real_uid != package_uid
        || state.effective_uid != package_uid
        || !regular_file
        || state.executable_mode & 0o7777 != 0o755
    {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(package_uid)
}

fn authorize_admin_membership(
    authenticated_uid: u32,
    primary_gid: u32,
    administrator_gid: u32,
    supplementary_groups: &[u32],
) -> BridgeResult<()> {
    if authenticated_uid == 0 {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    if !identity_belongs_to_group(primary_gid, administrator_gid, supplementary_groups) {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    Ok(())
}

fn identity_belongs_to_group(
    primary_gid: u32,
    required_gid: u32,
    supplementary_groups: &[u32],
) -> bool {
    primary_gid == required_gid || supplementary_groups.contains(&required_gid)
}

#[cfg(any(target_os = "linux", test))]
fn trusted_executable_mode(mode: u32, owner: (u32, u32)) -> bool {
    // Preserve the legacy root-owned helper contract, which may carry set-id
    // bits. Every non-root owner pair--including DSM's standard system:system
    // 1:1 target--must remain an ordinary non-set-id executable. Package files
    // are independently required to be exactly 0755.
    let root_owned = owner.0 == 0;
    mode & 0o170_000 == 0o100_000
        && mode & 0o022 == 0
        && mode & 0o111 != 0
        && (root_owned || mode & 0o6000 == 0)
}

#[cfg(any(target_os = "linux", test))]
fn trusted_directory_mode(mode: u32) -> bool {
    mode & 0o170_000 == 0o040_000 && mode & 0o022 == 0
}

fn parse_authentication_output(output: &[u8]) -> BridgeResult<String> {
    if output.is_empty() || output.len() > MAX_AUTH_OUTPUT_BYTES {
        return Err(BridgeError::new(ErrorKind::Unauthorized));
    }
    let output =
        std::str::from_utf8(output).map_err(|_| BridgeError::new(ErrorKind::Unauthorized))?;
    let username = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    if !valid_authenticated_username(username) || username.contains('\r') || username.contains('\n')
    {
        return Err(BridgeError::new(ErrorKind::Unauthorized));
    }
    Ok(username.to_owned())
}

fn authentication_command_environment(inputs: &AuthenticationInputs) -> Vec<(OsString, OsString)> {
    let authentication_query = inputs
        .synology_token
        .as_ref()
        .map_or_else(String::new, |token| {
            format!(
                "SynoToken={}",
                normalize_synology_token_query_value(token.as_bytes())
            )
        });
    let mut variables = vec![
        (
            OsString::from("PATH"),
            OsString::from("/usr/sbin:/usr/bin:/sbin:/bin"),
        ),
        (OsString::from("LANG"), OsString::from("C")),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (
            OsString::from("REQUEST_METHOD"),
            // authenticate.cgi is an authentication probe, not the original
            // application action.  Passing POST or the app's action query can
            // make DSM parse a body/query that belongs only to this package.
            OsString::from("GET"),
        ),
        (
            OsString::from("QUERY_STRING"),
            OsString::from(authentication_query),
        ),
        (
            OsString::from("HTTP_COOKIE"),
            OsString::from(inputs.cookie.as_str()),
        ),
    ];
    if let Some(synology_token) = &inputs.synology_token {
        variables.push((
            OsString::from("HTTP_X_SYNO_TOKEN"),
            OsString::from(synology_token.as_str()),
        ));
    }
    for (name, value) in [
        ("REMOTE_ADDR", inputs.remote_address.as_ref()),
        ("SERVER_ADDR", inputs.server_address.as_ref()),
        ("SERVER_NAME", inputs.server_name.as_ref()),
        ("SERVER_PORT", inputs.server_port.as_ref()),
        ("HTTPS", inputs.https.as_ref()),
        (
            "GATEWAY_INTERFACE",
            inputs.native_context.gateway_interface.as_ref(),
        ),
        ("HTTP_HOST", inputs.native_context.http_host.as_ref()),
        ("REMOTE_PORT", inputs.native_context.remote_port.as_ref()),
        (
            "REQUEST_SCHEME",
            inputs.native_context.request_scheme.as_ref(),
        ),
        (
            "SERVER_PROTOCOL",
            inputs.native_context.server_protocol.as_ref(),
        ),
        ("SCRIPT_NAME", inputs.native_context.script_name.as_ref()),
        (
            "SCRIPT_FILENAME",
            inputs.native_context.script_filename.as_ref(),
        ),
        (
            "DOCUMENT_ROOT",
            inputs.native_context.document_root.as_ref(),
        ),
        ("SCGI", inputs.native_context.scgi.as_ref()),
        ("SOCKET", inputs.native_context.socket.as_ref()),
    ] {
        if let Some(value) = value {
            variables.push((OsString::from(name), OsString::from(value)));
        }
    }
    variables
}

fn normalize_synology_token_query_value(value: &[u8]) -> String {
    // DSM's first-party JavaScript contract sends encodeURIComponent(raw) in
    // X-SYNO-TOKEN, while direct callers may still supply the raw token. Decode
    // only complete %HH triplets, preserve every malformed/literal percent,
    // and deliberately keep '+' literal rather than applying form semantics.
    // Encoding the resulting bytes once makes both supported representations
    // converge on one canonical helper query without changing the header.
    let mut decoded = Zeroizing::new(Vec::with_capacity(value.len()));
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%'
            && index + 2 < value.len()
            && let (Some(high), Some(low)) =
                (hex_nibble(value[index + 1]), hex_nibble(value[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
            continue;
        }
        decoded.push(value[index]);
        index += 1;
    }
    percent_encode_query_value(&decoded)
}

fn percent_encode_query_value(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for &byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

fn manager_command_environment() -> Vec<(OsString, OsString)> {
    vec![
        (
            OsString::from("PATH"),
            OsString::from("/usr/sbin:/usr/bin:/sbin:/bin"),
        ),
        (OsString::from("LANG"), OsString::from("C")),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from("HOME"), OsString::from(PACKAGE_HOME)),
        (
            OsString::from("SYNOPKG_PKGDEST"),
            OsString::from(PACKAGE_ROOT),
        ),
        (
            OsString::from("SYNOPKG_PKGHOME"),
            OsString::from(PACKAGE_HOME),
        ),
        (
            OsString::from("SYNOPKG_PKGVAR"),
            OsString::from(PACKAGE_VAR),
        ),
    ]
}

fn valid_cookie_octets(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            matches!(
                byte,
                0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e
            )
        })
}

fn dsm_session_cookie_id(cookie: &str) -> BridgeResult<&str> {
    let mut session_id = None;
    for raw_pair in cookie.split(';') {
        // RFC 6265 emits a single SP after `;`. Leading OWS is harmless, but
        // whitespace within a cookie-pair is not normalized because doing so
        // could disagree with DSM's authentication parser.
        let pair = raw_pair.trim_start_matches(' ');
        if pair.is_empty() {
            continue;
        }
        let Some((name, value)) = pair.split_once('=') else {
            if pair.trim_matches(' ').eq_ignore_ascii_case("id") {
                return Err(BridgeError::new(ErrorKind::Unauthorized));
            }
            continue;
        };
        let normalized_name = name.trim_matches(' ');
        if !normalized_name.eq_ignore_ascii_case("id") {
            continue;
        }
        if name != "id" || session_id.is_some() || !valid_cookie_octets(value) {
            return Err(BridgeError::new(ErrorKind::Unauthorized));
        }
        session_id = Some(value);
    }
    session_id.ok_or_else(|| BridgeError::new(ErrorKind::Unauthorized))
}

fn session_binding(
    username: &str,
    uid: u32,
    cookie: &str,
    synology_token: Option<&str>,
) -> BridgeResult<[u8; 32]> {
    let session_id = dsm_session_cookie_id(cookie)?;
    let mut digest = Sha256::new();
    digest.update(b"sdsync-dsm-session-v2\0");
    update_length_prefixed(&mut digest, username.as_bytes());
    digest.update(uid.to_be_bytes());
    update_length_prefixed(&mut digest, session_id.as_bytes());
    // An absent launch token occupies the otherwise-invalid empty-token slot,
    // keeping cookie-only and token-authenticated sessions distinct. There is
    // deliberately no raw-cookie v1 fallback: old bindings fail closed rather
    // than restoring dependence on mutable ancillary cookies.
    update_length_prefixed(&mut digest, synology_token.unwrap_or_default().as_bytes());
    Ok(digest.finalize().into())
}

fn audit_transaction_id(
    session_binding: &[u8; 32],
    client_request_id: &str,
    issued_at_epoch: u64,
    server_nonce: &[u8; 16],
) -> BridgeResult<String> {
    if !valid_client_request_id(client_request_id) {
        return Err(BridgeError::bad_request());
    }
    let mut digest = Sha256::new();
    digest.update(b"sdsync.dsm-audit-transaction.v1\0");
    digest.update(session_binding);
    digest.update(client_request_id.as_bytes());
    digest.update(issued_at_epoch.to_be_bytes());
    digest.update(server_nonce);
    Ok(hex_encode(&digest.finalize())[..48].to_owned())
}

fn mutation_request_fingerprint(
    idempotency_key: &[u8],
    mutation: &Mutation,
    secret: Option<&[u8]>,
) -> BridgeResult<String> {
    let arguments =
        serde_json::to_vec(&mutation.arguments_value()?).map_err(|_| BridgeError::internal())?;
    let mut mac = HmacSha256::new_from_slice(idempotency_key)
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    mac.update(b"sdsync.dsm-idempotency.v1\0");
    update_mac_length_prefixed(&mut mac, mutation.operation_id().as_bytes());
    update_mac_length_prefixed(&mut mac, &arguments);
    mac.update(&[u8::from(secret.is_some())]);
    update_mac_length_prefixed(&mut mac, secret.unwrap_or_default());
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

fn update_mac_length_prefixed(mac: &mut HmacSha256, value: &[u8]) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
}

fn update_length_prefixed(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn write_frame(writer: &mut impl Write, payload: &[u8], maximum: usize) -> BridgeResult<()> {
    if payload.is_empty() || payload.len() > maximum || payload.len() > u32::MAX as usize {
        return Err(BridgeError::new(ErrorKind::PayloadTooLarge));
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .and_then(|()| writer.write_all(payload))
        .and_then(|()| writer.flush())
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))
}

fn read_single_frame(
    reader: &mut impl Read,
    maximum: usize,
    malformed: ErrorKind,
) -> BridgeResult<Zeroizing<Vec<u8>>> {
    let mut header = [0_u8; 4];
    reader
        .read_exact(&mut header)
        .map_err(|_| BridgeError::new(malformed))?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > maximum {
        return Err(BridgeError::new(if length > maximum {
            ErrorKind::PayloadTooLarge
        } else {
            malformed
        }));
    }
    let mut payload = Zeroizing::new(vec![0_u8; length]);
    reader
        .read_exact(&mut payload)
        .map_err(|_| BridgeError::new(malformed))?;
    let mut trailing = [0_u8; 1];
    match reader.read(&mut trailing) {
        Ok(0) => Ok(payload),
        Ok(_) | Err(_) => Err(BridgeError::new(malformed)),
    }
}

fn encode_relay_response(response: &CgiResponse) -> BridgeResult<Zeroizing<Vec<u8>>> {
    if response.body.is_empty() || response.body.len() > MAX_MANAGER_OUTPUT_BYTES {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let mut payload = Zeroizing::new(Vec::with_capacity(response.body.len() + 2));
    payload.extend_from_slice(&response.status.to_be_bytes());
    payload.extend_from_slice(&response.body);
    Ok(payload)
}

fn decode_relay_response(payload: &[u8]) -> BridgeResult<CgiResponse> {
    if payload.len() <= 2 || payload.len() > MAX_RELAY_RESPONSE_BYTES {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let status = u16::from_be_bytes([payload[0], payload[1]]);
    if !matches!(
        status,
        200 | 202 | 400 | 401 | 403 | 405 | 409 | 410 | 413 | 415 | 500 | 503
    ) || serde_json::from_slice::<Value>(&payload[2..]).is_err()
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(CgiResponse {
        status,
        body: payload[2..].to_vec(),
    })
}

#[cfg(target_os = "linux")]
mod linux_runtime {
    use super::*;
    use reqwest::StatusCode;
    use reqwest::blocking::Client;
    use reqwest::header::{ACCEPT, CONTENT_LENGTH, COOKIE, HeaderValue};
    use reqwest::redirect::Policy;
    use std::os::linux::fs::MetadataExt;
    use std::ptr;

    const MAX_TRUSTED_SYMLINKS: usize = 16;
    // DSM exposes this built-in account as system:system, but executable trust
    // is enforced against the exact numeric identities reported by the
    // kernel rather than adding an NSS dependency to the CGI preflight.
    const DSM_AUTHENTICATION_HELPER_UID: u32 = 1;
    const DSM_AUTHENTICATION_HELPER_GID: u32 = 1;

    #[derive(Debug)]
    pub(super) struct TrustedExecutable {
        pub(super) path: PathBuf,
        device: u64,
        inode: u64,
        owner: u32,
        group: u32,
        mode: u32,
    }

    impl TrustedExecutable {
        pub(super) fn revalidate(&self) -> BridgeResult<()> {
            let metadata =
                fs::symlink_metadata(&self.path).map_err(|_| BridgeError::unsafe_runtime())?;
            if metadata.st_dev() != self.device
                || metadata.st_ino() != self.inode
                || metadata.st_uid() != self.owner
                || metadata.st_gid() != self.group
                || metadata.st_mode() != self.mode
                || !trusted_executable_mode(
                    metadata.st_mode(),
                    (metadata.st_uid(), metadata.st_gid()),
                )
            {
                return Err(BridgeError::unsafe_runtime());
            }
            Ok(())
        }
    }

    pub(super) fn clear_environment() -> BridgeResult<()> {
        // SAFETY: the bridge is deliberately single-threaded and calls clearenv
        // before creating any worker thread.  All required CGI values have
        // already been copied into bounded Rust-owned buffers.
        if unsafe { libc::clearenv() } != 0 {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    pub(super) fn identity_state() -> BridgeResult<IdentityState> {
        let executable = std::env::current_exe().map_err(|_| BridgeError::unsafe_runtime())?;
        let metadata =
            fs::symlink_metadata(executable).map_err(|_| BridgeError::unsafe_runtime())?;
        // SAFETY: these libc calls have no pointer arguments or preconditions.
        let real_uid = unsafe { libc::getuid() };
        // SAFETY: these libc calls have no pointer arguments or preconditions.
        let effective_uid = unsafe { libc::geteuid() };
        Ok(IdentityState {
            real_uid,
            effective_uid,
            executable_uid: metadata.st_uid(),
            executable_mode: metadata.st_mode(),
        })
    }

    #[derive(Debug)]
    pub(super) enum AuthenticationHelperSelection<T> {
        Direct(T),
        Loopback,
    }

    pub(super) fn select_authentication_helper<T>(
        probe_execute: impl FnOnce(&Path) -> io::Result<()>,
        validate: impl FnOnce(&Path) -> BridgeResult<T>,
    ) -> Result<AuthenticationHelperSelection<T>, CgiFailure> {
        let path = Path::new(AUTHENTICATE_PATH);
        match probe_execute(path) {
            Ok(()) => validate(path)
                .map(AuthenticationHelperSelection::Direct)
                .map_err(|error| {
                    CgiFailure::coded(
                        CgiFailureStage::Authentication,
                        error,
                        "dsm_authentication_helper_unsafe",
                    )
                }),
            Err(error) if is_execute_permission_denied(&error) => {
                Ok(AuthenticationHelperSelection::Loopback)
            }
            Err(_) => Err(CgiFailure::coded(
                CgiFailureStage::Authentication,
                BridgeError::new(ErrorKind::Unavailable),
                "dsm_authentication_helper_unavailable",
            )),
        }
    }

    pub(super) fn authenticate_and_authorize_cgi(
        inputs: &AuthenticationInputs,
        timeout: Duration,
    ) -> Result<AuthenticatedSession, CgiFailure> {
        let helper = match select_authentication_helper(probe_caller_execute, |path| {
            validate_dsm_authentication_helper(path)
        })? {
            AuthenticationHelperSelection::Loopback => {
                return authenticate_via_dsm_user_service(inputs, timeout);
            }
            AuthenticationHelperSelection::Direct(helper) => helper,
        };
        let mut command = Command::new(&helper.path);
        command
            .env_clear()
            .envs(authentication_command_environment(inputs))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        helper.revalidate().map_err(|error| {
            CgiFailure::coded(
                CgiFailureStage::Authentication,
                error,
                "dsm_authentication_helper_unsafe",
            )
        })?;
        let output = capture_bounded_command(
            &mut command,
            MAX_AUTH_OUTPUT_BYTES,
            MAX_HELPER_STDERR_BYTES,
            timeout,
            None,
        )
        .map_err(|error| {
            CgiFailure::coded(
                CgiFailureStage::Authentication,
                error,
                "dsm_authentication_helper_unavailable",
            )
        })?;
        let username = authenticated_helper_username(
            output.status_success,
            &output.stdout,
            inputs.native_context.http_host.as_deref(),
        )?;
        authorize_authenticated_username(username, inputs).map_err(|error| {
            CgiFailure::coded(
                CgiFailureStage::Authentication,
                error,
                "dsm_authentication_forbidden",
            )
        })
    }

    pub(super) fn authenticated_helper_username(
        status_success: bool,
        stdout: &[u8],
        http_host: Option<&str>,
    ) -> Result<String, CgiFailure> {
        if !status_success {
            return Err(CgiFailure::coded(
                CgiFailureStage::Authentication,
                BridgeError::new(ErrorKind::Unauthorized),
                authentication_rejection_code(http_host),
            ));
        }
        parse_authentication_output(stdout).map_err(|error| {
            CgiFailure::coded(
                CgiFailureStage::Authentication,
                error,
                authentication_rejection_code(http_host),
            )
        })
    }

    fn authenticate_via_dsm_user_service(
        inputs: &AuthenticationInputs,
        timeout: Duration,
    ) -> Result<AuthenticatedSession, CgiFailure> {
        let username = query_dsm_user_service(inputs, timeout).map_err(|error| {
            let code = match error.kind {
                ErrorKind::Unauthorized => "dsm_authentication_webapi_rejected",
                ErrorKind::Forbidden => "dsm_authentication_webapi_forbidden",
                _ => "dsm_authentication_webapi_unavailable",
            };
            CgiFailure::coded(CgiFailureStage::Authentication, error, code)
        })?;
        authorize_authenticated_username(username, inputs).map_err(|error| {
            CgiFailure::coded(
                CgiFailureStage::Authentication,
                error,
                "dsm_authentication_webapi_forbidden",
            )
        })
    }

    pub(super) fn authentication_rejection_code(http_host: Option<&str>) -> &'static str {
        if is_quickconnect_authority(http_host) {
            "dsm_authentication_quickconnect_unsupported"
        } else {
            "dsm_authentication_rejected"
        }
    }

    fn is_quickconnect_authority(value: Option<&str>) -> bool {
        let Some(authority) = value else {
            return false;
        };
        if authority.is_empty()
            || !authority.is_ascii()
            || authority.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return false;
        }
        let host = match authority.rsplit_once(':') {
            Some((host, port))
                if !host.contains(':')
                    && !host.is_empty()
                    && !port.is_empty()
                    && port.bytes().all(|byte| byte.is_ascii_digit())
                    && (port.len() == 1 || !port.starts_with('0'))
                    && port.parse::<u16>().is_ok_and(|port| port != 0) =>
            {
                host
            }
            Some(_) => return false,
            None => authority,
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        if host.len() > 253 {
            return false;
        }
        let normalized = host.to_ascii_lowercase();
        let Some(prefix) = normalized.strip_suffix(".quickconnect.to") else {
            return false;
        };
        !prefix.is_empty()
            && prefix.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && label
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && label
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric)
            })
    }

    fn authorize_authenticated_username(
        username: String,
        inputs: &AuthenticationInputs,
    ) -> BridgeResult<AuthenticatedSession> {
        let uid = authorize_relayed_username(&username)?;
        let binding = session_binding(
            &username,
            uid,
            &inputs.cookie,
            inputs.synology_token.as_ref().map(|value| value.as_str()),
        )?;
        Ok(AuthenticatedSession {
            username,
            uid,
            binding,
        })
    }

    fn probe_caller_execute(path: &Path) -> io::Result<()> {
        let path = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        // SAFETY: path is a live NUL-terminated string and X_OK only asks the
        // kernel to evaluate this process's real UID and group permissions.
        if unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }

    pub(super) fn is_execute_permission_denied(error: &io::Error) -> bool {
        error.raw_os_error() == Some(libc::EACCES)
    }

    #[derive(Deserialize)]
    struct DsmUserServiceEnvelope {
        success: bool,
        data: Option<DsmUserServiceData>,
    }

    #[derive(Deserialize)]
    struct DsmUserServiceData {
        #[serde(rename = "Session")]
        session: DsmUserServiceSession,
    }

    #[derive(Deserialize)]
    struct DsmUserServiceSession {
        user: String,
        is_admin: bool,
    }

    pub(super) fn query_dsm_user_service(
        inputs: &AuthenticationInputs,
        timeout: Duration,
    ) -> BridgeResult<String> {
        let port = inputs
            .server_port
            .as_deref()
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        if port.is_empty()
            || (port.len() > 1 && port.starts_with('0'))
            || !port.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        let https = is_https_request(inputs.https.as_deref());
        let scheme = if https { "https" } else { "http" };
        let url = format!(
            "{scheme}://127.0.0.1:{port}{DSM_USER_SERVICE_PATH}?api={DSM_USER_SERVICE_API}&version=1&method=get_user_service"
        );
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .http1_only()
            .connect_timeout(timeout)
            .timeout(timeout)
            // DSM commonly uses a user-selected or self-signed certificate.
            // The peer is still pinned to the IPv4 loopback literal above;
            // no hostname, proxy, redirect, or remote address is accepted.
            .danger_accept_invalid_certs(https)
            .build()
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        let mut cookie = HeaderValue::from_str(inputs.cookie.as_str())
            .map_err(|_| BridgeError::new(ErrorKind::Unauthorized))?;
        cookie.set_sensitive(true);
        let mut request = client
            .get(&url)
            .header(ACCEPT, "application/json")
            .header(COOKIE, cookie);
        if let Some(token) = &inputs.synology_token {
            let mut token = HeaderValue::from_str(token.as_str())
                .map_err(|_| BridgeError::new(ErrorKind::Unauthorized))?;
            token.set_sensitive(true);
            request = request.header("X-SYNO-TOKEN", token);
        }
        let response = request
            .send()
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        if response.status() != StatusCode::OK
            || response
                .remote_addr()
                .is_some_and(|peer| peer.ip() != IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let declared_length = response.headers().get(CONTENT_LENGTH).map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|length| *length > 0 && *length <= MAX_DSM_USER_SERVICE_OUTPUT_BYTES)
                .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))
        });
        let declared_length = declared_length.transpose()?;
        let mut body = Vec::with_capacity(declared_length.unwrap_or(4096));
        response
            .take((MAX_DSM_USER_SERVICE_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut body)
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        if body.is_empty()
            || body.len() > MAX_DSM_USER_SERVICE_OUTPUT_BYTES
            || declared_length.is_some_and(|length| body.len() != length)
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        parse_dsm_user_service_output(&body)
    }

    pub(super) fn parse_dsm_user_service_output(body: &[u8]) -> BridgeResult<String> {
        let envelope = serde_json::from_slice::<DsmUserServiceEnvelope>(body)
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        if !envelope.success {
            return Err(BridgeError::new(ErrorKind::Unauthorized));
        }
        let session = envelope
            .data
            .ok_or_else(|| BridgeError::new(ErrorKind::Unauthorized))?
            .session;
        if !valid_authenticated_username(&session.user) {
            return Err(BridgeError::new(ErrorKind::Unauthorized));
        }
        if !session.is_admin {
            return Err(BridgeError::new(ErrorKind::Forbidden));
        }
        Ok(session.user)
    }

    pub(super) fn authorize_relayed_username(username: &str) -> BridgeResult<u32> {
        if !valid_authenticated_username(username) {
            return Err(BridgeError::new(ErrorKind::Forbidden));
        }
        let (uid, primary_gid) = lookup_user(username)?;
        let administrator_gid = lookup_group(ADMINISTRATORS_GROUP)?;
        let groups = lookup_groups(username, primary_gid)?;
        authorize_admin_membership(uid, primary_gid, administrator_gid, &groups)?;
        Ok(uid)
    }

    pub(super) fn validate_package_manager() -> BridgeResult<()> {
        // SAFETY: geteuid has no pointer arguments or preconditions.
        let package_uid = unsafe { libc::geteuid() };
        let metadata =
            fs::symlink_metadata(MANAGER_PATH).map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o022 != 0
            || metadata.st_mode() & 0o6000 != 0
            || metadata.st_mode() & 0o111 == 0
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    fn lookup_user(name: &str) -> BridgeResult<(u32, u32)> {
        let name = CString::new(name).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = ptr::null_mut();
        let mut buffer = vec![0_u8; nss_buffer_size(libc::_SC_GETPW_R_SIZE_MAX)];
        // SAFETY: all pointers reference live writable buffers for the duration
        // of getpwnam_r; the returned pointer is used only to detect not-found.
        let status = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status != 0 || result.is_null() {
            return Err(BridgeError::new(ErrorKind::Forbidden));
        }
        // SAFETY: a non-null result with zero status initializes the record.
        let record = unsafe { record.assume_init() };
        Ok((record.pw_uid, record.pw_gid))
    }

    fn lookup_group(name: &str) -> BridgeResult<u32> {
        let name = CString::new(name).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
        let mut record = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut result = ptr::null_mut();
        let mut buffer = vec![0_u8; nss_buffer_size(libc::_SC_GETGR_R_SIZE_MAX)];
        // SAFETY: all pointers reference live writable buffers for the duration
        // of getgrnam_r; the returned pointer is used only to detect not-found.
        let status = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status != 0 || result.is_null() {
            return Err(BridgeError::new(ErrorKind::Forbidden));
        }
        // SAFETY: a non-null result with zero status initializes the record.
        Ok(unsafe { record.assume_init() }.gr_gid)
    }

    fn lookup_groups(username: &str, primary_gid: u32) -> BridgeResult<Vec<u32>> {
        let username =
            CString::new(username).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
        let mut count: libc::c_int = 16;
        let mut groups = vec![0 as libc::gid_t; count as usize];
        // SAFETY: getgrouplist receives a valid username, initialized primary
        // GID, and a writable array whose length is supplied in count.
        let mut status = unsafe {
            libc::getgrouplist(
                username.as_ptr(),
                primary_gid,
                groups.as_mut_ptr(),
                &mut count,
            )
        };
        if status == -1 {
            if count <= 0 || count > 1024 {
                return Err(BridgeError::new(ErrorKind::Forbidden));
            }
            groups.resize(count as usize, 0);
            // SAFETY: the resized buffer is exactly the length supplied.
            status = unsafe {
                libc::getgrouplist(
                    username.as_ptr(),
                    primary_gid,
                    groups.as_mut_ptr(),
                    &mut count,
                )
            };
        }
        if status == -1 || count < 0 || count as usize > groups.len() {
            return Err(BridgeError::new(ErrorKind::Forbidden));
        }
        groups.truncate(count as usize);
        Ok(groups)
    }

    fn nss_buffer_size(key: libc::c_int) -> usize {
        // SAFETY: sysconf accepts the documented constant and has no pointers.
        let suggested = unsafe { libc::sysconf(key) };
        if suggested <= 0 {
            16 * 1024
        } else {
            (suggested as usize).clamp(1024, 64 * 1024)
        }
    }

    fn normalize_within_root(path: &Path, validation_root: &Path) -> BridgeResult<PathBuf> {
        if !path.is_absolute() || !validation_root.is_absolute() {
            return Err(BridgeError::unsafe_runtime());
        }
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::RootDir => normalized.push(Path::new("/")),
                std::path::Component::CurDir => {}
                std::path::Component::Normal(value) => normalized.push(value),
                std::path::Component::ParentDir => {
                    if normalized == validation_root || !normalized.pop() {
                        return Err(BridgeError::unsafe_runtime());
                    }
                }
                std::path::Component::Prefix(_) => {
                    return Err(BridgeError::unsafe_runtime());
                }
            }
        }
        if normalized.starts_with(validation_root) {
            Ok(normalized)
        } else {
            Err(BridgeError::unsafe_runtime())
        }
    }

    fn same_metadata_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
        left.st_dev() == right.st_dev()
            && left.st_ino() == right.st_ino()
            && left.st_uid() == right.st_uid()
            && left.st_gid() == right.st_gid()
            && left.st_mode() == right.st_mode()
    }

    pub(super) fn trusted_executable_target_owner(
        owner_uid: u32,
        owner_gid: u32,
        boundary_uid: u32,
        alternate_owner: Option<(u32, u32)>,
    ) -> bool {
        owner_uid == boundary_uid
            || alternate_owner.is_some_and(|(uid, gid)| owner_uid == uid && owner_gid == gid)
    }

    pub(super) fn trusted_symlink_boundary(metadata: &fs::Metadata, expected_uid: u32) -> bool {
        metadata.file_type().is_symlink() && metadata.st_uid() == expected_uid
    }

    #[cfg(test)]
    pub(super) fn validate_trusted_executable(
        path: &Path,
        validation_root: &Path,
        expected_uid: u32,
    ) -> BridgeResult<TrustedExecutable> {
        validate_trusted_executable_with_target_owner(path, validation_root, expected_uid, None)
    }

    fn validate_dsm_authentication_helper(path: &Path) -> BridgeResult<TrustedExecutable> {
        // DSM's fixed path and symlink chain are root-owned while its standard
        // resolved vendor helper uses the built-in system:system identity
        // (numeric 1:1). Permit that identity only for the final executable;
        // every ancestor and symlink remains subject to the root-owned boundary
        // contract below.
        validate_trusted_executable_with_target_owner(
            path,
            Path::new("/"),
            0,
            Some((DSM_AUTHENTICATION_HELPER_UID, DSM_AUTHENTICATION_HELPER_GID)),
        )
    }

    fn validate_trusted_executable_with_target_owner(
        path: &Path,
        validation_root: &Path,
        expected_uid: u32,
        alternate_target_owner: Option<(u32, u32)>,
    ) -> BridgeResult<TrustedExecutable> {
        let root = normalize_within_root(validation_root, validation_root)?;
        let root_metadata =
            fs::symlink_metadata(&root).map_err(|_| BridgeError::unsafe_runtime())?;
        if root_metadata.st_uid() != expected_uid
            || !trusted_directory_mode(root_metadata.st_mode())
        {
            return Err(BridgeError::unsafe_runtime());
        }

        let mut candidate = normalize_within_root(path, &root)?;
        let mut observed = BTreeSet::new();
        for _ in 0..=MAX_TRUSTED_SYMLINKS {
            if !observed.insert(candidate.clone()) {
                return Err(BridgeError::unsafe_runtime());
            }
            let relative = candidate
                .strip_prefix(&root)
                .map_err(|_| BridgeError::unsafe_runtime())?;
            let components = relative
                .components()
                .map(|component| match component {
                    std::path::Component::Normal(value) => Ok(value.to_os_string()),
                    _ => Err(BridgeError::unsafe_runtime()),
                })
                .collect::<BridgeResult<Vec<_>>>()?;
            if components.is_empty() {
                return Err(BridgeError::unsafe_runtime());
            }

            let mut parent = root.clone();
            let mut redirected = false;
            for (index, component) in components.iter().enumerate() {
                let entry = parent.join(component);
                let before =
                    fs::symlink_metadata(&entry).map_err(|_| BridgeError::unsafe_runtime())?;
                if before.file_type().is_symlink() {
                    if !trusted_symlink_boundary(&before, expected_uid) {
                        return Err(BridgeError::unsafe_runtime());
                    }
                    let target =
                        fs::read_link(&entry).map_err(|_| BridgeError::unsafe_runtime())?;
                    if target.as_os_str().is_empty() {
                        return Err(BridgeError::unsafe_runtime());
                    }
                    let after =
                        fs::symlink_metadata(&entry).map_err(|_| BridgeError::unsafe_runtime())?;
                    if !same_metadata_identity(&before, &after) {
                        return Err(BridgeError::unsafe_runtime());
                    }
                    let mut rebound = if target.is_absolute() {
                        target
                    } else {
                        parent.join(target)
                    };
                    for remainder in &components[index + 1..] {
                        rebound.push(remainder);
                    }
                    candidate = normalize_within_root(&rebound, &root)?;
                    redirected = true;
                    break;
                }

                let final_component = index + 1 == components.len();
                if final_component {
                    if !trusted_executable_target_owner(
                        before.st_uid(),
                        before.st_gid(),
                        expected_uid,
                        alternate_target_owner,
                    ) || !trusted_executable_mode(
                        before.st_mode(),
                        (before.st_uid(), before.st_gid()),
                    ) {
                        return Err(BridgeError::unsafe_runtime());
                    }
                    let after =
                        fs::symlink_metadata(&entry).map_err(|_| BridgeError::unsafe_runtime())?;
                    if !same_metadata_identity(&before, &after) {
                        return Err(BridgeError::unsafe_runtime());
                    }
                    return Ok(TrustedExecutable {
                        path: entry,
                        device: after.st_dev(),
                        inode: after.st_ino(),
                        owner: after.st_uid(),
                        group: after.st_gid(),
                        mode: after.st_mode(),
                    });
                }
                if before.st_uid() != expected_uid || !trusted_directory_mode(before.st_mode()) {
                    return Err(BridgeError::unsafe_runtime());
                }
                parent = entry;
            }
            if !redirected {
                return Err(BridgeError::unsafe_runtime());
            }
        }
        Err(BridgeError::unsafe_runtime())
    }
}

#[cfg(target_os = "linux")]
mod linux_socket {
    use super::*;
    use std::mem;
    use std::net::Shutdown;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::linux::fs::MetadataExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Mutex;

    static UMASK_LOCK: Mutex<()> = Mutex::new(());

    enum ConnectAttemptError {
        Retryable,
        Terminal(BridgeError),
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) struct PeerCredentials {
        pub(super) uid: u32,
        pub(super) gid: u32,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub(super) struct TerminalProcessIdentity {
        pub(super) pid: u32,
        pub(super) start: u64,
        pub(super) boot: String,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum ExactProcessState {
        Absent,
        Live,
        Terminal,
    }

    pub(super) fn peer_credentials(stream: &UnixStream) -> BridgeResult<PeerCredentials> {
        // SAFETY: getsockopt writes at most the supplied ucred-sized buffer and
        // receives a valid file descriptor owned by the live UnixStream.
        unsafe {
            let mut credentials = mem::zeroed::<libc::ucred>();
            let mut length = mem::size_of::<libc::ucred>() as libc::socklen_t;
            if libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            ) != 0
                || length as usize != mem::size_of::<libc::ucred>()
            {
                return Err(BridgeError::unsafe_runtime());
            }
            Ok(PeerCredentials {
                uid: credentials.uid,
                gid: credentials.gid,
            })
        }
    }

    pub(super) fn validate_peer_uid(actual: u32, expected: u32) -> BridgeResult<()> {
        if actual == 0 || expected == 0 || actual != expected {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    pub(super) fn configure_stream(stream: &UnixStream) -> BridgeResult<()> {
        stream
            .set_read_timeout(Some(RELAY_IO_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(RELAY_IO_TIMEOUT)))
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))
    }

    #[cfg(test)]
    pub(super) fn connect(path: &Path, package_uid: u32) -> BridgeResult<UnixStream> {
        connect_attempt(path, package_uid).map_err(|error| match error {
            ConnectAttemptError::Retryable => BridgeError::new(ErrorKind::Unavailable),
            ConnectAttemptError::Terminal(error) => error,
        })
    }

    pub(super) fn connect_for_cgi(
        path: &Path,
        package_uid: u32,
        window: Duration,
    ) -> BridgeResult<UnixStream> {
        let deadline = Instant::now()
            .checked_add(window)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        loop {
            match connect_attempt(path, package_uid) {
                Ok(stream) => return Ok(stream),
                Err(ConnectAttemptError::Terminal(error)) => return Err(error),
                Err(ConnectAttemptError::Retryable) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(BridgeError::new(ErrorKind::Unavailable));
                    }
                    std::thread::sleep(
                        deadline.duration_since(now).min(CGI_SERVICE_RETRY_INTERVAL),
                    );
                }
            }
        }
    }

    fn connect_attempt(path: &Path, package_uid: u32) -> Result<UnixStream, ConnectAttemptError> {
        let before = match connectable_socket_metadata(path, package_uid) {
            Ok(metadata) => metadata,
            Err(error) if error.kind == ErrorKind::Unavailable => {
                return Err(ConnectAttemptError::Retryable);
            }
            Err(error) => return Err(ConnectAttemptError::Terminal(error)),
        };
        let stream = match connect_with_timeout(path, RELAY_CONNECT_TIMEOUT) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(ConnectAttemptError::Retryable);
            }
            Err(_) => {
                return Err(ConnectAttemptError::Terminal(BridgeError::new(
                    ErrorKind::Unavailable,
                )));
            }
        };
        configure_stream(&stream).map_err(ConnectAttemptError::Terminal)?;
        let credentials = peer_credentials(&stream).map_err(ConnectAttemptError::Terminal)?;
        validate_peer_uid(credentials.uid, package_uid).map_err(ConnectAttemptError::Terminal)?;
        // Once a verified peer accepted the connection, disappearance or
        // replacement of the filesystem name is a security failure, never a
        // readiness retry. This preserves the before/after inode contract.
        let after = socket_metadata(path, package_uid)
            .map_err(|_| ConnectAttemptError::Terminal(BridgeError::unsafe_runtime()))?;
        if !same_object(&before, &after) {
            return Err(ConnectAttemptError::Terminal(BridgeError::unsafe_runtime()));
        }
        Ok(stream)
    }

    #[cfg(test)]
    pub(super) fn bind(path: &Path, package_uid: u32) -> BridgeResult<UnixListener> {
        let (listener, identity) = bind_prepared(path, package_uid)?;
        if let Err(error) = activate_prepared(path, package_uid, &identity) {
            drop(listener);
            remove_new_socket(path, package_uid);
            return Err(error);
        }
        Ok(listener)
    }

    pub(super) fn bind_prepared(
        path: &Path,
        package_uid: u32,
    ) -> BridgeResult<(UnixListener, fs::Metadata)> {
        validate_socket_parent(path, package_uid)?;
        remove_stale_socket(path, package_uid, false)?;
        let listener = {
            let _lock = UMASK_LOCK
                .lock()
                .map_err(|_| BridgeError::unsafe_runtime())?;
            // DSM executes a package CGI as the owner of its executable. The
            // daemon and CGI therefore share the package UID, so a package-only
            // 0600 bind would expose the listener before lifecycle commit.
            // Bind as 0000 and activate the same inode to 0600 only after the
            // worker pool and exact readiness identity exist.
            let _umask = UmaskGuard::replace(0o777);
            UnixListener::bind(path).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?
        };
        let identity = match prepared_socket_metadata(path, package_uid) {
            Ok(identity) => identity,
            Err(error) => {
                drop(listener);
                remove_new_socket(path, package_uid);
                return Err(error);
            }
        };
        Ok((listener, identity))
    }

    pub(super) fn activate_prepared(
        path: &Path,
        package_uid: u32,
        expected: &fs::Metadata,
    ) -> BridgeResult<()> {
        let before = prepared_socket_metadata(path, package_uid)?;
        if !same_inode(expected, &before) {
            return Err(BridgeError::unsafe_runtime());
        }
        set_socket_contract(path, package_uid)?;
        let after = socket_metadata(path, package_uid)?;
        if !same_inode(expected, &after) {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    pub(super) fn cleanup_stale_service_socket(
        socket_path: &Path,
        pid_path: &Path,
        package_uid: u32,
        expected_terminal: Option<&TerminalProcessIdentity>,
    ) -> BridgeResult<()> {
        validate_socket_parent(socket_path, package_uid)?;
        let pid_metadata = match fs::symlink_metadata(pid_path) {
            Ok(before) => {
                let parent = pid_path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
                let parent_metadata =
                    fs::symlink_metadata(parent).map_err(|_| BridgeError::unsafe_runtime())?;
                if !parent_metadata.file_type().is_dir()
                    || parent_metadata.st_uid() != package_uid
                    || parent_metadata.st_mode() & 0o7777 != 0o700
                {
                    return Err(BridgeError::unsafe_runtime());
                }
                let mut options = OpenOptions::new();
                options
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
                let mut file = options
                    .open(pid_path)
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                let opened = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
                if !opened.file_type().is_file()
                    || opened.st_uid() != package_uid
                    || opened.st_mode() & 0o7777 != 0o600
                    || opened.st_nlink() != 1
                    || opened.len() > 32
                    || !same_object(&before, &opened)
                {
                    return Err(BridgeError::unsafe_runtime());
                }
                let mut bytes = Vec::with_capacity(opened.len() as usize);
                Read::by_ref(&mut file)
                    .take(33)
                    .read_to_end(&mut bytes)
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                let after_read = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
                if bytes.len() as u64 != opened.len() || !same_object(&opened, &after_read) {
                    return Err(BridgeError::unsafe_runtime());
                }
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| BridgeError::unsafe_runtime())?;
                let pid = text
                    .strip_suffix('\n')
                    .filter(|value| {
                        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
                    })
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|value| *value > 1 && *value <= libc::pid_t::MAX as u32)
                    .ok_or_else(BridgeError::unsafe_runtime)?;
                if let Some(expected) = expected_terminal {
                    if expected.pid != pid || !valid_boot_id(&expected.boot) {
                        return Err(BridgeError::unsafe_runtime());
                    }
                    match exact_process_state(expected, package_uid)? {
                        ExactProcessState::Absent | ExactProcessState::Terminal => {}
                        ExactProcessState::Live => {
                            return Err(BridgeError::new(ErrorKind::Conflict));
                        }
                    }
                } else {
                    // SAFETY: signal zero performs only a liveness/permission probe.
                    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
                        return Err(BridgeError::new(ErrorKind::Conflict));
                    }
                    if io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                        return Err(BridgeError::unsafe_runtime());
                    }
                }
                Some(opened)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        if expected_terminal.is_some() && pid_metadata.is_none() {
            return match fs::symlink_metadata(socket_path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                _ => Err(BridgeError::unsafe_runtime()),
            };
        }
        if let Some(expected) = expected_terminal {
            match exact_process_state(expected, package_uid)? {
                ExactProcessState::Absent | ExactProcessState::Terminal => {}
                ExactProcessState::Live => {
                    return Err(BridgeError::new(ErrorKind::Conflict));
                }
            }
        }

        remove_stale_socket(socket_path, package_uid, pid_metadata.is_some())?;
        if let Some(before) = pid_metadata {
            let after =
                fs::symlink_metadata(pid_path).map_err(|_| BridgeError::unsafe_runtime())?;
            if !same_object(&before, &after) {
                return Err(BridgeError::unsafe_runtime());
            }
            fs::remove_file(pid_path).map_err(|_| BridgeError::unsafe_runtime())?;
            let parent = pid_path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| BridgeError::unsafe_runtime())?;
        }
        Ok(())
    }

    pub(super) fn exact_process_state(
        expected: &TerminalProcessIdentity,
        package_uid: u32,
    ) -> BridgeResult<ExactProcessState> {
        if expected.pid <= 1
            || expected.pid > libc::pid_t::MAX as u32
            || expected.start == 0
            || !valid_boot_id(&expected.boot)
        {
            return Err(BridgeError::unsafe_runtime());
        }
        if exact_current_boot_id()? != expected.boot {
            return Err(BridgeError::unsafe_runtime());
        }

        let stat_path = PathBuf::from(format!("/proc/{}/stat", expected.pid));
        let stat = match read_bounded_proc_file(&stat_path, 4096)? {
            Some(value) => value,
            None => {
                return if proc_entry_absent(expected.pid)? {
                    Ok(ExactProcessState::Absent)
                } else {
                    Err(BridgeError::unsafe_runtime())
                };
            }
        };
        let tail = stat
            .rsplit_once(") ")
            .map(|(_, tail)| tail)
            .ok_or_else(BridgeError::unsafe_runtime)?;
        let mut fields = tail.split_ascii_whitespace();
        let state = fields
            .next()
            .filter(|value| value.len() == 1)
            .ok_or_else(BridgeError::unsafe_runtime)?;
        let start = fields
            .nth(18)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value != 0)
            .ok_or_else(BridgeError::unsafe_runtime)?;

        let status_path = PathBuf::from(format!("/proc/{}/status", expected.pid));
        let Some(status) = read_bounded_proc_file(&status_path, 64 * 1024)? else {
            return if proc_entry_absent(expected.pid)? {
                Ok(ExactProcessState::Absent)
            } else {
                Err(BridgeError::unsafe_runtime())
            };
        };
        let uid = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|line| line.split_ascii_whitespace().next())
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(BridgeError::unsafe_runtime)?;
        if start != expected.start || uid != package_uid {
            return Err(BridgeError::unsafe_runtime());
        }

        let state_recheck = read_bounded_proc_file(&stat_path, 4096)?;
        let Some(state_recheck) = state_recheck else {
            return if proc_entry_absent(expected.pid)? {
                Ok(ExactProcessState::Absent)
            } else {
                Err(BridgeError::unsafe_runtime())
            };
        };
        let tail_recheck = state_recheck
            .rsplit_once(") ")
            .map(|(_, tail)| tail)
            .ok_or_else(BridgeError::unsafe_runtime)?;
        let mut fields_recheck = tail_recheck.split_ascii_whitespace();
        let state_recheck = fields_recheck
            .next()
            .filter(|value| value.len() == 1)
            .ok_or_else(BridgeError::unsafe_runtime)?;
        let start_recheck = fields_recheck
            .nth(18)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value != 0)
            .ok_or_else(BridgeError::unsafe_runtime)?;
        if start_recheck != expected.start || state_recheck != state {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(match state_recheck {
            "Z" | "X" | "x" => ExactProcessState::Terminal,
            _ => ExactProcessState::Live,
        })
    }

    pub(super) fn proc_entry_absent(pid: u32) -> BridgeResult<bool> {
        match fs::symlink_metadata(format!("/proc/{pid}")) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(_) => Err(BridgeError::unsafe_runtime()),
        }
    }

    fn exact_current_boot_id() -> BridgeResult<String> {
        let path = Path::new("/proc/sys/kernel/random/boot_id");
        let metadata = fs::symlink_metadata(path).map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file() || metadata.st_uid() != 0 {
            return Err(BridgeError::unsafe_runtime());
        }
        let value = read_bounded_proc_file(path, 64)?.ok_or_else(BridgeError::unsafe_runtime)?;
        let value = value.strip_suffix('\n').unwrap_or(&value).to_owned();
        if !valid_boot_id(&value) {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(value)
    }

    fn read_bounded_proc_file(path: &Path, maximum: u64) -> BridgeResult<Option<String>> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        let metadata = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file() || metadata.len() > maximum {
            return Err(BridgeError::unsafe_runtime());
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(maximum + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        if bytes.len() as u64 > maximum {
            return Err(BridgeError::unsafe_runtime());
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| BridgeError::unsafe_runtime())
    }

    pub(super) fn shutdown_write(stream: &UnixStream) -> BridgeResult<()> {
        stream
            .shutdown(Shutdown::Write)
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))
    }

    pub(super) fn remove_own_socket(
        path: &Path,
        package_uid: u32,
        expected: &fs::Metadata,
    ) -> BridgeResult<()> {
        validate_socket_parent(path, package_uid)?;
        let before = match fs::symlink_metadata(path) {
            Ok(_) => stale_socket_metadata(path, package_uid)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        if !same_inode(expected, &before) {
            return Err(BridgeError::unsafe_runtime());
        }
        let after = stale_socket_metadata(path, package_uid)?;
        if !same_object(&before, &after) {
            return Err(BridgeError::unsafe_runtime());
        }
        fs::remove_file(path).map_err(|_| BridgeError::unsafe_runtime())?;
        let parent = path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| BridgeError::unsafe_runtime())
    }

    fn validate_socket_parent(path: &Path, package_uid: u32) -> BridgeResult<()> {
        let parent = path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
        let metadata = fs::symlink_metadata(parent).map_err(|_| BridgeError::unsafe_runtime())?;
        if package_uid == 0
            || !metadata.file_type().is_dir()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o6022 != 0
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    fn socket_metadata(path: &Path, package_uid: u32) -> BridgeResult<fs::Metadata> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                BridgeError::new(ErrorKind::Unavailable)
            } else {
                BridgeError::unsafe_runtime()
            }
        })?;
        // Group ownership is deliberately not an authorization input: 0600
        // grants no group access, while DSM's documented CGI contract binds
        // execution to the executable owner UID. The inode-stability checks
        // below still require its observed GID to remain unchanged.
        if package_uid == 0
            || !metadata.file_type().is_socket()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o7777 != 0o600
            || metadata.st_nlink() != 1
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(metadata)
    }

    fn connectable_socket_metadata(path: &Path, package_uid: u32) -> BridgeResult<fs::Metadata> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                BridgeError::new(ErrorKind::Unavailable)
            } else {
                BridgeError::unsafe_runtime()
            }
        })?;
        if package_uid == 0
            || !metadata.file_type().is_socket()
            || metadata.st_uid() != package_uid
            || metadata.st_nlink() != 1
        {
            return Err(BridgeError::unsafe_runtime());
        }
        // Classify the lifecycle state from one metadata snapshot. Reading the
        // active and prepared contracts separately lets a legitimate 0000 ->
        // 0600 activation occur between the reads, falsely turning readiness
        // into a terminal unsafe-runtime failure.
        match metadata.st_mode() & 0o7777 {
            0o600 => Ok(metadata),
            0o000 => Err(BridgeError::new(ErrorKind::Unavailable)),
            _ => Err(BridgeError::unsafe_runtime()),
        }
    }

    fn prepared_socket_metadata(path: &Path, package_uid: u32) -> BridgeResult<fs::Metadata> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                BridgeError::new(ErrorKind::Unavailable)
            } else {
                BridgeError::unsafe_runtime()
            }
        })?;
        if package_uid == 0
            || !metadata.file_type().is_socket()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o7777 != 0o000
            || metadata.st_nlink() != 1
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(metadata)
    }

    fn remove_stale_socket(
        path: &Path,
        package_uid: u32,
        dead_pid_verified: bool,
    ) -> BridgeResult<()> {
        let before = match fs::symlink_metadata(path) {
            Ok(_) => stale_socket_metadata(path, package_uid)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        match connect_with_timeout(path, RELAY_CONNECT_TIMEOUT) {
            Ok(_) => return Err(BridgeError::new(ErrorKind::Conflict)),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            // A prepared 0000 socket intentionally denies every rootless
            // connector, including its package owner. The cleanup CLI may
            // remove it only after validating the exact package-owned PID file
            // and proving that PID absent. Ordinary bind recovery never treats
            // EACCES as evidence that a listener is stale.
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && dead_pid_verified
                    && before.st_mode() & 0o7777 == 0o000 => {}
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        }
        let after = stale_socket_metadata(path, package_uid)?;
        if !same_object(&before, &after) {
            return Err(BridgeError::unsafe_runtime());
        }
        fs::remove_file(path).map_err(|_| BridgeError::unsafe_runtime())
    }

    fn connect_with_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
        let path_bytes = path.as_os_str().as_bytes();
        // sockaddr_un paths are NUL-terminated. Abstract sockets are outside the
        // fixed filesystem contract used by this bridge.
        // SAFETY: zero is a valid initial representation for sockaddr_un.
        let mut address = unsafe { mem::zeroed::<libc::sockaddr_un>() };
        let address_capacity = address.sun_path.len();
        if path_bytes.is_empty() || path_bytes.contains(&0) || path_bytes.len() >= address_capacity
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Unix socket path",
            ));
        }

        // SAFETY: socket returns a new descriptor or a negative errno sentinel.
        let descriptor = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ownership of the newly-created descriptor moves to stream.
        let stream = unsafe { UnixStream::from_raw_fd(descriptor) };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (destination, source) in address.sun_path.iter_mut().zip(path_bytes) {
            *destination = *source as libc::c_char;
        }

        // SAFETY: address is initialized, the descriptor is live, and the full
        // Linux sockaddr_un size includes the terminating zero from initialization.
        let connected = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if connected != 0 {
            let error = io::Error::last_os_error();
            if !matches!(
                error.raw_os_error(),
                Some(libc::EINPROGRESS | libc::EAGAIN | libc::EALREADY)
            ) {
                return Err(error);
            }
            wait_for_connect(&stream, timeout)?;
        }
        stream.set_nonblocking(false)?;
        Ok(stream)
    }

    fn wait_for_connect(stream: &UnixStream, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid timeout"))?;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Unix socket connect timed out",
                ));
            }
            let remaining = deadline.duration_since(now);
            let timeout_ms =
                remaining.as_millis().max(1).min(libc::c_int::MAX as u128) as libc::c_int;
            let mut descriptor = libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: descriptor points to one initialized pollfd for this call.
            let status = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if status == 0 {
                continue;
            }
            if status < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let mut socket_error: libc::c_int = 0;
            let mut length = mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: getsockopt writes one c_int to the initialized output.
            let status = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut socket_error as *mut libc::c_int).cast(),
                    &mut length,
                )
            };
            if status != 0 {
                return Err(io::Error::last_os_error());
            }
            if length as usize != mem::size_of::<libc::c_int>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid SO_ERROR response",
                ));
            }
            return if socket_error == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(socket_error))
            };
        }
    }

    fn set_socket_contract(path: &Path, package_uid: u32) -> BridgeResult<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| BridgeError::unsafe_runtime())?;
        socket_metadata(path, package_uid)?;
        Ok(())
    }

    fn remove_new_socket(path: &Path, package_uid: u32) {
        if let Ok(metadata) = fs::symlink_metadata(path)
            && metadata.file_type().is_socket()
            && metadata.st_uid() == package_uid
        {
            let _ = fs::remove_file(path);
        }
    }

    fn stale_socket_metadata(path: &Path, package_uid: u32) -> BridgeResult<fs::Metadata> {
        let metadata = fs::symlink_metadata(path).map_err(|_| BridgeError::unsafe_runtime())?;
        let permission = metadata.st_mode() & 0o7777;
        let is_restricted_bind = permission == 0o000;
        let is_final_socket = permission == 0o600;
        if package_uid == 0
            || !metadata.file_type().is_socket()
            || metadata.st_uid() != package_uid
            || metadata.st_nlink() != 1
            || (!is_restricted_bind && !is_final_socket)
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(metadata)
    }

    fn same_object(first: &fs::Metadata, second: &fs::Metadata) -> bool {
        first.st_dev() == second.st_dev()
            && first.st_ino() == second.st_ino()
            && first.st_uid() == second.st_uid()
            && first.st_gid() == second.st_gid()
            && first.st_mode() == second.st_mode()
            && first.st_nlink() == second.st_nlink()
    }

    fn same_inode(first: &fs::Metadata, second: &fs::Metadata) -> bool {
        first.st_dev() == second.st_dev() && first.st_ino() == second.st_ino()
    }

    struct UmaskGuard {
        previous: libc::mode_t,
    }

    impl UmaskGuard {
        fn replace(mode: libc::mode_t) -> Self {
            // SAFETY: umask has no pointer arguments. The caller holds the
            // module lock and production calls this before accepting clients.
            let previous = unsafe { libc::umask(mode) };
            Self { previous }
        }
    }

    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            // SAFETY: restoring the value returned by umask is always valid.
            unsafe {
                libc::umask(self.previous);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod linux_runtime {
    use super::*;

    pub(super) fn clear_environment() -> BridgeResult<()> {
        Err(BridgeError::unsafe_runtime())
    }

    pub(super) fn identity_state() -> BridgeResult<IdentityState> {
        Err(BridgeError::unsafe_runtime())
    }
}

struct CapturedOutput {
    status_success: bool,
    stdout: Zeroizing<Vec<u8>>,
}

#[cfg(target_os = "linux")]
fn capture_bounded_command(
    command: &mut Command,
    maximum_stdout: usize,
    maximum_stderr: usize,
    timeout: Duration,
    input: Option<&[u8]>,
) -> BridgeResult<CapturedOutput> {
    linux_process::capture_bounded_command(command, maximum_stdout, maximum_stderr, timeout, input)
}

#[cfg(not(target_os = "linux"))]
fn capture_bounded_command(
    _command: &mut Command,
    _maximum_stdout: usize,
    _maximum_stderr: usize,
    _timeout: Duration,
    _input: Option<&[u8]>,
) -> BridgeResult<CapturedOutput> {
    Err(BridgeError::unsafe_runtime())
}

#[cfg(target_os = "linux")]
mod linux_process {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus};

    const POLL_SLICE: Duration = Duration::from_millis(25);

    pub(super) fn capture_bounded_command(
        command: &mut Command,
        maximum_stdout: usize,
        maximum_stderr: usize,
        timeout: Duration,
        input: Option<&[u8]>,
    ) -> BridgeResult<CapturedOutput> {
        if maximum_stdout == 0
            || maximum_stderr == 0
            || timeout.is_zero()
            || input.is_some_and(|value| value.len() > MAX_SECRET_BYTES)
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setpgid is async-signal-safe and the callback performs no
        // allocation or synchronization between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }

        let mut child = command
            .spawn()
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        match capture_child(&mut child, maximum_stdout, maximum_stderr, timeout, input) {
            Ok(output) => Ok(output),
            Err(error) => {
                terminate_process_group(&mut child);
                Err(error)
            }
        }
    }

    pub(super) fn capture_queued_mutation_command(
        command: &mut Command,
        maximum_stdout: usize,
        input: Option<&[u8]>,
        termination_requested: &AtomicBool,
    ) -> BridgeResult<CapturedOutput> {
        if maximum_stdout == 0
            || input.is_some_and(|value| value.len() > MAX_SECRET_BYTES)
            || termination_requested.load(AtomicOrdering::Acquire)
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // The manager and everything it starts remain in a dedicated process
        // group. Normal queued work has no deadline, but consumer shutdown can
        // therefore terminate the complete in-flight operation without relying
        // on a runner lock that may not exist yet.
        let consumer_pid = unsafe { libc::getpid() };
        // SAFETY: setpgid, prctl, and getppid are async-signal-safe and the
        // callback performs no allocation or synchronization between fork and
        // exec. The parent comparison closes PR_SET_PDEATHSIG's setup race.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != consumer_pid {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }

        let mut child = command
            .spawn()
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        let process_group = match libc::pid_t::try_from(child.id()) {
            Ok(process_group) => process_group,
            Err(_) => {
                terminate_process_group(&mut child);
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }
        };
        match capture_queued_child(&mut child, maximum_stdout, input, termination_requested) {
            Ok(output) => match process_group_exists(process_group) {
                Ok(false) => Ok(output),
                Ok(true) | Err(_) => {
                    terminate_queued_process_group(&mut child, process_group, false);
                    Err(BridgeError::new(ErrorKind::Unavailable))
                }
            },
            Err(error) => {
                let cooperative = termination_requested.load(AtomicOrdering::Acquire);
                terminate_queued_process_group(&mut child, process_group, cooperative);
                Err(error)
            }
        }
    }

    fn capture_queued_child(
        child: &mut Child,
        maximum_stdout: usize,
        input: Option<&[u8]>,
        termination_requested: &AtomicBool,
    ) -> BridgeResult<CapturedOutput> {
        let mut stdout = child.stdout.take().ok_or_else(BridgeError::internal)?;
        let mut stdin = if input.is_some() {
            Some(child.stdin.take().ok_or_else(BridgeError::internal)?)
        } else {
            None
        };
        set_nonblocking(stdout.as_raw_fd())?;
        if let Some(writer) = &stdin {
            set_nonblocking(writer.as_raw_fd())?;
        }

        let mut input_bytes = Zeroizing::new(Vec::new());
        if let Some(input) = input {
            input_bytes
                .try_reserve_exact(input.len().saturating_add(1))
                .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
            input_bytes.extend_from_slice(input);
            input_bytes.push(b'\n');
        }
        let mut input_offset = 0_usize;
        let mut stdout_bytes = Zeroizing::new(Vec::with_capacity(maximum_stdout.min(8192)));
        let mut stdout_eof = false;

        loop {
            if termination_requested.load(AtomicOrdering::Acquire) {
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }
            stdout_eof |= drain_pipe(&mut stdout, &mut stdout_bytes, maximum_stdout)?;
            write_input(&mut stdin, &input_bytes, &mut input_offset)?;

            if let Some(status) = child
                .try_wait()
                .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?
            {
                stdout_eof |= drain_pipe(&mut stdout, &mut stdout_bytes, maximum_stdout)?;
                if stdout_eof && input_offset == input_bytes.len() && stdin.is_none() {
                    return completed_output(status, stdout_bytes);
                }
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }

            poll_queued_io(&stdout, stdout_eof, stdin.as_ref())?;
        }
    }

    fn capture_child(
        child: &mut Child,
        maximum_stdout: usize,
        maximum_stderr: usize,
        timeout: Duration,
        input: Option<&[u8]>,
    ) -> BridgeResult<CapturedOutput> {
        let mut stdout = child.stdout.take().ok_or_else(BridgeError::internal)?;
        let mut stderr = child.stderr.take().ok_or_else(BridgeError::internal)?;
        let mut stdin = if input.is_some() {
            Some(child.stdin.take().ok_or_else(BridgeError::internal)?)
        } else {
            None
        };
        set_nonblocking(stdout.as_raw_fd())?;
        set_nonblocking(stderr.as_raw_fd())?;
        if let Some(writer) = &stdin {
            set_nonblocking(writer.as_raw_fd())?;
        }

        let mut input_bytes = Zeroizing::new(Vec::new());
        if let Some(input) = input {
            input_bytes
                .try_reserve_exact(input.len().saturating_add(1))
                .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
            input_bytes.extend_from_slice(input);
            input_bytes.push(b'\n');
        }
        let mut input_offset = 0_usize;
        let mut stdout_bytes = Zeroizing::new(Vec::with_capacity(maximum_stdout.min(8192)));
        let mut stderr_bytes = Zeroizing::new(Vec::with_capacity(maximum_stderr.min(8192)));
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;

        loop {
            stdout_eof |= drain_pipe(&mut stdout, &mut stdout_bytes, maximum_stdout)?;
            stderr_eof |= drain_pipe(&mut stderr, &mut stderr_bytes, maximum_stderr)?;
            write_input(&mut stdin, &input_bytes, &mut input_offset)?;

            if let Some(status) = child
                .try_wait()
                .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?
            {
                // Recheck after observing exit so buffered bytes and ordinary
                // pipe EOF cannot be mistaken for an inherited descriptor.
                stdout_eof |= drain_pipe(&mut stdout, &mut stdout_bytes, maximum_stdout)?;
                stderr_eof |= drain_pipe(&mut stderr, &mut stderr_bytes, maximum_stderr)?;
                if stdout_eof && stderr_eof && input_offset == input_bytes.len() && stdin.is_none()
                {
                    return completed_output(status, stdout_bytes);
                }
                if input_offset != input_bytes.len() || stdin.is_some() {
                    return Err(BridgeError::new(ErrorKind::Unavailable));
                }

                // waitpid can become observable just before the final pipe
                // hangup on some kernels. Poll once, within both the existing
                // deadline and one ordinary I/O slice, then perform the final
                // bounded drain. A descendant that retained either descriptor
                // still fails closed after that single grace slice.
                let now = Instant::now();
                if now < deadline {
                    poll_child_io(
                        &stdout,
                        stdout_eof,
                        &stderr,
                        stderr_eof,
                        None,
                        deadline.duration_since(now).min(POLL_SLICE),
                    )?;
                    stdout_eof |= drain_pipe(&mut stdout, &mut stdout_bytes, maximum_stdout)?;
                    stderr_eof |= drain_pipe(&mut stderr, &mut stderr_bytes, maximum_stderr)?;
                    if stdout_eof && stderr_eof {
                        return completed_output(status, stdout_bytes);
                    }
                }
                // A descendant retained a pipe, or the hangup did not settle
                // within the single grace slice. Do not wait any further.
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }
            poll_child_io(
                &stdout,
                stdout_eof,
                &stderr,
                stderr_eof,
                stdin.as_ref(),
                deadline.duration_since(now).min(POLL_SLICE),
            )?;
        }
    }

    fn completed_output(
        status: ExitStatus,
        stdout: Zeroizing<Vec<u8>>,
    ) -> BridgeResult<CapturedOutput> {
        Ok(CapturedOutput {
            status_success: status.success(),
            stdout,
        })
    }

    fn drain_pipe(
        reader: &mut impl Read,
        output: &mut Zeroizing<Vec<u8>>,
        maximum: usize,
    ) -> BridgeResult<bool> {
        let mut chunk = Zeroizing::new([0_u8; 8192]);
        loop {
            match reader.read(&mut chunk[..]) {
                Ok(0) => return Ok(true),
                Ok(length) => {
                    if length > maximum.saturating_sub(output.len()) {
                        return Err(BridgeError::new(ErrorKind::Unavailable));
                    }
                    output.extend_from_slice(&chunk[..length]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(_) => return Err(BridgeError::new(ErrorKind::Unavailable)),
            }
        }
    }

    fn write_input(
        writer: &mut Option<ChildStdin>,
        input: &[u8],
        offset: &mut usize,
    ) -> BridgeResult<()> {
        while *offset < input.len() {
            let Some(writer) = writer.as_mut() else {
                return Err(BridgeError::new(ErrorKind::Unavailable));
            };
            match writer.write(&input[*offset..]) {
                Ok(0) => return Err(BridgeError::new(ErrorKind::Unavailable)),
                Ok(length) => *offset += length,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(_) => return Err(BridgeError::new(ErrorKind::Unavailable)),
            }
        }
        if *offset == input.len() {
            drop(writer.take());
        }
        Ok(())
    }

    fn poll_child_io(
        stdout: &ChildStdout,
        stdout_eof: bool,
        stderr: &ChildStderr,
        stderr_eof: bool,
        stdin: Option<&ChildStdin>,
        timeout: Duration,
    ) -> BridgeResult<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: if stdout_eof { -1 } else { stdout.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if stderr_eof { -1 } else { stderr.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stdin.map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLOUT,
                revents: 0,
            },
        ];
        let timeout_ms = timeout.as_millis().max(1).min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: descriptors is an initialized three-element pollfd array.
        let status = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if status < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }
        }
        Ok(())
    }

    fn poll_queued_io(
        stdout: &ChildStdout,
        stdout_eof: bool,
        stdin: Option<&ChildStdin>,
    ) -> BridgeResult<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: if stdout_eof { -1 } else { stdout.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stdin.map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLOUT,
                revents: 0,
            },
        ];
        // There is deliberately no operation deadline. This short poll slice
        // exists only so a controller signal is observed promptly.
        let timeout_ms = POLL_SLICE.as_millis() as libc::c_int;
        // SAFETY: descriptors is an initialized two-element pollfd array.
        let status = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if status < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }
        }
        Ok(())
    }

    fn set_nonblocking(descriptor: libc::c_int) -> BridgeResult<()> {
        // SAFETY: fcntl reads flags from a live pipe descriptor.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags < 0 {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        // SAFETY: fcntl updates flags on the same live descriptor.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        Ok(())
    }

    fn terminate_process_group(child: &mut Child) {
        if let Ok(process_group) = libc::pid_t::try_from(child.id()) {
            // SAFETY: the child created this process group in pre_exec. SIGKILL
            // is used only for the bounded helper and any pipe-holding children.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
        // Fall back to the direct child if the group disappeared or changed.
        let _ = child.kill();
        let _ = child.wait();
    }

    fn terminate_queued_process_group(
        child: &mut Child,
        process_group: libc::pid_t,
        cooperative: bool,
    ) {
        let mut direct_reaped = matches!(child.try_wait(), Ok(Some(_)));
        signal_process_group(
            process_group,
            if cooperative {
                libc::SIGTERM
            } else {
                libc::SIGKILL
            },
        );

        loop {
            if !direct_reaped {
                direct_reaped = matches!(child.try_wait(), Ok(Some(_)));
            }
            if direct_reaped {
                reap_adopted_group_children(process_group);
            }
            if !process_group_exists(process_group).unwrap_or(true) {
                if !direct_reaped {
                    let _ = child.wait();
                }
                reap_adopted_group_children(process_group);
                return;
            }
            if cooperative {
                // Keep supervising without imposing an operation timeout or
                // bypassing the manager's TERM cleanup. DSM's outer lifecycle
                // timeout will fail closed if the group refuses to terminate.
                signal_process_group(process_group, libc::SIGTERM);
            } else {
                signal_process_group(process_group, libc::SIGKILL);
                let _ = child.kill();
            }
            std::thread::sleep(POLL_SLICE);
        }
    }

    fn signal_process_group(process_group: libc::pid_t, signal: libc::c_int) {
        // SAFETY: the queued child created this positive process group in
        // pre_exec; a negative PID targets that complete group.
        unsafe {
            libc::kill(-process_group, signal);
        }
    }

    fn process_group_exists(process_group: libc::pid_t) -> BridgeResult<bool> {
        // SAFETY: signal zero performs a permission/liveness probe only.
        if unsafe { libc::kill(-process_group, 0) } == 0 {
            return Ok(true);
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(BridgeError::new(ErrorKind::Unavailable)),
        }
    }

    fn reap_adopted_group_children(process_group: libc::pid_t) {
        loop {
            let mut status = 0;
            // SAFETY: the consumer is a child subreaper; negative waitpid
            // reaps only adopted children from this manager process group.
            let waited = unsafe { libc::waitpid(-process_group, &mut status, libc::WNOHANG) };
            if waited <= 0 {
                return;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn capture_queued_mutation_command(
    command: &mut Command,
    maximum_stdout: usize,
    input: Option<&[u8]>,
    termination_requested: &AtomicBool,
) -> BridgeResult<CapturedOutput> {
    linux_process::capture_queued_mutation_command(
        command,
        maximum_stdout,
        input,
        termination_requested,
    )
}

fn issue_csrf_token(
    key: &[u8],
    session_binding: &[u8; 32],
    now: u64,
    nonce: &[u8; 16],
    lifetime_seconds: u64,
) -> BridgeResult<String> {
    if key.len() != 32 || !(60..=900).contains(&lifetime_seconds) {
        return Err(BridgeError::unsafe_runtime());
    }
    let expires = now
        .checked_add(lifetime_seconds)
        .ok_or_else(BridgeError::internal)?;
    let nonce_hex = hex_encode(nonce);
    let message = csrf_message(now, expires, &nonce_hex, session_binding);
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| BridgeError::internal())?;
    mac.update(message.as_bytes());
    let signature = mac.finalize().into_bytes();
    Ok(format!(
        "v1.{now}.{expires}.{nonce_hex}.{}",
        hex_encode(&signature)
    ))
}

fn verify_csrf_token(
    token: &str,
    key: &[u8],
    session_binding: &[u8; 32],
    now: u64,
    lifetime_seconds: u64,
) -> BridgeResult<()> {
    if token.len() > MAX_CSRF_BYTES || key.len() != 32 || !(60..=900).contains(&lifetime_seconds) {
        return Err(BridgeError::new(ErrorKind::CsrfRejected));
    }
    let components: Vec<&str> = token.split('.').collect();
    if components.len() != 5 || components[0] != "v1" {
        return Err(BridgeError::new(ErrorKind::CsrfRejected));
    }
    let issued = parse_canonical_u64(components[1])
        .map_err(|_| BridgeError::new(ErrorKind::CsrfRejected))?;
    let expires = parse_canonical_u64(components[2])
        .map_err(|_| BridgeError::new(ErrorKind::CsrfRejected))?;
    if expires.checked_sub(issued) != Some(lifetime_seconds)
        || issued > now.saturating_add(CLOCK_SKEW_SECONDS)
        || expires <= now
    {
        return Err(BridgeError::new(ErrorKind::CsrfRejected));
    }
    let nonce = hex_decode_exact::<16>(components[3])
        .ok_or_else(|| BridgeError::new(ErrorKind::CsrfRejected))?;
    let supplied_signature = hex_decode_exact::<32>(components[4])
        .ok_or_else(|| BridgeError::new(ErrorKind::CsrfRejected))?;
    let nonce_hex = hex_encode(&nonce);
    let message = csrf_message(issued, expires, &nonce_hex, session_binding);
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| BridgeError::new(ErrorKind::CsrfRejected))?;
    mac.update(message.as_bytes());
    let expected = mac.finalize().into_bytes();
    if !constant_time_equal(&expected, &supplied_signature) {
        return Err(BridgeError::new(ErrorKind::CsrfRejected));
    }
    Ok(())
}

fn csrf_message(issued: u64, expires: u64, nonce_hex: &str, session_binding: &[u8; 32]) -> String {
    format!(
        "sdsync-dsm-csrf-v1\n{issued}\n{expires}\n{nonce_hex}\n{}",
        hex_encode(session_binding)
    )
}

fn parse_canonical_u64(value: &str) -> BridgeResult<u64> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(BridgeError::bad_request());
    }
    value.parse().map_err(|_| BridgeError::bad_request())
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn hex_decode_exact<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 {
        return None;
    }
    let mut decoded = [0_u8; N];
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for (output, pair) in decoded.iter_mut().zip(pairs) {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        *output = (high << 4) | low;
    }
    Some(decoded)
}

fn read_manager_arguments(action: &ReadAction) -> BridgeResult<Vec<OsString>> {
    let arguments = match action {
        ReadAction::Snapshot => vec!["api".into(), "snapshot".into()],
        ReadAction::Logs { lines, source } => {
            let scan_lines = lines
                .saturating_mul(16)
                .max(lines.saturating_add(128))
                .min(1000);
            vec![
                "api".into(),
                "logs".into(),
                "--lines".into(),
                scan_lines.to_string().into(),
                "--source".into(),
                source.as_str().into(),
            ]
        }
        ReadAction::Activity { lines } => vec![
            "api".into(),
            "activity".into(),
            "--lines".into(),
            lines.to_string().into(),
        ],
        ReadAction::Csrf
        | ReadAction::SourceDirectories { .. }
        | ReadAction::SourcePath { .. }
        | ReadAction::Result { .. }
        | ReadAction::RequestStatus { .. } => {
            return Err(BridgeError::internal());
        }
    };
    Ok(arguments)
}

fn mutation_manager_arguments(mutation: &Mutation) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec!["api".into()];
    match mutation {
        Mutation::ConfigureProfile(value) => {
            arguments.push("configure-profile".into());
            push_pair(&mut arguments, "--name", &value.name);
            push_pair(&mut arguments, "--source", &value.source);
            push_pair(&mut arguments, "--url", &value.url);
            push_pair(&mut arguments, "--username", &value.username);
            push_pair(&mut arguments, "--remote", &value.remote);
            push_pair(&mut arguments, "--compare", value.compare.as_str());
            push_pair(&mut arguments, "--jobs", &value.jobs.to_string());
            push_pair(&mut arguments, "--delete", bool_text(value.delete));
            push_pair(
                &mut arguments,
                "--max-delete",
                &value.max_delete.to_string(),
            );
            push_pair(&mut arguments, "--allow-http", bool_text(value.allow_http));
            push_pair(
                &mut arguments,
                "--allow-empty-source",
                bool_text(value.allow_empty_source),
            );
            arguments.push("--clear-excludes".into());
            for exclude in &value.excludes {
                push_pair(&mut arguments, "--exclude", exclude);
            }
            push_pair(&mut arguments, "--retries", &value.retries.to_string());
            push_pair(
                &mut arguments,
                "--timeout",
                &value.timeout_seconds.to_string(),
            );
            push_pair(
                &mut arguments,
                "--connect-timeout",
                &value.connect_timeout_seconds.to_string(),
            );
            let maximum_rate = value
                .max_rate_bytes_per_second
                .map_or_else(|| "none".to_owned(), |rate| rate.to_string());
            push_pair(&mut arguments, "--max-rate", &maximum_rate);
            if let Some(certificate) = &value.ca_certificate {
                push_pair(&mut arguments, "--ca-certificate", certificate);
            }
            push_pair(
                &mut arguments,
                "--danger-accept-invalid-certs",
                bool_text(value.danger_accept_invalid_certs),
            );
            push_pair(&mut arguments, "--verbose", &value.verbosity.to_string());
            push_pair(&mut arguments, "--quiet", bool_text(value.quiet));
            push_pair(&mut arguments, "--log-level", value.log_level.as_str());
            push_pair(&mut arguments, "--log-format", value.log_format.as_str());
            push_pair(&mut arguments, "--progress", value.progress.as_str());
            push_pair(&mut arguments, "--output", value.output.as_str());
            if let Some(remote_log_url) = &value.remote_log_url {
                push_pair(&mut arguments, "--remote-log-url", remote_log_url);
            }
            push_pair(
                &mut arguments,
                "--remote-log-mode",
                value.remote_log_mode.as_str(),
            );
            push_pair(&mut arguments, "--default", bool_text(value.make_default));
        }
        Mutation::RemoveProfile(value) => {
            arguments.extend(["remove-profile".into(), value.name.clone().into()]);
        }
        Mutation::SetDefault(value) => {
            arguments.extend(["set-default".into(), value.name.clone().into()]);
        }
        Mutation::SetSecret(value) => {
            arguments.push("set-secret".into());
            push_pair(&mut arguments, "--profile", &value.profile);
            push_pair(&mut arguments, "--kind", value.kind.as_str());
            push_pair(&mut arguments, "--mode", value.mode.as_str());
        }
        Mutation::TestProfileAuth(_) | Mutation::BrowseRemote(_) => {
            // These operations execute inside the unprivileged Rust service so
            // credentials never enter argv, environment, or manager output.
            return Vec::new();
        }
        Mutation::Schedule(value) => {
            arguments.push("schedule".into());
            push_pair(&mut arguments, "--enabled", bool_text(value.enabled));
            push_pair(
                &mut arguments,
                "--interval",
                &value.interval_seconds.to_string(),
            );
            push_pair(
                &mut arguments,
                "--allow-delete",
                bool_text(value.allow_delete),
            );
            push_pair(
                &mut arguments,
                "--max-total-delete",
                &value.max_total_delete.to_string(),
            );
        }
        Mutation::Routine(value) => {
            arguments.push("routine".into());
            push_pair(&mut arguments, "--profile", &value.profile);
            push_pair(&mut arguments, "--enabled", bool_text(value.enabled));
            push_pair(&mut arguments, "--action", value.action.as_str());
            push_pair(&mut arguments, "--mode", value.mode.as_str());
            match value.mode {
                RoutineMode::Interval => {
                    if let Some(interval_seconds) = value.interval_seconds {
                        push_pair(&mut arguments, "--interval", &interval_seconds.to_string());
                    }
                }
                RoutineMode::Daily => {
                    if let Some(weekdays) = &value.weekdays {
                        let weekdays = weekdays
                            .iter()
                            .map(u8::to_string)
                            .collect::<Vec<_>>()
                            .join(",");
                        push_pair(&mut arguments, "--weekdays", &weekdays);
                    }
                    if let Some(window_start) = &value.time_window_start {
                        push_pair(&mut arguments, "--time-window-start", window_start);
                    }
                    if let Some(window_end) = &value.time_window_end {
                        push_pair(&mut arguments, "--time-window-end", window_end);
                    }
                }
                RoutineMode::Realtime => {
                    if let Some(debounce_seconds) = value.debounce_seconds {
                        push_pair(
                            &mut arguments,
                            "--debounce-seconds",
                            &debounce_seconds.to_string(),
                        );
                    }
                    if let Some(poll_seconds) = value.poll_seconds {
                        push_pair(&mut arguments, "--poll-seconds", &poll_seconds.to_string());
                    }
                }
            }
            push_pair(
                &mut arguments,
                "--retry-count",
                &value.retry_count.to_string(),
            );
            push_pair(
                &mut arguments,
                "--retry-backoff-seconds",
                &value.retry_backoff_seconds.to_string(),
            );
            push_pair(
                &mut arguments,
                "--retry-exponential",
                bool_text(value.retry_exponential),
            );
            push_pair(
                &mut arguments,
                "--allow-delete",
                bool_text(value.allow_delete),
            );
            push_pair(
                &mut arguments,
                "--max-total-delete",
                &value.max_total_delete.to_string(),
            );
            for dependency in &value.depends_on {
                push_pair(&mut arguments, "--depends-on", dependency);
            }
        }
        Mutation::RemoveRoutine(value) => {
            arguments.extend(["remove-routine".into(), value.name.clone().into()]);
        }
        Mutation::AlertPolicy(value) => {
            arguments.push("alert-policy".into());
            push_pair(&mut arguments, "--enabled", bool_text(value.enabled));
            push_pair(&mut arguments, "--on-success", bool_text(value.on_success));
            push_pair(&mut arguments, "--on-failure", bool_text(value.on_failure));
            push_pair(
                &mut arguments,
                "--failure-threshold",
                &value.failure_threshold.to_string(),
            );
            push_pair(
                &mut arguments,
                "--cooldown",
                &value.cooldown_seconds.to_string(),
            );
        }
        Mutation::SecurityPolicy(value) => {
            arguments.push("security-policy".into());
            for (name, enabled) in [
                ("--require-https", value.require_https),
                ("--allow-interface-changes", value.allow_interface_changes),
                ("--allow-profile-changes", value.allow_profile_changes),
                ("--allow-secret-changes", value.allow_secret_changes),
                ("--allow-routine-changes", value.allow_routine_changes),
                (
                    "--allow-notification-changes",
                    value.allow_notification_changes,
                ),
                (
                    "--allow-operational-actions",
                    value.allow_operational_actions,
                ),
                ("--allow-http-targets", value.allow_http_targets),
                ("--allow-invalid-tls", value.allow_invalid_tls),
                ("--allow-destructive-sync", value.allow_destructive_sync),
                ("--allow-doctor-write-test", value.allow_doctor_write_test),
                ("--allow-remote-logging", value.allow_remote_logging),
                ("--allow-empty-source", value.allow_empty_source),
            ] {
                push_pair(&mut arguments, name, bool_text(enabled));
            }
            push_pair(
                &mut arguments,
                "--csrf-lifetime",
                &value.csrf_lifetime_seconds.to_string(),
            );
            push_pair(
                &mut arguments,
                "--result-retention",
                &value.result_retention_seconds.to_string(),
            );
            push_pair(
                &mut arguments,
                "--max-outstanding-jobs",
                &value.max_outstanding_jobs.to_string(),
            );
            for (name, level) in [
                ("--audit-log-level", value.audit_log_level),
                ("--bridge-log-level", value.bridge_log_level),
                ("--authentication-log-level", value.authentication_log_level),
                ("--security-log-level", value.security_log_level),
                ("--configuration-log-level", value.configuration_log_level),
                ("--secrets-log-level", value.secrets_log_level),
                ("--routines-log-level", value.routines_log_level),
                ("--operations-log-level", value.operations_log_level),
                ("--notifications-log-level", value.notifications_log_level),
                ("--sync-log-level", value.sync_log_level),
                ("--controller-log-level", value.controller_log_level),
                ("--scheduler-log-level", value.scheduler_log_level),
            ] {
                push_pair(&mut arguments, name, level.as_str());
            }
        }
        Mutation::ClientEvent(value) => {
            arguments.push("client-event".into());
            push_pair(&mut arguments, "--event", value.event.as_str());
        }
        Mutation::Action(value) => {
            arguments.push("action".into());
            push_pair(&mut arguments, "--kind", value.kind.as_str());
            push_pair(&mut arguments, "--scope", &value.scope);
            match value.kind {
                OperationalActionKind::Doctor => {
                    let level = value.level.unwrap_or(if value.write_test == Some(true) {
                        OperationalDoctorLevel::Extensive
                    } else {
                        OperationalDoctorLevel::Standard
                    });
                    push_pair(&mut arguments, "--level", level.as_str());
                    push_pair(
                        &mut arguments,
                        "--write-test",
                        bool_text(value.write_test.unwrap_or(false)),
                    );
                }
                OperationalActionKind::Plan | OperationalActionKind::Run => {
                    push_pair(
                        &mut arguments,
                        "--allow-delete",
                        bool_text(value.allow_delete.unwrap_or(false)),
                    );
                    if let Some(maximum) = value.max_total_delete {
                        push_pair(&mut arguments, "--max-total-delete", &maximum.to_string());
                    }
                }
            }
        }
    }
    arguments
}

fn push_pair(arguments: &mut Vec<OsString>, name: &str, value: &str) {
    arguments.push(name.into());
    arguments.push(value.into());
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn parse_and_sanitize_manager_json(
    bytes: &[u8],
    action: &ReadAction,
    exact_secret: Option<&[u8]>,
    runtime_policy: Option<&SecurityPolicyArgs>,
) -> BridgeResult<Vec<u8>> {
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    let expected_schema = match action {
        ReadAction::Snapshot => "sdsync.dsm-api.v1",
        ReadAction::Logs { .. } => "sdsync.dsm-logs.v1",
        ReadAction::Activity { .. } => "sdsync.dsm-activity.v1",
        ReadAction::Csrf
        | ReadAction::SourceDirectories { .. }
        | ReadAction::SourcePath { .. }
        | ReadAction::Result { .. }
        | ReadAction::RequestStatus { .. } => {
            return Err(BridgeError::internal());
        }
    };
    let root = value
        .as_object()
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    if root.get("schema").and_then(Value::as_str) != Some(expected_schema) {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    redact_secret_fields(&mut value, exact_secret);
    match action {
        ReadAction::Snapshot => {
            let root = value
                .as_object_mut()
                .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
            // This value is embedded by build.rs and cannot be influenced by
            // the package INFO file or any mutable DSM runtime state.
            root.insert(
                "package".to_owned(),
                json!({ "version": env!("SDSYNC_VERSION") }),
            );
            root.insert(
                "capabilities".to_owned(),
                json!({
                    "mutations": true,
                    "secrets": true,
                    "write_test": true,
                    "private_queue": true,
                    "source_browser": true,
                    "profile_connection_test": true,
                    "remote_browser": true,
                    "request_reconciliation": true,
                }),
            );
            if let Some(policy) = runtime_policy {
                root.insert(
                    "security_policy".to_owned(),
                    security_policy_snapshot_value(policy),
                );
            }
        }
        ReadAction::Logs {
            source,
            lines: requested,
        } => {
            let logs = value
                .get_mut("logs")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
            if *source != LogSource::All {
                logs.retain(|entry| {
                    entry.get("source").and_then(Value::as_str) == Some(source.as_str())
                });
            }
            if let Some(policy) = runtime_policy {
                for entry in logs {
                    let source = entry
                        .get("source")
                        .and_then(Value::as_str)
                        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?
                        .to_owned();
                    let lines = entry
                        .get_mut("lines")
                        .and_then(Value::as_array_mut)
                        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
                    lines.retain(|line| {
                        line.as_str().is_some_and(|line| {
                            log_line_visible_at_threshold(policy, &source, line)
                        })
                    });
                    let requested = usize::from(*requested);
                    if lines.len() > requested {
                        lines.drain(..lines.len() - requested);
                    }
                }
            }
        }
        ReadAction::Activity { .. } => {
            if let Some(policy) = runtime_policy {
                let events = value
                    .get_mut("events")
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
                events.retain(|event| {
                    let Some(category) = event.get("category").and_then(Value::as_str) else {
                        return false;
                    };
                    let Some(level) = event.get("level").and_then(Value::as_str) else {
                        return false;
                    };
                    let mandatory = event
                        .get("code")
                        .and_then(Value::as_str)
                        .is_some_and(|code| code.starts_with("audit."));
                    event_visible_at_threshold(policy, category, level, mandatory)
                });
            }
        }
        ReadAction::Csrf
        | ReadAction::SourceDirectories { .. }
        | ReadAction::SourcePath { .. }
        | ReadAction::Result { .. }
        | ReadAction::RequestStatus { .. } => {}
    }
    serde_json::to_vec(&value).map_err(|_| BridgeError::internal())
}

fn security_policy_snapshot_value(policy: &SecurityPolicyArgs) -> Value {
    json!({
        "schema": "sdsync.dsm-security-policy.v1",
        "policy_version": 1,
        "require_https": policy.require_https,
        "allow_interface_changes": policy.allow_interface_changes,
        "allow_profile_changes": policy.allow_profile_changes,
        "allow_secret_changes": policy.allow_secret_changes,
        "allow_routine_changes": policy.allow_routine_changes,
        "allow_notification_changes": policy.allow_notification_changes,
        "allow_operational_actions": policy.allow_operational_actions,
        "allow_http_targets": policy.allow_http_targets,
        "allow_invalid_tls": policy.allow_invalid_tls,
        "allow_destructive_sync": policy.allow_destructive_sync,
        "allow_doctor_write_test": policy.allow_doctor_write_test,
        "allow_remote_logging": policy.allow_remote_logging,
        "allow_empty_source": policy.allow_empty_source,
        "csrf_lifetime_seconds": policy.csrf_lifetime_seconds,
        "result_retention_seconds": policy.result_retention_seconds,
        "max_outstanding_jobs": policy.max_outstanding_jobs,
        "queue_limits": {
            "active_request_and_processing_jobs": policy.max_outstanding_jobs,
            "retained_terminal_responses": policy.max_outstanding_jobs,
            "worst_case_total_job_records": policy.max_outstanding_jobs * 2,
        },
        "log_levels": {
            "audit": policy.audit_log_level.as_str(),
            "bridge": policy.bridge_log_level.as_str(),
            "authentication": policy.authentication_log_level.as_str(),
            "security": policy.security_log_level.as_str(),
            "configuration": policy.configuration_log_level.as_str(),
            "secrets": policy.secrets_log_level.as_str(),
            "routines": policy.routines_log_level.as_str(),
            "operations": policy.operations_log_level.as_str(),
            "notifications": policy.notifications_log_level.as_str(),
            "sync": policy.sync_log_level.as_str(),
            "controller": policy.controller_log_level.as_str(),
            "scheduler": policy.scheduler_log_level.as_str(),
        },
        "immutable": {
            "administrator_only": true,
            "same_origin_cookie_authentication": true,
            "csrf_required_for_mutations": true,
            "session_bound_results": true,
            "secret_values_never_returned": true,
            "private_fixed_paths": true,
            "unix_peer_credentials_verified": true,
            "root_privileges": false,
            "setuid_or_capabilities": false,
            "fail_closed": true,
            "mandatory_minimal_audit": true,
        }
    })
}

fn redact_secret_fields(value: &mut Value, exact_secret: Option<&[u8]>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                let is_presence_flag = normalized.starts_with("has_");
                let sensitive_key = !is_presence_flag
                    && (normalized.contains("password")
                        || normalized.contains("secret")
                        || normalized.contains("synotoken")
                        || normalized == "token"
                        || normalized.ends_with("_token")
                        || normalized == "authorization"
                        || normalized == "cookie");
                if sensitive_key {
                    *child = Value::String("[redacted]".to_owned());
                } else {
                    redact_secret_fields(child, exact_secret);
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                redact_secret_fields(child, exact_secret);
            }
        }
        Value::String(text)
            if exact_secret.is_some_and(|secret| constant_time_equal(text.as_bytes(), secret)) =>
        {
            text.zeroize();
            *text = "[redacted]".to_owned();
        }
        _ => {}
    }
}

// Keep every immutable queue identity field explicit at the one serialization
// boundary; grouping them into a loosely validated map would make schema drift
// and secret-bearing fingerprint mistakes easier to miss.
#[allow(clippy::too_many_arguments)]
fn canonical_job_bytes(
    request_id: &str,
    client_request_id: &str,
    requested_by: &str,
    requested_uid: u32,
    session_binding: &[u8; 32],
    audit_transaction: &str,
    request_fingerprint: &str,
    issued_at_epoch: u64,
    mutation: &Mutation,
) -> BridgeResult<Vec<u8>> {
    if requested_uid == 0
        || !valid_server_job_id(audit_transaction)
        || !valid_request_fingerprint(request_fingerprint)
    {
        return Err(BridgeError::bad_request());
    }
    let value = json!({
        "schema": "sdsync.dsm-job.v1",
        "request_id": request_id,
        "client_request_id": client_request_id,
        "requested_by": requested_by,
        "requested_uid": requested_uid,
        "session_binding": hex_encode(session_binding),
        "audit_transaction": audit_transaction,
        "request_fingerprint": request_fingerprint,
        "issued_at_epoch": issued_at_epoch,
        "operation": mutation.operation_id(),
        "arguments": mutation.arguments_value()?,
    });
    let bytes = serde_json::to_vec(&value).map_err(|_| BridgeError::internal())?;
    if bytes.len() > MAX_JOB_BYTES {
        return Err(BridgeError::new(ErrorKind::PayloadTooLarge));
    }
    Ok(bytes)
}

fn validate_processing_job_path(request: &Path) -> BridgeResult<String> {
    if request.parent() != Some(Path::new(PROCESSING_DIR)) {
        return Err(BridgeError::bad_request());
    }
    let request_name = request
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(BridgeError::bad_request)?;
    if !request_name.ends_with(".json") {
        return Err(BridgeError::bad_request());
    }
    let request_id = &request_name[..request_name.len() - ".json".len()];
    if !valid_server_job_id(request_id) {
        return Err(BridgeError::bad_request());
    }
    Ok(request_id.to_owned())
}

fn validate_consumer_paths(request: &Path, response: &Path) -> BridgeResult<String> {
    let request_id = validate_processing_job_path(request)?;
    if response.parent() != Some(Path::new(RESPONSES_DIR)) {
        return Err(BridgeError::bad_request());
    }
    let request_name = request
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(BridgeError::bad_request)?;
    let response_name = response
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(BridgeError::bad_request)?;
    if request_name != response_name || !request_name.ends_with(".json") {
        return Err(BridgeError::bad_request());
    }
    Ok(request_id)
}

fn parse_manager_result(bytes: &[u8], exact_secret: Option<&[u8]>) -> BridgeResult<Value> {
    parse_manager_result_for_operation(bytes, exact_secret, None)
}

fn parse_manager_result_for_operation(
    bytes: &[u8],
    exact_secret: Option<&[u8]>,
    operation: Option<&str>,
) -> BridgeResult<Value> {
    if bytes.is_empty() || bytes.len() > MAX_MANAGER_OUTPUT_BYTES {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    if json_contains_sensitive_value(&value, exact_secret) {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    if matches!(operation, Some("test-profile-auth" | "browse-remote")) {
        validate_connection_manager_result(&value, operation.unwrap())?;
        return Ok(value);
    }
    let root = value
        .as_object()
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    const ALLOWED_FIELDS: &[&str] = &[
        "schema",
        "ok",
        "message",
        "code",
        "exit_code",
        "status",
        "scope",
        "output",
        "has_password",
        "has_totp",
        "has_remote_log_token",
    ];
    if root
        .keys()
        .any(|key| !ALLOWED_FIELDS.contains(&key.as_str()))
        || root.get("schema").and_then(Value::as_str) != Some("sdsync.dsm-result.v1")
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let ok = root
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    let message = root
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    validate_result_text(message, 65_536)?;

    let code = root.get("code");
    if ok {
        if code.is_some() {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
    } else {
        let code = code
            .and_then(Value::as_str)
            .filter(|code| {
                matches!(
                    *code,
                    "invalid_request"
                        | "not_configured"
                        | "unavailable"
                        | "unsafe_state"
                        | "busy"
                        | "forbidden"
                        | "bridge_required"
                        | "corrupt_config"
                        | "corrupt_state"
                        | "internal_error"
                        | "response_too_large"
                        | "operation_failed"
                )
            })
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        validate_result_text(code, 32)?;
    }

    let has_action_fields = ["status", "scope", "output"]
        .iter()
        .any(|field| root.contains_key(*field));
    if has_action_fields {
        if !["status", "scope", "output", "exit_code"]
            .iter()
            .all(|field| root.contains_key(*field))
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let status = root
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        if !matches!((ok, status), (true, "succeeded") | (false, "failed")) {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let scope = root
            .get("scope")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        if scope != "all" && validate_existing_name(scope).is_err() {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let output = root
            .get("output")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        validate_result_text(output, 65_536)?;
    }

    if let Some(exit_code) = root.get("exit_code")
        && exit_code.as_u64().is_none_or(|code| code > 255)
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }

    let presence_fields = ["has_password", "has_totp", "has_remote_log_token"];
    let presence_count = presence_fields
        .iter()
        .filter(|field| root.contains_key(**field))
        .count();
    if presence_count != 0
        && (presence_count != presence_fields.len()
            || !ok
            || presence_fields
                .iter()
                .any(|field| root.get(*field).and_then(Value::as_bool).is_none()))
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(value)
}

fn validate_connection_manager_result(value: &Value, operation: &str) -> BridgeResult<()> {
    let root = value
        .as_object()
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    if root.get("schema").and_then(Value::as_str) != Some("sdsync.dsm-result.v1") {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let ok = root
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    let message = root
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    validate_result_text(message, 2048)?;

    if !ok {
        const FAILURE_FIELDS: &[&str] = &["schema", "ok", "message", "code"];
        if root.len() != FAILURE_FIELDS.len()
            || root
                .keys()
                .any(|key| !FAILURE_FIELDS.contains(&key.as_str()))
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let code = root
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        let generic_internal_failure =
            code == "operation_failed" && message == "Operation could not be completed.";
        let valid_code = matches!(
            code,
            "file_station_connection_failed"
                | "file_station_totp_required"
                | "file_station_totp_rejected"
                | "file_station_authentication_failed"
                | "file_station_logout_failed"
        ) || generic_internal_failure
            || (operation == "browse-remote"
                && matches!(
                    code,
                    "file_station_listing_denied"
                        | "file_station_listing_failed"
                        | "file_station_denied_logout_failed"
                        | "file_station_listing_logout_failed"
                        | "file_station_operation_logout_failed"
                ));
        if !valid_code {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        return Ok(());
    }

    match operation {
        "test-profile-auth" => {
            const AUTH_FIELDS: &[&str] = &[
                "schema",
                "ok",
                "message",
                "connection_proof",
                "connection_proof_expires_at_epoch",
            ];
            let proof = root.get("connection_proof").and_then(Value::as_str);
            let expires = root
                .get("connection_proof_expires_at_epoch")
                .and_then(Value::as_u64);
            let proof_expires = proof
                .and_then(|value| value.split('.').nth(1))
                .and_then(|value| value.parse::<u64>().ok());
            if root.len() != AUTH_FIELDS.len()
                || root.keys().any(|key| !AUTH_FIELDS.contains(&key.as_str()))
                || message
                    != "Authentication succeeded and the temporary File Station session was closed."
                || !proof.is_some_and(valid_connection_proof_syntax)
                || expires.is_none_or(|epoch| epoch == 0)
                || proof_expires != expires
            {
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }
        }
        "browse-remote" => validate_remote_directory_result(root, message)?,
        _ => return Err(BridgeError::new(ErrorKind::Unavailable)),
    }
    Ok(())
}

fn validate_remote_directory_result(
    root: &serde_json::Map<String, Value>,
    message: &str,
) -> BridgeResult<()> {
    const LIST_FIELDS: &[&str] = &[
        "schema",
        "ok",
        "message",
        "directory_schema",
        "current",
        "directories",
        "truncated",
    ];
    if root.len() != LIST_FIELDS.len()
        || root.keys().any(|key| !LIST_FIELDS.contains(&key.as_str()))
        || message != "File Station directories loaded."
        || root.get("directory_schema").and_then(Value::as_str)
            != Some("sdsync.dsm-remote-directories.v1")
        || root.get("truncated").and_then(Value::as_bool).is_none()
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let current = root
        .get("current")
        .and_then(Value::as_str)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    validate_remote_browser_parent(current)
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    let directories = root
        .get("directories")
        .and_then(Value::as_array)
        .filter(|values| values.len() <= 500)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    let mut seen = BTreeSet::new();
    for directory in directories {
        let directory = directory
            .as_object()
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        if directory.len() != 2
            || directory
                .keys()
                .any(|key| !matches!(key.as_str(), "name" | "path"))
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let name = directory
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        let path = directory
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
        if name.is_empty()
            || name.len() > 255
            || matches!(name, "." | "..")
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let expected = if current == "/" {
            format!("/{name}")
        } else {
            format!("{current}/{name}")
        };
        if path != expected
            || validate_remote_browser_parent(path).is_err()
            || !seen.insert(path.to_owned())
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
    }
    Ok(())
}

fn validate_set_secret_manager_result(value: &Value) -> BridgeResult<()> {
    let root = value
        .as_object()
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    const EXACT_FIELDS: &[&str] = &[
        "schema",
        "ok",
        "message",
        "has_password",
        "has_totp",
        "has_remote_log_token",
    ];
    if root.len() != EXACT_FIELDS.len()
        || root.keys().any(|key| !EXACT_FIELDS.contains(&key.as_str()))
        || root.get("schema").and_then(Value::as_str) != Some("sdsync.dsm-result.v1")
        || root.get("ok").and_then(Value::as_bool) != Some(true)
        || root.get("message").and_then(Value::as_str) != Some("secret state updated")
        || ["has_password", "has_totp", "has_remote_log_token"]
            .iter()
            .any(|field| root.get(*field).and_then(Value::as_bool).is_none())
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(())
}

fn validate_result_text(value: &str, maximum: usize) -> BridgeResult<()> {
    if value.len() > maximum || value.contains('\0') {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(())
}

fn canonical_queued_response_bytes(
    job: &ParsedJob,
    completed_at_epoch: u64,
    result: &Value,
    audit_pending: bool,
) -> BridgeResult<Vec<u8>> {
    if completed_at_epoch < job.issued_at_epoch {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let result_bytes =
        serde_json::to_vec(result).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    parse_manager_result_for_operation(&result_bytes, None, Some(job.mutation.operation_id()))?;
    let audit_terminal_state = if result.get("ok").and_then(Value::as_bool) == Some(true) {
        "succeeded"
    } else {
        "failed"
    };
    let bytes = serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-queued-response.v2",
        "job_id": job.request_id,
        "operation": job.mutation.operation_id(),
        "client_request_id": job.client_request_id,
        "requested_by": job.requested_by,
        "requested_uid": job.requested_uid,
        "session_binding": hex_encode(&job.session_binding),
        "request_fingerprint": job.request_fingerprint,
        "audit_transaction": job.audit_transaction,
        "audit_pending": audit_pending,
        "audit_terminal_state": audit_terminal_state,
        "issued_at_epoch": job.issued_at_epoch,
        "completed_at_epoch": completed_at_epoch,
        "result": result,
    }))
    .map_err(|_| BridgeError::internal())?;
    if bytes.len() > MAX_MANAGER_OUTPUT_BYTES {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(bytes)
}

fn parse_queued_response(
    bytes: &[u8],
    expected_job_id: &str,
) -> BridgeResult<ParsedQueuedResponse> {
    let response: RawQueuedResponse<'_> =
        serde_json::from_slice(bytes).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    let operation = match (response.schema, response.operation) {
        ("sdsync.dsm-queued-response.v1", None) => None,
        ("sdsync.dsm-queued-response.v2", Some(operation)) => Some(operation),
        _ => return Err(BridgeError::new(ErrorKind::Unavailable)),
    };
    if response.job_id != expected_job_id
        || !valid_server_job_id(response.job_id)
        || operation.is_some_and(|operation| !valid_mutation_operation(operation))
        || !valid_client_request_id(response.client_request_id)
        || !valid_authenticated_username(response.requested_by)
        || response.requested_uid == 0
        || !valid_request_fingerprint(response.request_fingerprint)
        || !valid_audit_transaction(response.audit_transaction)
        || !matches!(response.audit_terminal_state, "succeeded" | "failed")
        || response.completed_at_epoch < response.issued_at_epoch
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let session_binding = hex_decode_exact::<32>(response.session_binding)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    let result =
        parse_manager_result_for_operation(response.result.get().as_bytes(), None, operation)?;
    let expected_terminal = if result.get("ok").and_then(Value::as_bool) == Some(true) {
        "succeeded"
    } else {
        "failed"
    };
    if response.audit_terminal_state != expected_terminal {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(ParsedQueuedResponse {
        operation: operation.map(str::to_owned),
        client_request_id: response.client_request_id.to_owned(),
        requested_by: response.requested_by.to_owned(),
        requested_uid: response.requested_uid,
        session_binding,
        request_fingerprint: response.request_fingerprint.to_owned(),
        audit_transaction: response.audit_transaction.to_owned(),
        audit_pending: response.audit_pending,
        audit_terminal_state: response.audit_terminal_state.to_owned(),
        completed_at_epoch: response.completed_at_epoch,
        result,
    })
}

fn json_contains_sensitive_value(value: &Value, exact_secret: Option<&[u8]>) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, child)| {
            let normalized = key.to_ascii_lowercase().replace('-', "_");
            let sensitive_key = !normalized.starts_with("has_")
                && (normalized.contains("password")
                    || normalized.contains("secret")
                    || normalized.contains("synotoken")
                    || normalized == "token"
                    || normalized.ends_with("_token")
                    || normalized == "authorization"
                    || normalized == "cookie");
            sensitive_key || json_contains_sensitive_value(child, exact_secret)
        }),
        Value::Array(values) => values
            .iter()
            .any(|child| json_contains_sensitive_value(child, exact_secret)),
        Value::String(text) => {
            exact_secret.is_some_and(|secret| constant_time_equal(text.as_bytes(), secret))
        }
        _ => false,
    }
}

#[cfg(test)]
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| constant_time_equal(window, needle))
}

fn next_enqueue_sequence(previous: u64, wall_clock_micros: u64) -> BridgeResult<u64> {
    Ok(wall_clock_micros.max(
        previous
            .checked_add(1)
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?,
    ))
}

fn sortable_job_id(sequence: u64, random: &[u8; 16]) -> String {
    format!("{sequence:016x}{}", hex_encode(random))
}

fn generic_manager_result() -> Vec<u8> {
    br#"{"schema":"sdsync.dsm-result.v1","ok":false,"code":"operation_failed","message":"Operation could not be completed."}"#
        .to_vec()
}

fn generic_manager_result_value() -> Value {
    serde_json::from_slice(&generic_manager_result()).unwrap_or_else(|_| {
        json!({
            "schema": "sdsync.dsm-result.v1",
            "ok": false,
            "code": "operation_failed",
            "message": "Operation could not be completed.",
        })
    })
}

struct TerminalizedConsumeResult {
    value: Value,
    #[cfg_attr(not(test), allow(dead_code))]
    state: &'static str,
    audit_pending: bool,
}

fn terminalize_consume_result<F>(
    result: BridgeResult<Value>,
    mut record_terminal: F,
) -> TerminalizedConsumeResult
where
    F: FnMut(&str) -> BridgeResult<bool>,
{
    let (value, state) = match result {
        Ok(value) => {
            let state = if value.get("ok").and_then(Value::as_bool) == Some(true) {
                "succeeded"
            } else {
                "failed"
            };
            (value, state)
        }
        Err(_) => (generic_manager_result_value(), "failed"),
    };
    let audit_pending = record_terminal(state).unwrap_or(true);
    TerminalizedConsumeResult {
        value,
        state,
        audit_pending,
    }
}

#[cfg(target_os = "linux")]
fn manager_command(arguments: &[OsString], has_secret_input: bool) -> BridgeResult<Command> {
    linux_runtime::validate_package_manager()?;
    let mut command = Command::new(MANAGER_PATH);
    command
        .args(arguments)
        .env_clear()
        .envs(manager_command_environment());
    if has_secret_input {
        command.env("SDSYNC_DSM_EXACT_SECRET_INPUT", "true");
    }
    Ok(command)
}

#[cfg(target_os = "linux")]
fn run_read_manager(arguments: &[OsString]) -> BridgeResult<CapturedOutput> {
    let mut command = manager_command(arguments, false)?;
    capture_bounded_command(
        &mut command,
        MAX_MANAGER_OUTPUT_BYTES,
        MAX_HELPER_STDERR_BYTES,
        READ_MANAGER_TIMEOUT,
        None,
    )
}

#[cfg(target_os = "linux")]
fn wake_controller_after_enqueue() {
    // The canonical request is externally visible after its durability attempt
    // at this point. A wake failure must never turn accepted work (including a
    // durability-uncertain publication) into an ordinary error or invite a
    // duplicate dispatch. The controller's bounded polling fallback remains.
    let Ok(mut command) = manager_command(&["api".into(), "controller-wake".into()], false) else {
        return;
    };
    let _ = capture_bounded_command(&mut command, 1024, 1024, CONTROLLER_WAKE_TIMEOUT, None);
}

#[cfg(target_os = "linux")]
fn record_rejected_post(audit_actor: &str, audit_actor_uid: u32) -> BridgeResult<()> {
    record_rejected_operation(audit_actor, audit_actor_uid, "bridge")
}

#[cfg(target_os = "linux")]
fn record_rejected_operation(
    audit_actor: &str,
    audit_actor_uid: u32,
    origin: &str,
) -> BridgeResult<()> {
    if !valid_authenticated_username(audit_actor) || audit_actor_uid == 0 {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    if !matches!(origin, "bridge" | "controller") {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let mut command = manager_command(&["api".into(), "audit-rejected".into()], false)?;
    command.env("SDSYNC_DSM_AUDIT_ACTOR", audit_actor);
    command.env("SDSYNC_DSM_AUDIT_ACTOR_UID", audit_actor_uid.to_string());
    command.env("SDSYNC_DSM_AUDIT_ORIGIN", origin);
    let output = capture_bounded_command(
        &mut command,
        MAX_MANAGER_OUTPUT_BYTES,
        MAX_HELPER_STDERR_BYTES,
        READ_MANAGER_TIMEOUT,
        None,
    )?;
    if output.status_success {
        Ok(())
    } else {
        Err(BridgeError::new(ErrorKind::Unavailable))
    }
}

fn mutation_audit_operation(mutation: &Mutation) -> &'static str {
    match mutation {
        Mutation::SetSecret(value) => match (value.kind, value.mode) {
            (SecretKind::Password, SecretMode::Replace) => "set-password",
            (SecretKind::Password, SecretMode::Clear) => "remove-password",
            (SecretKind::Totp, SecretMode::Replace) => "set-totp",
            (SecretKind::Totp, SecretMode::Clear) => "remove-totp",
            (SecretKind::RemoteLogToken, SecretMode::Replace) => "set-remote-log-token",
            (SecretKind::RemoteLogToken, SecretMode::Clear) => "remove-remote-log-token",
        },
        Mutation::TestProfileAuth(_) => "test-profile-auth",
        Mutation::BrowseRemote(_) => "browse-remote",
        Mutation::ClientEvent(value) => value.event.as_str(),
        Mutation::Action(value) => value.kind.as_str(),
        _ => mutation.operation_id(),
    }
}

fn mutation_audit_profile(mutation: &Mutation) -> &str {
    match mutation {
        Mutation::ConfigureProfile(value) => &value.name,
        Mutation::RemoveProfile(value) | Mutation::SetDefault(value) => &value.name,
        Mutation::SetSecret(value) => &value.profile,
        Mutation::TestProfileAuth(value) => value.profile.as_deref().unwrap_or("none"),
        Mutation::BrowseRemote(value) => value.connection.profile.as_deref().unwrap_or("none"),
        Mutation::Routine(value) => &value.profile,
        Mutation::RemoveRoutine(value) => &value.name,
        Mutation::Action(value) => &value.scope,
        Mutation::Schedule(_)
        | Mutation::AlertPolicy(_)
        | Mutation::SecurityPolicy(_)
        | Mutation::ClientEvent(_) => "all",
    }
}

fn valid_audit_transaction(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_mutation_operation(value: &str) -> bool {
    matches!(
        value,
        "configure-profile"
            | "remove-profile"
            | "set-default"
            | "set-secret"
            | "test-profile-auth"
            | "browse-remote"
            | "schedule"
            | "routine"
            | "remove-routine"
            | "alert-policy"
            | "security-policy"
            | "client-event"
            | "action"
    )
}

fn valid_audit_operation(value: &str) -> bool {
    matches!(
        value,
        "configure-profile"
            | "remove-profile"
            | "set-default"
            | "set-secret"
            | "set-password"
            | "remove-password"
            | "set-totp"
            | "remove-totp"
            | "set-remote-log-token"
            | "remove-remote-log-token"
            | "test-profile-auth"
            | "browse-remote"
            | "schedule"
            | "routine"
            | "remove-routine"
            | "alert-policy"
            | "security-policy"
            | "interface-settings"
            | "session-notifications"
            | "rejected-post"
            | "doctor"
            | "plan"
            | "run"
    )
}

fn valid_audit_profile(value: &str) -> bool {
    matches!(value, "all" | "none") || validate_existing_name(value).is_ok()
}

fn valid_audit_origin(value: &str) -> bool {
    matches!(
        value,
        "bridge" | "cli" | "manager" | "scheduler" | "controller"
    )
}

fn validate_audit_identity(record: &AuditOutboxRecord) -> BridgeResult<()> {
    if record.schema != "sdsync.dsm-audit-outbox.v1"
        || !valid_audit_transaction(&record.transaction)
        || !valid_audit_operation(&record.operation)
        || !valid_audit_profile(&record.profile)
        || !valid_authenticated_username(&record.actor)
        || record.actor_uid == 0
        || !valid_audit_origin(&record.origin)
        || record
            .client_request_id
            .as_deref()
            .is_some_and(|value| !valid_client_request_id(value))
    {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(())
}

fn validate_audit_outbox_record(record: &AuditOutboxRecord) -> BridgeResult<()> {
    validate_audit_identity(record)?;
    if record
        .job_id
        .as_deref()
        .is_some_and(|value| !valid_server_job_id(value))
        || (record.origin == "bridge") != record.job_id.is_some()
        || (record.origin == "bridge") != record.client_request_id.is_some()
        || record.owner_pid <= 1
        || record.owner_start == 0
        || !valid_boot_id(&record.owner_boot)
    {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(())
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
            }
        })
}

#[cfg(target_os = "linux")]
fn record_audit_event(record: &AuditOutboxRecord, state: &str) -> BridgeResult<()> {
    validate_audit_outbox_record(record)?;
    if !matches!(
        state,
        "requested" | "succeeded" | "failed" | "outcome_unknown"
    ) {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let arguments = [
        "api".into(),
        "audit-event".into(),
        "--operation".into(),
        record.operation.clone().into(),
        "--state".into(),
        state.into(),
        "--profile".into(),
        record.profile.clone().into(),
    ];
    let mut command = manager_command(&arguments, false)?;
    command.env("SDSYNC_DSM_AUDIT_ACTOR", &record.actor);
    command.env("SDSYNC_DSM_AUDIT_ACTOR_UID", record.actor_uid.to_string());
    command.env("SDSYNC_DSM_AUDIT_ORIGIN", &record.origin);
    command.env("SDSYNC_DSM_AUDIT_TRANSACTION", &record.transaction);
    if let Some(client_request_id) = record.client_request_id.as_deref() {
        command.env("SDSYNC_DSM_CLIENT_REQUEST_ID", client_request_id);
    }
    let output = capture_bounded_command(
        &mut command,
        MAX_MANAGER_OUTPUT_BYTES,
        MAX_HELPER_STDERR_BYTES,
        READ_MANAGER_TIMEOUT,
        None,
    )?;
    if output.status_success {
        Ok(())
    } else {
        Err(BridgeError::new(ErrorKind::Unavailable))
    }
}

#[cfg(target_os = "linux")]
fn run_queued_mutation_manager(
    arguments: &[OsString],
    secret: Option<&[u8]>,
    termination_requested: &AtomicBool,
) -> BridgeResult<CapturedOutput> {
    let mut command = manager_command(arguments, secret.is_some())?;
    capture_queued_mutation_command(
        &mut command,
        MAX_MANAGER_OUTPUT_BYTES,
        secret,
        termination_requested,
    )
}

#[cfg(target_os = "linux")]
mod linux_files {
    use super::*;
    use std::os::fd::AsRawFd;
    #[cfg(test)]
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::linux::fs::MetadataExt;
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

    const NOFOLLOW_CLOEXEC: i32 = libc::O_NOFOLLOW | libc::O_CLOEXEC;
    const CGI_FAILURE_COALESCE_SECONDS: u64 = 30;
    const MAX_CGI_FAILURE_STATE_BYTES: u64 = 256;
    pub(super) const MAX_API_LOG_BYTES: u64 = 10 * 1024 * 1024;
    pub(super) const API_LOG_ROTATIONS: usize = 5;
    const MAX_CGI_FAILURE_RECORD_BYTES: usize = 512;

    struct StateFlock {
        file: File,
    }

    impl StateFlock {
        fn try_acquire(file: File) -> BridgeResult<Option<Self>> {
            let lock_status =
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if lock_status == 0 {
                return Ok(Some(Self { file }));
            }
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EAGAIN) => Ok(None),
                _ => Err(BridgeError::unsafe_runtime()),
            }
        }
    }

    impl Drop for StateFlock {
        fn drop(&mut self) {
            // O_CLOEXEC closes the descriptor at exec, but a concurrently
            // forked child can retain this open-file description until then.
            // Explicitly unlocking releases the lock from that shared
            // description before the next CGI attempts a nonblocking lock.
            // SAFETY: the guard owns a live File that successfully acquired
            // this flock. LOCK_UN is nonblocking, and the descriptor closes
            // immediately afterward if the kernel reports an unexpected error.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
    #[cfg(test)]
    thread_local! {
        static FAIL_AUDIT_READY_WRITE_ONCE: std::cell::Cell<bool> = const {
            std::cell::Cell::new(false)
        };
    }

    #[cfg(test)]
    pub(super) fn fail_next_audit_ready_write() {
        FAIL_AUDIT_READY_WRITE_ONCE.with(|flag| flag.set(true));
    }

    pub(super) fn record_pre_relay_cgi_failure(
        package_uid: u32,
        now: u64,
        stage: &str,
        code: &str,
        status: u16,
    ) -> BridgeResult<bool> {
        let policy = load_security_policy(package_uid)?;
        record_pre_relay_cgi_failure_under_policy_at(
            Path::new(LOG_ROOT),
            Path::new(API_LOG_PATH),
            Path::new(CGI_FAILURE_STATE_PATH),
            package_uid,
            now,
            stage,
            code,
            status,
            &policy,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_pre_relay_cgi_failure_at(
        log_root: &Path,
        api_log: &Path,
        state_path: &Path,
        package_uid: u32,
        now: u64,
        stage: &str,
        code: &str,
        status: u16,
    ) -> BridgeResult<bool> {
        record_pre_relay_cgi_failure_under_policy_at(
            log_root,
            api_log,
            state_path,
            package_uid,
            now,
            stage,
            code,
            status,
            &SecurityPolicyArgs::default(),
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_pre_relay_cgi_failure_with_policy_at(
        log_root: &Path,
        api_log: &Path,
        state_path: &Path,
        policy_path: &Path,
        package_uid: u32,
        now: u64,
        stage: &str,
        code: &str,
        status: u16,
    ) -> BridgeResult<bool> {
        let policy = load_security_policy_at(policy_path, package_uid)?;
        record_pre_relay_cgi_failure_under_policy_at(
            log_root,
            api_log,
            state_path,
            package_uid,
            now,
            stage,
            code,
            status,
            &policy,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_pre_relay_cgi_failure_under_policy_at(
        log_root: &Path,
        api_log: &Path,
        state_path: &Path,
        package_uid: u32,
        now: u64,
        stage: &str,
        code: &str,
        status: u16,
        policy: &SecurityPolicyArgs,
    ) -> BridgeResult<bool> {
        let category = cgi_failure_category(stage).ok_or_else(BridgeError::bad_request)?;
        if !event_visible_at_threshold(policy, category, "warn", false) {
            return Ok(false);
        }
        if now == 0
            || !matches!(status, 400 | 401 | 403 | 405 | 413 | 415 | 500 | 503)
            || !matches!(
                stage,
                "request"
                    | "cgi_identity"
                    | "dsm_authentication"
                    | "cgi_runtime"
                    | "bridge_connect"
                    | "bridge_io"
                    | "bridge_protocol"
            )
            || code.is_empty()
            || code.len() > 64
            || !code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(BridgeError::bad_request());
        }
        validate_private_directory(log_root, package_uid)?;
        let state_root = state_path
            .parent()
            .ok_or_else(BridgeError::unsafe_runtime)?;
        validate_private_directory(state_root, package_uid)?;

        let mut state_options = OpenOptions::new();
        state_options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW_CLOEXEC);
        let state = state_options
            .open(state_path)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let state_metadata = state
            .metadata()
            .map_err(|_| BridgeError::unsafe_runtime())?;
        if !state_metadata.file_type().is_file()
            || state_metadata.st_uid() != package_uid
            || state_metadata.st_mode() & 0o7777 != 0o600
            || state_metadata.st_nlink() != 1
            || state_metadata.len() > MAX_CGI_FAILURE_STATE_BYTES
        {
            return Err(BridgeError::unsafe_runtime());
        }
        // A CGI must never wait behind another failing request. The state file
        // is both the nonblocking coalescing lock and the bounded last-record
        // cache; the existing activity sink retains its own event-log lock.
        let Some(mut state) = StateFlock::try_acquire(state)? else {
            return Ok(false);
        };

        let mut previous = String::new();
        state
            .file
            .read_to_string(&mut previous)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        if !previous.is_empty() {
            let previous = previous
                .strip_suffix('\n')
                .ok_or_else(BridgeError::unsafe_runtime)?;
            let fields = previous.split('|').collect::<Vec<_>>();
            if fields.len() != 4 {
                return Err(BridgeError::unsafe_runtime());
            }
            let previous_epoch = fields[0]
                .parse::<u64>()
                .ok()
                .filter(|epoch| *epoch != 0)
                .ok_or_else(BridgeError::unsafe_runtime)?;
            if now.saturating_sub(previous_epoch) < CGI_FAILURE_COALESCE_SECONDS {
                return Ok(false);
            }
        }

        let mut record = serde_json::to_vec(&json!({
            "epoch": now,
            "level": "warn",
            "category": category,
            "event": "cgi_failure",
            "service": "synology-drive-sync",
            "stage": stage,
            "code": code,
            "status": status,
        }))
        .map_err(|_| BridgeError::internal())?;
        record.push(b'\n');
        if record.len() > MAX_CGI_FAILURE_RECORD_BYTES {
            return Err(BridgeError::internal());
        }

        rotate_private_api_log(log_root, api_log, package_uid, record.len() as u64)?;
        let mut log_options = OpenOptions::new();
        log_options
            .write(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW_CLOEXEC);
        let log = log_options
            .open(api_log)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let log_metadata = log.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
        if !log_metadata.file_type().is_file()
            || log_metadata.st_uid() != package_uid
            || log_metadata.st_mode() & 0o7777 != 0o600
            || log_metadata.st_nlink() != 1
        {
            return Err(BridgeError::unsafe_runtime());
        }
        write_single_record(&log, &record)?;

        let state_record = format!("{now}|{stage}|{code}|{status}\n");
        if state_record.len() as u64 > MAX_CGI_FAILURE_STATE_BYTES {
            return Err(BridgeError::internal());
        }
        state
            .file
            .set_len(0)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        state
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|_| BridgeError::unsafe_runtime())?;
        write_single_record(&state.file, state_record.as_bytes())?;
        Ok(true)
    }

    #[cfg(test)]
    pub(super) fn state_flock_unlocks_shared_description_at(path: &Path) -> BridgeResult<()> {
        let open = || {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .custom_flags(NOFOLLOW_CLOEXEC);
            options
                .open(path)
                .map_err(|_| BridgeError::unsafe_runtime())
        };
        let lock = StateFlock::try_acquire(open()?)?.ok_or_else(BridgeError::unsafe_runtime)?;
        // SAFETY: dup receives the live descriptor owned by the lock. A
        // successful result is immediately wrapped in OwnedFd below.
        let duplicated = unsafe { libc::dup(lock.file.as_raw_fd()) };
        if duplicated < 0 {
            return Err(BridgeError::unsafe_runtime());
        }
        // SAFETY: duplicated is a fresh nonnegative descriptor returned by
        // dup and has not been transferred or closed.
        let duplicated = unsafe { OwnedFd::from_raw_fd(duplicated) };

        drop(lock);
        let contender = open()?;
        let lock_status =
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        drop(duplicated);
        if lock_status != 0 {
            return Err(BridgeError::unsafe_runtime());
        }
        let unlock_status = unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_UN) };
        if unlock_status != 0 {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    fn rotated_path(base: &Path, index: usize) -> PathBuf {
        let mut path = base.as_os_str().to_os_string();
        path.push(format!(".{index}"));
        PathBuf::from(path)
    }

    fn private_log_metadata(path: &Path, package_uid: u32) -> BridgeResult<Option<fs::Metadata>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o7777 != 0o600
            || metadata.st_nlink() != 1
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(Some(metadata))
    }

    fn rotate_private_api_log(
        log_root: &Path,
        api_log: &Path,
        package_uid: u32,
        incoming: u64,
    ) -> BridgeResult<()> {
        if api_log.parent() != Some(log_root) || incoming == 0 || incoming > MAX_API_LOG_BYTES {
            return Err(BridgeError::unsafe_runtime());
        }
        let active = private_log_metadata(api_log, package_uid)?;
        for index in 1..=API_LOG_ROTATIONS {
            private_log_metadata(&rotated_path(api_log, index), package_uid)?;
        }
        if active
            .as_ref()
            .is_none_or(|metadata| metadata.len().saturating_add(incoming) <= MAX_API_LOG_BYTES)
        {
            return Ok(());
        }

        let oldest = rotated_path(api_log, API_LOG_ROTATIONS);
        if private_log_metadata(&oldest, package_uid)?.is_some() {
            fs::remove_file(&oldest).map_err(|_| BridgeError::unsafe_runtime())?;
        }
        for index in (2..=API_LOG_ROTATIONS).rev() {
            let previous = rotated_path(api_log, index - 1);
            if private_log_metadata(&previous, package_uid)?.is_some() {
                fs::rename(&previous, rotated_path(api_log, index))
                    .map_err(|_| BridgeError::unsafe_runtime())?;
            }
        }
        fs::rename(api_log, rotated_path(api_log, 1)).map_err(|_| BridgeError::unsafe_runtime())?;
        sync_directory(log_root)?;
        Ok(())
    }

    fn write_single_record(file: &File, record: &[u8]) -> BridgeResult<()> {
        // This is deliberately one bounded O_APPEND write so concurrent API
        // service output cannot be interleaved with a CGI diagnostic. A rare
        // EINTR/partial write is reported as best-effort failure rather than
        // retried into a potentially interleaved or duplicated record.
        let written = unsafe {
            libc::write(
                file.as_raw_fd(),
                record.as_ptr().cast::<libc::c_void>(),
                record.len(),
            )
        };
        if written == record.len() as isize {
            Ok(())
        } else {
            Err(BridgeError::unsafe_runtime())
        }
    }

    pub(super) fn durably_verify_audit_event(
        expected: &AuditOutboxRecord,
        expected_state: &str,
        package_uid: u32,
    ) -> BridgeResult<()> {
        durably_verify_audit_event_at(
            expected,
            expected_state,
            package_uid,
            Path::new(LOG_ROOT),
            Path::new(AUDIT_LOG_PATH),
            Path::new(ACTIVITY_LOG_PATH),
        )
    }

    pub(super) fn durably_verify_audit_event_at(
        expected: &AuditOutboxRecord,
        expected_state: &str,
        package_uid: u32,
        log_root: &Path,
        audit_log: &Path,
        activity_log: &Path,
    ) -> BridgeResult<()> {
        // Durable log verification binds the immutable audit identity. It is
        // intentionally independent from queue topology: the private verify
        // helper receives no job path, including for authenticated bridge
        // events, while the lifecycle outbox validator still requires a
        // bridge record to carry its exact job id.
        validate_audit_identity(expected)?;
        if !matches!(
            expected_state,
            "requested" | "succeeded" | "failed" | "outcome_unknown"
        ) {
            return Err(BridgeError::bad_request());
        }
        validate_private_directory(log_root, package_uid)?;

        let mut audit_found = false;
        for path in rotating_log_paths(audit_log, 5) {
            let Some((file, bytes)) = open_private_log_file(&path, package_uid, 11 * 1024 * 1024)?
            else {
                continue;
            };
            validate_canonical_log_records(&bytes, |line| {
                validate_audit_log_line(line).map(|_| ())
            })?;
            for line in bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                let parsed = validate_audit_log_line(line)?;
                if parsed.transaction == expected.transaction && parsed.state == expected_state {
                    if parsed.operation != expected.operation
                        || parsed.profile != expected.profile
                        || parsed.actor != expected.actor
                        || parsed.actor_uid != Some(expected.actor_uid)
                        || parsed.origin != expected.origin
                        || parsed.client_request_id != expected.client_request_id.as_deref()
                    {
                        return Err(BridgeError::unsafe_runtime());
                    }
                    audit_found = true;
                    file.sync_all().map_err(|_| BridgeError::unsafe_runtime())?;
                }
            }
        }

        let expected_code = format!("audit.{expected_state}");
        let expected_activity_state = if expected_state == "outcome_unknown" {
            "unavailable"
        } else {
            expected_state
        };
        let mut expected_message = format!(
            "Module {} {} [{}]",
            expected.operation, expected_state, expected.transaction
        );
        if let Some(client_request_id) = expected.client_request_id.as_deref() {
            expected_message.push_str(" request_id=");
            expected_message.push_str(client_request_id);
        }
        let mut activity_found = false;
        for path in rotating_log_paths(activity_log, 3) {
            let Some((file, bytes)) = open_private_log_file(&path, package_uid, 2 * 1024 * 1024)?
            else {
                continue;
            };
            validate_canonical_log_records(&bytes, |line| {
                validate_activity_log_line(line).map(|_| ())
            })?;
            for line in bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                let text = validate_activity_log_line(line)?;
                let fields: Vec<&str> = text.split('|').collect();
                match fields.len() {
                    // Released builds wrote five-field activity records. They
                    // remain readable history, but cannot satisfy a new audit
                    // transaction's exact durable-record proof.
                    5 => {
                        if fields[1] == expected_code && fields[4].contains(&expected.transaction) {
                            return Err(BridgeError::unsafe_runtime());
                        }
                    }
                    // The immediately preceding package format had category
                    // and level but no stable actor identity. It remains valid
                    // unrelated history, but cannot prove a new transaction.
                    7 => {
                        if fields[1] == expected_code && fields[6].contains(&expected.transaction) {
                            return Err(BridgeError::unsafe_runtime());
                        }
                    }
                    9 => {
                        let actor_uid = fields[6]
                            .parse::<u32>()
                            .ok()
                            .filter(|value| *value != 0)
                            .ok_or_else(BridgeError::unsafe_runtime)?;
                        if !valid_authenticated_username(fields[7]) {
                            return Err(BridgeError::unsafe_runtime());
                        }
                        if fields[1] == expected_code && fields[8].contains(&expected.transaction) {
                            if fields[2] != expected.profile
                                || fields[3] != expected_activity_state
                                || fields[4] != "audit"
                                || actor_uid != expected.actor_uid
                                || fields[7] != expected.actor
                                || fields[8] != expected_message
                            {
                                return Err(BridgeError::unsafe_runtime());
                            }
                            activity_found = true;
                            file.sync_all().map_err(|_| BridgeError::unsafe_runtime())?;
                        }
                    }
                    _ => return Err(BridgeError::unsafe_runtime()),
                }
            }
        }
        if !audit_found || !activity_found {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        sync_directory(log_root)
    }

    fn require_canonical_log_termination(bytes: &[u8]) -> BridgeResult<()> {
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    fn validate_canonical_log_records<T>(
        bytes: &[u8],
        validate_line: impl Fn(&[u8]) -> BridgeResult<T>,
    ) -> BridgeResult<()> {
        require_canonical_log_termination(bytes)?;
        if bytes.is_empty() {
            return Ok(());
        }
        let records = &bytes[..bytes.len() - 1];
        if records.is_empty() {
            return Err(BridgeError::unsafe_runtime());
        }
        for line in records.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                return Err(BridgeError::unsafe_runtime());
            }
            validate_line(line)?;
        }
        Ok(())
    }

    pub(super) fn validate_audit_log_line(line: &[u8]) -> BridgeResult<AuditLogRecord<'_>> {
        let parsed: AuditLogRecord<'_> =
            serde_json::from_slice(line).map_err(|_| BridgeError::unsafe_runtime())?;
        let expected_category = match parsed.operation {
            "configure-profile" | "remove-profile" | "set-default" | "schedule"
            | "alert-policy" | "interface-settings" => "configuration",
            "set-secret"
            | "set-password"
            | "remove-password"
            | "set-totp"
            | "remove-totp"
            | "set-remote-log-token"
            | "remove-remote-log-token" => "secrets",
            "test-profile-auth" | "browse-remote" => "authentication",
            "routine" | "remove-routine" => "routines",
            "security-policy" => "security",
            "rejected-post" => "bridge",
            "session-notifications" => "notifications",
            "doctor" | "plan" | "run" => "operations",
            _ => return Err(BridgeError::unsafe_runtime()),
        };
        let expected_level = match parsed.state {
            "requested" | "succeeded" => "info",
            "failed" => "error",
            "outcome_unknown" => "warn",
            _ => return Err(BridgeError::unsafe_runtime()),
        };
        if parsed.epoch == 0
            || PolicyLogLevel::parse(parsed.level).is_none()
            || PolicyLogLevel::parse(parsed.configured_level).is_none()
            || PolicyLogLevel::parse(parsed.subject_level).is_none()
            || !parsed.mandatory
            || parsed.category != "audit"
            || parsed.subject_category != expected_category
            || parsed.level != expected_level
            || !valid_audit_transaction(parsed.transaction)
            || !valid_audit_operation(parsed.operation)
            || !valid_audit_profile(parsed.profile)
            || !valid_authenticated_username(parsed.actor)
            || parsed.actor_uid.is_none_or(|value| value == 0)
            || !valid_audit_origin(parsed.origin)
            || parsed
                .client_request_id
                .is_some_and(|value| !valid_client_request_id(value))
            || if parsed.operation == "rejected-post" {
                !matches!(parsed.origin, "bridge" | "controller")
                    || parsed.client_request_id.is_some()
            } else {
                (parsed.origin == "bridge") != parsed.client_request_id.is_some()
            }
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(parsed)
    }

    pub(super) fn valid_activity_code(value: &str) -> bool {
        let Some((namespace, event)) = value.split_once('.') else {
            return false;
        };
        let valid_segment = |segment: &str| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        };
        valid_segment(namespace) && valid_segment(event) && !event.contains('.')
    }

    pub(super) fn validate_activity_log_line(line: &[u8]) -> BridgeResult<&str> {
        let text = std::str::from_utf8(line).map_err(|_| BridgeError::unsafe_runtime())?;
        let fields: Vec<&str> = text.split('|').collect();
        if !matches!(fields.len(), 5 | 7 | 9)
            || fields[0].parse::<u64>().is_err()
            || !valid_activity_code(fields[1])
            || !valid_audit_profile(fields[2])
            || !matches!(
                fields[3],
                "running"
                    | "succeeded"
                    | "failed"
                    | "deferred"
                    | "scheduled"
                    | "changed"
                    | "unavailable"
                    | "requested"
            )
        {
            return Err(BridgeError::unsafe_runtime());
        }
        if fields.len() >= 7
            && (policy_level_for_category(&SecurityPolicyArgs::default(), fields[4]).is_none()
                || !matches!(fields[5], "trace" | "debug" | "info" | "warn" | "error"))
        {
            return Err(BridgeError::unsafe_runtime());
        }
        if fields.len() == 9
            && (fields[6]
                .parse::<u32>()
                .ok()
                .filter(|value| *value != 0)
                .is_none()
                || !valid_authenticated_username(fields[7]))
        {
            return Err(BridgeError::unsafe_runtime());
        }
        let message = fields[fields.len() - 1];
        if message.len() > 4096
            || message
                .chars()
                .any(|character| character.is_control() || character == '\u{7f}')
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(text)
    }

    pub(super) fn repair_durable_log_tail(kind: &str, package_uid: u32) -> BridgeResult<bool> {
        match kind {
            "audit" => repair_durable_log_tail_at(
                Path::new(LOG_ROOT),
                Path::new(AUDIT_LOG_PATH),
                package_uid,
                5,
                11 * 1024 * 1024,
                |line| validate_audit_log_line(line).map(|_| ()),
            ),
            "activity" => repair_durable_log_tail_at(
                Path::new(LOG_ROOT),
                Path::new(ACTIVITY_LOG_PATH),
                package_uid,
                3,
                2 * 1024 * 1024,
                |line| validate_activity_log_line(line).map(|_| ()),
            ),
            _ => Err(BridgeError::bad_request()),
        }
    }

    pub(super) fn repair_durable_log_tail_at<T>(
        log_root: &Path,
        active_log: &Path,
        package_uid: u32,
        keep: usize,
        maximum: usize,
        validate_line: impl Fn(&[u8]) -> BridgeResult<T>,
    ) -> BridgeResult<bool> {
        validate_private_directory(log_root, package_uid)?;
        let mut repaired = false;
        for (index, path) in rotating_log_paths(active_log, keep).into_iter().enumerate() {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(index == 0)
                .custom_flags(NOFOLLOW_CLOEXEC);
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(BridgeError::unsafe_runtime()),
            };
            let metadata = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
            if !metadata.file_type().is_file()
                || metadata.st_uid() != package_uid
                || metadata.st_mode() & 0o7777 != 0o600
                || metadata.st_nlink() != 1
                || metadata.len() > maximum as u64
            {
                return Err(BridgeError::unsafe_runtime());
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize + 1);
            Read::by_ref(&mut file)
                .take((maximum + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| BridgeError::unsafe_runtime())?;
            if bytes.len() > maximum || bytes.len() as u64 != metadata.len() {
                return Err(BridgeError::unsafe_runtime());
            }

            if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
                if index != 0 {
                    return Err(BridgeError::unsafe_runtime());
                }
                let tail_start = bytes
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |position| position + 1);
                validate_canonical_log_records(&bytes[..tail_start], &validate_line)?;
                if validate_line(&bytes[tail_start..]).is_ok() {
                    if bytes.len() == maximum {
                        return Err(BridgeError::unsafe_runtime());
                    }
                    file.seek(SeekFrom::End(0))
                        .and_then(|_| file.write_all(b"\n"))
                        .map_err(|_| BridgeError::unsafe_runtime())?;
                    bytes.push(b'\n');
                } else {
                    file.set_len(tail_start as u64)
                        .map_err(|_| BridgeError::unsafe_runtime())?;
                    bytes.truncate(tail_start);
                }
                file.sync_all().map_err(|_| BridgeError::unsafe_runtime())?;
                repaired = true;
            }
            validate_canonical_log_records(&bytes, &validate_line)?;
        }
        if repaired {
            sync_directory(log_root)?;
        }
        Ok(repaired)
    }

    fn rotating_log_paths(base: &Path, keep: usize) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(keep + 1);
        paths.push(base.to_owned());
        for index in 1..=keep {
            let mut rotated = base.as_os_str().to_os_string();
            rotated.push(format!(".{index}"));
            paths.push(PathBuf::from(rotated));
        }
        paths
    }

    fn open_private_log_file(
        path: &Path,
        package_uid: u32,
        maximum: usize,
    ) -> BridgeResult<Option<(File, Vec<u8>)>> {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(NOFOLLOW_CLOEXEC);
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        let metadata = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o7777 != 0o600
            || metadata.st_nlink() != 1
            || metadata.len() > maximum as u64
        {
            return Err(BridgeError::unsafe_runtime());
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take((maximum + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        if bytes.len() > maximum || bytes.len() as u64 != metadata.len() {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(Some((file, bytes)))
    }

    pub(super) fn load_security_policy(package_uid: u32) -> BridgeResult<SecurityPolicyArgs> {
        load_security_policy_at(Path::new(SECURITY_POLICY_PATH), package_uid)
    }

    pub(super) fn load_security_policy_at(
        path: &Path,
        package_uid: u32,
    ) -> BridgeResult<SecurityPolicyArgs> {
        let parent = path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
        validate_private_directory(parent, package_uid)?;
        match read_optional_single_link_private_file(path, package_uid, 8 * 1024)? {
            Some(bytes) => parse_security_policy_file(&bytes),
            None => Ok(SecurityPolicyArgs::default()),
        }
    }

    pub(super) fn migrate_security_policy(package_uid: u32) -> BridgeResult<bool> {
        migrate_security_policy_at(Path::new(SECURITY_POLICY_PATH), package_uid)
    }

    pub(super) fn security_policy_migration_required(package_uid: u32) -> BridgeResult<bool> {
        security_policy_migration_required_at(Path::new(SECURITY_POLICY_PATH), package_uid)
    }

    fn migrated_security_policy(existing: &[u8]) -> BridgeResult<Option<Vec<u8>>> {
        if parse_security_policy_file(existing).is_ok() {
            return Ok(None);
        }
        let text = std::str::from_utf8(existing).map_err(|_| BridgeError::unsafe_runtime())?;
        if text.lines().any(|line| line.starts_with("policy_version=")) {
            return Err(BridgeError::unsafe_runtime());
        }
        let mut migrated = Vec::with_capacity(existing.len() + 17);
        migrated.extend_from_slice(b"policy_version=1\n");
        migrated.extend_from_slice(existing);
        parse_security_policy_file(&migrated)?;
        Ok(Some(migrated))
    }

    pub(super) fn security_policy_migration_required_at(
        path: &Path,
        package_uid: u32,
    ) -> BridgeResult<bool> {
        let parent = path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
        validate_private_directory(parent, package_uid)?;
        let Some(existing) = read_optional_single_link_private_file(path, package_uid, 8 * 1024)?
        else {
            return Ok(false);
        };
        Ok(migrated_security_policy(&existing)?.is_some())
    }

    pub(super) fn migrate_security_policy_at(path: &Path, package_uid: u32) -> BridgeResult<bool> {
        let parent = path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
        validate_private_directory(parent, package_uid)?;
        let Some(existing) = read_optional_single_link_private_file(path, package_uid, 8 * 1024)?
        else {
            return Ok(false);
        };
        let Some(migrated) = migrated_security_policy(&existing)? else {
            return Ok(false);
        };

        let temporary = parent.join(".security.conf.policy-v1.tmp");
        match fs::symlink_metadata(&temporary) {
            Ok(_) => {
                let _ = read_exact_single_link_private_file(&temporary, package_uid, 8 * 1024)?;
                fs::remove_file(&temporary).map_err(|_| BridgeError::unsafe_runtime())?;
                sync_directory(parent)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        }
        create_private_file(&temporary, package_uid, &migrated)?;
        fs::rename(&temporary, path).map_err(|_| BridgeError::unsafe_runtime())?;
        sync_directory(parent)?;
        Ok(true)
    }

    pub(super) fn load_or_create_csrf_key(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<Zeroizing<[u8; 32]>> {
        validate_private_directory(paths.root, package_uid)?;
        match read_exact_private_file(paths.csrf_key, package_uid, 32) {
            Ok(bytes) => key_from_bytes(bytes),
            Err(error) if error.kind == ErrorKind::Unavailable => {
                let mut generated = Zeroizing::new([0_u8; 32]);
                fill_random(&mut generated[..])?;
                match create_private_file(paths.csrf_key, package_uid, &generated[..]) {
                    Ok(()) => Ok(generated),
                    Err(error) if error.kind == ErrorKind::Conflict => {
                        generated.zeroize();
                        key_from_bytes(read_exact_private_file(paths.csrf_key, package_uid, 32)?)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn random_nonce() -> BridgeResult<[u8; 16]> {
        let mut nonce = [0_u8; 16];
        fill_random(&mut nonce)?;
        Ok(nonce)
    }

    pub(super) fn enqueue<F>(
        paths: &ControlPaths<'_>,
        request: EnqueueRequest<'_>,
        maximum_outstanding_jobs: usize,
        record_audit: F,
    ) -> BridgeResult<EnqueueOutcome>
    where
        F: FnMut(&AuditOutboxRecord, &str) -> BridgeResult<()>,
    {
        enqueue_with_admission_hook(
            paths,
            request,
            maximum_outstanding_jobs,
            record_audit,
            || {},
        )
    }

    pub(super) fn enqueue_with_admission_hook<F, H>(
        paths: &ControlPaths<'_>,
        request: EnqueueRequest<'_>,
        maximum_outstanding_jobs: usize,
        mut record_audit: F,
        after_admission: H,
    ) -> BridgeResult<EnqueueOutcome>
    where
        F: FnMut(&AuditOutboxRecord, &str) -> BridgeResult<()>,
        H: FnOnce(),
    {
        let EnqueueRequest {
            package_uid,
            client_request_id,
            requested_by,
            requested_uid,
            session_binding,
            audit_transaction,
            request_fingerprint,
            issued_at_epoch,
            mutation,
            secret,
        } = request;
        validate_private_directory(paths.root, package_uid)?;
        validate_private_directory(paths.requests, package_uid)?;
        validate_private_directory(paths.processing, package_uid)?;
        validate_private_directory(paths.responses, package_uid)?;
        validate_private_directory(paths.staging, package_uid)?;
        let _enqueue_lock = open_enqueue_lock(paths, package_uid)?;
        // This check is authoritative because lifecycle/upgrade marker writes
        // take the same enqueue flock. Either publication wins before the
        // close fence, or the closed/transition state is observed before any
        // audit or queue artifact is staged.
        require_open_runtime_admission(paths, package_uid)?;
        after_admission();
        let audit_paths = paths.audit_outbox();
        reconcile_audit_transactions(&audit_paths, package_uid, &mut record_audit)?;
        recover_legacy_queue_temps(paths, package_uid)?;
        recover_staging_files(paths, package_uid, issued_at_epoch)?;
        recover_orphan_canonical_secrets(paths, package_uid)?;
        if let Some(existing) = find_idempotent_job(
            paths,
            package_uid,
            client_request_id,
            requested_by,
            requested_uid,
            session_binding,
            request_fingerprint,
        )? {
            return Ok(EnqueueOutcome::Existing(existing));
        }
        validate_outstanding_queue_capacity(paths, package_uid, maximum_outstanding_jobs)?;
        let request_id = next_job_id(paths, package_uid)?;
        let job = canonical_job_bytes(
            &request_id,
            client_request_id,
            requested_by,
            requested_uid,
            session_binding,
            audit_transaction,
            request_fingerprint,
            issued_at_epoch,
            mutation,
        )?;
        if job.is_empty() || job.len() > MAX_JOB_BYTES {
            return Err(BridgeError::bad_request());
        }
        let final_job = paths.requests.join(format!("{request_id}.json"));
        let temporary_job = paths.staging.join(format!("{request_id}.job.tmp"));
        let secret_path = paths.requests.join(format!("{request_id}.secret"));
        let temporary_secret = paths.staging.join(format!("{request_id}.secret.tmp"));
        if final_job.exists() || secret_path.exists() {
            return Err(BridgeError::new(ErrorKind::Conflict));
        }
        if secret.is_some_and(|value| value.is_empty() || value.len() > MAX_CONNECTION_SECRET_BYTES)
        {
            return Err(BridgeError::bad_request());
        }
        create_private_file(&temporary_job, package_uid, &job)?;
        let (owner_pid, owner_start, owner_boot) = current_process_identity()?;
        let audit_record = AuditOutboxRecord {
            schema: "sdsync.dsm-audit-outbox.v1".to_owned(),
            transaction: audit_transaction.to_owned(),
            operation: mutation_audit_operation(mutation).to_owned(),
            profile: mutation_audit_profile(mutation).to_owned(),
            actor: requested_by.to_owned(),
            actor_uid: requested_uid,
            origin: "bridge".to_owned(),
            client_request_id: Some(client_request_id.to_owned()),
            job_id: Some(request_id.clone()),
            owner_pid,
            owner_start,
            owner_boot,
            phase: AuditOutboxPhase::Prepared,
        };
        if let Err(error) = audit_transaction_begin(
            &audit_paths,
            package_uid,
            audit_record,
            AuditOutboxPhase::Publishing,
            &mut record_audit,
        ) {
            let _ = fs::remove_file(&temporary_job);
            return Err(error);
        }

        if let Some(secret) = secret {
            let mut secret_line = Zeroizing::new(Vec::with_capacity(secret.len() + 1));
            secret_line.extend_from_slice(secret);
            secret_line.push(b'\n');
            if let Err(error) = create_private_file(&temporary_secret, package_uid, &secret_line)
                .and_then(|()| {
                    fs::hard_link(&temporary_secret, &secret_path)
                        .map_err(|error| map_create_error(&error))
                })
            {
                let _ = fs::remove_file(&temporary_job);
                let _ = fs::remove_file(&temporary_secret);
                let _ = fs::remove_file(&secret_path);
                audit_transaction_complete(
                    &audit_paths,
                    package_uid,
                    audit_transaction,
                    AuditOutboxPhase::Failed,
                    &mut record_audit,
                )?;
                return Err(error);
            }
        }
        if let Err(error) =
            fs::hard_link(&temporary_job, &final_job).map_err(|error| map_create_error(&error))
        {
            let _ = fs::remove_file(&temporary_job);
            let _ = fs::remove_file(&temporary_secret);
            let _ = fs::remove_file(&secret_path);
            audit_transaction_complete(
                &audit_paths,
                package_uid,
                audit_transaction,
                AuditOutboxPhase::Failed,
                &mut record_audit,
            )?;
            return Err(error);
        }

        // Once the canonical job name is visible, never report an ordinary
        // enqueue failure or remove its secret: the controller may execute it
        // immediately. Cleanup/durability failures are an accepted but
        // outcome-unknown state tied to the same server job ID.
        let mut durability_uncertain = mark_audit_transaction_queued(
            &audit_paths,
            package_uid,
            audit_transaction,
            &request_id,
        )
        .is_err();
        for temporary in [&temporary_job, &temporary_secret] {
            if (temporary.exists() || fs::symlink_metadata(temporary).is_ok())
                && fs::remove_file(temporary).is_err()
            {
                durability_uncertain = true;
            }
        }
        if sync_directory(paths.requests).is_err() || sync_directory(paths.staging).is_err() {
            durability_uncertain = true;
        }
        Ok(EnqueueOutcome::Published {
            job_id: request_id,
            durability_uncertain,
        })
    }

    pub(super) fn read_job(
        paths: &ControlPaths<'_>,
        path: &Path,
        package_uid: u32,
    ) -> BridgeResult<Zeroizing<Vec<u8>>> {
        validate_private_directory(paths.root, package_uid)?;
        validate_private_directory(paths.processing, package_uid)?;
        read_exact_private_file(path, package_uid, MAX_JOB_BYTES)
    }

    pub(super) fn read_optional_response(
        paths: &ControlPaths<'_>,
        request_id: &str,
        package_uid: u32,
    ) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        if !valid_server_job_id(request_id) {
            return Err(BridgeError::bad_request());
        }
        validate_private_directory(paths.root, package_uid)?;
        validate_private_directory(paths.responses, package_uid)?;
        let path = paths.responses.join(format!("{request_id}.json"));
        read_optional_private_file(&path, package_uid, MAX_MANAGER_OUTPUT_BYTES)
    }

    pub(super) fn read_optional_pending_job(
        paths: &ControlPaths<'_>,
        request_id: &str,
        package_uid: u32,
        processing: bool,
    ) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        if !valid_server_job_id(request_id) {
            return Err(BridgeError::bad_request());
        }
        validate_private_directory(paths.root, package_uid)?;
        let directory = if processing {
            paths.processing
        } else {
            paths.requests
        };
        validate_private_directory(directory, package_uid)?;
        let path = directory.join(format!("{request_id}.json"));
        read_optional_private_file(&path, package_uid, MAX_JOB_BYTES)
    }

    pub(super) fn remove_expired_response(
        paths: &ControlPaths<'_>,
        request_id: &str,
        package_uid: u32,
    ) -> BridgeResult<()> {
        if !valid_server_job_id(request_id) {
            return Err(BridgeError::bad_request());
        }
        validate_private_directory(paths.responses, package_uid)?;
        let path = paths.responses.join(format!("{request_id}.json"));
        let metadata = fs::symlink_metadata(&path).map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o777 != 0o600
            || metadata.len() > MAX_MANAGER_OUTPUT_BYTES as u64
        {
            return Err(BridgeError::unsafe_runtime());
        }
        fs::remove_file(&path).map_err(|_| BridgeError::unsafe_runtime())?;
        sync_directory(paths.responses)
    }

    pub(super) fn read_claimed_secret(
        paths: &ControlPaths<'_>,
        request_id: &str,
        package_uid: u32,
        required: bool,
    ) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        let path = paths.processing.join(format!("{request_id}.secret"));
        let guard = SecretRemovalGuard { path: path.clone() };
        if !path.exists() {
            return if required {
                Err(BridgeError::bad_request())
            } else {
                Ok(None)
            };
        }
        if !required {
            return Err(BridgeError::bad_request());
        }
        let mut line = read_exact_private_file(&path, package_uid, MAX_SECRET_BYTES + 1)?;
        if line.last() != Some(&b'\n')
            || line[..line.len() - 1].contains(&b'\n')
            || line[..line.len() - 1].contains(&b'\r')
        {
            return Err(BridgeError::bad_request());
        }
        line.pop();
        let text = std::str::from_utf8(&line).map_err(|_| BridgeError::bad_request())?;
        validate_secret(text)?;
        drop(guard);
        Ok(Some(line))
    }

    pub(super) fn read_claimed_connection_secret(
        paths: &ControlPaths<'_>,
        request_id: &str,
        package_uid: u32,
        required: bool,
    ) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        let path = paths.processing.join(format!("{request_id}.secret"));
        let guard = SecretRemovalGuard { path: path.clone() };
        if !path.exists() {
            return if required {
                Err(BridgeError::bad_request())
            } else {
                Ok(None)
            };
        }
        if !required {
            return Err(BridgeError::bad_request());
        }
        let mut value =
            read_exact_private_file(&path, package_uid, MAX_CONNECTION_SECRET_BYTES + 1)?;
        if value.last() != Some(&b'\n') {
            return Err(BridgeError::bad_request());
        }
        value.pop();
        if value.is_empty() || value.len() > MAX_CONNECTION_SECRET_BYTES {
            return Err(BridgeError::bad_request());
        }
        drop(guard);
        Ok(Some(value))
    }

    pub(super) fn read_profile_secret(
        profile: &str,
        kind: &str,
        package_uid: u32,
    ) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        validate_existing_name(profile)?;
        if !matches!(kind, "password" | "totp") {
            return Err(BridgeError::bad_request());
        }
        let root = Path::new(PROFILE_SECRET_ROOT);
        validate_private_directory(root, package_uid)?;
        let path = root.join(format!("{profile}.{kind}"));
        if !path.exists() {
            if fs::symlink_metadata(&path).is_ok() {
                return Err(BridgeError::unsafe_runtime());
            }
            return Ok(None);
        }
        let mut line = read_exact_private_file(&path, package_uid, MAX_SECRET_BYTES + 1)?;
        if line.last() != Some(&b'\n')
            || line[..line.len() - 1].contains(&b'\n')
            || line[..line.len() - 1].contains(&b'\r')
        {
            return Err(BridgeError::unsafe_runtime());
        }
        line.pop();
        let text = std::str::from_utf8(&line).map_err(|_| BridgeError::unsafe_runtime())?;
        validate_secret(text).map_err(|_| BridgeError::unsafe_runtime())?;
        Ok(Some(line))
    }

    pub(super) fn reject_unexpected_secret(
        paths: &ControlPaths<'_>,
        request_id: &str,
        package_uid: u32,
    ) -> BridgeResult<()> {
        validate_private_directory(paths.processing, package_uid)?;
        let path = paths.processing.join(format!("{request_id}.secret"));
        if path.exists() || fs::symlink_metadata(&path).is_ok() {
            let guard = SecretRemovalGuard { path };
            drop(guard);
            return Err(BridgeError::bad_request());
        }
        Ok(())
    }

    pub(super) fn remove_claimed_secret(paths: &ControlPaths<'_>, request_id: &str) {
        let path = paths.processing.join(format!("{request_id}.secret"));
        let _ = fs::remove_file(path);
    }

    pub(super) fn write_response(
        paths: &ControlPaths<'_>,
        path: &Path,
        request_id: &str,
        package_uid: u32,
        response: &[u8],
    ) -> BridgeResult<()> {
        validate_private_directory(paths.responses, package_uid)?;
        if response.is_empty() || response.len() > MAX_MANAGER_OUTPUT_BYTES {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let temporary = paths.staging.join(format!("{request_id}.response.tmp"));
        if path.exists() {
            return Err(BridgeError::new(ErrorKind::Conflict));
        }
        if temporary.exists() || fs::symlink_metadata(&temporary).is_ok() {
            let companions = vec![path.to_path_buf()];
            remove_private_staging_file(
                &temporary,
                package_uid,
                MAX_MANAGER_OUTPUT_BYTES,
                &companions,
            )?;
        }
        create_private_file(&temporary, package_uid, response)?;
        if let Err(error) =
            fs::hard_link(&temporary, path).map_err(|error| map_create_error(&error))
        {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        // As with request publication, a visible canonical response is the
        // authoritative outcome. Never convert post-publication cleanup or
        // fsync trouble into a false consumer failure.
        let _ = fs::remove_file(&temporary);
        let _ = sync_directory(paths.responses);
        let _ = sync_directory(paths.staging);
        Ok(())
    }

    fn key_from_bytes(bytes: Zeroizing<Vec<u8>>) -> BridgeResult<Zeroizing<[u8; 32]>> {
        let mut key = Zeroizing::new([0_u8; 32]);
        if bytes.len() != key.len() {
            return Err(BridgeError::unsafe_runtime());
        }
        key.copy_from_slice(&bytes);
        Ok(key)
    }

    fn open_enqueue_lock(paths: &ControlPaths<'_>, package_uid: u32) -> BridgeResult<File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW_CLOEXEC);
        let file = options
            .open(paths.enqueue_lock)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let metadata = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o777 != 0o600
        {
            return Err(BridgeError::unsafe_runtime());
        }
        // SAFETY: flock receives a valid live file descriptor and a fixed
        // operation.  The descriptor remains open for the full enqueue.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(file)
    }

    pub(super) fn private_file_metadata_with_companion(
        path: &Path,
        package_uid: u32,
        maximum: usize,
        published_companions: &[PathBuf],
    ) -> BridgeResult<fs::Metadata> {
        let metadata = fs::symlink_metadata(path).map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o777 != 0o600
            || !matches!(metadata.st_nlink(), 1 | 2)
            || metadata.len() > maximum as u64
        {
            return Err(BridgeError::unsafe_runtime());
        }
        match metadata.st_nlink() {
            1 => {}
            2 => {
                let mut matching_companions = 0_u8;
                for companion in published_companions {
                    let companion_metadata = match fs::symlink_metadata(companion) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(_) => return Err(BridgeError::unsafe_runtime()),
                    };
                    if companion_metadata.file_type().is_file()
                        && companion_metadata.st_uid() == package_uid
                        && companion_metadata.st_mode() & 0o777 == 0o600
                        && companion_metadata.st_nlink() == 2
                        && companion_metadata.st_dev() == metadata.st_dev()
                        && companion_metadata.st_ino() == metadata.st_ino()
                    {
                        matching_companions = matching_companions.saturating_add(1);
                    }
                }
                if matching_companions != 1 {
                    return Err(BridgeError::unsafe_runtime());
                }
            }
            _ => return Err(BridgeError::unsafe_runtime()),
        }
        Ok(metadata)
    }

    fn private_file_metadata(
        path: &Path,
        package_uid: u32,
        maximum: usize,
    ) -> BridgeResult<fs::Metadata> {
        private_file_metadata_with_companion(path, package_uid, maximum, &[])
    }

    fn remove_private_staging_file(
        path: &Path,
        package_uid: u32,
        maximum: usize,
        published_companions: &[PathBuf],
    ) -> BridgeResult<()> {
        let _ =
            private_file_metadata_with_companion(path, package_uid, maximum, published_companions)?;
        fs::remove_file(path).map_err(|_| BridgeError::unsafe_runtime())
    }

    fn legacy_temporary_name(name: &str, suffix: &str) -> bool {
        let Some(value) = name
            .strip_prefix('.')
            .and_then(|value| value.strip_suffix(suffix))
        else {
            return false;
        };
        let Some((request_id, pid)) = value.rsplit_once('.') else {
            return false;
        };
        valid_server_job_id(request_id)
            && !pid.is_empty()
            && !pid.starts_with('0')
            && pid.bytes().all(|byte| byte.is_ascii_digit())
    }

    pub(super) fn recover_legacy_queue_temps(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<()> {
        for (directory, suffix, maximum) in [
            (paths.requests, ".job", MAX_JOB_BYTES),
            (paths.responses, ".response", MAX_MANAGER_OUTPUT_BYTES),
        ] {
            for entry in fs::read_dir(directory).map_err(|_| BridgeError::unsafe_runtime())? {
                let entry = entry.map_err(|_| BridgeError::unsafe_runtime())?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                if !name.starts_with('.') {
                    continue;
                }
                if directory == paths.responses
                    && response_audit_reconcile_temp_job_id(&name).is_some()
                {
                    validate_response_audit_reconcile_temp(&entry.path(), package_uid)?;
                    continue;
                }
                if !legacy_temporary_name(&name, suffix) {
                    return Err(BridgeError::unsafe_runtime());
                }
                let path = entry.path();
                let value = name
                    .strip_prefix('.')
                    .and_then(|value| value.strip_suffix(suffix))
                    .ok_or_else(BridgeError::unsafe_runtime)?;
                let (request_id, _) = value
                    .rsplit_once('.')
                    .ok_or_else(BridgeError::unsafe_runtime)?;
                let mut companions = vec![directory.join(format!("{request_id}.json"))];
                if directory == paths.requests {
                    companions.push(paths.processing.join(format!("{request_id}.json")));
                }
                remove_private_staging_file(&path, package_uid, maximum, &companions)?;
            }
            sync_directory(directory)?;
        }
        Ok(())
    }

    fn staging_entry_maximum(name: &str) -> Option<usize> {
        if name == "enqueue.sequence.tmp" {
            return Some(16);
        }
        let (request_id, maximum) = if let Some(request_id) = name.strip_suffix(".job.tmp") {
            (request_id, MAX_JOB_BYTES)
        } else if let Some(request_id) = name.strip_suffix(".secret.tmp") {
            (request_id, MAX_CONNECTION_SECRET_BYTES + 1)
        } else {
            let request_id = name.strip_suffix(".response.tmp")?;
            (request_id, MAX_MANAGER_OUTPUT_BYTES)
        };
        valid_server_job_id(request_id).then_some(maximum)
    }

    fn staging_published_companions(paths: &ControlPaths<'_>, name: &str) -> Vec<PathBuf> {
        if let Some(request_id) = name.strip_suffix(".job.tmp") {
            if valid_server_job_id(request_id) {
                vec![
                    paths.requests.join(format!("{request_id}.json")),
                    paths.processing.join(format!("{request_id}.json")),
                ]
            } else {
                Vec::new()
            }
        } else if let Some(request_id) = name.strip_suffix(".secret.tmp") {
            if valid_server_job_id(request_id) {
                vec![
                    paths.requests.join(format!("{request_id}.secret")),
                    paths.processing.join(format!("{request_id}.secret")),
                ]
            } else {
                Vec::new()
            }
        } else if let Some(request_id) = name.strip_suffix(".response.tmp") {
            if valid_server_job_id(request_id) {
                vec![paths.responses.join(format!("{request_id}.json"))]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    fn recover_staging_files(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        now: u64,
    ) -> BridgeResult<()> {
        for entry in fs::read_dir(paths.staging).map_err(|_| BridgeError::unsafe_runtime())? {
            let entry = entry.map_err(|_| BridgeError::unsafe_runtime())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| BridgeError::unsafe_runtime())?;
            let maximum = staging_entry_maximum(&name).ok_or_else(BridgeError::unsafe_runtime)?;
            let path = entry.path();
            let companions = staging_published_companions(paths, &name);
            let metadata =
                private_file_metadata_with_companion(&path, package_uid, maximum, &companions)?;
            let modified = metadata
                .modified()
                .map_err(|_| BridgeError::unsafe_runtime())?
                .duration_since(UNIX_EPOCH)
                .map_err(|_| BridgeError::unsafe_runtime())?
                .as_secs();
            if modified > now.saturating_add(CLOCK_SKEW_SECONDS) {
                return Err(BridgeError::unsafe_runtime());
            }
            let serialized_enqueue_staging = name == "enqueue.sequence.tmp"
                || name.ends_with(".job.tmp")
                || name.ends_with(".secret.tmp");
            if metadata.st_nlink() == 2
                || serialized_enqueue_staging
                || now.saturating_sub(modified) > MAX_JOB_AGE_SECONDS
            {
                fs::remove_file(&path).map_err(|_| BridgeError::unsafe_runtime())?;
            }
        }
        sync_directory(paths.staging)
    }

    fn canonical_job_is_present(
        paths: &ControlPaths<'_>,
        request_id: &str,
        package_uid: u32,
    ) -> BridgeResult<bool> {
        // Check the queued name before the processing name. A controller move
        // is atomic within this filesystem, so that order cannot miss a job
        // moving requests -> processing.
        for directory in [paths.requests, paths.processing] {
            let job = directory.join(format!("{request_id}.json"));
            let metadata = match fs::symlink_metadata(&job) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(BridgeError::unsafe_runtime()),
            };
            if !metadata.file_type().is_file()
                || metadata.st_uid() != package_uid
                || metadata.st_mode() & 0o777 != 0o600
                || metadata.st_nlink() != 1
                || metadata.len() > MAX_JOB_BYTES as u64
            {
                return Err(BridgeError::unsafe_runtime());
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn recover_orphan_canonical_secrets(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<()> {
        for directory in [paths.requests, paths.processing] {
            validate_private_directory(directory, package_uid)?;
            for entry in fs::read_dir(directory).map_err(|_| BridgeError::unsafe_runtime())? {
                let entry = entry.map_err(|_| BridgeError::unsafe_runtime())?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                let Some(request_id) = name.strip_suffix(".secret") else {
                    continue;
                };
                if !valid_server_job_id(request_id) {
                    return Err(BridgeError::unsafe_runtime());
                }
                let secret_path = entry.path();
                let before = match fs::symlink_metadata(&secret_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(_) => return Err(BridgeError::unsafe_runtime()),
                };
                if !before.file_type().is_file()
                    || before.st_uid() != package_uid
                    || before.st_mode() & 0o777 != 0o600
                    || before.st_nlink() != 1
                    || before.len() > (MAX_CONNECTION_SECRET_BYTES + 1) as u64
                {
                    return Err(BridgeError::unsafe_runtime());
                }
                if canonical_job_is_present(paths, request_id, package_uid)? {
                    continue;
                }

                // Re-observe both the name and queue topology before deleting.
                // A vanished/moved candidate is a controller race and is left
                // alone; an existing replacement or malformed artifact is an
                // unsafe state. Only the exact stable inode with no executable
                // job is an orphan from a pre-publication crash.
                std::thread::yield_now();
                if canonical_job_is_present(paths, request_id, package_uid)? {
                    continue;
                }
                let current = match fs::symlink_metadata(&secret_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(_) => return Err(BridgeError::unsafe_runtime()),
                };
                if !current.file_type().is_file()
                    || current.st_uid() != package_uid
                    || current.st_mode() & 0o777 != 0o600
                    || current.st_nlink() != 1
                    || current.len() > (MAX_CONNECTION_SECRET_BYTES + 1) as u64
                    || current.st_dev() != before.st_dev()
                    || current.st_ino() != before.st_ino()
                {
                    return Err(BridgeError::unsafe_runtime());
                }
                match fs::remove_file(&secret_path) {
                    Ok(()) => sync_directory(directory)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => return Err(BridgeError::unsafe_runtime()),
                }
            }
        }
        Ok(())
    }

    // These are the two complete immutable identities being compared. Keeping
    // every field explicit makes omission of actor, session, or fingerprint
    // material visible at this security boundary.
    #[allow(clippy::too_many_arguments)]
    fn matching_idempotency_key(
        client_request_id: &str,
        requested_by: &str,
        requested_uid: u32,
        session_binding: &[u8; 32],
        request_fingerprint: &str,
        existing_client_request_id: &str,
        existing_requested_by: &str,
        existing_requested_uid: u32,
        existing_session_binding: &[u8; 32],
        existing_request_fingerprint: &str,
    ) -> BridgeResult<bool> {
        if existing_client_request_id != client_request_id
            || !session_binding_matches(existing_session_binding, session_binding)
        {
            return Ok(false);
        }
        if existing_requested_by != requested_by || existing_requested_uid != requested_uid {
            return Err(BridgeError::unsafe_runtime());
        }
        if !constant_time_equal(
            existing_request_fingerprint.as_bytes(),
            request_fingerprint.as_bytes(),
        ) {
            return Err(BridgeError::new(ErrorKind::Conflict));
        }
        Ok(true)
    }

    fn response_audit_reconcile_temp_job_id(name: &str) -> Option<&str> {
        name.strip_prefix('.')
            .and_then(|value| value.strip_suffix(".audit-reconciled.tmp"))
            .filter(|value| valid_server_job_id(value))
    }

    fn validate_response_audit_reconcile_temp(path: &Path, package_uid: u32) -> BridgeResult<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o777 != 0o600
            || metadata.st_nlink() != 1
            || metadata.len() > MAX_MANAGER_OUTPUT_BYTES as u64
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    pub(super) fn collect_json_job_ids(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<BTreeSet<String>> {
        let mut ids = BTreeSet::new();
        for directory in [paths.requests, paths.processing, paths.responses] {
            for entry in fs::read_dir(directory).map_err(|_| BridgeError::unsafe_runtime())? {
                let entry = entry.map_err(|_| BridgeError::unsafe_runtime())?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                if directory == paths.responses
                    && response_audit_reconcile_temp_job_id(&name).is_some()
                {
                    // Audit reconciliation atomically replaces a terminal
                    // response while holding the outbox lock. Enqueue uses a
                    // separate lock, so its stable idempotency scan may see
                    // this bounded private staging name. It is not a queue
                    // object and must not make a valid enqueue fail. Existing
                    // malformed lookalikes still fail closed.
                    validate_response_audit_reconcile_temp(&entry.path(), package_uid)?;
                    continue;
                }
                if name.ends_with(".secret")
                    && (directory == paths.requests || directory == paths.processing)
                {
                    continue;
                }
                let Some(request_id) = name.strip_suffix(".json") else {
                    return Err(BridgeError::unsafe_runtime());
                };
                if !valid_server_job_id(request_id) {
                    return Err(BridgeError::unsafe_runtime());
                }
                ids.insert(request_id.to_owned());
            }
        }
        Ok(ids)
    }

    fn read_transient_optional_private_file(
        path: &Path,
        package_uid: u32,
        maximum: usize,
    ) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(BridgeError::unsafe_runtime()),
            Ok(_) => match read_exact_private_file(path, package_uid, maximum) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error)
                    if error.kind == ErrorKind::Unavailable
                        && fs::symlink_metadata(path).is_err_and(|follow_up| {
                            follow_up.kind() == io::ErrorKind::NotFound
                        }) =>
                {
                    Ok(None)
                }
                Err(error) => Err(error),
            },
        }
    }

    type IdempotencyRecord = (String, String, u32, [u8; 32], String);

    struct SessionRequestRecord {
        client_request_id: String,
        requested_by: String,
        requested_uid: u32,
        session_binding: [u8; 32],
        operation: Option<String>,
        complete: bool,
    }

    impl Drop for SessionRequestRecord {
        fn drop(&mut self) {
            self.session_binding.zeroize();
        }
    }

    fn read_any_idempotency_record(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        request_id: &str,
    ) -> BridgeResult<Option<IdempotencyRecord>> {
        for (directory, response) in [
            (paths.requests, false),
            (paths.processing, false),
            (paths.responses, true),
        ] {
            let path = directory.join(format!("{request_id}.json"));
            let maximum = if response {
                MAX_MANAGER_OUTPUT_BYTES
            } else {
                MAX_JOB_BYTES
            };
            let Some(bytes) = read_transient_optional_private_file(&path, package_uid, maximum)?
            else {
                continue;
            };
            if response {
                let parsed = parse_queued_response(&bytes, request_id)?;
                return Ok(Some((
                    parsed.client_request_id.clone(),
                    parsed.requested_by.clone(),
                    parsed.requested_uid,
                    parsed.session_binding,
                    parsed.request_fingerprint.clone(),
                )));
            }
            let parsed = parse_job(&bytes)?;
            return Ok(Some((
                parsed.client_request_id.clone(),
                parsed.requested_by.clone(),
                parsed.requested_uid,
                parsed.session_binding,
                parsed.request_fingerprint.clone(),
            )));
        }
        Ok(None)
    }

    fn read_any_session_request_record(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        job_id: &str,
    ) -> BridgeResult<Option<SessionRequestRecord>> {
        let response_path = paths.responses.join(format!("{job_id}.json"));
        if let Some(bytes) = read_transient_optional_private_file(
            &response_path,
            package_uid,
            MAX_MANAGER_OUTPUT_BYTES,
        )? {
            let parsed = parse_queued_response(&bytes, job_id)?;
            return Ok(Some(SessionRequestRecord {
                client_request_id: parsed.client_request_id.clone(),
                requested_by: parsed.requested_by.clone(),
                requested_uid: parsed.requested_uid,
                session_binding: parsed.session_binding,
                operation: parsed.operation.clone(),
                complete: true,
            }));
        }

        for directory in [paths.requests, paths.processing] {
            let path = directory.join(format!("{job_id}.json"));
            let Some(bytes) =
                read_transient_optional_private_file(&path, package_uid, MAX_JOB_BYTES)?
            else {
                continue;
            };
            let parsed = parse_job(&bytes)?;
            return Ok(Some(SessionRequestRecord {
                client_request_id: parsed.client_request_id.clone(),
                requested_by: parsed.requested_by.clone(),
                requested_uid: parsed.requested_uid,
                session_binding: parsed.session_binding,
                operation: Some(parsed.mutation.operation_id().to_owned()),
                complete: false,
            }));
        }
        Ok(None)
    }

    pub(super) fn find_session_request(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        client_request_id: &str,
        requested_by: &str,
        requested_uid: u32,
        session_binding: &[u8; 32],
    ) -> BridgeResult<Option<SessionRequestStatus>> {
        if !valid_client_request_id(client_request_id)
            || !valid_authenticated_username(requested_by)
            || requested_uid == 0
        {
            return Err(BridgeError::bad_request());
        }
        validate_private_directory(paths.requests, package_uid)?;
        validate_private_directory(paths.processing, package_uid)?;
        validate_private_directory(paths.responses, package_uid)?;

        for _ in 0..4 {
            let before = collect_json_job_ids(paths, package_uid)?;
            let mut owned_job_id: Option<String> = None;
            let mut found: Option<SessionRequestStatus> = None;
            let mut vanished = false;
            for job_id in &before {
                let Some(record) = read_any_session_request_record(paths, package_uid, job_id)?
                else {
                    vanished = true;
                    continue;
                };
                if record.client_request_id != client_request_id
                    || !session_binding_matches(&record.session_binding, session_binding)
                {
                    continue;
                }
                if record.requested_by != requested_by || record.requested_uid != requested_uid {
                    return Err(BridgeError::unsafe_runtime());
                }
                if owned_job_id
                    .as_deref()
                    .is_some_and(|existing| existing != job_id)
                {
                    return Err(BridgeError::unsafe_runtime());
                }
                owned_job_id = Some(job_id.to_owned());
                found = record.operation.as_ref().map(|operation| {
                    if record.complete {
                        SessionRequestStatus::Complete {
                            job_id: job_id.to_owned(),
                            operation: operation.clone(),
                        }
                    } else {
                        SessionRequestStatus::Pending {
                            job_id: job_id.to_owned(),
                            operation: operation.clone(),
                        }
                    }
                });
            }
            let after = collect_json_job_ids(paths, package_uid)?;
            if !vanished && before == after {
                return Ok(found);
            }
            std::thread::yield_now();
        }
        Err(BridgeError::new(ErrorKind::Unavailable))
    }

    fn find_idempotent_job(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        client_request_id: &str,
        requested_by: &str,
        requested_uid: u32,
        session_binding: &[u8; 32],
        request_fingerprint: &str,
    ) -> BridgeResult<Option<String>> {
        for _ in 0..4 {
            let before = collect_json_job_ids(paths, package_uid)?;
            let mut found: Option<String> = None;
            let mut conflict = false;
            let mut vanished = false;
            for request_id in &before {
                let Some((
                    existing_client,
                    existing_actor,
                    existing_actor_uid,
                    existing_binding,
                    existing_fingerprint,
                )) = read_any_idempotency_record(paths, package_uid, request_id)?
                else {
                    vanished = true;
                    continue;
                };
                match matching_idempotency_key(
                    client_request_id,
                    requested_by,
                    requested_uid,
                    session_binding,
                    request_fingerprint,
                    &existing_client,
                    &existing_actor,
                    existing_actor_uid,
                    &existing_binding,
                    &existing_fingerprint,
                ) {
                    Ok(false) => {}
                    Ok(true) => {
                        if found
                            .as_deref()
                            .is_some_and(|existing| existing != request_id)
                        {
                            return Err(BridgeError::unsafe_runtime());
                        }
                        found = Some(request_id.to_owned());
                    }
                    Err(error) if error.kind == ErrorKind::Conflict => conflict = true,
                    Err(error) => return Err(error),
                }
            }
            let after = collect_json_job_ids(paths, package_uid)?;
            if !vanished && before == after {
                if conflict {
                    return Err(BridgeError::new(ErrorKind::Conflict));
                }
                return Ok(found);
            }
            std::thread::yield_now();
        }
        Err(BridgeError::new(ErrorKind::Unavailable))
    }

    fn validate_outstanding_queue_capacity(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        maximum_outstanding_jobs: usize,
    ) -> BridgeResult<()> {
        if !(1..=MAX_OUTSTANDING_JOBS).contains(&maximum_outstanding_jobs) {
            return Err(BridgeError::unsafe_runtime());
        }
        let mut jobs = BTreeSet::new();
        for directory in [paths.requests, paths.processing] {
            validate_private_directory(directory, package_uid)?;
            for entry in fs::read_dir(directory).map_err(|_| BridgeError::unsafe_runtime())? {
                let entry = entry.map_err(|_| BridgeError::unsafe_runtime())?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                let (request_id, is_job) = if let Some(request_id) = name.strip_suffix(".json") {
                    (request_id, true)
                } else if let Some(request_id) = name.strip_suffix(".secret") {
                    (request_id, false)
                } else {
                    return Err(BridgeError::unsafe_runtime());
                };
                if !valid_server_job_id(request_id) {
                    return Err(BridgeError::unsafe_runtime());
                }
                let metadata = fs::symlink_metadata(entry.path())
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                let maximum = if is_job {
                    MAX_JOB_BYTES as u64
                } else {
                    (MAX_CONNECTION_SECRET_BYTES + 1) as u64
                };
                if !metadata.file_type().is_file()
                    || metadata.st_uid() != package_uid
                    || metadata.st_mode() & 0o777 != 0o600
                    || metadata.len() > maximum
                {
                    return Err(BridgeError::unsafe_runtime());
                }
                if is_job {
                    jobs.insert(request_id.to_owned());
                }
            }
        }
        if jobs.len() >= maximum_outstanding_jobs {
            return Err(BridgeError::new(ErrorKind::Conflict));
        }
        Ok(())
    }

    fn parse_sequence(bytes: &[u8]) -> Option<u64> {
        if bytes.len() != 16
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return None;
        }
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|value| u64::from_str_radix(value, 16).ok())
    }

    fn maximum_published_sequence(paths: &ControlPaths<'_>, package_uid: u32) -> BridgeResult<u64> {
        let mut maximum = 0_u64;
        for directory in [paths.requests, paths.processing, paths.responses] {
            for entry in fs::read_dir(directory).map_err(|_| BridgeError::unsafe_runtime())? {
                let entry = entry.map_err(|_| BridgeError::unsafe_runtime())?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                let Some(request_id) = name.strip_suffix(".json") else {
                    continue;
                };
                if !valid_server_job_id(request_id) {
                    return Err(BridgeError::unsafe_runtime());
                }
                let maximum_size = if directory == paths.responses {
                    MAX_MANAGER_OUTPUT_BYTES
                } else {
                    MAX_JOB_BYTES
                };
                let _ = private_file_metadata(&entry.path(), package_uid, maximum_size)?;
                let prefix = u64::from_str_radix(&request_id[..16], 16)
                    .map_err(|_| BridgeError::unsafe_runtime())?;
                maximum = maximum.max(prefix);
            }
        }
        Ok(maximum)
    }

    fn publish_enqueue_sequence(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        encoded: &[u8; 16],
    ) -> BridgeResult<()> {
        let temporary = paths.staging.join("enqueue.sequence.tmp");
        if temporary.exists() || fs::symlink_metadata(&temporary).is_ok() {
            remove_private_staging_file(&temporary, package_uid, 16, &[])?;
        }
        if paths.enqueue_sequence.exists() || fs::symlink_metadata(paths.enqueue_sequence).is_ok() {
            let _ = private_file_metadata(paths.enqueue_sequence, package_uid, 16)?;
        }
        create_private_file(&temporary, package_uid, encoded)?;
        fs::rename(&temporary, paths.enqueue_sequence)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        sync_directory(paths.root)?;
        sync_directory(paths.staging)
    }

    pub(super) fn next_job_id(paths: &ControlPaths<'_>, package_uid: u32) -> BridgeResult<String> {
        let saved = match fs::symlink_metadata(paths.enqueue_sequence) {
            Ok(_) => {
                let _ = private_file_metadata(paths.enqueue_sequence, package_uid, 16)?;
                let bytes = read_exact_private_file(paths.enqueue_sequence, package_uid, 16)?;
                parse_sequence(&bytes).unwrap_or(0)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        let previous = saved.max(maximum_published_sequence(paths, package_uid)?);
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        let wall_clock = u64::try_from(elapsed.as_micros())
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        let sequence = next_enqueue_sequence(previous, wall_clock)?;
        let mut random = [0_u8; 16];
        fill_random(&mut random)?;
        let encoded_sequence: [u8; 16] = format!("{sequence:016x}")
            .into_bytes()
            .try_into()
            .map_err(|_| BridgeError::internal())?;
        publish_enqueue_sequence(paths, package_uid, &encoded_sequence)?;
        Ok(sortable_job_id(sequence, &random))
    }

    pub(super) fn parent_process_identity() -> BridgeResult<(u32, u64, String)> {
        // SAFETY: getppid has no pointer arguments or preconditions.
        let parent = unsafe { libc::getppid() };
        let pid = u32::try_from(parent).map_err(|_| BridgeError::unsafe_runtime())?;
        if pid <= 1 {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok((pid, process_start(pid)?, current_boot_id()?))
    }

    pub(super) fn current_process_identity() -> BridgeResult<(u32, u64, String)> {
        let pid = std::process::id();
        if pid <= 1 {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok((pid, process_start(pid)?, current_boot_id()?))
    }

    pub(super) fn validate_live_process_identity(
        pid: u32,
        start: u64,
        boot: &str,
        expected_uid: u32,
    ) -> BridgeResult<()> {
        if pid <= 1
            || start == 0
            || expected_uid == 0
            || !valid_boot_id(boot)
            || !process_identity_is_live(pid, start, boot)?
        {
            return Err(BridgeError::unsafe_runtime());
        }
        let status = fs::read_to_string(format!("/proc/{pid}/status"))
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let actual_uid = status
            .lines()
            .find_map(|line| {
                line.strip_prefix("Uid:")
                    .and_then(|tail| tail.split_ascii_whitespace().next())
                    .and_then(|value| value.parse::<u32>().ok())
            })
            .ok_or_else(BridgeError::unsafe_runtime)?;
        if actual_uid != expected_uid {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    pub(super) fn publish_service_identity(
        path: &Path,
        package_uid: u32,
        bytes: &[u8],
    ) -> BridgeResult<()> {
        if bytes.is_empty() || bytes.len() > 96 {
            return Err(BridgeError::unsafe_runtime());
        }
        let parent = path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
        let parent_metadata =
            fs::symlink_metadata(parent).map_err(|_| BridgeError::unsafe_runtime())?;
        if !parent_metadata.file_type().is_dir()
            || parent_metadata.st_uid() != package_uid
            || parent_metadata.st_mode() & 0o7777 != 0o700
        {
            return Err(BridgeError::unsafe_runtime());
        }
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(BridgeError::unsafe_runtime()),
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(BridgeError::unsafe_runtime)?;
        let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
        match fs::symlink_metadata(&temporary) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(BridgeError::unsafe_runtime()),
        }
        create_private_file(&temporary, package_uid, bytes)?;
        if fs::rename(&temporary, path).is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(BridgeError::unsafe_runtime());
        }
        let published = read_exact_single_link_private_file(path, package_uid, 96)?;
        if published.as_slice() != bytes {
            return Err(BridgeError::unsafe_runtime());
        }
        sync_directory(parent)
    }

    pub(super) fn service_start_is_committed(
        path: &Path,
        package_uid: u32,
        parent_pid: u32,
        parent_start: u64,
        parent_boot: &str,
    ) -> BridgeResult<bool> {
        if parent_pid <= 1 || parent_start == 0 || !valid_boot_id(parent_boot) {
            return Err(BridgeError::unsafe_runtime());
        }
        let bytes = read_exact_single_link_private_file(path, package_uid, 112)?;
        let prepared = format!("{parent_pid}\n{parent_start}\n{parent_boot}\n");
        let committed = format!("{prepared}committed\n");
        let is_committed = if bytes.as_slice() == committed.as_bytes() {
            true
        } else if bytes.as_slice() == prepared.as_bytes() {
            false
        } else {
            return Err(BridgeError::unsafe_runtime());
        };
        validate_live_process_identity(parent_pid, parent_start, parent_boot, package_uid)?;
        Ok(is_committed)
    }

    fn publish_idempotent_private_marker(
        path: &Path,
        package_uid: u32,
        expected: &[u8],
    ) -> BridgeResult<()> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                publish_service_identity(path, package_uid, expected)
            }
            Ok(_) => {
                let actual = read_exact_single_link_private_file(path, package_uid, 112)?;
                if actual.as_slice() == expected {
                    Ok(())
                } else {
                    Err(BridgeError::unsafe_runtime())
                }
            }
            Err(_) => Err(BridgeError::unsafe_runtime()),
        }
    }

    fn remove_allowed_private_marker(
        path: &Path,
        package_uid: u32,
        allowed: &[&[u8]],
    ) -> BridgeResult<()> {
        let actual = match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Ok(_) => read_exact_single_link_private_file(path, package_uid, 112)?,
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        if !allowed
            .iter()
            .any(|expected| actual.as_slice() == *expected)
        {
            return Err(BridgeError::unsafe_runtime());
        }
        remove_own_service_identity(path, package_uid, actual.as_slice())
    }

    pub(super) fn package_transition_state(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<&'static str> {
        let path = paths.package_transition;
        let bytes = match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok("open"),
            Ok(_) => read_exact_single_link_private_file(path, package_uid, 16)?,
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        match bytes.as_slice() {
            b"upgrade\n" => Ok("upgrade"),
            b"uninstall\n" => Ok("uninstall"),
            _ => Err(BridgeError::unsafe_runtime()),
        }
    }

    pub(super) fn prepare_package_transition(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        kind: &str,
    ) -> BridgeResult<()> {
        prepare_package_transition_with_hook(paths, package_uid, kind, || {})
    }

    pub(super) fn prepare_package_transition_with_hook<H>(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        kind: &str,
        after_lock: H,
    ) -> BridgeResult<()>
    where
        H: FnOnce(),
    {
        let expected: &[u8] = match kind {
            "upgrade" => b"upgrade\n",
            "uninstall" => b"uninstall\n",
            _ => return Err(BridgeError::bad_request()),
        };
        validate_private_directory(paths.root, package_uid)?;
        let _enqueue_lock = open_enqueue_lock(paths, package_uid)?;
        after_lock();
        publish_idempotent_private_marker(paths.package_transition, package_uid, expected)
    }

    pub(super) fn clear_package_transition(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<()> {
        clear_package_transition_with_hook(paths, package_uid, || {})
    }

    fn clear_package_transition_with_hook<H>(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        after_lock: H,
    ) -> BridgeResult<()>
    where
        H: FnOnce(),
    {
        validate_private_directory(paths.root, package_uid)?;
        let _enqueue_lock = open_enqueue_lock(paths, package_uid)?;
        after_lock();
        remove_allowed_private_marker(
            paths.package_transition,
            package_uid,
            &[b"upgrade\n", b"uninstall\n"],
        )
    }

    pub(super) fn service_admission_state(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<&'static str> {
        let path = paths.service_closed;
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok("open"),
            Ok(_) => {
                let bytes = read_exact_single_link_private_file(path, package_uid, 16)?;
                if bytes.as_slice() == b"closed\n" {
                    Ok("closed")
                } else {
                    Err(BridgeError::unsafe_runtime())
                }
            }
            Err(_) => Err(BridgeError::unsafe_runtime()),
        }
    }

    pub(super) fn close_service_admission(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<()> {
        close_service_admission_with_hook(paths, package_uid, || {})
    }

    pub(super) fn close_service_admission_with_hook<H>(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        after_lock: H,
    ) -> BridgeResult<()>
    where
        H: FnOnce(),
    {
        validate_private_directory(paths.root, package_uid)?;
        let _enqueue_lock = open_enqueue_lock(paths, package_uid)?;
        after_lock();
        publish_idempotent_private_marker(paths.service_closed, package_uid, b"closed\n")
    }

    pub(super) fn open_service_admission(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<()> {
        open_service_admission_with_hook(paths, package_uid, || {})
    }

    pub(super) fn open_service_admission_with_hook<H>(
        paths: &ControlPaths<'_>,
        package_uid: u32,
        after_lock: H,
    ) -> BridgeResult<()>
    where
        H: FnOnce(),
    {
        validate_private_directory(paths.root, package_uid)?;
        let _enqueue_lock = open_enqueue_lock(paths, package_uid)?;
        after_lock();
        remove_allowed_private_marker(paths.service_closed, package_uid, &[b"closed\n"])
    }

    pub(super) fn require_open_runtime_admission(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<()> {
        if package_transition_state(paths, package_uid)? != "open"
            || service_admission_state(paths, package_uid)? != "open"
        {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        Ok(())
    }

    fn failed_start_child_path(kind: &str) -> BridgeResult<&'static Path> {
        match kind {
            "api" => Ok(Path::new(FAILED_API_CHILD_PATH)),
            "controller" => Ok(Path::new(FAILED_CONTROLLER_CHILD_PATH)),
            _ => Err(BridgeError::bad_request()),
        }
    }

    fn failed_start_child_bytes(
        kind: &str,
        pid: u32,
        start: u64,
        boot: &str,
    ) -> BridgeResult<Vec<u8>> {
        if pid <= 1 || start == 0 || !valid_boot_id(boot) {
            return Err(BridgeError::bad_request());
        }
        let bytes = format!("{kind}\n{pid}\n{start}\n{boot}\n").into_bytes();
        if bytes.len() > 96 {
            return Err(BridgeError::bad_request());
        }
        Ok(bytes)
    }

    pub(super) fn record_failed_start_child(
        package_uid: u32,
        kind: &str,
        pid: u32,
        start: u64,
        boot: &str,
    ) -> BridgeResult<()> {
        let path = failed_start_child_path(kind)?;
        let bytes = failed_start_child_bytes(kind, pid, start, boot)?;
        publish_idempotent_private_marker(path, package_uid, &bytes)
    }

    pub(super) fn failed_start_child_state(
        package_uid: u32,
        kind: &str,
    ) -> BridgeResult<Option<(u32, u64, String)>> {
        let path = failed_start_child_path(kind)?;
        let bytes = match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Ok(_) => read_exact_single_link_private_file(path, package_uid, 96)?,
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| BridgeError::unsafe_runtime())?;
        let fields = text.split('\n').collect::<Vec<_>>();
        if fields.len() != 5 || !fields[4].is_empty() || fields[0] != kind {
            return Err(BridgeError::unsafe_runtime());
        }
        let pid = fields[1]
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 1)
            .ok_or_else(BridgeError::unsafe_runtime)?;
        let start = fields[2]
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(BridgeError::unsafe_runtime)?;
        if !valid_boot_id(fields[3]) {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(Some((pid, start, fields[3].to_owned())))
    }

    pub(super) fn clear_failed_start_child(
        package_uid: u32,
        kind: &str,
        pid: u32,
        start: u64,
        boot: &str,
    ) -> BridgeResult<()> {
        let path = failed_start_child_path(kind)?;
        let bytes = failed_start_child_bytes(kind, pid, start, boot)?;
        remove_allowed_private_marker(path, package_uid, &[bytes.as_slice()])
    }

    pub(super) fn remove_own_service_identity(
        path: &Path,
        package_uid: u32,
        expected: &[u8],
    ) -> BridgeResult<()> {
        let before = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        let bytes = read_exact_single_link_private_file(path, package_uid, 96)?;
        if bytes.as_slice() != expected {
            return Err(BridgeError::unsafe_runtime());
        }
        let current = fs::symlink_metadata(path).map_err(|_| BridgeError::unsafe_runtime())?;
        if before.st_dev() != current.st_dev()
            || before.st_ino() != current.st_ino()
            || before.st_uid() != current.st_uid()
            || before.st_mode() != current.st_mode()
            || before.st_nlink() != current.st_nlink()
        {
            return Err(BridgeError::unsafe_runtime());
        }
        fs::remove_file(path).map_err(|_| BridgeError::unsafe_runtime())?;
        let parent = path.parent().ok_or_else(BridgeError::unsafe_runtime)?;
        sync_directory(parent)
    }

    pub(super) fn audit_transaction_begin<F>(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        mut record: AuditOutboxRecord,
        ready_phase: AuditOutboxPhase,
        mut append: F,
    ) -> BridgeResult<()>
    where
        F: FnMut(&AuditOutboxRecord, &str) -> BridgeResult<()>,
    {
        validate_audit_outbox_record(&record)?;
        if record.phase != AuditOutboxPhase::Prepared
            || !matches!(
                ready_phase,
                AuditOutboxPhase::Prepared
                    | AuditOutboxPhase::Publishing
                    | AuditOutboxPhase::Executing
            )
            || (ready_phase == AuditOutboxPhase::Publishing) != record.job_id.is_some()
        {
            return Err(BridgeError::bad_request());
        }
        let _lock = open_audit_outbox_lock(paths, package_uid)?;
        recover_audit_outbox_temps(paths, package_uid)?;
        let target = audit_outbox_path(paths, &record.transaction)?;
        if fs::symlink_metadata(&target).is_ok() {
            return Err(BridgeError::new(ErrorKind::Conflict));
        }
        write_audit_outbox_record(paths, package_uid, &record)?;
        if let Err(error) = append(&record, "requested") {
            record.phase = AuditOutboxPhase::Failed;
            write_audit_outbox_record(paths, package_uid, &record)?;
            if append(&record, "requested").is_ok() && append(&record, "failed").is_ok() {
                remove_audit_outbox_record(paths, package_uid, &record.transaction)?;
            }
            return Err(error);
        }
        record.phase = ready_phase;
        #[cfg(test)]
        let ready_write_result = if FAIL_AUDIT_READY_WRITE_ONCE.with(|flag| flag.replace(false)) {
            Err(BridgeError::unsafe_runtime())
        } else {
            write_audit_outbox_record(paths, package_uid, &record)
        };
        #[cfg(not(test))]
        let ready_write_result = write_audit_outbox_record(paths, package_uid, &record);
        if let Err(error) = ready_write_result {
            // The durable Prepared record and requested event already exist.
            // Terminalize the known pre-publication failure while this lock is
            // still held; otherwise a long-lived API owner would make
            // reconciliation defer the unterminated request indefinitely.
            record.phase = AuditOutboxPhase::Failed;
            if write_audit_outbox_record(paths, package_uid, &record).is_ok()
                && append(&record, "requested").is_ok()
                && append(&record, "failed").is_ok()
            {
                let _ = remove_audit_outbox_record(paths, package_uid, &record.transaction);
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn audit_transaction_complete<F>(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        transaction: &str,
        terminal: AuditOutboxPhase,
        mut append: F,
    ) -> BridgeResult<bool>
    where
        F: FnMut(&AuditOutboxRecord, &str) -> BridgeResult<()>,
    {
        if terminal.terminal_state().is_none() || !valid_audit_transaction(transaction) {
            return Err(BridgeError::bad_request());
        }
        let _lock = open_audit_outbox_lock(paths, package_uid)?;
        recover_audit_outbox_temps(paths, package_uid)?;
        let mut record = read_audit_outbox_record(paths, package_uid, transaction)?;
        match record.phase {
            AuditOutboxPhase::Executing => {
                record.phase = terminal;
                write_audit_outbox_record(paths, package_uid, &record)?;
            }
            AuditOutboxPhase::Prepared if terminal == AuditOutboxPhase::Failed => {
                record.phase = terminal;
                write_audit_outbox_record(paths, package_uid, &record)?;
            }
            AuditOutboxPhase::Publishing if terminal == AuditOutboxPhase::Failed => {
                let (owner_pid, owner_start, owner_boot) = current_process_identity()?;
                if record.owner_pid != owner_pid
                    || record.owner_start != owner_start
                    || record.owner_boot != owner_boot
                    || bridge_job_state(paths, package_uid, &record)? != BridgeJobState::Missing
                {
                    return Err(BridgeError::unsafe_runtime());
                }
                record.phase = terminal;
                write_audit_outbox_record(paths, package_uid, &record)?;
            }
            phase if phase == terminal => {}
            _ => return Err(BridgeError::unsafe_runtime()),
        }
        let state = record
            .phase
            .terminal_state()
            .ok_or_else(BridgeError::unsafe_runtime)?;
        if append(&record, "requested").is_ok() && append(&record, state).is_ok() {
            mark_response_audit_reconciled(paths, package_uid, &record)?;
            remove_audit_outbox_record(paths, package_uid, transaction)?;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    pub(super) fn mark_audit_transaction_executing(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        transaction: &str,
    ) -> BridgeResult<()> {
        if !valid_audit_transaction(transaction) {
            return Err(BridgeError::bad_request());
        }
        let _lock = open_audit_outbox_lock(paths, package_uid)?;
        recover_audit_outbox_temps(paths, package_uid)?;
        let mut record = read_audit_outbox_record(paths, package_uid, transaction)?;
        let (owner_pid, owner_start, owner_boot) = parent_process_identity()?;
        if record.phase != AuditOutboxPhase::Prepared
            || record.job_id.is_some()
            || record.owner_pid != owner_pid
            || record.owner_start != owner_start
            || record.owner_boot != owner_boot
        {
            return Err(BridgeError::unsafe_runtime());
        }
        record.phase = AuditOutboxPhase::Executing;
        write_audit_outbox_record(paths, package_uid, &record)
    }

    pub(super) fn claim_queued_audit_transaction(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        transaction: &str,
        expected_job_id: &str,
    ) -> BridgeResult<()> {
        if !valid_server_job_id(expected_job_id) {
            return Err(BridgeError::bad_request());
        }
        let _lock = open_audit_outbox_lock(paths, package_uid)?;
        recover_audit_outbox_temps(paths, package_uid)?;
        let mut record = read_audit_outbox_record(paths, package_uid, transaction)?;
        if !matches!(
            record.phase,
            AuditOutboxPhase::Publishing | AuditOutboxPhase::Queued
        ) || record.job_id.as_deref() != Some(expected_job_id)
            || bridge_job_state(paths, package_uid, &record)? != BridgeJobState::Active
        {
            return Err(BridgeError::unsafe_runtime());
        }
        let (owner_pid, owner_start, owner_boot) = current_process_identity()?;
        record.owner_pid = owner_pid;
        record.owner_start = owner_start;
        record.owner_boot = owner_boot;
        record.phase = AuditOutboxPhase::Executing;
        write_audit_outbox_record(paths, package_uid, &record)
    }

    pub(super) fn mark_audit_transaction_queued(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        transaction: &str,
        expected_job_id: &str,
    ) -> BridgeResult<()> {
        let _lock = open_audit_outbox_lock(paths, package_uid)?;
        recover_audit_outbox_temps(paths, package_uid)?;
        let mut record = read_audit_outbox_record(paths, package_uid, transaction)?;
        if record.phase != AuditOutboxPhase::Publishing
            || record.job_id.as_deref() != Some(expected_job_id)
        {
            return Err(BridgeError::unsafe_runtime());
        }
        match bridge_job_state(paths, package_uid, &record)? {
            BridgeJobState::Active => {
                record.phase = AuditOutboxPhase::Queued;
                write_audit_outbox_record(paths, package_uid, &record)
            }
            BridgeJobState::Complete(_) | BridgeJobState::Missing => {
                Err(BridgeError::unsafe_runtime())
            }
        }
    }

    pub(super) fn reconcile_audit_transactions<F>(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        mut append: F,
    ) -> BridgeResult<usize>
    where
        F: FnMut(&AuditOutboxRecord, &str) -> BridgeResult<()>,
    {
        let _lock = open_audit_outbox_lock(paths, package_uid)?;
        recover_audit_outbox_temps(paths, package_uid)?;
        let mut names = Vec::new();
        for entry in fs::read_dir(paths.directory).map_err(|_| BridgeError::unsafe_runtime())? {
            let entry = entry.map_err(|_| BridgeError::unsafe_runtime())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| BridgeError::unsafe_runtime())?;
            if name.starts_with('.') {
                return Err(BridgeError::unsafe_runtime());
            }
            let transaction = name
                .strip_suffix(".event")
                .filter(|value| valid_audit_transaction(value))
                .ok_or_else(BridgeError::unsafe_runtime)?;
            names.push(transaction.to_owned());
        }
        names.sort();
        let mut reconciled = 0;
        for transaction in names {
            let mut record = read_audit_outbox_record(paths, package_uid, &transaction)?;
            match record.phase {
                AuditOutboxPhase::Prepared => {
                    if process_identity_is_live(
                        record.owner_pid,
                        record.owner_start,
                        &record.owner_boot,
                    )? {
                        continue;
                    }
                    record.phase = AuditOutboxPhase::Failed;
                    write_audit_outbox_record(paths, package_uid, &record)?;
                }
                AuditOutboxPhase::Publishing => {
                    match bridge_job_state(paths, package_uid, &record)? {
                        BridgeJobState::Active => {
                            record.phase = AuditOutboxPhase::Queued;
                            write_audit_outbox_record(paths, package_uid, &record)?;
                            continue;
                        }
                        BridgeJobState::Complete(succeeded) => {
                            record.phase = if succeeded {
                                AuditOutboxPhase::Succeeded
                            } else {
                                AuditOutboxPhase::Failed
                            };
                            write_audit_outbox_record(paths, package_uid, &record)?;
                        }
                        BridgeJobState::Missing => {
                            if process_identity_is_live(
                                record.owner_pid,
                                record.owner_start,
                                &record.owner_boot,
                            )? {
                                continue;
                            }
                            record.phase = AuditOutboxPhase::Failed;
                            write_audit_outbox_record(paths, package_uid, &record)?;
                        }
                    }
                }
                AuditOutboxPhase::Queued => match bridge_job_state(paths, package_uid, &record)? {
                    BridgeJobState::Active => continue,
                    BridgeJobState::Complete(succeeded) => {
                        record.phase = if succeeded {
                            AuditOutboxPhase::Succeeded
                        } else {
                            AuditOutboxPhase::Failed
                        };
                        write_audit_outbox_record(paths, package_uid, &record)?;
                    }
                    BridgeJobState::Missing => {
                        record.phase = AuditOutboxPhase::Failed;
                        write_audit_outbox_record(paths, package_uid, &record)?;
                    }
                },
                AuditOutboxPhase::Executing => {
                    match bridge_job_state(paths, package_uid, &record)? {
                        BridgeJobState::Complete(succeeded) => {
                            record.phase = if succeeded {
                                AuditOutboxPhase::Succeeded
                            } else {
                                AuditOutboxPhase::Failed
                            };
                            write_audit_outbox_record(paths, package_uid, &record)?;
                        }
                        BridgeJobState::Active | BridgeJobState::Missing => {}
                    }
                    if record.phase != AuditOutboxPhase::Executing {
                        // A canonical response proves the terminal result even
                        // if the original consumer disappeared before logging.
                    } else if process_identity_is_live(
                        record.owner_pid,
                        record.owner_start,
                        &record.owner_boot,
                    )? {
                        continue;
                    } else {
                        record.phase = AuditOutboxPhase::OutcomeUnknown;
                        write_audit_outbox_record(paths, package_uid, &record)?;
                    }
                }
                AuditOutboxPhase::Succeeded
                | AuditOutboxPhase::Failed
                | AuditOutboxPhase::OutcomeUnknown => {}
            }
            let state = record
                .phase
                .terminal_state()
                .ok_or_else(BridgeError::unsafe_runtime)?;
            append(&record, "requested")?;
            append(&record, state)?;
            mark_response_audit_reconciled(paths, package_uid, &record)?;
            remove_audit_outbox_record(paths, package_uid, &transaction)?;
            reconciled += 1;
        }
        Ok(reconciled)
    }

    fn mark_response_audit_reconciled(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        record: &AuditOutboxRecord,
    ) -> BridgeResult<()> {
        let (Some(responses), Some(job_id)) = (paths.responses, record.job_id.as_deref()) else {
            return Ok(());
        };
        let response_path = responses.join(format!("{job_id}.json"));
        let before = match fs::symlink_metadata(&response_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        };
        let bytes = read_exact_private_file(&response_path, package_uid, MAX_MANAGER_OUTPUT_BYTES)?;
        let parsed = parse_queued_response(&bytes, job_id)?;
        if parsed.audit_transaction != record.transaction {
            return Err(BridgeError::unsafe_runtime());
        }
        if !parsed.audit_pending {
            return Ok(());
        }
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|_| BridgeError::unsafe_runtime())?;
        value["audit_pending"] = Value::Bool(false);
        let rewritten = serde_json::to_vec(&value).map_err(|_| BridgeError::internal())?;
        if rewritten.len() > MAX_MANAGER_OUTPUT_BYTES {
            return Err(BridgeError::unsafe_runtime());
        }
        let temporary = responses.join(format!(".{job_id}.audit-reconciled.tmp"));
        if fs::symlink_metadata(&temporary).is_ok() {
            let _ = read_exact_private_file(&temporary, package_uid, MAX_MANAGER_OUTPUT_BYTES)?;
            fs::remove_file(&temporary).map_err(|_| BridgeError::unsafe_runtime())?;
            sync_directory(responses)?;
        }
        create_private_file(&temporary, package_uid, &rewritten)?;
        let current =
            fs::symlink_metadata(&response_path).map_err(|_| BridgeError::unsafe_runtime())?;
        if before.st_dev() != current.st_dev()
            || before.st_ino() != current.st_ino()
            || current.st_uid() != package_uid
            || current.st_mode() & 0o777 != 0o600
            || current.st_nlink() != 1
        {
            let _ = fs::remove_file(&temporary);
            return Err(BridgeError::unsafe_runtime());
        }
        fs::rename(&temporary, &response_path).map_err(|_| BridgeError::unsafe_runtime())?;
        sync_directory(responses)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum BridgeJobState {
        Active,
        Complete(bool),
        Missing,
    }

    fn bridge_job_state(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        record: &AuditOutboxRecord,
    ) -> BridgeResult<BridgeJobState> {
        let Some(job_id) = record.job_id.as_deref() else {
            return Ok(BridgeJobState::Missing);
        };
        let (Some(requests), Some(processing), Some(responses)) =
            (paths.requests, paths.processing, paths.responses)
        else {
            return Err(BridgeError::unsafe_runtime());
        };
        for directory in [requests, processing, responses] {
            validate_private_directory(directory, package_uid)?;
        }
        for directory in [requests, processing] {
            let job_path = directory.join(format!("{job_id}.json"));
            if let Some(bytes) =
                read_transient_optional_private_file(&job_path, package_uid, MAX_JOB_BYTES)?
            {
                let job = parse_job(&bytes)?;
                if job.request_id != job_id || job.audit_transaction != record.transaction {
                    return Err(BridgeError::unsafe_runtime());
                }
                return Ok(BridgeJobState::Active);
            }
        }
        // The controller publishes the response before removing the processing
        // job. Observing active paths first and the response last closes the
        // normal publish/remove transition without a false Missing result.
        let response_path = responses.join(format!("{job_id}.json"));
        if let Some(bytes) = read_transient_optional_private_file(
            &response_path,
            package_uid,
            MAX_MANAGER_OUTPUT_BYTES,
        )? {
            let response = parse_queued_response(&bytes, job_id)?;
            if response.audit_transaction != record.transaction {
                return Err(BridgeError::unsafe_runtime());
            }
            return Ok(BridgeJobState::Complete(
                response.audit_terminal_state == "succeeded",
            ));
        }
        Ok(BridgeJobState::Missing)
    }

    fn open_audit_outbox_lock(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<File> {
        validate_private_directory(paths.directory, package_uid)?;
        let parent = paths
            .lock
            .parent()
            .ok_or_else(BridgeError::unsafe_runtime)?;
        validate_private_directory(parent, package_uid)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW_CLOEXEC);
        let file = options
            .open(paths.lock)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let metadata = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o777 != 0o600
            || metadata.st_nlink() != 1
        {
            return Err(BridgeError::unsafe_runtime());
        }
        // SAFETY: flock receives a valid descriptor and a fixed operation.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(file)
    }

    fn audit_outbox_path(paths: &AuditOutboxPaths<'_>, transaction: &str) -> BridgeResult<PathBuf> {
        if !valid_audit_transaction(transaction) {
            return Err(BridgeError::bad_request());
        }
        Ok(paths.directory.join(format!("{transaction}.event")))
    }

    fn read_audit_outbox_record(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        transaction: &str,
    ) -> BridgeResult<AuditOutboxRecord> {
        let path = audit_outbox_path(paths, transaction)?;
        let bytes = read_exact_private_file(&path, package_uid, MAX_AUDIT_OUTBOX_BYTES)?;
        let record: AuditOutboxRecord =
            serde_json::from_slice(&bytes).map_err(|_| BridgeError::unsafe_runtime())?;
        validate_audit_outbox_record(&record)?;
        if record.transaction != transaction {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(record)
    }

    fn write_audit_outbox_record(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        record: &AuditOutboxRecord,
    ) -> BridgeResult<()> {
        validate_audit_outbox_record(record)?;
        let target = audit_outbox_path(paths, &record.transaction)?;
        if fs::symlink_metadata(&target).is_ok() {
            let _ = private_file_metadata(&target, package_uid, MAX_AUDIT_OUTBOX_BYTES)?;
        }
        let temporary = paths.directory.join(format!(
            ".{}.{}.tmp",
            record.transaction,
            std::process::id()
        ));
        match fs::symlink_metadata(&temporary) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                let _ = private_file_metadata(&temporary, package_uid, MAX_AUDIT_OUTBOX_BYTES)?;
                fs::remove_file(&temporary).map_err(|_| BridgeError::unsafe_runtime())?;
            }
            Err(_) => return Err(BridgeError::unsafe_runtime()),
        }
        let bytes = serde_json::to_vec(record).map_err(|_| BridgeError::internal())?;
        if bytes.is_empty() || bytes.len() > MAX_AUDIT_OUTBOX_BYTES {
            return Err(BridgeError::unsafe_runtime());
        }
        create_private_file(&temporary, package_uid, &bytes)?;
        fs::rename(&temporary, &target).map_err(|_| BridgeError::unsafe_runtime())?;
        sync_directory(paths.directory)
    }

    fn remove_audit_outbox_record(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
        transaction: &str,
    ) -> BridgeResult<()> {
        let path = audit_outbox_path(paths, transaction)?;
        let _ = private_file_metadata(&path, package_uid, MAX_AUDIT_OUTBOX_BYTES)?;
        fs::remove_file(path).map_err(|_| BridgeError::unsafe_runtime())?;
        sync_directory(paths.directory)
    }

    fn recover_audit_outbox_temps(
        paths: &AuditOutboxPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<()> {
        for entry in fs::read_dir(paths.directory).map_err(|_| BridgeError::unsafe_runtime())? {
            let entry = entry.map_err(|_| BridgeError::unsafe_runtime())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| BridgeError::unsafe_runtime())?;
            if !name.starts_with('.') {
                continue;
            }
            let Some(stem) = name
                .strip_prefix('.')
                .and_then(|value| value.strip_suffix(".tmp"))
            else {
                return Err(BridgeError::unsafe_runtime());
            };
            let Some((transaction, pid)) = stem.rsplit_once('.') else {
                return Err(BridgeError::unsafe_runtime());
            };
            if !valid_audit_transaction(transaction)
                || pid.parse::<u32>().ok().is_none_or(|value| value <= 1)
            {
                return Err(BridgeError::unsafe_runtime());
            }
            let _ = private_file_metadata(&entry.path(), package_uid, MAX_AUDIT_OUTBOX_BYTES)?;
            fs::remove_file(entry.path()).map_err(|_| BridgeError::unsafe_runtime())?;
        }
        sync_directory(paths.directory)
    }

    fn current_boot_id() -> BridgeResult<String> {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(NOFOLLOW_CLOEXEC);
        let file = options
            .open("/proc/sys/kernel/random/boot_id")
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let metadata = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file() || metadata.st_uid() != 0 || metadata.len() > 64 {
            return Err(BridgeError::unsafe_runtime());
        }
        let mut value = String::new();
        file.take(65)
            .read_to_string(&mut value)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let value = value.trim_end_matches('\n').to_owned();
        if !valid_boot_id(&value) {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(value)
    }

    fn process_start(pid: u32) -> BridgeResult<u64> {
        let value = fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let tail = value
            .rsplit_once(") ")
            .map(|(_, tail)| tail)
            .ok_or_else(BridgeError::unsafe_runtime)?;
        tail.split_ascii_whitespace()
            .nth(19)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value != 0)
            .ok_or_else(BridgeError::unsafe_runtime)
    }

    fn process_identity_is_live(pid: u32, start: u64, boot: &str) -> BridgeResult<bool> {
        if current_boot_id()? != boot {
            return Ok(false);
        }
        // SAFETY: kill with signal zero performs a permission/liveness probe.
        let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if status != 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(false),
                _ => Err(BridgeError::unsafe_runtime()),
            };
        }
        match process_start(pid) {
            Ok(actual) => Ok(actual == start),
            Err(_) => Ok(false),
        }
    }

    fn validate_private_directory(path: &Path, package_uid: u32) -> BridgeResult<()> {
        let metadata = fs::symlink_metadata(path).map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_dir()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o777 != 0o700
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    pub(super) fn validate_private_executable(path: &Path, package_uid: u32) -> BridgeResult<()> {
        let metadata = fs::symlink_metadata(path).map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o7777 != 0o755
            || metadata.st_nlink() != 1
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    fn read_exact_private_file(
        path: &Path,
        package_uid: u32,
        maximum: usize,
    ) -> BridgeResult<Zeroizing<Vec<u8>>> {
        read_private_file_with_link_contract(path, package_uid, maximum, false)
    }

    fn read_exact_single_link_private_file(
        path: &Path,
        package_uid: u32,
        maximum: usize,
    ) -> BridgeResult<Zeroizing<Vec<u8>>> {
        read_private_file_with_link_contract(path, package_uid, maximum, true)
    }

    fn read_private_file_with_link_contract(
        path: &Path,
        package_uid: u32,
        maximum: usize,
        require_single_link: bool,
    ) -> BridgeResult<Zeroizing<Vec<u8>>> {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(NOFOLLOW_CLOEXEC);
        let mut file = options.open(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                BridgeError::new(ErrorKind::Unavailable)
            } else {
                BridgeError::unsafe_runtime()
            }
        })?;
        let metadata = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o7777 != 0o600
            || (require_single_link && metadata.st_nlink() != 1)
            || metadata.len() > maximum as u64
        {
            return Err(BridgeError::unsafe_runtime());
        }
        let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
        Read::by_ref(&mut file)
            .take((maximum + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        if bytes.len() > maximum || bytes.len() as u64 != metadata.len() {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(bytes)
    }

    fn read_optional_private_file(
        path: &Path,
        package_uid: u32,
        maximum: usize,
    ) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        match fs::symlink_metadata(path) {
            Ok(_) => read_exact_private_file(path, package_uid, maximum).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(BridgeError::unsafe_runtime()),
        }
    }

    fn read_optional_single_link_private_file(
        path: &Path,
        package_uid: u32,
        maximum: usize,
    ) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        match fs::symlink_metadata(path) {
            Ok(_) => read_exact_single_link_private_file(path, package_uid, maximum).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(BridgeError::unsafe_runtime()),
        }
    }

    fn create_private_file(path: &Path, package_uid: u32, bytes: &[u8]) -> BridgeResult<()> {
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW_CLOEXEC);
        let mut file = options
            .open(path)
            .map_err(|error| map_create_error(&error))?;
        let metadata = file.metadata().map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != package_uid
            || metadata.st_mode() & 0o777 != 0o600
        {
            let _ = fs::remove_file(path);
            return Err(BridgeError::unsafe_runtime());
        }
        if file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(path);
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    fn fill_random(output: &mut [u8]) -> BridgeResult<()> {
        fill_random_with(output, |chunk| {
            // SAFETY: the pointer targets a valid mutable slice and the slice
            // length is supplied exactly.
            let result = unsafe { libc::getrandom(chunk.as_mut_ptr().cast(), chunk.len(), 0) };
            if result >= 0 {
                Ok(result as usize)
            } else {
                Err(io::Error::last_os_error())
            }
        })
    }

    pub(super) fn fill_random_with(
        output: &mut [u8],
        mut getrandom_chunk: impl FnMut(&mut [u8]) -> io::Result<usize>,
    ) -> BridgeResult<()> {
        let mut written = 0;
        while written < output.len() {
            match getrandom_chunk(&mut output[written..]) {
                Ok(0) => break,
                Ok(count) if count <= output.len() - written => written += count,
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {
                    // Linux getrandom(2) postdates the 3.2 kernel shipped on
                    // some DSM 7.1 models. Replace the whole buffer from the
                    // kernel random character device rather than combining a
                    // possibly partial syscall result with the fallback.
                    output.zeroize();
                    return fill_random_from_device(output, Path::new("/dev/urandom"));
                }
                Err(_) => break,
            }
        }
        if written == output.len() {
            Ok(())
        } else {
            output.zeroize();
            Err(BridgeError::new(ErrorKind::Unavailable))
        }
    }

    fn fill_random_from_device(output: &mut [u8], path: &Path) -> BridgeResult<()> {
        let result = (|| {
            let mut options = OpenOptions::new();
            options.read(true).custom_flags(NOFOLLOW_CLOEXEC);
            let mut source = options
                .open(path)
                .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
            let metadata = source
                .metadata()
                .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
            if !metadata.file_type().is_char_device() || metadata.st_uid() != 0 {
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }
            let mut read = 0;
            while read < output.len() {
                match source.read(&mut output[read..]) {
                    Ok(0) => return Err(BridgeError::new(ErrorKind::Unavailable)),
                    Ok(count) => read += count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return Err(BridgeError::new(ErrorKind::Unavailable)),
                }
            }
            Ok(())
        })();
        if result.is_err() {
            output.zeroize();
        }
        result
    }

    fn sync_directory(path: &Path) -> BridgeResult<()> {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| BridgeError::unsafe_runtime())
    }

    fn map_create_error(error: &io::Error) -> BridgeError {
        if error.kind() == io::ErrorKind::AlreadyExists {
            BridgeError::new(ErrorKind::Conflict)
        } else {
            BridgeError::unsafe_runtime()
        }
    }

    struct SecretRemovalGuard {
        path: PathBuf,
    }

    impl Drop for SecretRemovalGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug)]
struct CgiResponse {
    status: u16,
    body: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderedCgiError<'a> {
    schema: &'a str,
    ok: bool,
    status: u16,
    code: &'a str,
    stage: Option<&'a str>,
    message: &'a str,
}

impl CgiResponse {
    fn success(body: Vec<u8>) -> Self {
        Self { status: 200, body }
    }

    fn accepted(body: Vec<u8>) -> Self {
        Self { status: 202, body }
    }

    fn gone(body: Vec<u8>) -> Self {
        Self { status: 410, body }
    }

    #[cfg(test)]
    fn service_unavailable() -> Self {
        Self::staged_error(
            CgiFailureStage::BridgeConnect,
            BridgeError::new(ErrorKind::Unavailable),
        )
    }

    fn error(error: BridgeError) -> Self {
        Self::error_payload(error, None, None)
    }

    fn staged_error(stage: CgiFailureStage, error: BridgeError) -> Self {
        Self::error_payload(error, Some(stage), None)
    }

    fn failure(failure: CgiFailure) -> Self {
        Self::error_payload(failure.error, Some(failure.stage), failure.code)
    }

    fn error_payload(
        error: BridgeError,
        stage: Option<CgiFailureStage>,
        explicit_code: Option<&'static str>,
    ) -> Self {
        let (status, default_code) = match error.kind {
            ErrorKind::BadRequest => (400, "invalid_request"),
            ErrorKind::Unauthorized => (401, "unauthorized"),
            ErrorKind::Forbidden => (403, "forbidden"),
            ErrorKind::CsrfRejected => (403, "csrf_rejected"),
            ErrorKind::MethodNotAllowed => (405, "method_not_allowed"),
            ErrorKind::UnsupportedMediaType => (415, "unsupported_media_type"),
            ErrorKind::PayloadTooLarge => (413, "payload_too_large"),
            ErrorKind::Conflict => (409, "conflict"),
            ErrorKind::UnsafeRuntime | ErrorKind::Unavailable => (503, "unavailable"),
            ErrorKind::Internal => (500, "internal_error"),
        };
        let code = explicit_code.unwrap_or(match (error.kind, stage) {
            (ErrorKind::UnsafeRuntime, Some(CgiFailureStage::Identity)) => "cgi_identity_unsafe",
            (ErrorKind::UnsafeRuntime, Some(CgiFailureStage::Authentication)) => {
                "dsm_authentication_unsafe"
            }
            (ErrorKind::Unavailable, Some(CgiFailureStage::Authentication)) => {
                "dsm_authentication_unavailable"
            }
            (ErrorKind::UnsafeRuntime, Some(CgiFailureStage::Runtime)) => "cgi_runtime_unsafe",
            (ErrorKind::Unavailable, Some(CgiFailureStage::Runtime)) => "cgi_runtime_unavailable",
            (ErrorKind::UnsafeRuntime, Some(CgiFailureStage::BridgeConnect)) => {
                "bridge_socket_unsafe"
            }
            (ErrorKind::Unavailable, Some(CgiFailureStage::BridgeConnect)) => "service_unavailable",
            (ErrorKind::UnsafeRuntime, Some(CgiFailureStage::BridgeIo)) => "bridge_io_unsafe",
            (ErrorKind::Unavailable, Some(CgiFailureStage::BridgeIo)) => "bridge_io_unavailable",
            (ErrorKind::UnsafeRuntime, Some(CgiFailureStage::BridgeProtocol)) => {
                "bridge_protocol_unsafe"
            }
            (ErrorKind::Unavailable, Some(CgiFailureStage::BridgeProtocol)) => {
                "bridge_protocol_unavailable"
            }
            (
                ErrorKind::UnsafeRuntime | ErrorKind::Unavailable,
                Some(CgiFailureStage::ServiceRequest),
            ) => "service_request_unavailable",
            _ => default_code,
        });
        let message = if stage == Some(CgiFailureStage::BridgeConnect)
            && error.kind == ErrorKind::Unavailable
        {
            "The package service is not ready. Retry shortly. If this persists, restart Synology Drive Sync in Package Center and inspect its controller log."
        } else {
            "Request could not be completed."
        };
        let body = serde_json::to_vec(&json!({
            "schema": "sdsync.dsm-error.v1",
            "ok": false,
            "status": status,
            "code": code,
            "stage": stage.map(CgiFailureStage::as_str),
            "message": message,
        }))
        .unwrap_or_else(|_| {
            format!(
                r#"{{"schema":"sdsync.dsm-error.v1","ok":false,"status":{status},"code":"internal_error","stage":null,"message":"Request could not be completed."}}"#
            )
            .into_bytes()
        });
        Self { status, body }
    }

    fn for_cgi_transport(mut self, is_get: bool) -> Self {
        if is_get && self.is_trusted_error_envelope() {
            // Webman can replace CGI 4xx/5xx bodies with an empty gateway
            // response. GET is read-only, so carry the original application
            // status in the authenticated JSON envelope while keeping the CGI
            // transport successful. Mutation failures retain their real HTTP
            // status and are never normalized here.
            self.status = 200;
        }
        self
    }

    fn is_trusted_error_envelope(&self) -> bool {
        let Ok(payload) = serde_json::from_slice::<RenderedCgiError<'_>>(&self.body) else {
            return false;
        };
        payload.schema == "sdsync.dsm-error.v1"
            && !payload.ok
            && payload.status == self.status
            && !payload.code.is_empty()
            && payload.code.len() <= 64
            && payload
                .code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            && payload.stage.is_none_or(|stage| {
                matches!(
                    stage,
                    "request"
                        | "cgi_identity"
                        | "dsm_authentication"
                        | "cgi_runtime"
                        | "bridge_connect"
                        | "bridge_io"
                        | "bridge_protocol"
                        | "service_request"
                )
            })
            && !payload.message.is_empty()
            && payload.message.len() <= 512
            && !payload.message.bytes().any(|byte| byte.is_ascii_control())
    }
}

#[cfg(target_os = "linux")]
fn record_pre_relay_cgi_failure(failure: &CgiFailure) {
    let diagnostic = CgiResponse::failure(*failure);
    if !diagnostic.is_trusted_error_envelope() {
        return;
    }
    let Ok(payload) = serde_json::from_slice::<RenderedCgiError<'_>>(&diagnostic.body) else {
        return;
    };
    let Some(stage) = payload.stage else {
        return;
    };
    let package_uid =
        linux_runtime::identity_state().and_then(|identity| validate_cgi_identity(&identity));
    let Ok(package_uid) = package_uid else {
        return;
    };
    let Ok(now) = current_epoch() else {
        return;
    };
    let Ok(true) = linux_files::record_pre_relay_cgi_failure(
        package_uid,
        now,
        stage,
        payload.code,
        payload.status,
    ) else {
        return;
    };
    let _ = record_pre_relay_activity(stage, payload.code, payload.status);
}

#[cfg(target_os = "linux")]
fn record_pre_relay_activity(stage: &str, code: &str, status: u16) -> BridgeResult<()> {
    linux_runtime::validate_package_manager()?;
    let status = status.to_string();
    let mut command = Command::new(MANAGER_PATH);
    command
        .env_clear()
        .envs(manager_command_environment())
        .args([
            "api",
            "cgi-failure",
            "--stage",
            stage,
            "--code",
            code,
            "--status",
            &status,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = capture_bounded_command(
        &mut command,
        4 * 1024,
        MAX_HELPER_STDERR_BYTES,
        Duration::from_secs(2),
        None,
    )?;
    if output.status_success {
        Ok(())
    } else {
        Err(BridgeError::new(ErrorKind::Unavailable))
    }
}

pub(crate) fn main_entry() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() {
        let is_get = std::env::var("REQUEST_METHOD").is_ok_and(|method| method == "GET");
        let response = match run_cgi() {
            Ok(response) => response,
            Err(failure) => {
                #[cfg(target_os = "linux")]
                record_pre_relay_cgi_failure(&failure);
                CgiResponse::failure(failure)
            }
        }
        .for_cgi_transport(is_get);
        cgi_exit_code(write_cgi_response(&response).is_ok())
    } else if arguments.len() == 1 && arguments[0] == "--serve" {
        match run_server() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        }
    } else if arguments.len() == 4 && arguments[0] == "--serve-supervised" {
        #[cfg(target_os = "linux")]
        {
            let result = (|| {
                let parent_pid = arguments[1]
                    .to_str()
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|value| *value > 1)
                    .ok_or_else(BridgeError::bad_request)?;
                let parent_start = arguments[2]
                    .to_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(BridgeError::bad_request)?;
                let parent_boot = arguments[3]
                    .to_str()
                    .filter(|value| valid_boot_id(value))
                    .ok_or_else(BridgeError::bad_request)?
                    .to_owned();
                run_supervised_server(SupervisedParent {
                    pid: parent_pid,
                    start: parent_start,
                    boot: parent_boot,
                })
            })();
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            ExitCode::FAILURE
        }
    } else if arguments.len() == 4 && arguments[0] == "--exec-supervised-controller" {
        #[cfg(target_os = "linux")]
        {
            let result = (|| {
                let parent_pid = arguments[1]
                    .to_str()
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|value| *value > 1)
                    .ok_or_else(BridgeError::bad_request)?;
                let parent_start = arguments[2]
                    .to_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(BridgeError::bad_request)?;
                let parent_boot = arguments[3]
                    .to_str()
                    .filter(|value| valid_boot_id(value))
                    .ok_or_else(BridgeError::bad_request)?
                    .to_owned();
                exec_supervised_controller(SupervisedParent {
                    pid: parent_pid,
                    start: parent_start,
                    boot: parent_boot,
                })
            })();
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            ExitCode::FAILURE
        }
    } else if arguments.len() >= 6
        && arguments[0] == "--exec-supervised-core"
        && arguments[4] == "--"
    {
        #[cfg(target_os = "linux")]
        {
            let result = (|| {
                let parent_pid = arguments[1]
                    .to_str()
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|value| *value > 1)
                    .ok_or_else(BridgeError::bad_request)?;
                let parent_start = arguments[2]
                    .to_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(BridgeError::bad_request)?;
                let parent_boot = arguments[3]
                    .to_str()
                    .filter(|value| valid_boot_id(value))
                    .ok_or_else(BridgeError::bad_request)?
                    .to_owned();
                if arguments[5] != BINARY_PATH {
                    return Err(BridgeError::bad_request());
                }
                exec_supervised_core(
                    SupervisedParent {
                        pid: parent_pid,
                        start: parent_start,
                        boot: parent_boot,
                    },
                    &arguments[6..],
                )
            })();
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            ExitCode::FAILURE
        }
    } else if arguments.len() == 2 && arguments[0] == "--classify-queued-job" {
        let Some(request_id) = arguments[1].to_str() else {
            return ExitCode::from(64);
        };
        match classify_queued_job(request_id) {
            Ok(classification) => {
                println!("{}", classification.as_str());
                ExitCode::SUCCESS
            }
            Err(error) if error.kind == ErrorKind::BadRequest => ExitCode::from(64),
            Err(_) => ExitCode::from(73),
        }
    } else if arguments.len() == 3 && arguments[0] == "--consume-job" {
        let request = PathBuf::from(&arguments[1]);
        let response = PathBuf::from(&arguments[2]);
        match run_consumer(&request, &response) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        }
    } else if arguments.len() == 3 && arguments[0] == "--reject-job" {
        let request = PathBuf::from(&arguments[1]);
        let response = PathBuf::from(&arguments[2]);
        match reject_claimed_job(&request, &response) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        }
    } else if matches!(arguments.len(), 1 | 4) && arguments[0] == "--cleanup-stale-api-socket" {
        #[cfg(target_os = "linux")]
        {
            let result = (|| {
                let expected_terminal = if arguments.len() == 4 {
                    let pid = arguments[1]
                        .to_str()
                        .and_then(|value| value.parse::<u32>().ok())
                        .filter(|value| *value > 1 && *value <= libc::pid_t::MAX as u32)
                        .ok_or_else(BridgeError::bad_request)?;
                    let start = arguments[2]
                        .to_str()
                        .and_then(|value| value.parse::<u64>().ok())
                        .filter(|value| *value != 0)
                        .ok_or_else(BridgeError::bad_request)?;
                    let boot = arguments[3]
                        .to_str()
                        .filter(|value| valid_boot_id(value))
                        .ok_or_else(BridgeError::bad_request)?
                        .to_owned();
                    Some(linux_socket::TerminalProcessIdentity { pid, start, boot })
                } else {
                    None
                };
                let identity = linux_runtime::identity_state()?;
                let package_uid = validate_package_identity(&identity)?;
                linux_runtime::clear_environment()?;
                linux_socket::cleanup_stale_service_socket(
                    Path::new(API_SOCKET_PATH),
                    Path::new(API_PID_PATH),
                    package_uid,
                    expected_terminal.as_ref(),
                )
            })();
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) if error.kind == ErrorKind::Conflict => ExitCode::from(75),
                Err(_) => ExitCode::from(73),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            ExitCode::from(73)
        }
    } else if arguments.len() == 1
        && (arguments[0] == "--migrate-security-policy"
            || arguments[0] == "--security-policy-migration-status")
    {
        #[cfg(target_os = "linux")]
        {
            let migration_requested = arguments[0] == "--migrate-security-policy";
            let result = (|| {
                let identity = linux_runtime::identity_state()?;
                let package_uid = validate_package_identity(&identity)?;
                linux_runtime::clear_environment()?;
                if migration_requested {
                    linux_files::migrate_security_policy(package_uid)
                } else {
                    linux_files::security_policy_migration_required(package_uid)
                }
            })();
            match result {
                Ok(changed_or_required) => {
                    println!(
                        "{}",
                        if changed_or_required {
                            if migration_requested {
                                "migrated"
                            } else {
                                "required"
                            }
                        } else {
                            "unchanged"
                        }
                    );
                    ExitCode::SUCCESS
                }
                Err(_) => ExitCode::from(73),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            ExitCode::from(73)
        }
    } else if arguments.first().is_some_and(|value| {
        value == "--package-transition"
            || value == "--service-admission"
            || value == "--failed-start-child"
    }) {
        match run_runtime_marker_cli(&arguments) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) if error.kind == ErrorKind::BadRequest => ExitCode::from(64),
            Err(error) if error.kind == ErrorKind::Forbidden => ExitCode::from(77),
            Err(_) => ExitCode::from(73),
        }
    } else if arguments
        .first()
        .is_some_and(|value| value == "--audit-transaction")
    {
        match run_audit_transaction_cli(&arguments[1..]) {
            Ok(false) => ExitCode::SUCCESS,
            Ok(true) => ExitCode::from(75),
            Err(error) if error.kind == ErrorKind::BadRequest => ExitCode::from(64),
            Err(error) if error.kind == ErrorKind::Forbidden => ExitCode::from(77),
            Err(_) => ExitCode::from(73),
        }
    } else if std::env::var_os("REQUEST_METHOD").is_some() {
        let is_get = std::env::var("REQUEST_METHOD").is_ok_and(|method| method == "GET");
        let response = CgiResponse::error(BridgeError::bad_request()).for_cgi_transport(is_get);
        cgi_exit_code(write_cgi_response(&response).is_ok())
    } else {
        ExitCode::FAILURE
    }
}

fn cgi_exit_code(response_written: bool) -> ExitCode {
    // The CGI Status header carries the HTTP outcome. Once a complete response
    // has been written, the transport itself succeeded; returning a process
    // failure can make Webman replace an intentional 4xx/5xx response with an
    // undifferentiated gateway error and discard the safe diagnostic payload.
    if response_written {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "linux")]
fn run_runtime_marker_cli(arguments: &[OsString]) -> BridgeResult<()> {
    let identity = linux_runtime::identity_state()?;
    let package_uid = validate_package_identity(&identity)?;
    let text = |index: usize| -> BridgeResult<&str> {
        arguments
            .get(index)
            .and_then(|value| value.to_str())
            .ok_or_else(BridgeError::bad_request)
    };
    linux_runtime::clear_environment()?;
    let control_paths = ControlPaths::production();
    match (text(0)?, text(1)?, arguments.len()) {
        ("--package-transition", "status", 2) => {
            println!(
                "{}",
                linux_files::package_transition_state(&control_paths, package_uid)?
            );
            Ok(())
        }
        ("--package-transition", "prepare", 3) => {
            linux_files::prepare_package_transition(&control_paths, package_uid, text(2)?)
        }
        ("--package-transition", "clear", 2) => {
            linux_files::clear_package_transition(&control_paths, package_uid)
        }
        ("--service-admission", "status", 2) => {
            println!(
                "{}",
                linux_files::service_admission_state(&control_paths, package_uid)?
            );
            Ok(())
        }
        ("--service-admission", "close", 2) => {
            linux_files::close_service_admission(&control_paths, package_uid)
        }
        ("--service-admission", "open", 2) => {
            linux_files::open_service_admission(&control_paths, package_uid)
        }
        ("--failed-start-child", "status", 3) => {
            match linux_files::failed_start_child_state(package_uid, text(2)?)? {
                Some((pid, start, boot)) => println!("present {pid} {start} {boot}"),
                None => println!("absent"),
            }
            Ok(())
        }
        ("--failed-start-child", action @ ("record" | "clear"), 6) => {
            let kind = text(2)?;
            let pid = text(3)?
                .parse::<u32>()
                .ok()
                .filter(|value| *value > 1)
                .ok_or_else(BridgeError::bad_request)?;
            let start = text(4)?
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(BridgeError::bad_request)?;
            let boot = text(5)?;
            if action == "record" {
                linux_files::record_failed_start_child(package_uid, kind, pid, start, boot)
            } else {
                linux_files::clear_failed_start_child(package_uid, kind, pid, start, boot)
            }
        }
        _ => Err(BridgeError::bad_request()),
    }
}

#[cfg(not(target_os = "linux"))]
fn run_runtime_marker_cli(_arguments: &[OsString]) -> BridgeResult<()> {
    Err(BridgeError::unsafe_runtime())
}

#[cfg(target_os = "linux")]
fn run_audit_transaction_cli(arguments: &[OsString]) -> BridgeResult<bool> {
    let identity = linux_runtime::identity_state()?;
    let package_uid = validate_package_identity(&identity)?;
    let paths = AuditOutboxPaths::production();
    if arguments.len() == 2
        && arguments
            .first()
            .is_some_and(|value| value == "repair-log-tail")
    {
        let kind = arguments[1].to_str().ok_or_else(BridgeError::bad_request)?;
        linux_runtime::clear_environment()?;
        let repaired = linux_files::repair_durable_log_tail(kind, package_uid)?;
        println!("{}", if repaired { "repaired" } else { "clean" });
        return Ok(false);
    }
    let client_request_id = std::env::var_os("SDSYNC_DSM_CLIENT_REQUEST_ID")
        .map(|value| {
            value
                .into_string()
                .ok()
                .filter(|value| valid_client_request_id(value))
                .ok_or_else(BridgeError::bad_request)
        })
        .transpose()?;
    let text = |index: usize| -> BridgeResult<String> {
        arguments
            .get(index)
            .and_then(|value| value.to_str())
            .map(ToOwned::to_owned)
            .ok_or_else(BridgeError::bad_request)
    };
    match arguments.first().and_then(|value| value.to_str()) {
        Some("create") if arguments.len() == 2 => {
            let origin = text(1)?;
            if !valid_audit_origin(&origin) {
                return Err(BridgeError::bad_request());
            }
            let (owner_pid, owner_start, owner_boot) = linux_files::parent_process_identity()?;
            let nonce = linux_files::random_nonce()?;
            let mut digest = Sha256::new();
            digest.update(b"sdsync.dsm-audit-transaction.v1\0");
            digest.update(origin.as_bytes());
            digest.update(owner_pid.to_be_bytes());
            digest.update(owner_start.to_be_bytes());
            digest.update(owner_boot.as_bytes());
            digest.update(nonce);
            let transaction = format!("{origin}-{}", hex_encode(&digest.finalize()));
            if !valid_audit_transaction(&transaction) {
                return Err(BridgeError::internal());
            }
            println!("{transaction}");
            Ok(false)
        }
        Some("reconcile") if arguments.len() == 1 => {
            linux_runtime::clear_environment()?;
            linux_files::reconcile_audit_transactions(&paths, package_uid, record_audit_event)?;
            Ok(false)
        }
        Some("begin") if arguments.len() == 7 => {
            let operation = text(1)?;
            let profile = text(2)?;
            let actor = text(3)?;
            let actor_uid = text(4)?
                .parse::<u32>()
                .ok()
                .filter(|value| *value != 0)
                .ok_or_else(BridgeError::bad_request)?;
            let origin = text(5)?;
            let transaction = text(6)?;
            let (owner_pid, owner_start, owner_boot) = linux_files::parent_process_identity()?;
            let record = AuditOutboxRecord {
                schema: "sdsync.dsm-audit-outbox.v1".to_owned(),
                transaction,
                operation,
                profile,
                actor,
                actor_uid,
                origin,
                client_request_id: client_request_id.clone(),
                job_id: None,
                owner_pid,
                owner_start,
                owner_boot,
                phase: AuditOutboxPhase::Prepared,
            };
            linux_runtime::clear_environment()?;
            linux_files::audit_transaction_begin(
                &paths,
                package_uid,
                record,
                AuditOutboxPhase::Prepared,
                record_audit_event,
            )?;
            Ok(false)
        }
        Some("execute") if arguments.len() == 2 => {
            let transaction = text(1)?;
            linux_runtime::clear_environment()?;
            linux_files::mark_audit_transaction_executing(&paths, package_uid, &transaction)?;
            Ok(false)
        }
        Some("complete") if arguments.len() == 3 => {
            let transaction = text(1)?;
            let terminal = match arguments.get(2).and_then(|value| value.to_str()) {
                Some("succeeded") => AuditOutboxPhase::Succeeded,
                Some("failed") => AuditOutboxPhase::Failed,
                Some("outcome_unknown") => AuditOutboxPhase::OutcomeUnknown,
                _ => return Err(BridgeError::bad_request()),
            };
            linux_runtime::clear_environment()?;
            linux_files::audit_transaction_complete(
                &paths,
                package_uid,
                &transaction,
                terminal,
                record_audit_event,
            )
        }
        Some("verify") if arguments.len() == 8 => {
            let operation = text(1)?;
            let profile = text(2)?;
            let actor = text(3)?;
            let actor_uid = text(4)?
                .parse::<u32>()
                .ok()
                .filter(|value| *value != 0)
                .ok_or_else(BridgeError::bad_request)?;
            let origin = text(5)?;
            let transaction = text(6)?;
            let state = text(7)?;
            let (owner_pid, owner_start, owner_boot) = linux_files::parent_process_identity()?;
            let record = AuditOutboxRecord {
                schema: "sdsync.dsm-audit-outbox.v1".to_owned(),
                transaction,
                operation,
                profile,
                actor,
                actor_uid,
                origin,
                client_request_id: client_request_id.clone(),
                job_id: None,
                owner_pid,
                owner_start,
                owner_boot,
                phase: AuditOutboxPhase::Prepared,
            };
            linux_runtime::clear_environment()?;
            linux_files::durably_verify_audit_event(&record, &state, package_uid)?;
            Ok(false)
        }
        Some("validate") if arguments.len() == 7 => {
            let operation = text(1)?;
            let profile = text(2)?;
            let actor = text(3)?;
            let actor_uid = text(4)?
                .parse::<u32>()
                .ok()
                .filter(|value| *value != 0)
                .ok_or_else(BridgeError::bad_request)?;
            let origin = text(5)?;
            let transaction = text(6)?;
            let (owner_pid, owner_start, owner_boot) = linux_files::parent_process_identity()?;
            let record = AuditOutboxRecord {
                schema: "sdsync.dsm-audit-outbox.v1".to_owned(),
                transaction,
                operation,
                profile,
                actor,
                actor_uid,
                origin,
                client_request_id,
                job_id: None,
                owner_pid,
                owner_start,
                owner_boot,
                phase: AuditOutboxPhase::Prepared,
            };
            linux_runtime::clear_environment()?;
            validate_audit_identity(&record)?;
            Ok(false)
        }
        _ => Err(BridgeError::bad_request()),
    }
}

#[cfg(not(target_os = "linux"))]
fn run_audit_transaction_cli(_arguments: &[OsString]) -> BridgeResult<bool> {
    Err(BridgeError::unsafe_runtime())
}

fn run_cgi() -> Result<CgiResponse, CgiFailure> {
    #[cfg(not(target_os = "linux"))]
    return Err(CgiFailure::new(
        CgiFailureStage::Identity,
        BridgeError::unsafe_runtime(),
    ));

    #[cfg(target_os = "linux")]
    {
        let environment = process_environment()
            .map_err(|error| CgiFailure::new(CgiFailureStage::Request, error))?;
        let identity = linux_runtime::identity_state()
            .map_err(|error| CgiFailure::new(CgiFailureStage::Identity, error))?;
        let package_uid = validate_cgi_identity(&identity)
            .map_err(|error| CgiFailure::new(CgiFailureStage::Identity, error))?;
        let request = validate_http_request(environment.clone())
            .map_err(|error| CgiFailure::new(CgiFailureStage::Request, error))?;
        let authentication = match &request {
            ValidatedHttpRequest::Get { authentication, .. }
            | ValidatedHttpRequest::Post { authentication, .. } => authentication,
        };
        // Synology documents authenticate.cgi being invoked from the custom
        // CGI, where DSM's native request environment is still authoritative.
        let session =
            linux_runtime::authenticate_and_authorize_cgi(authentication, AUTH_HELPER_TIMEOUT)?;
        let body = match request {
            ValidatedHttpRequest::Get { .. } => None,
            ValidatedHttpRequest::Post { content_length, .. } => Some(
                read_exact_body(&mut io::stdin().lock(), content_length)
                    .map_err(|error| CgiFailure::new(CgiFailureStage::Request, error))?,
            ),
        };
        let encoded = encode_relay_request(
            &environment,
            body.as_ref().map(|value| value.as_slice()),
            &session,
        )
        .map_err(|error| CgiFailure::new(CgiFailureStage::Runtime, error))?;
        linux_runtime::clear_environment()
            .map_err(|error| CgiFailure::new(CgiFailureStage::Runtime, error))?;
        let mut stream = match linux_socket::connect_for_cgi(
            Path::new(API_SOCKET_PATH),
            package_uid,
            CGI_SERVICE_CONNECT_WINDOW,
        ) {
            Ok(stream) => stream,
            Err(error) if error.kind == ErrorKind::Unavailable => {
                return Err(CgiFailure::new(CgiFailureStage::BridgeConnect, error));
            }
            Err(error) => {
                return Err(CgiFailure::new(CgiFailureStage::BridgeConnect, error));
            }
        };
        write_frame(&mut stream, &encoded, MAX_RELAY_REQUEST_BYTES)
            .map_err(|error| CgiFailure::new(CgiFailureStage::BridgeIo, error))?;
        linux_socket::shutdown_write(&stream)
            .map_err(|error| CgiFailure::new(CgiFailureStage::BridgeIo, error))?;
        let response = read_single_frame(
            &mut stream,
            MAX_RELAY_RESPONSE_BYTES,
            ErrorKind::Unavailable,
        )
        .map_err(|error| CgiFailure::new(CgiFailureStage::BridgeIo, error))?;
        decode_relay_response(&response)
            .map_err(|error| CgiFailure::new(CgiFailureStage::BridgeProtocol, error))
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum DispatchError<T> {
    Full(T),
    Disconnected(T),
}

#[cfg(target_os = "linux")]
struct BoundedWorkerPool<T> {
    sender: Option<std::sync::mpsc::SyncSender<T>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl<T: Send + 'static> BoundedWorkerPool<T> {
    fn start<F>(worker_count: usize, queue_capacity: usize, handler: F) -> BridgeResult<Self>
    where
        F: Fn(T) + Send + Sync + 'static,
    {
        if worker_count == 0 || queue_capacity == 0 {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(queue_capacity);
        let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
        let handler = std::sync::Arc::new(handler);
        let mut workers = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let worker_receiver = std::sync::Arc::clone(&receiver);
            let worker_handler = std::sync::Arc::clone(&handler);
            let spawn_result = std::thread::Builder::new()
                .name(format!("dsm-api-worker-{index}"))
                .spawn(move || {
                    loop {
                        // Receiver is not Sync. The lock is held only while
                        // selecting one bounded item, never while handling it.
                        let item = match worker_receiver.lock() {
                            Ok(receiver) => match receiver.recv() {
                                Ok(item) => item,
                                Err(_) => return,
                            },
                            Err(_) => return,
                        };
                        worker_handler(item);
                    }
                });
            match spawn_result {
                Ok(worker) => workers.push(worker),
                Err(_) => {
                    drop(sender);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(BridgeError::new(ErrorKind::Unavailable));
                }
            }
        }
        Ok(Self {
            sender: Some(sender),
            workers,
        })
    }

    fn try_dispatch(&self, item: T) -> Result<(), DispatchError<T>> {
        let Some(sender) = &self.sender else {
            return Err(DispatchError::Disconnected(item));
        };
        sender.try_send(item).map_err(|error| match error {
            std::sync::mpsc::TrySendError::Full(item) => DispatchError::Full(item),
            std::sync::mpsc::TrySendError::Disconnected(item) => DispatchError::Disconnected(item),
        })
    }
}

#[cfg(target_os = "linux")]
impl<T> Drop for BoundedWorkerPool<T> {
    fn drop(&mut self) {
        drop(self.sender.take());
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SupervisedParent {
    pid: u32,
    start: u64,
    boot: String,
}

#[cfg(target_os = "linux")]
struct SupervisedServerFiles {
    package_uid: u32,
    pid: u32,
    start: u64,
    boot: String,
}

#[cfg(target_os = "linux")]
struct ServerSocketFile {
    path: &'static Path,
    package_uid: u32,
    identity: fs::Metadata,
}

#[cfg(target_os = "linux")]
impl Drop for ServerSocketFile {
    fn drop(&mut self) {
        let _ = linux_socket::remove_own_socket(self.path, self.package_uid, &self.identity);
    }
}

#[cfg(target_os = "linux")]
impl Drop for SupervisedServerFiles {
    fn drop(&mut self) {
        let ready = format!("{}\n{}\n{}\n", self.pid, self.start, self.boot);
        let pid = format!("{}\n", self.pid);
        let _ = linux_files::remove_own_service_identity(
            Path::new(API_READY_PATH),
            self.package_uid,
            ready.as_bytes(),
        );
        let _ = linux_files::remove_own_service_identity(
            Path::new(API_BOUND_PATH),
            self.package_uid,
            ready.as_bytes(),
        );
        let _ = linux_files::remove_own_service_identity(
            Path::new(API_PID_PATH),
            self.package_uid,
            pid.as_bytes(),
        );
    }
}

#[cfg(target_os = "linux")]
fn set_parent_death_signal(signal: libc::c_int) -> BridgeResult<()> {
    // SAFETY: PR_SET_PDEATHSIG changes only the calling process and receives a
    // plain signal number. Failure is fail-closed even on an unexpected old
    // vendor kernel because otherwise the API could outlive its lifecycle
    // publisher before a PID/readiness identity exists.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, signal, 0, 0, 0) } != 0 {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn exact_parent_is_live(parent: &SupervisedParent, package_uid: u32) -> BridgeResult<()> {
    // SAFETY: getppid has no pointer arguments or preconditions.
    let actual = unsafe { libc::getppid() };
    if u32::try_from(actual).ok() != Some(parent.pid) {
        return Err(BridgeError::unsafe_runtime());
    }
    linux_files::validate_live_process_identity(parent.pid, parent.start, &parent.boot, package_uid)
}

#[cfg(target_os = "linux")]
fn server_identity() -> BridgeResult<u32> {
    if CGI_ORIGIN_VARIABLES
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return Err(BridgeError::unsafe_runtime());
    }
    let identity = linux_runtime::identity_state()?;
    let package_uid = validate_package_identity(&identity)?;
    Ok(package_uid)
}

#[cfg(target_os = "linux")]
fn run_supervised_server(parent: SupervisedParent) -> BridgeResult<()> {
    // Arm the kernel handshake before identity/layout work. The immediate
    // parent comparison closes the race where the lifecycle publisher died
    // immediately before PR_SET_PDEATHSIG was installed.
    set_parent_death_signal(libc::SIGKILL)?;
    // SAFETY: getppid has no pointer arguments or preconditions.
    if u32::try_from(unsafe { libc::getppid() }).ok() != Some(parent.pid) {
        return Err(BridgeError::unsafe_runtime());
    }
    let package_uid = server_identity()?;
    exact_parent_is_live(&parent, package_uid)?;
    let (pid, start, boot) = linux_files::current_process_identity()?;
    let pid_record = format!("{pid}\n");
    linux_files::publish_service_identity(
        Path::new(API_PID_PATH),
        package_uid,
        pid_record.as_bytes(),
    )?;
    let _files = SupervisedServerFiles {
        package_uid,
        pid,
        start,
        boot: boot.clone(),
    };
    linux_runtime::clear_environment()?;
    run_server_loop(package_uid, Some((&parent, start, &boot)))
}

#[cfg(target_os = "linux")]
fn exec_supervised_controller(parent: SupervisedParent) -> BridgeResult<()> {
    use std::os::unix::process::CommandExt;

    // SIGUSR1 is fatal by default and remains armed across exec(2). The shell
    // controller installs an explicit fatal handler immediately, then changes
    // it to ignored only immediately before publishing controller.ready. Thus
    // a lifecycle-parent death cannot strand a controller before its lock/PID
    // becomes discoverable, while the normal parent exit after readiness does
    // not terminate the long-lived controller.
    set_parent_death_signal(libc::SIGUSR1)?;
    // SAFETY: getppid has no pointer arguments or preconditions.
    if u32::try_from(unsafe { libc::getppid() }).ok() != Some(parent.pid) {
        return Err(BridgeError::unsafe_runtime());
    }
    if CGI_ORIGIN_VARIABLES
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return Err(BridgeError::unsafe_runtime());
    }
    let identity = linux_runtime::identity_state()?;
    let package_uid = validate_package_identity(&identity)?;
    exact_parent_is_live(&parent, package_uid)?;
    linux_files::validate_private_executable(Path::new(CONTROLLER_PATH), package_uid)?;
    let error = Command::new(CONTROLLER_PATH).exec();
    Err(if error.raw_os_error().is_some() {
        BridgeError::new(ErrorKind::Unavailable)
    } else {
        BridgeError::unsafe_runtime()
    })
}

#[cfg(target_os = "linux")]
fn exec_supervised_core(parent: SupervisedParent, arguments: &[OsString]) -> BridgeResult<()> {
    use std::os::unix::process::CommandExt;

    // The runner owns run.lock and remains the sole process lifecycle parent.
    // Keep SIGKILL armed across exec so SIGKILL/OOM of that shell cannot strand
    // a sync core after the authoritative lock owner disappears.
    set_parent_death_signal(libc::SIGKILL)?;
    // SAFETY: getppid has no pointer arguments or preconditions.
    if u32::try_from(unsafe { libc::getppid() }).ok() != Some(parent.pid) {
        return Err(BridgeError::unsafe_runtime());
    }
    let identity = linux_runtime::identity_state()?;
    let package_uid = validate_package_identity(&identity)?;
    exact_parent_is_live(&parent, package_uid)?;
    linux_files::validate_private_executable(Path::new(BINARY_PATH), package_uid)?;
    let mut command = Command::new(BINARY_PATH);
    command.args(arguments);
    for name in CORE_CLI_ENVIRONMENT_VARIABLES {
        command.env_remove(name);
    }
    let error = command.exec();
    Err(if error.raw_os_error().is_some() {
        BridgeError::new(ErrorKind::Unavailable)
    } else {
        BridgeError::unsafe_runtime()
    })
}

fn run_server() -> BridgeResult<()> {
    #[cfg(not(target_os = "linux"))]
    return Err(BridgeError::unsafe_runtime());

    #[cfg(target_os = "linux")]
    {
        let package_uid = server_identity()?;
        linux_runtime::clear_environment()?;
        run_server_loop(package_uid, None)
    }
}

#[cfg(target_os = "linux")]
fn run_server_loop(
    package_uid: u32,
    supervisor: Option<(&SupervisedParent, u64, &str)>,
) -> BridgeResult<()> {
    let socket_path = Path::new(API_SOCKET_PATH);
    let (listener, prepared_socket_identity) =
        linux_socket::bind_prepared(socket_path, package_uid)?;
    let socket_file = ServerSocketFile {
        path: Path::new(API_SOCKET_PATH),
        package_uid,
        identity: prepared_socket_identity,
    };
    if let Some((parent, start, boot)) = supervisor {
        // PID + socket + this private bound identity form the pre-commit
        // topology. Keep the parent-death signal armed until the lifecycle
        // parent has observed both services and atomically committed its
        // startup lease. A failed/timed-out start therefore cannot strand a
        // child that publishes during cleanup.
        let ready = format!("{}\n{}\n{}\n", std::process::id(), start, boot);
        linux_files::publish_service_identity(
            Path::new(API_BOUND_PATH),
            package_uid,
            ready.as_bytes(),
        )?;
        loop {
            exact_parent_is_live(parent, package_uid)?;
            if linux_files::service_start_is_committed(
                Path::new(CONTROLLER_START_PATH),
                package_uid,
                parent.pid,
                parent.start,
                &parent.boot,
            )? {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        set_parent_death_signal(0)?;
        exact_parent_is_live(parent, package_uid)?;
        if !linux_files::service_start_is_committed(
            Path::new(CONTROLLER_START_PATH),
            package_uid,
            parent.pid,
            parent.start,
            &parent.boot,
        )? {
            return Err(BridgeError::unsafe_runtime());
        }
        linux_files::remove_own_service_identity(
            Path::new(API_BOUND_PATH),
            package_uid,
            ready.as_bytes(),
        )?;
        linux_files::publish_service_identity(
            Path::new(API_READY_PATH),
            package_uid,
            ready.as_bytes(),
        )?;
    }
    let workers =
        BoundedWorkerPool::start(API_WORKER_COUNT, API_QUEUE_CAPACITY, move |mut stream| {
            let _ = serve_connection(&mut stream, package_uid);
        })?;
    // The pre-commit listener is deliberately inaccessible at 0000. Publish
    // the worker pool and, for supervised starts, exact readiness identity
    // first, then activate the same inode for the package-owned CGI at 0600. A
    // CGI can therefore never connect to a listener whose accept loop does not
    // exist yet.
    linux_socket::activate_prepared(socket_path, package_uid, &socket_file.identity)?;
    loop {
        match listener.accept() {
            Ok((stream, _)) => match workers.try_dispatch(stream) {
                Ok(()) | Err(DispatchError::Full(_)) => {}
                Err(DispatchError::Disconnected(_)) => {
                    return Err(BridgeError::new(ErrorKind::Unavailable));
                }
            },
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(BridgeError::new(ErrorKind::Unavailable)),
        }
    }
}

#[cfg(target_os = "linux")]
fn serve_connection(
    stream: &mut std::os::unix::net::UnixStream,
    package_uid: u32,
) -> BridgeResult<()> {
    linux_socket::configure_stream(stream)?;
    let credentials = linux_socket::peer_credentials(stream)?;
    linux_socket::validate_peer_uid(credentials.uid, package_uid)?;
    let response = match read_single_frame(stream, MAX_RELAY_REQUEST_BYTES, ErrorKind::BadRequest) {
        Ok(request) => handle_relay_request(&request, package_uid)
            .unwrap_or_else(|failure| CgiResponse::staged_error(failure.stage, failure.error)),
        Err(error) => CgiResponse::staged_error(CgiFailureStage::BridgeProtocol, error),
    };
    let encoded = encode_relay_response(&response)?;
    write_frame(stream, &encoded, MAX_RELAY_RESPONSE_BYTES)?;
    linux_socket::shutdown_write(stream)
}

#[cfg(target_os = "linux")]
fn handle_relay_request(encoded: &[u8], package_uid: u32) -> Result<CgiResponse, CgiFailure> {
    let relay = decode_relay_request(encoded)
        .map_err(|error| CgiFailure::new(CgiFailureStage::BridgeProtocol, error))?;
    let (request, body) = validate_relay_http_request(&relay)
        .map_err(|error| CgiFailure::new(CgiFailureStage::BridgeProtocol, error))?;
    let authentication = match &request {
        ValidatedHttpRequest::Get { authentication, .. }
        | ValidatedHttpRequest::Post { authentication, .. } => authentication,
    };
    // The DSM-launched CGI performs the documented authenticate.cgi check
    // exactly once while DSM's native request environment is authoritative.
    // The package daemon never re-executes that protected system helper. It
    // independently resolves the relayed username through NSS, rechecks
    // administrator membership, requires the exact package UID at SO_PEERCRED
    // in serve_connection, and recomputes the cookie/token session binding.
    let resolved_uid = linux_runtime::authorize_relayed_username(&relay.authenticated_username)
        .map_err(|error| CgiFailure::new(CgiFailureStage::Authentication, error))?;
    let session = validate_relay_authenticated_session(&relay, authentication, resolved_uid)
        .map_err(|error| CgiFailure::new(CgiFailureStage::Authentication, error))?;
    let control_paths = ControlPaths::production();
    linux_files::require_open_runtime_admission(&control_paths, package_uid)
        .map_err(|error| CgiFailure::new(CgiFailureStage::ServiceRequest, error))?;
    let policy = linux_files::load_security_policy(package_uid)
        .map_err(|error| CgiFailure::new(CgiFailureStage::ServiceRequest, error))?;
    let is_post = matches!(request, ValidatedHttpRequest::Post { .. });
    let result = if policy.require_https && !is_https_request(authentication.https.as_deref()) {
        Err(BridgeError::new(ErrorKind::Forbidden))
    } else {
        execute_authenticated_request(
            request,
            body,
            &session,
            package_uid,
            current_epoch()
                .map_err(|error| CgiFailure::new(CgiFailureStage::ServiceRequest, error))?,
            &policy,
        )
    };
    if is_post && result.is_err() {
        let _ = record_rejected_post(&session.username, session.uid);
    }
    result.map_err(|error| CgiFailure::new(CgiFailureStage::ServiceRequest, error))
}

fn is_https_request(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value.eq_ignore_ascii_case("on") || value.eq_ignore_ascii_case("true") || value == "1"
    })
}

#[cfg(target_os = "linux")]
fn execute_authenticated_request(
    request: ValidatedHttpRequest,
    body: Option<&[u8]>,
    session: &AuthenticatedSession,
    package_uid: u32,
    now: u64,
    policy: &SecurityPolicyArgs,
) -> BridgeResult<CgiResponse> {
    let control_paths = ControlPaths::production();
    match request {
        ValidatedHttpRequest::Get { action, .. } => match action {
            ReadAction::Csrf => {
                let key = linux_files::load_or_create_csrf_key(&control_paths, package_uid)?;
                let nonce = linux_files::random_nonce()?;
                let token = issue_csrf_token(
                    &key[..],
                    &session.binding,
                    now,
                    &nonce,
                    policy.csrf_lifetime_seconds,
                )?;
                let body = serde_json::to_vec(&json!({
                    "schema": "sdsync.dsm-csrf.v1",
                    "csrf_token": token,
                    "expires_at_epoch": now + policy.csrf_lifetime_seconds,
                }))
                .map_err(|_| BridgeError::internal())?;
                Ok(CgiResponse::success(body))
            }
            ReadAction::Result { job_id } => execute_result_action(
                &control_paths,
                &job_id,
                &session.binding,
                session.uid,
                package_uid,
                now,
                policy.result_retention_seconds,
            ),
            ReadAction::RequestStatus { request_id } => execute_request_status_action(
                &control_paths,
                &request_id,
                &session.username,
                &session.binding,
                session.uid,
                package_uid,
            ),
            action => execute_read_action(&action, policy),
        },
        ValidatedHttpRequest::Post { csrf_token, .. } => {
            let key = linux_files::load_or_create_csrf_key(&control_paths, package_uid)?;
            verify_csrf_token(
                &csrf_token,
                &key[..],
                &session.binding,
                now,
                policy.csrf_lifetime_seconds,
            )?;
            let body = body.ok_or_else(BridgeError::bad_request)?;
            let parsed = parse_mutation_request(body)?;
            validate_mutation_against_security_policy(&parsed.mutation, policy)?;
            let request_fingerprint = mutation_request_fingerprint(
                &key[..],
                &parsed.mutation,
                parsed.secret.as_ref().map(|secret| secret.as_slice()),
            )?;
            let audit_nonce = linux_files::random_nonce()?;
            let audit_transaction =
                audit_transaction_id(&session.binding, &parsed.request_id, now, &audit_nonce)?;
            let enqueue_outcome = linux_files::enqueue(
                &control_paths,
                EnqueueRequest {
                    package_uid,
                    client_request_id: &parsed.request_id,
                    requested_by: &session.username,
                    requested_uid: session.uid,
                    session_binding: &session.binding,
                    audit_transaction: &audit_transaction,
                    request_fingerprint: &request_fingerprint,
                    issued_at_epoch: now,
                    mutation: &parsed.mutation,
                    secret: parsed.secret.as_ref().map(|secret| secret.as_slice()),
                },
                policy.max_outstanding_jobs,
                record_audit_event,
            )?;
            let job_id = enqueue_outcome.job_id().to_owned();
            let state = enqueue_outcome.response_state();
            let replayed = enqueue_outcome.replayed();
            let durability_warning = enqueue_outcome.durability_warning();
            if enqueue_outcome.should_wake_controller() {
                wake_controller_after_enqueue();
            }
            let response = serde_json::to_vec(&json!({
                "schema": "sdsync.dsm-queued.v1",
                "ok": true,
                "request_id": parsed.request_id,
                "job_id": job_id,
                "state": state,
                "replayed": replayed,
                "durability_warning": durability_warning,
            }))
            .map_err(|_| BridgeError::internal())?;
            Ok(CgiResponse::accepted(response))
        }
    }
}

#[cfg(target_os = "linux")]
fn execute_request_status_action(
    paths: &ControlPaths<'_>,
    request_id: &str,
    authenticated_username: &str,
    session_binding: &[u8; 32],
    authenticated_uid: u32,
    package_uid: u32,
) -> BridgeResult<CgiResponse> {
    if !valid_client_request_id(request_id) {
        return Err(BridgeError::bad_request());
    }
    let status = linux_files::find_session_request(
        paths,
        package_uid,
        request_id,
        authenticated_username,
        authenticated_uid,
        session_binding,
    )?;
    match status {
        Some(SessionRequestStatus::Pending { job_id, operation }) => {
            request_status_found_response(request_id, &job_id, &operation, "pending", false)
        }
        Some(SessionRequestStatus::Complete { job_id, operation }) => {
            request_status_found_response(request_id, &job_id, &operation, "complete", true)
        }
        None => request_status_unresolved_response(request_id),
    }
}

#[cfg(target_os = "linux")]
fn request_status_found_response(
    request_id: &str,
    job_id: &str,
    operation: &str,
    state: &str,
    complete: bool,
) -> BridgeResult<CgiResponse> {
    if !valid_client_request_id(request_id)
        || !valid_server_job_id(job_id)
        || !valid_mutation_operation(operation)
        || !matches!(state, "pending" | "complete")
        || (state == "complete") != complete
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let body = serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-request-status.v1",
        "request_id": request_id,
        "job_id": job_id,
        "operation": operation,
        "state": state,
    }))
    .map_err(|_| BridgeError::internal())?;
    if complete {
        Ok(CgiResponse::success(body))
    } else {
        Ok(CgiResponse::accepted(body))
    }
}

#[cfg(target_os = "linux")]
fn request_status_unresolved_response(request_id: &str) -> BridgeResult<CgiResponse> {
    if !valid_client_request_id(request_id) {
        return Err(BridgeError::bad_request());
    }
    let body = serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-request-status.v1",
        "request_id": request_id,
        "state": "unresolved",
    }))
    .map_err(|_| BridgeError::internal())?;
    Ok(CgiResponse::accepted(body))
}

#[cfg(target_os = "linux")]
fn execute_result_action(
    paths: &ControlPaths<'_>,
    job_id: &str,
    session_binding: &[u8; 32],
    authenticated_uid: u32,
    package_uid: u32,
    now: u64,
    result_retention_seconds: u64,
) -> BridgeResult<CgiResponse> {
    if !valid_server_job_id(job_id) {
        return Err(BridgeError::bad_request());
    }
    if let Some(response) = completed_result_response(
        paths,
        job_id,
        session_binding,
        authenticated_uid,
        package_uid,
        now,
        result_retention_seconds,
    )? {
        return Ok(response);
    }

    // Read requests before processing: an atomic request -> processing rename
    // can then be observed in either location without a false missing result.
    for processing in [false, true] {
        let Some(bytes) =
            linux_files::read_optional_pending_job(paths, job_id, package_uid, processing)?
        else {
            continue;
        };
        let job = parse_job(&bytes).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        if job.request_id != job_id {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        if job.requested_uid != authenticated_uid
            || !session_binding_matches(&job.session_binding, session_binding)
        {
            return queued_pending_response(job_id);
        }
        if job.issued_at_epoch > now.saturating_add(CLOCK_SKEW_SECONDS) {
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
        if now.saturating_sub(job.issued_at_epoch) > MAX_JOB_AGE_SECONDS {
            return queued_expired_response(job_id);
        }
        return queued_pending_response(job_id);
    }
    // Close the processing -> response publish/removal race before declaring
    // the server-generated identifier gone.
    if let Some(response) = completed_result_response(
        paths,
        job_id,
        session_binding,
        authenticated_uid,
        package_uid,
        now,
        result_retention_seconds,
    )? {
        return Ok(response);
    }
    queued_expired_response(job_id)
}

#[cfg(target_os = "linux")]
fn completed_result_response(
    paths: &ControlPaths<'_>,
    job_id: &str,
    session_binding: &[u8; 32],
    authenticated_uid: u32,
    package_uid: u32,
    now: u64,
    result_retention_seconds: u64,
) -> BridgeResult<Option<CgiResponse>> {
    let Some(bytes) = linux_files::read_optional_response(paths, job_id, package_uid)? else {
        return Ok(None);
    };
    let mut response = parse_queued_response(&bytes, job_id)?;
    if response.requested_uid != authenticated_uid
        || !session_binding_matches(&response.session_binding, session_binding)
    {
        return queued_pending_response(job_id).map(Some);
    }
    if response.completed_at_epoch > now.saturating_add(CLOCK_SKEW_SECONDS) {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    if response.audit_pending {
        // Polling is also a bounded reconciliation opportunity, but an audit
        // sink outage must never hide or delete a known terminal result. The
        // controller provides autonomous retry; GET keeps serving the truthful
        // result with audit_pending until the durable record is confirmed.
        let audit_paths = paths.audit_outbox();
        if linux_files::reconcile_audit_transactions(&audit_paths, package_uid, record_audit_event)
            .is_ok()
        {
            let Some(refreshed) = linux_files::read_optional_response(paths, job_id, package_uid)?
            else {
                return queued_expired_response(job_id).map(Some);
            };
            response = parse_queued_response(&refreshed, job_id)?;
        }
    }
    if !response.audit_pending
        && now.saturating_sub(response.completed_at_epoch) > result_retention_seconds
    {
        linux_files::remove_expired_response(paths, job_id, package_uid)?;
        return queued_expired_response(job_id).map(Some);
    }
    queued_complete_response(
        job_id,
        &response.client_request_id,
        response.requested_uid,
        &response.result,
        response.audit_pending,
    )
    .map(Some)
}

fn queued_pending_response(job_id: &str) -> BridgeResult<CgiResponse> {
    let body = serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-result-status.v1",
        "job_id": job_id,
        "state": "pending",
    }))
    .map_err(|_| BridgeError::internal())?;
    Ok(CgiResponse::accepted(body))
}

fn queued_complete_response(
    job_id: &str,
    client_request_id: &str,
    actor_uid: u32,
    result: &Value,
    audit_pending: bool,
) -> BridgeResult<CgiResponse> {
    let body = serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-result-status.v1",
        "job_id": job_id,
        "client_request_id": client_request_id,
        "actor_uid": actor_uid,
        "state": "complete",
        "audit_pending": audit_pending,
        "result": result,
    }))
    .map_err(|_| BridgeError::internal())?;
    if body.len() > MAX_MANAGER_OUTPUT_BYTES {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(CgiResponse::success(body))
}

fn queued_expired_response(job_id: &str) -> BridgeResult<CgiResponse> {
    let body = serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-result-status.v1",
        "job_id": job_id,
        "state": "expired_or_missing",
        "result": {
            "schema": "sdsync.dsm-result.v1",
            "ok": false,
            "code": "expired_or_missing",
            "message": "Queued result is no longer available.",
        },
    }))
    .map_err(|_| BridgeError::internal())?;
    Ok(CgiResponse::gone(body))
}

#[cfg(target_os = "linux")]
fn execute_read_action(
    action: &ReadAction,
    policy: &SecurityPolicyArgs,
) -> BridgeResult<CgiResponse> {
    if let ReadAction::SourceDirectories { parent } = action {
        let body = source_directories_document(Path::new("/"), parent)?;
        return Ok(CgiResponse::success(body));
    }
    if let ReadAction::SourcePath { path } = action {
        let body = source_path_document(Path::new("/"), path)?;
        return Ok(CgiResponse::success(body));
    }
    let arguments = read_manager_arguments(action)?;
    let output = run_read_manager(&arguments)?;
    if !output.status_success {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let body = parse_and_sanitize_manager_json(&output.stdout, action, None, Some(policy))?;
    Ok(CgiResponse::success(body))
}

fn run_consumer(request: &Path, response: &Path) -> BridgeResult<()> {
    #[cfg(not(target_os = "linux"))]
    return Err(BridgeError::unsafe_runtime());

    #[cfg(target_os = "linux")]
    {
        if CGI_ORIGIN_VARIABLES
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return Err(BridgeError::unsafe_runtime());
        }
        let identity = linux_runtime::identity_state()?;
        let package_uid = validate_package_identity(&identity)?;
        let request_id = validate_consumer_paths(request, response)?;
        linux_runtime::clear_environment()?;
        let control_paths = ControlPaths::production();

        let response_result = (|| {
            let job = match linux_files::read_job(&control_paths, request, package_uid)
                .and_then(|bytes| parse_job(&bytes))
            {
                Ok(job) => job,
                Err(error) => {
                    // No untrusted field is attributable until the strict job
                    // document parses. Preserve a durable, bounded controller
                    // rejection record before the controller quarantines it.
                    record_rejected_operation("package-controller", package_uid, "controller")?;
                    return Err(error);
                }
            };
            let audit_transaction = job.audit_transaction.clone();
            let mut termination_requested = None;
            let execution_result = (|| {
                // Unsupported subreaper mode on DSM's older Linux 3.2 kernels
                // is safe here: every manager is still isolated in a process
                // group, and cancellation waits until that entire group has
                // disappeared. Subreaping only improves local waitpid cleanup.
                let _subreaper_enabled = enable_consumer_subreaper()?;
                let flag = install_consumer_termination_handler()?;
                termination_requested = Some(flag);
                let termination_requested = termination_requested
                    .as_ref()
                    .ok_or_else(BridgeError::internal)?;
                validate_job_freshness(job.issued_at_epoch, current_epoch()?)?;
                if job.request_id != request_id {
                    return Err(BridgeError::bad_request());
                }
                linux_files::claim_queued_audit_transaction(
                    &control_paths.audit_outbox(),
                    package_uid,
                    &audit_transaction,
                    &request_id,
                )?;
                consume_job_inner(&control_paths, &job, package_uid, termination_requested)
            })();
            let result = terminalize_consume_result(execution_result, |state| {
                let phase = if state == "succeeded" {
                    AuditOutboxPhase::Succeeded
                } else {
                    AuditOutboxPhase::Failed
                };
                linux_files::audit_transaction_complete(
                    &control_paths.audit_outbox(),
                    package_uid,
                    &audit_transaction,
                    phase,
                    record_audit_event,
                )
            });
            canonical_queued_response_bytes(
                &job,
                current_epoch()?,
                &result.value,
                result.audit_pending,
            )
        })();
        linux_files::remove_claimed_secret(&control_paths, &request_id);
        let response_bytes = response_result?;
        linux_files::write_response(
            &control_paths,
            response,
            &request_id,
            package_uid,
            &response_bytes,
        )
    }
}

fn classify_queued_job(request_id: &str) -> BridgeResult<QueuedJobClass> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = request_id;
        Err(BridgeError::unsafe_runtime())
    }

    #[cfg(target_os = "linux")]
    {
        validate_queued_job_classifier_request(
            request_id,
            CGI_ORIGIN_VARIABLES
                .iter()
                .any(|name| std::env::var_os(name).is_some()),
        )?;
        let identity = linux_runtime::identity_state()?;
        let package_uid = validate_package_identity(&identity)?;
        linux_runtime::clear_environment()?;
        let control_paths = ControlPaths::production();
        classify_queued_job_from_paths(&control_paths, request_id, package_uid)
    }
}

#[cfg(target_os = "linux")]
fn validate_queued_job_classifier_request(
    request_id: &str,
    cgi_origin_present: bool,
) -> BridgeResult<()> {
    if !valid_server_job_id(request_id) {
        return Err(BridgeError::bad_request());
    }
    if cgi_origin_present {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn classify_queued_job_from_paths(
    control_paths: &ControlPaths<'_>,
    request_id: &str,
    package_uid: u32,
) -> BridgeResult<QueuedJobClass> {
    validate_queued_job_classifier_request(request_id, false)?;
    let bytes =
        linux_files::read_optional_pending_job(control_paths, request_id, package_uid, false)?
            .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    let job = parse_job(&bytes).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    if job.request_id != request_id {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(queued_job_class(&job))
}

fn reject_claimed_job(request: &Path, response: &Path) -> BridgeResult<()> {
    #[cfg(not(target_os = "linux"))]
    return Err(BridgeError::unsafe_runtime());

    #[cfg(target_os = "linux")]
    {
        if CGI_ORIGIN_VARIABLES
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return Err(BridgeError::unsafe_runtime());
        }
        let identity = linux_runtime::identity_state()?;
        let package_uid = validate_package_identity(&identity)?;
        let request_id = validate_consumer_paths(request, response)?;
        linux_runtime::clear_environment()?;
        let control_paths = ControlPaths::production();
        let job = linux_files::read_job(&control_paths, request, package_uid)
            .and_then(|bytes| parse_job(&bytes))?;
        if job.request_id != request_id {
            return Err(BridgeError::bad_request());
        }
        let audit_transaction = job.audit_transaction.clone();
        let _ = linux_files::claim_queued_audit_transaction(
            &control_paths.audit_outbox(),
            package_uid,
            &audit_transaction,
            &request_id,
        );
        let result =
            terminalize_consume_result(Err(BridgeError::new(ErrorKind::Unavailable)), |state| {
                debug_assert_eq!(state, "failed");
                linux_files::audit_transaction_complete(
                    &control_paths.audit_outbox(),
                    package_uid,
                    &audit_transaction,
                    AuditOutboxPhase::Failed,
                    record_audit_event,
                )
            });
        let response_bytes = canonical_queued_response_bytes(
            &job,
            current_epoch()?,
            &result.value,
            result.audit_pending,
        )?;
        linux_files::remove_claimed_secret(&control_paths, &request_id);
        linux_files::write_response(
            &control_paths,
            response,
            &request_id,
            package_uid,
            &response_bytes,
        )
    }
}

#[cfg(target_os = "linux")]
fn classify_consumer_subreaper_result(result: i32, errno: Option<i32>) -> BridgeResult<bool> {
    if result == 0 {
        return Ok(true);
    }
    match errno {
        // DSM 7.1 still supports families whose vendor kernel predates
        // PR_SET_CHILD_SUBREAPER. EINVAL is also the documented response when
        // this prctl operation is unknown to an older kernel.
        Some(libc::ENOSYS) | Some(libc::EINVAL) => Ok(false),
        _ => Err(BridgeError::new(ErrorKind::Unavailable)),
    }
}

#[cfg(target_os = "linux")]
fn enable_consumer_subreaper() -> BridgeResult<bool> {
    // SAFETY: PR_SET_CHILD_SUBREAPER changes only this dedicated consumer
    // process. It lets shutdown reap manager grandchildren before reporting
    // the queued process group terminal.
    let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    let errno = (result != 0)
        .then(|| std::io::Error::last_os_error().raw_os_error())
        .flatten();
    classify_consumer_subreaper_result(result, errno)
}

#[cfg(target_os = "linux")]
fn install_consumer_termination_handler() -> BridgeResult<Arc<AtomicBool>> {
    let termination_requested = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&termination_requested);
    // A controller-spawned background process may inherit ignored terminal
    // signals from its shell. This dedicated one-job process must own these
    // dispositions so TERM/HUP/INT always become cooperative cancellation.
    ctrlc::set_handler(move || {
        handler_flag.store(true, AtomicOrdering::Release);
    })
    .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    Ok(termination_requested)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteConnectionFailure {
    Connect,
    Authentication(Option<i64>),
    Permission,
    Listing,
    Logout,
    PermissionAndLogout,
    ListingAndLogout,
    OperationAndLogout,
}

#[cfg(target_os = "linux")]
fn connection_failure_result(failure: RemoteConnectionFailure) -> Value {
    let (code, message) = match failure {
        RemoteConnectionFailure::Connect => (
            "file_station_connection_failed",
            "The File Station endpoint could not be reached with the current network and TLS settings.",
        ),
        RemoteConnectionFailure::Authentication(Some(403 | 406)) => (
            "file_station_totp_required",
            "DSM requires TOTP authentication. Provide a valid Base32 seed or otpauth URI and test again.",
        ),
        RemoteConnectionFailure::Authentication(Some(404)) => (
            "file_station_totp_rejected",
            "DSM rejected the generated TOTP code. Check the seed and NAS clock, then test again.",
        ),
        RemoteConnectionFailure::Authentication(_) => (
            "file_station_authentication_failed",
            "DSM rejected the username, password, account policy, or sign-in method.",
        ),
        RemoteConnectionFailure::Permission => (
            "file_station_listing_denied",
            "The authenticated DSM account cannot list that File Station location.",
        ),
        RemoteConnectionFailure::Listing => (
            "file_station_listing_failed",
            "File Station could not return a valid bounded directory listing.",
        ),
        RemoteConnectionFailure::Logout => (
            "file_station_logout_failed",
            "The temporary File Station session could not be closed safely.",
        ),
        RemoteConnectionFailure::PermissionAndLogout => (
            "file_station_denied_logout_failed",
            "The authenticated DSM account could not list that File Station location, and the temporary session could not be closed safely.",
        ),
        RemoteConnectionFailure::ListingAndLogout => (
            "file_station_listing_logout_failed",
            "File Station could not return a valid bounded directory listing, and the temporary session could not be closed safely.",
        ),
        RemoteConnectionFailure::OperationAndLogout => (
            "file_station_operation_logout_failed",
            "The File Station operation failed, and the temporary session could not be closed safely.",
        ),
    };
    json!({
        "schema": "sdsync.dsm-result.v1",
        "ok": false,
        "code": code,
        "message": message,
    })
}

#[cfg(target_os = "linux")]
fn connection_secret_required(value: &ConnectionJobArgs) -> bool {
    value.password_source == CredentialSource::Provided
        || value.totp_source == CredentialSource::Provided
}

#[cfg(target_os = "linux")]
fn resolve_connection_secrets(
    paths: &ControlPaths<'_>,
    request_id: &str,
    value: &ConnectionJobArgs,
    package_uid: u32,
) -> BridgeResult<ResolvedConnectionSecrets> {
    let envelope = linux_files::read_claimed_connection_secret(
        paths,
        request_id,
        package_uid,
        connection_secret_required(value),
    )?;
    let (provided_password, provided_totp) = decode_connection_secret_envelope(envelope)?;
    let password = match value.password_source {
        CredentialSource::Provided => provided_password.ok_or_else(BridgeError::bad_request)?,
        CredentialSource::Stored => {
            if provided_password.is_some() {
                return Err(BridgeError::bad_request());
            }
            linux_files::read_profile_secret(
                value
                    .profile
                    .as_deref()
                    .ok_or_else(BridgeError::bad_request)?,
                "password",
                package_uid,
            )?
            .ok_or_else(BridgeError::bad_request)?
        }
        CredentialSource::None => return Err(BridgeError::bad_request()),
    };
    let totp = match value.totp_source {
        CredentialSource::Provided => Some(provided_totp.ok_or_else(BridgeError::bad_request)?),
        CredentialSource::Stored => {
            if provided_totp.is_some() {
                return Err(BridgeError::bad_request());
            }
            linux_files::read_profile_secret(
                value
                    .profile
                    .as_deref()
                    .ok_or_else(BridgeError::bad_request)?,
                "totp",
                package_uid,
            )?
        }
        CredentialSource::None => {
            if provided_totp.is_some() {
                return Err(BridgeError::bad_request());
            }
            None
        }
    };
    Ok((password, totp))
}

#[cfg(target_os = "linux")]
fn connection_fingerprint(
    key: &[u8],
    value: &ConnectionJobArgs,
    password: &[u8],
    totp: Option<&[u8]>,
) -> BridgeResult<String> {
    if key.len() != 32 || password.is_empty() {
        return Err(BridgeError::unsafe_runtime());
    }
    let arguments = serde_json::to_vec(value).map_err(|_| BridgeError::internal())?;
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| BridgeError::internal())?;
    mac.update(b"sdsync-file-station-connection-v1\0");
    mac.update(&(arguments.len() as u64).to_be_bytes());
    mac.update(&arguments);
    mac.update(&(password.len() as u64).to_be_bytes());
    mac.update(password);
    let totp = totp.unwrap_or_default();
    mac.update(&(totp.len() as u64).to_be_bytes());
    mac.update(totp);
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

fn connection_proof_message(expires: u64, session_binding: &[u8; 32], fingerprint: &str) -> String {
    format!(
        "sdsync-file-station-proof-v1\n{expires}\n{}\n{fingerprint}",
        hex_encode(session_binding)
    )
}

fn issue_connection_proof(
    key: &[u8],
    session_binding: &[u8; 32],
    fingerprint: &str,
    now: u64,
) -> BridgeResult<(String, u64)> {
    if key.len() != 32 || hex_decode_exact::<32>(fingerprint).is_none() {
        return Err(BridgeError::unsafe_runtime());
    }
    let expires = now
        .checked_add(CONNECTION_PROOF_LIFETIME_SECONDS)
        .ok_or_else(BridgeError::internal)?;
    let message = connection_proof_message(expires, session_binding, fingerprint);
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| BridgeError::internal())?;
    mac.update(message.as_bytes());
    let signature = hex_encode(&mac.finalize().into_bytes());
    Ok((format!("v1.{expires}.{fingerprint}.{signature}"), expires))
}

fn verify_connection_proof(
    proof: &str,
    key: &[u8],
    session_binding: &[u8; 32],
    fingerprint: &str,
    now: u64,
) -> BridgeResult<()> {
    if !valid_connection_proof_syntax(proof) || key.len() != 32 {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let components: Vec<_> = proof.split('.').collect();
    let expires =
        parse_canonical_u64(components[1]).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    if expires <= now
        || expires > now.saturating_add(CONNECTION_PROOF_LIFETIME_SECONDS + CLOCK_SKEW_SECONDS)
        || !constant_time_equal(components[2].as_bytes(), fingerprint.as_bytes())
    {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let supplied = hex_decode_exact::<32>(components[3])
        .ok_or_else(|| BridgeError::new(ErrorKind::Forbidden))?;
    let message = connection_proof_message(expires, session_binding, fingerprint);
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    mac.update(message.as_bytes());
    if !constant_time_equal(&mac.finalize().into_bytes(), &supplied) {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn authenticate_file_station(
    value: &ConnectionJobArgs,
    password: &[u8],
    totp: Option<&[u8]>,
    total_timeout: Duration,
) -> Result<ApiClient, RemoteConnectionFailure> {
    let password =
        std::str::from_utf8(password).map_err(|_| RemoteConnectionFailure::Authentication(None))?;
    let parsed_totp = totp
        .map(|secret| {
            let secret = std::str::from_utf8(secret)
                .map_err(|_| RemoteConnectionFailure::Authentication(None))?;
            parse_totp_secret(secret).map_err(|_| RemoteConnectionFailure::Authentication(None))
        })
        .transpose()?;
    let options = ClientOptions {
        base_url: value.url.clone(),
        allow_http: value.allow_http,
        accept_invalid_certs: value.danger_accept_invalid_certs,
        ca_certificate: value.ca_certificate.as_ref().map(PathBuf::from),
        connect_timeout: Duration::from_secs(value.connect_timeout_seconds.into()),
        request_timeout: Duration::from_secs(value.timeout_seconds.into()),
        // One absolute probe deadline is shared by discovery, login (including
        // a challenged TOTP login), and any subsequent directory listing.
        // Retrying inside that interactive UI probe could otherwise turn one
        // request into minutes of waiting. Normal sync clients are constructed
        // elsewhere and retain their configured retry policy.
        retries: 0,
    };
    let mut client = ApiClient::connect_for_browsing_bounded(&options, total_timeout)
        .map_err(|_| RemoteConnectionFailure::Connect)?;

    match client.login(&value.username, password, None) {
        Ok(()) => Ok(client),
        Err(error) if matches!(error.api_code(), Some(403 | 406)) => {
            let Some(secret) = parsed_totp else {
                return Err(RemoteConnectionFailure::Authentication(error.api_code()));
            };
            let code = generate_totp(&secret)
                .map_err(|_| RemoteConnectionFailure::Authentication(error.api_code()))?;
            client
                .login(&value.username, password, Some(&code))
                .map_err(|error| RemoteConnectionFailure::Authentication(error.api_code()))?;
            Ok(client)
        }
        Err(error) => Err(RemoteConnectionFailure::Authentication(error.api_code())),
    }
}

#[cfg(target_os = "linux")]
fn logout_result_with<T, F>(
    result: Result<T, RemoteConnectionFailure>,
    logout: F,
) -> Result<T, RemoteConnectionFailure>
where
    F: FnOnce() -> Result<(), RemoteConnectionFailure>,
{
    let logout = logout();
    match (result, logout) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(failure)) => Err(failure),
        (Err(RemoteConnectionFailure::Permission), Err(_)) => {
            Err(RemoteConnectionFailure::PermissionAndLogout)
        }
        (Err(RemoteConnectionFailure::Listing), Err(_)) => {
            Err(RemoteConnectionFailure::ListingAndLogout)
        }
        (Err(_), Err(_)) => Err(RemoteConnectionFailure::OperationAndLogout),
        (Err(failure), Ok(())) => Err(failure),
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InteractiveConnectionBudget {
    probe: Duration,
    logout: Duration,
}

#[cfg(target_os = "linux")]
fn interactive_connection_budget(mutation: &Mutation) -> Option<InteractiveConnectionBudget> {
    match mutation {
        Mutation::TestProfileAuth(_) => Some(InteractiveConnectionBudget {
            probe: INTERACTIVE_AUTH_TEST_PROBE_TIMEOUT,
            logout: INTERACTIVE_AUTH_TEST_LOGOUT_TIMEOUT,
        }),
        Mutation::BrowseRemote(_) => Some(InteractiveConnectionBudget {
            probe: INTERACTIVE_REMOTE_BROWSE_PROBE_TIMEOUT,
            logout: INTERACTIVE_REMOTE_BROWSE_LOGOUT_TIMEOUT,
        }),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn authentication_test_result_after_logout<F>(
    logout: Result<(), RemoteConnectionFailure>,
    issue_proof: F,
) -> BridgeResult<Value>
where
    F: FnOnce() -> BridgeResult<(String, u64)>,
{
    match logout {
        Ok(()) => {
            let (proof, expires) = issue_proof()?;
            Ok(json!({
                "schema": "sdsync.dsm-result.v1",
                "ok": true,
                "message": "Authentication succeeded and the temporary File Station session was closed.",
                "connection_proof": proof,
                "connection_proof_expires_at_epoch": expires,
            }))
        }
        Err(failure) => Ok(connection_failure_result(failure)),
    }
}

#[cfg(target_os = "linux")]
fn execute_connection_mutation(
    paths: &ControlPaths<'_>,
    job: &ParsedJob,
    package_uid: u32,
) -> BridgeResult<Value> {
    let budget = interactive_connection_budget(&job.mutation).ok_or_else(BridgeError::internal)?;
    let connection = match &job.mutation {
        Mutation::TestProfileAuth(value) => value,
        Mutation::BrowseRemote(value) => &value.connection,
        _ => return Err(BridgeError::internal()),
    };
    let (password, totp) =
        resolve_connection_secrets(paths, &job.request_id, connection, package_uid)?;
    let key = linux_files::load_or_create_csrf_key(paths, package_uid)?;
    let fingerprint = connection_fingerprint(
        &key[..],
        connection,
        &password[..],
        totp.as_ref().map(|value| value.as_slice()),
    )?;
    let now = current_epoch()?;

    if let Mutation::BrowseRemote(arguments) = &job.mutation {
        verify_connection_proof(
            &arguments.connection_proof,
            &key[..],
            &job.session_binding,
            &fingerprint,
            now,
        )?;
    }

    let mut client = match authenticate_file_station(
        connection,
        &password,
        totp.as_ref().map(|value| value.as_slice()),
        budget.probe,
    ) {
        Ok(client) => client,
        Err(failure) => return Ok(connection_failure_result(failure)),
    };

    match &job.mutation {
        Mutation::TestProfileAuth(_) => {
            let logout = logout_result_with(Ok(()), || {
                client
                    .logout_bounded(budget.logout)
                    .map_err(|_| RemoteConnectionFailure::Logout)
            });
            authentication_test_result_after_logout(logout, || {
                // Start the proof lifetime only after authentication and the
                // mandatory logout have both completed. A slow target must not
                // consume the chooser window before it is returned to the UI.
                let proof_issued_at = current_epoch()?;
                issue_connection_proof(
                    &key[..],
                    &job.session_binding,
                    &fingerprint,
                    proof_issued_at,
                )
            })
        }
        Mutation::BrowseRemote(arguments) => {
            let listing =
                client
                    .browse_directories(&arguments.parent, 500)
                    .map_err(|error: SyncError| {
                        if matches!(error.api_code(), Some(105 | 407)) {
                            RemoteConnectionFailure::Permission
                        } else {
                            RemoteConnectionFailure::Listing
                        }
                    });
            let listing = logout_result_with(listing, || {
                client
                    .logout_bounded(budget.logout)
                    .map_err(|_| RemoteConnectionFailure::Logout)
            });
            match listing {
                Ok(page) => Ok(json!({
                    "schema": "sdsync.dsm-result.v1",
                    "ok": true,
                    "message": "File Station directories loaded.",
                    "directory_schema": "sdsync.dsm-remote-directories.v1",
                    "current": page.parent,
                    "directories": page.directories,
                    "truncated": page.truncated,
                })),
                Err(failure) => Ok(connection_failure_result(failure)),
            }
        }
        _ => Err(BridgeError::internal()),
    }
}

#[cfg(target_os = "linux")]
fn consume_job_inner(
    paths: &ControlPaths<'_>,
    job: &ParsedJob,
    package_uid: u32,
    termination_requested: &AtomicBool,
) -> BridgeResult<Value> {
    // Policy is deliberately reloaded immediately before execution. A queued
    // destructive/TLS/remote-log/operational action is revoked if an
    // administrator tightened policy after it was accepted.
    let current_policy = linux_files::load_security_policy(package_uid)?;
    validate_mutation_against_security_policy(&job.mutation, &current_policy)?;
    if matches!(
        &job.mutation,
        Mutation::TestProfileAuth(_) | Mutation::BrowseRemote(_)
    ) {
        return execute_connection_mutation(paths, job, package_uid);
    }
    let secret = match &job.mutation {
        Mutation::SetSecret(arguments) if arguments.mode == SecretMode::Replace => {
            linux_files::read_claimed_secret(paths, &job.request_id, package_uid, true)?
        }
        Mutation::SetSecret(_) => {
            linux_files::reject_unexpected_secret(paths, &job.request_id, package_uid)?;
            None
        }
        _ => {
            linux_files::reject_unexpected_secret(paths, &job.request_id, package_uid)?;
            None
        }
    };
    let arguments = mutation_manager_arguments(&job.mutation);
    let output = run_queued_mutation_manager(
        &arguments,
        secret.as_ref().map(|value| value.as_slice()),
        termination_requested,
    )?;
    let result = parse_manager_result(
        &output.stdout,
        secret.as_ref().map(|secret| secret.as_slice()),
    )?;
    if matches!(&job.mutation, Mutation::SetSecret(_)) {
        validate_set_secret_manager_result(&result)?;
    }
    if result.get("ok").and_then(Value::as_bool) != Some(output.status_success) {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    Ok(result)
}

fn current_epoch() -> BridgeResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))
}

fn write_cgi_response(response: &CgiResponse) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        410 => "Gone",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut stdout = io::stdout().lock();
    write!(
        stdout,
        "Status: {} {}\r\nContent-Type: application/json; charset=utf-8\r\nCache-Control: no-store\r\nPragma: no-cache\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'\r\nContent-Length: {}\r\n\r\n",
        response.status,
        reason,
        response.body.len()
    )?;
    stdout.write_all(&response.body)?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[cfg(target_os = "linux")]
    use std::os::linux::fs::MetadataExt;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[cfg(target_os = "linux")]
    use std::sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    };

    const REQUEST_ID: &str = "0123456789abcdef0123456789abcdef";
    const JOB_ID: &str = "00060f5e12345678fedcba98765432100123456789abcdef";

    fn environment(method: &str, query: &str) -> CgiEnvironment {
        CgiEnvironment {
            method: method.to_owned(),
            content_length: None,
            content_type: None,
            query: Zeroizing::new(query.to_owned()),
            cookie: Zeroizing::new("id=authenticated-session".to_owned()),
            request_marker: Some("1".to_owned()),
            synology_token_header: None,
            csrf_header: None,
            remote_address: Some("192.0.2.8".to_owned()),
            server_address: Some("192.0.2.2".to_owned()),
            server_name: Some("nas.example.invalid".to_owned()),
            server_port: Some("5001".to_owned()),
            https: Some("on".to_owned()),
            transfer_encoding: None,
            native_authentication_context: NativeAuthenticationContext {
                gateway_interface: Some("CGI/1.1".to_owned()),
                http_host: Some("nas.example.invalid:5001".to_owned()),
                remote_port: Some("54321".to_owned()),
                request_scheme: Some("https".to_owned()),
                server_protocol: Some("HTTP/2.0".to_owned()),
                script_name: Some("/webman/3rdparty/synology-drive-sync/api.cgi".to_owned()),
                script_filename: Some(
                    "/var/packages/synology-drive-sync/target/ui/api.cgi".to_owned(),
                ),
                document_root: Some("/usr/syno/synoman".to_owned()),
                scgi: Some("1".to_owned()),
                socket: Some("/run/synoscgi.sock".to_owned()),
            },
        }
    }

    fn authenticated_session() -> AuthenticatedSession {
        AuthenticatedSession {
            username: "admin".to_owned(),
            uid: 1000,
            binding: [7_u8; 32],
        }
    }

    fn bound_authenticated_session(
        environment: &CgiEnvironment,
        authenticated_uid: u32,
    ) -> AuthenticatedSession {
        let request = validate_http_request(environment.clone()).unwrap();
        let authentication = match &request {
            ValidatedHttpRequest::Get { authentication, .. }
            | ValidatedHttpRequest::Post { authentication, .. } => authentication,
        };
        AuthenticatedSession {
            username: "admin".to_owned(),
            uid: authenticated_uid,
            binding: session_binding(
                "admin",
                authenticated_uid,
                &authentication.cookie,
                authentication
                    .synology_token
                    .as_ref()
                    .map(|value| value.as_str()),
            )
            .unwrap(),
        }
    }

    fn post_environment(body_length: usize) -> CgiEnvironment {
        let mut environment = environment("POST", "");
        environment.content_length = Some(body_length.to_string());
        environment.content_type = Some("application/json; charset=utf-8".to_owned());
        environment.synology_token_header = Some(Zeroizing::new("dsm-token".to_owned()));
        environment.csrf_header = Some(Zeroizing::new("csrf-token".to_owned()));
        environment
    }

    fn request(operation: &str, arguments: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema": "sdsync.dsm-request.v1",
            "request_id": REQUEST_ID,
            "operation": operation,
            "arguments": arguments,
        }))
        .unwrap()
    }

    fn configure_arguments() -> Value {
        json!({
            "name": "nightly",
            "source": "/volume1/source",
            "url": "https://nas.example.invalid",
            "username": "backup-user",
            "remote": "/home/Drive/Backup",
            "compare": "content",
            "jobs": 2,
            "delete": false,
            "max_delete": 100,
            "allow_http": false,
            "allow_empty_source": false,
            "excludes": ["@eaDir/", "**/@eaDir/"],
            "retries": 2,
            "timeout_seconds": 7200,
            "connect_timeout_seconds": 15,
            "max_rate_bytes_per_second": null,
            "ca_certificate": null,
            "danger_accept_invalid_certs": false,
            "verbosity": 0,
            "quiet": false,
            "log_level": "info",
            "log_format": "human",
            "progress": "auto",
            "output": "json",
            "remote_log_url": null,
            "remote_log_mode": "best-effort",
            "make_default": true
        })
    }

    fn routine_arguments() -> Value {
        json!({
            "profile": "nightly",
            "enabled": true,
            "action": "sync",
            "mode": "daily",
            "weekdays": [1, 2, 3, 4, 5],
            "time_window_start": "01:30",
            "time_window_end": "04:00",
            "retry_count": 5,
            "retry_backoff_seconds": 60,
            "retry_exponential": true,
            "allow_delete": false,
            "max_total_delete": 100,
            "depends_on": ["upstream"]
        })
    }

    fn security_policy_arguments() -> Value {
        serde_json::to_value(SecurityPolicyArgs::default()).unwrap()
    }

    fn security_policy_document() -> String {
        concat!(
            "policy_version=1\n",
            "require_https=false\n",
            "allow_interface_changes=true\n",
            "allow_profile_changes=true\n",
            "allow_secret_changes=true\n",
            "allow_routine_changes=true\n",
            "allow_notification_changes=true\n",
            "allow_operational_actions=true\n",
            "allow_http_targets=true\n",
            "allow_invalid_tls=true\n",
            "allow_destructive_sync=true\n",
            "allow_doctor_write_test=true\n",
            "allow_remote_logging=true\n",
            "allow_empty_source=true\n",
            "csrf_lifetime_seconds=300\n",
            "result_retention_seconds=3600\n",
            "max_outstanding_jobs=256\n",
            "audit_log_level=info\n",
            "bridge_log_level=info\n",
            "authentication_log_level=warn\n",
            "security_log_level=warn\n",
            "configuration_log_level=info\n",
            "secrets_log_level=info\n",
            "routines_log_level=info\n",
            "operations_log_level=info\n",
            "notifications_log_level=warn\n",
            "sync_log_level=info\n",
            "controller_log_level=info\n",
            "scheduler_log_level=info\n",
        )
        .to_owned()
    }

    fn set_category_level(policy: &mut SecurityPolicyArgs, category: &str, level: PolicyLogLevel) {
        match category {
            "audit" => policy.audit_log_level = level,
            "bridge" => policy.bridge_log_level = level,
            "authentication" => policy.authentication_log_level = level,
            "security" => policy.security_log_level = level,
            "configuration" => policy.configuration_log_level = level,
            "secrets" => policy.secrets_log_level = level,
            "routines" => policy.routines_log_level = level,
            "operations" => policy.operations_log_level = level,
            "notifications" => policy.notifications_log_level = level,
            "sync" => policy.sync_log_level = level,
            "controller" => policy.controller_log_level = level,
            "scheduler" => policy.scheduler_log_level = level,
            _ => panic!("unknown test category: {category}"),
        }
    }

    fn argument_strings(mutation: &Mutation) -> Vec<String> {
        mutation_manager_arguments(mutation)
            .into_iter()
            .map(|value| value.into_string().unwrap())
            .collect()
    }

    #[cfg(target_os = "linux")]
    static NEXT_CONTROL_FIXTURE: AtomicU64 = AtomicU64::new(0);
    #[cfg(target_os = "linux")]
    static CONTROL_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(target_os = "linux")]
    fn user_service_inputs(port: u16, token: Option<&str>) -> AuthenticationInputs {
        AuthenticationInputs {
            cookie: Zeroizing::new("id=loopback-session-secret".to_owned()),
            synology_token: token.map(|value| Zeroizing::new(value.to_owned())),
            remote_address: Some("192.0.2.8".to_owned()),
            server_address: Some("192.0.2.2".to_owned()),
            server_name: Some("attacker-controlled.invalid".to_owned()),
            server_port: Some(port.to_string()),
            https: Some("off".to_owned()),
            native_context: NativeAuthenticationContext::default(),
        }
    }

    #[cfg(target_os = "linux")]
    fn spawn_user_service_response(
        response: Vec<u8>,
        delay: Duration,
    ) -> (u16, std::thread::JoinHandle<String>) {
        use std::net::TcpListener;

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = std::thread::spawn(move || {
            let (mut stream, peer) = listener.accept().unwrap();
            assert!(peer.ip().is_loopback());
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
                assert!(request.len() <= 16 * 1024);
            }
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let _ = stream.write_all(&response);
            String::from_utf8(request).unwrap()
        });
        (port, worker)
    }

    #[cfg(target_os = "linux")]
    struct TestControlFixture {
        _process_global_guard: MutexGuard<'static, ()>,
        root: PathBuf,
        requests: PathBuf,
        processing: PathBuf,
        responses: PathBuf,
        staging: PathBuf,
        csrf_key: PathBuf,
        enqueue_lock: PathBuf,
        enqueue_sequence: PathBuf,
        audit_outbox_directory: PathBuf,
        audit_outbox_lock: PathBuf,
        package_transition: PathBuf,
        service_closed: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl TestControlFixture {
        fn new(label: &str) -> Self {
            // bind_prepared deliberately changes the process-global umask while
            // atomically creating an inaccessible socket. Serialize fixtures
            // that also create mode-sensitive private files so parallel tests
            // cannot observe that production startup-only transition.
            let process_global_guard = CONTROL_FIXTURE_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let sequence = NEXT_CONTROL_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "sdsync-dsm-api-{label}-{}-{sequence}",
                std::process::id()
            ));
            let requests = root.join("requests");
            let processing = root.join("processing");
            let responses = root.join("responses");
            let staging = root.join("staging");
            let audit_outbox_directory = root.join("audit-outbox");
            for directory in [
                &root,
                &requests,
                &processing,
                &responses,
                &staging,
                &audit_outbox_directory,
            ] {
                fs::create_dir(directory).unwrap();
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self {
                _process_global_guard: process_global_guard,
                csrf_key: root.join("csrf.key"),
                enqueue_lock: root.join("enqueue.lock"),
                enqueue_sequence: root.join("enqueue.sequence"),
                audit_outbox_directory,
                audit_outbox_lock: root.join("audit-outbox.flock"),
                package_transition: root.join("package.transition"),
                service_closed: root.join("service.closed"),
                root,
                requests,
                processing,
                responses,
                staging,
            }
        }

        fn paths(&self) -> ControlPaths<'_> {
            ControlPaths {
                root: &self.root,
                requests: &self.requests,
                processing: &self.processing,
                responses: &self.responses,
                staging: &self.staging,
                csrf_key: &self.csrf_key,
                enqueue_lock: &self.enqueue_lock,
                enqueue_sequence: &self.enqueue_sequence,
                audit_outbox_directory: &self.audit_outbox_directory,
                audit_outbox_lock: &self.audit_outbox_lock,
                package_transition: &self.package_transition,
                service_closed: &self.service_closed,
            }
        }

        fn write_private(&self, path: &Path, bytes: &[u8]) {
            fs::write(path, bytes).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        fn package_uid() -> u32 {
            // SAFETY: geteuid has no pointer arguments or preconditions.
            unsafe { libc::geteuid() }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TestControlFixture {
        fn drop(&mut self) {
            let safe_name = self
                .root
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("sdsync-dsm-api-"));
            if safe_name {
                let _ = fs::remove_dir_all(&self.root);
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn production_control_paths_are_fixed_to_the_dsm_contract() {
        let paths = ControlPaths::production();
        assert_eq!(paths.root, Path::new(CONTROL_ROOT));
        assert_eq!(paths.requests, Path::new(REQUESTS_DIR));
        assert_eq!(paths.processing, Path::new(PROCESSING_DIR));
        assert_eq!(paths.responses, Path::new(RESPONSES_DIR));
        assert_eq!(paths.staging, Path::new(STAGING_DIR));
        assert_eq!(paths.csrf_key, Path::new(CSRF_KEY_PATH));
        assert_eq!(paths.enqueue_lock, Path::new(ENQUEUE_LOCK_PATH));
        assert_eq!(paths.enqueue_sequence, Path::new(ENQUEUE_SEQUENCE_PATH));
        assert_eq!(paths.audit_outbox_directory, Path::new(AUDIT_OUTBOX_DIR));
        assert_eq!(paths.audit_outbox_lock, Path::new(AUDIT_OUTBOX_LOCK_PATH));
        assert_eq!(
            Path::new(API_SOCKET_PATH),
            Path::new(PACKAGE_VAR).join("run/api.sock")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queued_job_classifier_is_strict_and_fail_closed() {
        let fixture = TestControlFixture::new("queued-job-classifier");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        assert_eq!(
            validate_queued_job_classifier_request("not-a-job-id", false)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );
        assert_eq!(
            validate_queued_job_classifier_request(JOB_ID, true)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );

        let connection = ConnectionJobArgs {
            profile: Some("nightly".to_owned()),
            url: "https://nas.example.invalid".to_owned(),
            username: "backup-user".to_owned(),
            allow_http: false,
            danger_accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout_seconds: 15,
            timeout_seconds: 120,
            retries: 2,
            password_source: CredentialSource::Stored,
            totp_source: CredentialSource::None,
        };
        let mutations = [
            (
                Mutation::TestProfileAuth(connection.clone()),
                QueuedJobClass::Connection,
            ),
            (
                Mutation::BrowseRemote(BrowseRemoteJobArgs {
                    connection,
                    parent: "/".to_owned(),
                    connection_proof: format!("v1.10300.{}.{}", "b".repeat(64), "c".repeat(64)),
                }),
                QueuedJobClass::Connection,
            ),
            (
                Mutation::RemoveProfile(NameArgs {
                    name: "nightly".to_owned(),
                }),
                QueuedJobClass::Serialized,
            ),
            (
                Mutation::Action(OperationalActionArgs {
                    kind: OperationalActionKind::Plan,
                    scope: "nightly".to_owned(),
                    level: None,
                    write_test: None,
                    allow_delete: Some(false),
                    max_total_delete: None,
                }),
                QueuedJobClass::Concurrent,
            ),
        ];
        for (index, (mutation, expected)) in mutations.into_iter().enumerate() {
            let request_id = format!("{:048x}", index + 1);
            let job = canonical_job_bytes(
                &request_id,
                REQUEST_ID,
                "admin",
                package_uid.max(1),
                &[7_u8; 32],
                &request_id,
                &"a".repeat(64),
                10_000,
                &mutation,
            )
            .unwrap();
            let request = fixture.requests.join(format!("{request_id}.json"));
            fixture.write_private(&request, &job);
            assert_eq!(
                classify_queued_job_from_paths(&paths, &request_id, package_uid).unwrap(),
                expected
            );
            fs::remove_file(request).unwrap();
        }

        let malformed_id = "d".repeat(48);
        fixture.write_private(
            &fixture.requests.join(format!("{malformed_id}.json")),
            b"{}\n",
        );
        assert_eq!(
            classify_queued_job_from_paths(&paths, &malformed_id, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );

        let mismatched_file_id = "e".repeat(48);
        let embedded_id = "f".repeat(48);
        let mismatch = canonical_job_bytes(
            &embedded_id,
            REQUEST_ID,
            "admin",
            package_uid.max(1),
            &[7_u8; 32],
            &embedded_id,
            &"a".repeat(64),
            10_000,
            &Mutation::RemoveProfile(NameArgs {
                name: "nightly".to_owned(),
            }),
        )
        .unwrap();
        fixture.write_private(
            &fixture.requests.join(format!("{mismatched_file_id}.json")),
            &mismatch,
        );
        assert_eq!(
            classify_queued_job_from_paths(&paths, &mismatched_file_id, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_dsm_helper_accepts_rooted_relative_and_absolute_symlinks() {
        let fixture = TestControlFixture::new("trusted-auth-helper-links");
        let uid = TestControlFixture::package_uid();
        let synoman = fixture.root.join("usr/syno/synoman");
        let modules = synoman.join("webman/modules");
        for directory in [
            fixture.root.join("usr"),
            fixture.root.join("usr/syno"),
            synoman.clone(),
            synoman.join("webman"),
            modules.clone(),
        ] {
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let target = synoman.join("authenticate.real");
        fs::write(&target, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        let relative_link = modules.join("authenticate-relative.cgi");
        symlink("../../authenticate.real", &relative_link).unwrap();
        let relative =
            linux_runtime::validate_trusted_executable(&relative_link, &fixture.root, uid).unwrap();
        assert_eq!(relative.path, target);
        relative.revalidate().unwrap();

        let absolute_link = modules.join("authenticate-absolute.cgi");
        symlink(&target, &absolute_link).unwrap();
        let absolute =
            linux_runtime::validate_trusted_executable(&absolute_link, &fixture.root, uid).unwrap();
        assert_eq!(absolute.path, target);
        absolute.revalidate().unwrap();

        let link_metadata = fs::symlink_metadata(&relative_link).unwrap();
        assert!(link_metadata.file_type().is_symlink());
        assert_eq!(link_metadata.st_uid(), uid);
        assert!(!linux_runtime::trusted_symlink_boundary(
            &link_metadata,
            uid.wrapping_add(1)
        ));
        // Linux reports symlink permissions as 0777; mutation safety comes
        // from the trusted link owner and its validated parent directory.
        assert_eq!(link_metadata.st_mode() & 0o777, 0o777);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_dsm_helper_target_owner_policy_is_exact() {
        assert!(linux_runtime::trusted_executable_target_owner(
            0,
            0,
            0,
            Some((1, 1))
        ));
        assert!(linux_runtime::trusted_executable_target_owner(
            0,
            99,
            0,
            Some((1, 1))
        ));
        assert!(linux_runtime::trusted_executable_target_owner(
            1,
            1,
            0,
            Some((1, 1))
        ));
        assert!(!linux_runtime::trusted_executable_target_owner(
            1,
            0,
            0,
            Some((1, 1))
        ));
        assert!(!linux_runtime::trusted_executable_target_owner(
            2,
            1,
            0,
            Some((1, 1))
        ));
        assert!(!linux_runtime::trusted_executable_target_owner(
            1, 1, 0, None
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_dsm_helper_rejects_loops_escapes_and_writable_ancestors() {
        let fixture = TestControlFixture::new("unsafe-auth-helper-links");
        let uid = TestControlFixture::package_uid();
        let trusted = fixture.root.join("trusted");
        let modules = trusted.join("modules");
        fs::create_dir(&trusted).unwrap();
        fs::create_dir(&modules).unwrap();
        fs::set_permissions(&trusted, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&modules, fs::Permissions::from_mode(0o700)).unwrap();

        let loop_one = modules.join("loop-one");
        let loop_two = modules.join("loop-two");
        symlink("loop-two", &loop_one).unwrap();
        symlink("loop-one", &loop_two).unwrap();
        assert!(linux_runtime::validate_trusted_executable(&loop_one, &fixture.root, uid).is_err());

        let escape = modules.join("escape");
        symlink("../../../outside-validation-root", &escape).unwrap();
        assert!(linux_runtime::validate_trusted_executable(&escape, &fixture.root, uid).is_err());

        let target = trusted.join("authenticate.real");
        fs::write(&target, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = modules.join("authenticate.cgi");
        symlink("../authenticate.real", &link).unwrap();
        fs::set_permissions(&modules, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(linux_runtime::validate_trusted_executable(&link, &fixture.root, uid).is_err());
        fs::set_permissions(&modules, fs::Permissions::from_mode(0o700)).unwrap();

        fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(linux_runtime::validate_trusted_executable(&link, &fixture.root, uid).is_err());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            linux_runtime::validate_trusted_executable(&link, &fixture.root, uid.wrapping_add(1))
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_dsm_helper_revalidation_detects_target_replacement() {
        let fixture = TestControlFixture::new("auth-helper-revalidation");
        let uid = TestControlFixture::package_uid();
        let trusted = fixture.root.join("trusted");
        fs::create_dir(&trusted).unwrap();
        fs::set_permissions(&trusted, fs::Permissions::from_mode(0o700)).unwrap();
        let target = trusted.join("authenticate.real");
        fs::write(&target, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = trusted.join("authenticate.cgi");
        symlink("authenticate.real", &link).unwrap();
        let validated =
            linux_runtime::validate_trusted_executable(&link, &fixture.root, uid).unwrap();

        fs::rename(&target, trusted.join("authenticate.original")).unwrap();
        fs::write(&target, b"#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validated.revalidate().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dsm_user_service_fallback_is_loopback_only_and_keeps_secrets_in_headers() {
        let body = br#"{"success":true,"data":{"Session":{"user":"fixture-admin","is_admin":true},"UserSettings":{},"AppPrivilege":[],"ServiceStatus":{}}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        )
        .into_bytes();
        let (port, worker) = spawn_user_service_response(response, Duration::ZERO);
        let inputs = user_service_inputs(port, Some("native-token%2B%2F%3D"));
        let username =
            linux_runtime::query_dsm_user_service(&inputs, Duration::from_secs(2)).unwrap();
        assert_eq!(username, "fixture-admin");

        let request = worker.join().unwrap();
        assert!(request.starts_with(
            "GET /webapi/entry.cgi?api=SYNO.Core.Desktop.Initdata&version=1&method=get_user_service HTTP/1.1\r\n"
        ));
        assert!(!request.contains("attacker-controlled.invalid"));
        assert!(!request.contains("SynoToken="));
        assert!(request.to_ascii_lowercase().contains("host: 127.0.0.1:"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("cookie: id=loopback-session-secret")
        );
        let forwarded_token = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-syno-token")
                    .then_some(value.trim())
            })
            .unwrap();
        assert_eq!(forwarded_token, "native-token%2B%2F%3D");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dsm_user_service_accepts_bounded_chunked_json_without_content_length() {
        let body =
            br#"{"success":true,"data":{"Session":{"user":"chunked-admin","is_admin":true}}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            std::str::from_utf8(body).unwrap()
        )
        .into_bytes();
        let (port, worker) = spawn_user_service_response(response, Duration::ZERO);
        let inputs = user_service_inputs(port, None);
        assert_eq!(
            linux_runtime::query_dsm_user_service(&inputs, Duration::from_secs(2)).unwrap(),
            "chunked-admin"
        );
        worker.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dsm_user_service_rejects_status_length_redirect_timeout_and_malformed_identity() {
        fn unavailable(error: BridgeError) {
            assert_eq!(error.kind, ErrorKind::Unavailable);
            let rendered = format!("{error:?}");
            assert!(!rendered.contains("loopback-session-secret"));
            assert!(!rendered.contains("header-token-secret"));
        }

        let cases = [
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: invalid\r\nConnection: close\r\n\r\n{}"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n{}"
                .to_vec(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_DSM_USER_SERVICE_OUTPUT_BYTES + 1
            )
            .into_bytes(),
            b"HTTP/1.1 302 Found\r\nLocation: http://192.0.2.99/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        ];
        for response in cases {
            let (port, worker) = spawn_user_service_response(response, Duration::ZERO);
            let inputs = user_service_inputs(port, Some("header-token-secret"));
            unavailable(
                linux_runtime::query_dsm_user_service(&inputs, Duration::from_secs(2)).unwrap_err(),
            );
            let request = worker.join().unwrap();
            assert!(!request.contains("SynoToken="));
        }

        let (port, worker) = spawn_user_service_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec(),
            Duration::from_millis(250),
        );
        let inputs = user_service_inputs(port, None);
        unavailable(
            linux_runtime::query_dsm_user_service(&inputs, Duration::from_millis(75)).unwrap_err(),
        );
        worker.join().unwrap();

        for body in [
            br#"not-json"#.as_slice(),
            br#"{"success":true,"data":{"Session":{"is_admin":true}}}"#.as_slice(),
            br#"{"success":true,"data":{"Session":{"user":null,"is_admin":true}}}"#.as_slice(),
            br#"{"success":true,"data":{"Session":{"user":"admin","is_admin":"yes"}}}"#.as_slice(),
            br#"{"success":true,"data":{"Session":{"user":"admin"}}}"#.as_slice(),
        ] {
            unavailable(linux_runtime::parse_dsm_user_service_output(body).unwrap_err());
        }
        assert_eq!(
            linux_runtime::parse_dsm_user_service_output(br#"{"success":true}"#)
                .unwrap_err()
                .kind,
            ErrorKind::Unauthorized
        );

        for user in ["", "line\nbreak", "bidi\u{202e}name"] {
            let body = serde_json::to_vec(&json!({
                "success": true,
                "data": {"Session": {"user": user, "is_admin": true}}
            }))
            .unwrap();
            assert_eq!(
                linux_runtime::parse_dsm_user_service_output(&body)
                    .unwrap_err()
                    .kind,
                ErrorKind::Unauthorized
            );
        }
        let non_admin =
            br#"{"success":true,"data":{"Session":{"user":"ordinary","is_admin":false}}}"#;
        assert_eq!(
            linux_runtime::parse_dsm_user_service_output(non_admin)
                .unwrap_err()
                .kind,
            ErrorKind::Forbidden
        );
        let rejected = br#"{"success":false,"error":{"code":119}}"#;
        assert_eq!(
            linux_runtime::parse_dsm_user_service_output(rejected)
                .unwrap_err()
                .kind,
            ErrorKind::Unauthorized
        );
        assert_eq!(
            linux_runtime::authorize_relayed_username("root")
                .unwrap_err()
                .kind,
            ErrorKind::Forbidden
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quickconnect_authentication_diagnostic_uses_a_strict_host_suffix_only() {
        for authority in [
            Some("example-nas.eu.quickconnect.to"),
            Some("192.0.2.10:5001"),
            Some("nas.example.invalid"),
            None,
        ] {
            assert_eq!(
                linux_runtime::authenticated_helper_username(
                    true,
                    b"authenticated-admin\n",
                    authority,
                )
                .unwrap(),
                "authenticated-admin",
                "a host diagnostic must never alter helper acceptance"
            );
        }

        for authority in [
            "example-nas.eu.quickconnect.to",
            "EXAMPLE-NAS.EU.QUICKCONNECT.TO",
            "device.direct.quickconnect.to:443",
            "device.quickconnect.to.",
        ] {
            assert_eq!(
                linux_runtime::authentication_rejection_code(Some(authority)),
                "dsm_authentication_quickconnect_unsupported",
                "authority {authority}"
            );
            assert_eq!(
                linux_runtime::authenticated_helper_username(false, b"", Some(authority))
                    .unwrap_err()
                    .code,
                Some("dsm_authentication_quickconnect_unsupported")
            );
        }

        for authority in [
            "192.0.2.10:5001",
            "nas.example.invalid",
            "quickconnect.to",
            "quickconnect.to.evil.invalid",
            "device.quickconnect.to.evil.invalid",
            "device.quickconnect.to:invalid",
            "device.quickconnect.to:0",
            "device.quickconnect.to:0443",
            "device.quickconnect.to:65536",
            "device.quickconnect.to:443:extra",
            "-device.quickconnect.to",
            "device-.quickconnect.to",
            "device..quickconnect.to",
            "device quickconnect.to",
            "[::1]:5001",
        ] {
            assert_eq!(
                linux_runtime::authentication_rejection_code(Some(authority)),
                "dsm_authentication_rejected",
                "authority {authority}"
            );
            assert_eq!(
                linux_runtime::authenticated_helper_username(false, b"", Some(authority))
                    .unwrap_err()
                    .code,
                Some("dsm_authentication_rejected")
            );
        }
        assert_eq!(
            linux_runtime::authentication_rejection_code(None),
            "dsm_authentication_rejected"
        );
        assert_eq!(
            linux_runtime::authenticated_helper_username(
                true,
                b"\n",
                Some("device.quickconnect.to"),
            )
            .unwrap_err()
            .code,
            Some("dsm_authentication_quickconnect_unsupported")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn authentication_helper_execute_probe_precedes_metadata_validation() {
        let calls = std::cell::RefCell::new(Vec::new());
        let selection = linux_runtime::select_authentication_helper(
            |path| {
                assert_eq!(path, Path::new(AUTHENTICATE_PATH));
                calls.borrow_mut().push("probe");
                Ok(())
            },
            |path| {
                assert_eq!(path, Path::new(AUTHENTICATE_PATH));
                calls.borrow_mut().push("validate");
                Ok(7_u8)
            },
        )
        .unwrap();
        assert!(matches!(
            selection,
            linux_runtime::AuthenticationHelperSelection::Direct(7)
        ));
        assert_eq!(*calls.borrow(), ["probe", "validate"]);

        let calls = std::cell::RefCell::new(Vec::new());
        let failure = linux_runtime::select_authentication_helper(
            |_| {
                calls.borrow_mut().push("probe");
                Ok(())
            },
            |_| {
                calls.borrow_mut().push("validate");
                Err::<u8, _>(BridgeError::unsafe_runtime())
            },
        )
        .unwrap_err();
        assert_eq!(failure.stage, CgiFailureStage::Authentication);
        assert_eq!(failure.code, Some("dsm_authentication_helper_unsafe"));
        assert_eq!(*calls.borrow(), ["probe", "validate"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_eacces_selects_loopback_without_metadata_validation() {
        let calls = std::cell::RefCell::new(Vec::new());
        let selection = linux_runtime::select_authentication_helper(
            |_| {
                calls.borrow_mut().push("probe");
                Err(io::Error::from_raw_os_error(libc::EACCES))
            },
            |_| {
                calls.borrow_mut().push("validate");
                Ok(7_u8)
            },
        )
        .unwrap();
        assert!(matches!(
            selection,
            linux_runtime::AuthenticationHelperSelection::Loopback
        ));
        assert_eq!(*calls.borrow(), ["probe"]);

        for errno in [
            libc::EPERM,
            libc::ENOENT,
            libc::ELOOP,
            libc::ENOTDIR,
            libc::EIO,
        ] {
            let calls = std::cell::RefCell::new(Vec::new());
            let failure = linux_runtime::select_authentication_helper(
                |_| {
                    calls.borrow_mut().push("probe");
                    Err(io::Error::from_raw_os_error(errno))
                },
                |_| {
                    calls.borrow_mut().push("validate");
                    Ok(7_u8)
                },
            )
            .unwrap_err();
            assert_eq!(failure.stage, CgiFailureStage::Authentication);
            assert_eq!(failure.error.kind, ErrorKind::Unavailable);
            assert_eq!(failure.code, Some("dsm_authentication_helper_unavailable"));
            assert_eq!(*calls.borrow(), ["probe"], "errno {errno}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pre_relay_state_lock_is_explicitly_released_from_shared_descriptors() {
        let fixture = TestControlFixture::new("cgi-failure-lock-release");
        let state = fixture.root.join("cgi-failure.state");
        fixture.write_private(&state, b"");

        linux_files::state_flock_unlocks_shared_description_at(&state).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pre_relay_cgi_diagnostics_are_bounded_secret_free_and_coalesced() {
        let fixture = TestControlFixture::new("cgi-failure-diagnostics");
        let uid = TestControlFixture::package_uid();
        let log_root = fixture.root.join("log");
        let runtime_root = fixture.root.join("run");
        for directory in [&log_root, &runtime_root] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let api_log = log_root.join("api.log");
        let state = runtime_root.join("cgi-failure.state");
        let arguments = (
            "dsm_authentication",
            "dsm_authentication_helper_unsafe",
            503,
        );
        assert!(
            linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &api_log,
                &state,
                uid,
                10_000,
                arguments.0,
                arguments.1,
                arguments.2,
            )
            .unwrap()
        );
        assert!(
            !linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &api_log,
                &state,
                uid,
                10_029,
                arguments.0,
                arguments.1,
                arguments.2,
            )
            .unwrap()
        );
        assert!(
            !linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &api_log,
                &state,
                uid,
                10_029,
                "bridge_connect",
                "service_unavailable",
                503,
            )
            .unwrap()
        );
        assert!(
            linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &api_log,
                &state,
                uid,
                10_030,
                arguments.0,
                arguments.1,
                arguments.2,
            )
            .unwrap()
        );

        let records = fs::read_to_string(&api_log).unwrap();
        let lines = records.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        for line in lines {
            assert!(line.len() < 512);
            let record: Value = serde_json::from_str(line).unwrap();
            let keys = record
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            assert_eq!(
                keys,
                [
                    "category", "code", "epoch", "event", "level", "service", "stage", "status",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            );
            assert_eq!(record["service"], "synology-drive-sync");
            assert_eq!(record["stage"], arguments.0);
            assert_eq!(record["code"], arguments.1);
            assert_eq!(record["status"], arguments.2);
            for forbidden in [
                "id=authenticated-session",
                "SynoToken",
                "admin",
                "QUERY_STRING",
                "authenticate.cgi",
                fixture.root.to_str().unwrap(),
            ] {
                assert!(!line.contains(forbidden));
            }
        }
        let metadata = fs::symlink_metadata(&api_log).unwrap();
        assert_eq!(metadata.st_uid(), uid);
        assert_eq!(metadata.st_mode() & 0o7777, 0o600);
        assert_eq!(metadata.st_nlink(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pre_relay_persistence_obeys_validated_category_policy_before_writing() {
        let fixture = TestControlFixture::new("cgi-failure-policy");
        let uid = TestControlFixture::package_uid();
        let log_root = fixture.root.join("log");
        let runtime_root = fixture.root.join("run");
        let config_root = fixture.root.join("config");
        for directory in [&log_root, &runtime_root, &config_root] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let api_log = log_root.join("api.log");
        let state = runtime_root.join("cgi-failure.state");
        let policy_path = config_root.join("security.conf");

        let authentication_off = security_policy_document().replace(
            "authentication_log_level=warn",
            "authentication_log_level=off",
        );
        fixture.write_private(&policy_path, authentication_off.as_bytes());
        assert!(
            !linux_files::record_pre_relay_cgi_failure_with_policy_at(
                &log_root,
                &api_log,
                &state,
                &policy_path,
                uid,
                40_000,
                "dsm_authentication",
                "dsm_authentication_helper_unsafe",
                503,
            )
            .unwrap()
        );
        assert!(!api_log.exists());
        assert!(!state.exists());

        assert!(
            linux_files::record_pre_relay_cgi_failure_with_policy_at(
                &log_root,
                &api_log,
                &state,
                &policy_path,
                uid,
                40_000,
                "bridge_connect",
                "service_unavailable",
                503,
            )
            .unwrap()
        );
        let first = fs::read_to_string(&api_log).unwrap();
        assert!(first.contains(r#""category":"bridge""#));

        let separated = security_policy_document()
            .replace("bridge_log_level=info", "bridge_log_level=off")
            .replace("security_log_level=warn", "security_log_level=off");
        fixture.write_private(&policy_path, separated.as_bytes());
        assert!(
            linux_files::record_pre_relay_cgi_failure_with_policy_at(
                &log_root,
                &api_log,
                &state,
                &policy_path,
                uid,
                40_030,
                "dsm_authentication",
                "dsm_authentication_helper_unsafe",
                503,
            )
            .unwrap()
        );
        assert!(
            !linux_files::record_pre_relay_cgi_failure_with_policy_at(
                &log_root,
                &api_log,
                &state,
                &policy_path,
                uid,
                40_060,
                "cgi_identity",
                "cgi_identity_unsafe",
                503,
            )
            .unwrap()
        );
        let records = fs::read_to_string(&api_log).unwrap();
        assert_eq!(records.lines().count(), 2);
        assert!(records.contains(r#""category":"authentication""#));
        assert!(!records.contains(r#""category":"security""#));

        fixture.write_private(&policy_path, b"corrupt\n");
        assert!(
            linux_files::record_pre_relay_cgi_failure_with_policy_at(
                &log_root,
                &api_log,
                &state,
                &policy_path,
                uid,
                40_060,
                "dsm_authentication",
                "dsm_authentication_helper_unsafe",
                503,
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&api_log).unwrap(), records);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pre_relay_api_log_rotation_is_bounded_and_rejects_unsafe_entries() {
        let fixture = TestControlFixture::new("cgi-failure-rotation");
        let uid = TestControlFixture::package_uid();
        let log_root = fixture.root.join("log");
        let runtime_root = fixture.root.join("run");
        for directory in [&log_root, &runtime_root] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let api_log = log_root.join("api.log");
        let state = runtime_root.join("cgi-failure.state");
        let create_private = |path: &Path, size: u64| {
            let file = File::create(path).unwrap();
            file.set_len(size).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        };
        create_private(&api_log, linux_files::MAX_API_LOG_BYTES);

        let rotated_one = log_root.join("api.log.1");
        symlink("attacker", &rotated_one).unwrap();
        assert!(
            linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &api_log,
                &state,
                uid,
                20_000,
                "bridge_connect",
                "service_unavailable",
                503,
            )
            .is_err()
        );
        assert!(rotated_one.is_symlink());
        assert_eq!(
            fs::metadata(&api_log).unwrap().len(),
            linux_files::MAX_API_LOG_BYTES
        );
        fs::remove_file(&rotated_one).unwrap();

        let hardlink_source = fixture.root.join("hardlink-source");
        create_private(&hardlink_source, 1);
        fs::hard_link(&hardlink_source, &rotated_one).unwrap();
        assert!(
            linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &api_log,
                &state,
                uid,
                20_000,
                "bridge_connect",
                "service_unavailable",
                503,
            )
            .is_err()
        );
        assert_eq!(fs::symlink_metadata(&rotated_one).unwrap().st_nlink(), 2);
        fs::remove_file(&rotated_one).unwrap();
        fs::remove_file(&hardlink_source).unwrap();

        create_private(&rotated_one, 1);
        fs::set_permissions(&rotated_one, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(
            linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &api_log,
                &state,
                uid,
                20_000,
                "bridge_connect",
                "service_unavailable",
                503,
            )
            .is_err()
        );
        assert_eq!(
            fs::symlink_metadata(&rotated_one).unwrap().st_mode() & 0o7777,
            0o640
        );
        fs::remove_file(&rotated_one).unwrap();

        for index in 1..=linux_files::API_LOG_ROTATIONS {
            create_private(&log_root.join(format!("api.log.{index}")), index as u64);
        }
        assert!(
            linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &api_log,
                &state,
                uid,
                20_000,
                "bridge_connect",
                "service_unavailable",
                503,
            )
            .unwrap()
        );
        let active = fs::symlink_metadata(&api_log).unwrap();
        assert_eq!(active.st_uid(), uid);
        assert_eq!(active.st_mode() & 0o7777, 0o600);
        assert_eq!(active.st_nlink(), 1);
        assert!(active.len() < 512);
        assert_eq!(
            fs::metadata(&rotated_one).unwrap().len(),
            linux_files::MAX_API_LOG_BYTES
        );
        assert_eq!(fs::metadata(log_root.join("api.log.5")).unwrap().len(), 4);
        assert!(!log_root.join("api.log.6").exists());

        let unsafe_active = log_root.join("unsafe-active");
        let unsafe_active_state = runtime_root.join("unsafe-active.state");
        fixture.write_private(&unsafe_active_state, b"");
        symlink("api.log", &unsafe_active).unwrap();
        assert!(
            linux_files::record_pre_relay_cgi_failure_at(
                &log_root,
                &unsafe_active,
                &unsafe_active_state,
                uid,
                20_030,
                "bridge_connect",
                "service_unavailable",
                503,
            )
            .is_err()
        );
        assert!(unsafe_active.is_symlink());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pre_relay_cgi_coalescing_has_one_global_concurrent_emission_window() {
        use std::sync::{Arc, Barrier};

        let fixture = TestControlFixture::new("cgi-failure-concurrency");
        let uid = TestControlFixture::package_uid();
        let log_root = fixture.root.join("log");
        let runtime_root = fixture.root.join("run");
        for directory in [&log_root, &runtime_root] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let api_log = log_root.join("api.log");
        let state = runtime_root.join("cgi-failure.state");
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();
        for index in 0..8 {
            let barrier = Arc::clone(&barrier);
            let log_root = log_root.clone();
            let api_log = api_log.clone();
            let state = state.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let (stage, code) = if index % 2 == 0 {
                    ("dsm_authentication", "dsm_authentication_helper_unsafe")
                } else {
                    ("bridge_connect", "service_unavailable")
                };
                linux_files::record_pre_relay_cgi_failure_at(
                    &log_root, &api_log, &state, uid, 30_000, stage, code, 503,
                )
            }));
        }
        let mut emitted = 0;
        for worker in workers {
            if worker.join().unwrap().unwrap() {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 1);
        let records = fs::read_to_string(&api_log).unwrap();
        assert_eq!(records.lines().count(), 1);
        serde_json::from_str::<Value>(records.trim_end()).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unix_socket_peers_are_credential_checked_in_both_directions() {
        use std::os::unix::net::UnixStream;

        // SAFETY: these identity calls have no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        // SAFETY: these identity calls have no pointer arguments or preconditions.
        let gid = unsafe { libc::getegid() };
        if uid == 0 || gid == 0 {
            return;
        }
        let (first, second) = UnixStream::pair().unwrap();
        let first_peer = linux_socket::peer_credentials(&first).unwrap();
        let second_peer = linux_socket::peer_credentials(&second).unwrap();
        assert_eq!(first_peer.uid, uid);
        assert_eq!(first_peer.gid, gid);
        assert_eq!(second_peer.uid, uid);
        linux_socket::validate_peer_uid(first_peer.uid, uid).unwrap();
        let wrong_uid = uid.checked_add(1).unwrap_or(uid - 1);
        assert!(linux_socket::validate_peer_uid(first_peer.uid, wrong_uid).is_err());

        let fixture = TestControlFixture::new("socket-peer");
        let socket = fixture.root.join("api.sock");
        let listener = linux_socket::bind(&socket, uid).unwrap();
        let client = linux_socket::connect(&socket, uid).unwrap();
        let (server, _) = listener.accept().unwrap();
        assert_eq!(linux_socket::peer_credentials(&client).unwrap().uid, uid);
        assert_eq!(linux_socket::peer_credentials(&server).unwrap().uid, uid);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_admission_markers_linearize_with_enqueue_publication() {
        let fixture = TestControlFixture::new("runtime-admission-fence");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        if package_uid == 0 {
            return;
        }
        let session = [11_u8; 32];
        let mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let fingerprint = "11".repeat(32);

        let (admitted_sender, admitted_receiver) = std::sync::mpsc::sync_channel(0);
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
        let (closed_sender, closed_receiver) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let enqueue_paths = paths;
            let enqueue_session = &session;
            let enqueue_fingerprint = &fingerprint;
            let enqueue_mutation = &mutation;
            let enqueue = scope.spawn(move || {
                linux_files::enqueue_with_admission_hook(
                    &enqueue_paths,
                    EnqueueRequest {
                        package_uid,
                        client_request_id: REQUEST_ID,
                        requested_by: "admin",
                        requested_uid: 1000,
                        session_binding: enqueue_session,
                        audit_transaction: JOB_ID,
                        request_fingerprint: enqueue_fingerprint,
                        issued_at_epoch: 10_000,
                        mutation: enqueue_mutation,
                        secret: None,
                    },
                    MAX_OUTSTANDING_JOBS,
                    |_, _| Ok(()),
                    || {
                        admitted_sender.send(()).unwrap();
                        release_receiver.recv().unwrap();
                    },
                )
            });
            admitted_receiver.recv().unwrap();
            let close_paths = paths;
            let close = scope.spawn(move || {
                let result = linux_files::close_service_admission(&close_paths, package_uid);
                closed_sender.send(()).unwrap();
                result
            });
            assert!(
                closed_receiver
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
            release_sender.send(()).unwrap();
            assert!(enqueue.join().unwrap().is_ok());
            assert!(close.join().unwrap().is_ok());
            closed_receiver.recv().unwrap();
        });
        assert_eq!(fs::read_dir(&fixture.requests).unwrap().count(), 1);
        assert_eq!(
            fs::read_dir(&fixture.audit_outbox_directory)
                .unwrap()
                .count(),
            1
        );

        let denied_mutation = Mutation::RemoveRoutine(NameArgs {
            name: "nightly".to_owned(),
        });
        let denied_fingerprint = "22".repeat(32);
        let denied = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                package_uid,
                client_request_id: "11111111111111111111111111111111",
                requested_by: "admin",
                requested_uid: 1000,
                session_binding: &session,
                audit_transaction: "11160f5e12345678fedcba98765432100123456789abcdef",
                request_fingerprint: &denied_fingerprint,
                issued_at_epoch: 10_001,
                mutation: &denied_mutation,
                secret: None,
            },
            MAX_OUTSTANDING_JOBS,
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert_eq!(denied.kind, ErrorKind::Unavailable);
        assert_eq!(fs::read_dir(&fixture.requests).unwrap().count(), 1);
        assert_eq!(
            fs::read_dir(&fixture.audit_outbox_directory)
                .unwrap()
                .count(),
            1
        );
        assert!(
            !fs::read_dir(&fixture.staging)
                .unwrap()
                .any(|entry| entry.is_ok())
        );

        let (open_locked_sender, open_locked_receiver) = std::sync::mpsc::sync_channel(0);
        let (open_release_sender, open_release_receiver) = std::sync::mpsc::sync_channel(0);
        let (enqueue_done_sender, enqueue_done_receiver) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let open_paths = paths;
            let open = scope.spawn(move || {
                linux_files::open_service_admission_with_hook(&open_paths, package_uid, || {
                    open_locked_sender.send(()).unwrap();
                    open_release_receiver.recv().unwrap();
                })
            });
            open_locked_receiver.recv().unwrap();
            let enqueue_paths = paths;
            let enqueue_session = &session;
            let enqueue_fingerprint = &denied_fingerprint;
            let enqueue_mutation = &denied_mutation;
            let enqueue = scope.spawn(move || {
                let result = linux_files::enqueue(
                    &enqueue_paths,
                    EnqueueRequest {
                        package_uid,
                        client_request_id: "22222222222222222222222222222222",
                        requested_by: "admin",
                        requested_uid: 1000,
                        session_binding: enqueue_session,
                        audit_transaction: "22260f5e12345678fedcba98765432100123456789abcdef",
                        request_fingerprint: enqueue_fingerprint,
                        issued_at_epoch: 10_002,
                        mutation: enqueue_mutation,
                        secret: None,
                    },
                    MAX_OUTSTANDING_JOBS,
                    |_, _| Ok(()),
                );
                enqueue_done_sender.send(()).unwrap();
                result
            });
            assert!(
                enqueue_done_receiver
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
            open_release_sender.send(()).unwrap();
            assert!(open.join().unwrap().is_ok());
            assert!(enqueue.join().unwrap().is_ok());
            enqueue_done_receiver.recv().unwrap();
        });
        assert_eq!(fs::read_dir(&fixture.requests).unwrap().count(), 2);

        let (close_locked_sender, close_locked_receiver) = std::sync::mpsc::sync_channel(0);
        let (close_release_sender, close_release_receiver) = std::sync::mpsc::sync_channel(0);
        let (close_enqueue_sender, close_enqueue_receiver) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let close_paths = paths;
            let close = scope.spawn(move || {
                linux_files::close_service_admission_with_hook(&close_paths, package_uid, || {
                    close_locked_sender.send(()).unwrap();
                    close_release_receiver.recv().unwrap();
                })
            });
            close_locked_receiver.recv().unwrap();
            let enqueue_paths = paths;
            let enqueue_session = &session;
            let enqueue_fingerprint = &fingerprint;
            let enqueue_mutation = &mutation;
            let enqueue = scope.spawn(move || {
                let result = linux_files::enqueue(
                    &enqueue_paths,
                    EnqueueRequest {
                        package_uid,
                        client_request_id: "44444444444444444444444444444444",
                        requested_by: "admin",
                        requested_uid: 1000,
                        session_binding: enqueue_session,
                        audit_transaction: "44460f5e12345678fedcba98765432100123456789abcdef",
                        request_fingerprint: enqueue_fingerprint,
                        issued_at_epoch: 10_004,
                        mutation: enqueue_mutation,
                        secret: None,
                    },
                    MAX_OUTSTANDING_JOBS,
                    |_, _| Ok(()),
                );
                close_enqueue_sender.send(()).unwrap();
                result
            });
            assert!(
                close_enqueue_receiver
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
            close_release_sender.send(()).unwrap();
            assert!(close.join().unwrap().is_ok());
            let denied = enqueue.join().unwrap().unwrap_err();
            assert_eq!(denied.kind, ErrorKind::Unavailable);
            close_enqueue_receiver.recv().unwrap();
        });
        assert_eq!(fs::read_dir(&fixture.requests).unwrap().count(), 2);
        assert_eq!(
            fs::read_dir(&fixture.audit_outbox_directory)
                .unwrap()
                .count(),
            2
        );
        linux_files::open_service_admission(&paths, package_uid).unwrap();

        let (transition_locked_sender, transition_locked_receiver) =
            std::sync::mpsc::sync_channel(0);
        let (transition_release_sender, transition_release_receiver) =
            std::sync::mpsc::sync_channel(0);
        let (transition_enqueue_sender, transition_enqueue_receiver) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let transition_paths = paths;
            let transition = scope.spawn(move || {
                linux_files::prepare_package_transition_with_hook(
                    &transition_paths,
                    package_uid,
                    "upgrade",
                    || {
                        transition_locked_sender.send(()).unwrap();
                        transition_release_receiver.recv().unwrap();
                    },
                )
            });
            transition_locked_receiver.recv().unwrap();
            let enqueue_paths = paths;
            let enqueue_session = &session;
            let enqueue_fingerprint = &fingerprint;
            let enqueue_mutation = &mutation;
            let enqueue = scope.spawn(move || {
                let result = linux_files::enqueue(
                    &enqueue_paths,
                    EnqueueRequest {
                        package_uid,
                        client_request_id: "33333333333333333333333333333333",
                        requested_by: "admin",
                        requested_uid: 1000,
                        session_binding: enqueue_session,
                        audit_transaction: "33360f5e12345678fedcba98765432100123456789abcdef",
                        request_fingerprint: enqueue_fingerprint,
                        issued_at_epoch: 10_003,
                        mutation: enqueue_mutation,
                        secret: None,
                    },
                    MAX_OUTSTANDING_JOBS,
                    |_, _| Ok(()),
                );
                transition_enqueue_sender.send(()).unwrap();
                result
            });
            assert!(
                transition_enqueue_receiver
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
            transition_release_sender.send(()).unwrap();
            assert!(transition.join().unwrap().is_ok());
            let denied = enqueue.join().unwrap().unwrap_err();
            assert_eq!(denied.kind, ErrorKind::Unavailable);
            transition_enqueue_receiver.recv().unwrap();
        });
        assert_eq!(fs::read_dir(&fixture.requests).unwrap().count(), 2);
        assert_eq!(
            fs::read_dir(&fixture.audit_outbox_directory)
                .unwrap()
                .count(),
            2
        );
        linux_files::clear_package_transition(&paths, package_uid).unwrap();
        linux_files::require_open_runtime_admission(&paths, package_uid).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgi_socket_retry_is_bounded_and_never_retries_unsafe_metadata() {
        // SAFETY: these identity calls have no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            return;
        }
        let fixture = TestControlFixture::new("cgi-socket-retry");
        let socket = fixture.root.join("api.sock");
        let delayed_socket = socket.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let listener = linux_socket::bind(&delayed_socket, uid).unwrap();
            let (stream, _) = listener.accept().unwrap();
            assert_eq!(linux_socket::peer_credentials(&stream).unwrap().uid, uid);
        });
        let started = Instant::now();
        let client = linux_socket::connect_for_cgi(&socket, uid, Duration::from_secs(1)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(75));
        drop(client);
        server.join().unwrap();
        fs::remove_file(&socket).unwrap();

        let (prepared, prepared_identity) = linux_socket::bind_prepared(&socket, uid).unwrap();
        assert_eq!(
            fs::symlink_metadata(&socket).unwrap().permissions().mode() & 0o7777,
            0o000
        );
        let prepared_started = Instant::now();
        assert_eq!(
            linux_socket::connect_for_cgi(&socket, uid, Duration::from_millis(125),)
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
        assert!(prepared_started.elapsed() >= Duration::from_millis(100));
        prepared.set_nonblocking(true).unwrap();
        assert_eq!(
            prepared.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "a pre-commit CGI reached the prepared listener"
        );
        prepared.set_nonblocking(false).unwrap();
        linux_socket::activate_prepared(&socket, uid, &prepared_identity).unwrap();
        let activated_identity = fs::symlink_metadata(&socket).unwrap();
        assert_eq!(activated_identity.st_dev(), prepared_identity.st_dev());
        assert_eq!(activated_identity.st_ino(), prepared_identity.st_ino());
        assert_eq!(activated_identity.st_uid(), uid);
        assert_eq!(activated_identity.st_gid(), prepared_identity.st_gid());
        assert_eq!(activated_identity.permissions().mode() & 0o7777, 0o600);
        let active =
            linux_socket::connect_for_cgi(&socket, uid, Duration::from_millis(125)).unwrap();
        let (accepted, _) = prepared.accept().unwrap();
        drop(active);
        drop(accepted);
        drop(prepared);
        fs::remove_file(&socket).unwrap();

        let (original, original_identity) = linux_socket::bind_prepared(&socket, uid).unwrap();
        fs::remove_file(&socket).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(
            linux_socket::activate_prepared(&socket, uid, &original_identity)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        drop(original);
        drop(replacement);
        fs::remove_file(&socket).unwrap();

        let missing_started = Instant::now();
        assert_eq!(
            linux_socket::connect_for_cgi(&socket, uid, Duration::from_millis(125),)
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
        assert!(missing_started.elapsed() >= Duration::from_millis(100));
        assert!(missing_started.elapsed() < Duration::from_secs(1));

        let outside = fixture.root.join("outside");
        fs::write(&outside, b"preserve").unwrap();
        symlink(&outside, &socket).unwrap();
        let unsafe_started = Instant::now();
        assert_eq!(
            linux_socket::connect_for_cgi(&socket, uid, Duration::from_secs(1),)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        assert!(unsafe_started.elapsed() < Duration::from_millis(250));
        assert!(
            fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&outside).unwrap(), b"preserve");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_worker_pool_rejects_saturation_and_recovers() {
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
        let release_receiver = std::sync::Arc::new(std::sync::Mutex::new(release_receiver));
        let worker_release = std::sync::Arc::clone(&release_receiver);
        let workers = BoundedWorkerPool::start(1, 1, move |item| {
            started_sender.send(item).unwrap();
            worker_release.lock().unwrap().recv().unwrap();
        })
        .unwrap();

        workers.try_dispatch(1_u8).unwrap();
        assert_eq!(
            started_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            1
        );
        workers.try_dispatch(2).unwrap();
        assert!(matches!(
            workers.try_dispatch(3),
            Err(DispatchError::Full(3))
        ));

        release_sender.send(()).unwrap();
        assert_eq!(
            started_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            2
        );
        workers.try_dispatch(4).unwrap();
        release_sender.send(()).unwrap();
        assert_eq!(
            started_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            4
        );
        release_sender.send(()).unwrap();
        drop(workers);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn helper_capture_bounds_input_output_and_stderr_without_leaking() {
        let secret = Zeroizing::new(b"bounded-input".to_vec());
        for _ in 0..32 {
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                "IFS= read -r value; printf '%s' \"$value\"; printf 'discarded' >&2",
            ]);
            let output = capture_bounded_command(
                &mut command,
                64,
                64,
                Duration::from_secs(2),
                Some(&secret),
            )
            .unwrap();
            assert!(output.status_success);
            assert_eq!(&output.stdout[..], b"bounded-input");
        }

        let oversized_input = Zeroizing::new(vec![b'x'; MAX_SECRET_BYTES + 1]);
        let mut rejected_input = Command::new("/bin/true");
        assert!(
            capture_bounded_command(
                &mut rejected_input,
                64,
                64,
                Duration::from_secs(2),
                Some(&oversized_input),
            )
            .is_err()
        );

        for script in ["printf 123456789", "printf 123456789 >&2"] {
            let mut overflowing = Command::new("/bin/sh");
            overflowing.args(["-c", script]);
            let error =
                match capture_bounded_command(&mut overflowing, 8, 8, Duration::from_secs(2), None)
                {
                    Err(error) => error,
                    Ok(_) => panic!("oversized helper output was accepted"),
                };
            assert_eq!(error.kind, ErrorKind::Unavailable);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn helper_capture_kills_hangs_and_pipe_holding_descendants_then_recovers() {
        for script in ["sleep 5", "(sleep 5) & printf ok"] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]);
            let started = Instant::now();
            let error = match capture_bounded_command(
                &mut command,
                64,
                64,
                Duration::from_millis(150),
                None,
            ) {
                Err(error) => error,
                Ok(_) => panic!("hung helper was accepted"),
            };
            assert_eq!(error.kind, ErrorKind::Unavailable);
            assert!(started.elapsed() < Duration::from_secs(2));
        }

        let mut recovery = Command::new("/bin/sh");
        recovery.args(["-c", "printf recovered"]);
        let output =
            capture_bounded_command(&mut recovery, 64, 64, Duration::from_secs(2), None).unwrap();
        assert!(output.status_success);
        assert_eq!(&output.stdout[..], b"recovered");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queued_mutation_capture_has_no_deadline_and_uses_an_isolated_group() {
        // SAFETY: getpgrp has no pointer arguments or preconditions.
        let existing_group = unsafe { libc::getpgrp() };
        let termination_requested = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "IFS= read -r value; sleep 0.2; printf '%s|' \"$value\"; awk '{print $1, $5}' /proc/$$/stat",
        ]);
        let secret = Zeroizing::new(b"queued-secret".to_vec());
        let started = Instant::now();
        let output = capture_queued_mutation_command(
            &mut command,
            128,
            Some(&secret),
            &termination_requested,
        )
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(output.status_success);
        let text = std::str::from_utf8(&output.stdout).unwrap();
        let (received_secret, identity) = text.split_once('|').unwrap();
        assert_eq!(received_secret, "queued-secret");
        let identity = identity
            .split_ascii_whitespace()
            .map(|value| value.parse::<libc::pid_t>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(identity.len(), 2);
        assert_eq!(identity[0], identity[1]);
        assert_ne!(identity[1], existing_group);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queued_mutation_cancellation_is_group_safe_without_subreaper() {
        const CHILD_MODE: &str = "SDSYNC_TEST_QUEUED_SIGNAL_CHILD";
        const PID_FILE: &str = "SDSYNC_TEST_QUEUED_SIGNAL_PIDS";
        if std::env::var_os(CHILD_MODE).is_some() {
            assert!(!classify_consumer_subreaper_result(-1, Some(libc::EINVAL)).unwrap());
            let termination_requested = install_consumer_termination_handler().unwrap();
            let pid_file = PathBuf::from(std::env::var_os(PID_FILE).unwrap());
            let mut command = Command::new("/bin/sh");
            command.env("SDSYNC_TEST_PID_FILE", &pid_file).args([
                "-c",
                "trap 'wait \"$descendant\" 2>/dev/null || true; exit 143' TERM INT HUP; sleep 30 & descendant=$!; manager_group=$(awk '{print $5}' /proc/$$/stat); printf '%s %s %s\n' \"$$\" \"$manager_group\" \"$descendant\" > \"$SDSYNC_TEST_PID_FILE\"; wait \"$descendant\"",
            ]);
            // A maximum-sized secret plus the manager's newline exceeds a 4
            // KiB pipe. TERM must remain observable even if this helper never
            // reads the secret input.
            let secret = Zeroizing::new(vec![b's'; MAX_SECRET_BYTES]);
            let error = match capture_queued_mutation_command(
                &mut command,
                128,
                Some(&secret),
                termination_requested.as_ref(),
            ) {
                Err(error) => error,
                Ok(_) => panic!("terminated queued helper was accepted"),
            };
            assert_eq!(error.kind, ErrorKind::Unavailable);
            assert!(termination_requested.load(AtomicOrdering::Acquire));
            let manager_pid = fs::read_to_string(&pid_file)
                .unwrap()
                .split_ascii_whitespace()
                .next()
                .unwrap()
                .parse::<libc::pid_t>()
                .unwrap();
            let mut status = 0;
            // SAFETY: the capture helper must already have reaped its direct
            // manager before returning from cooperative cancellation.
            assert_eq!(
                unsafe { libc::waitpid(manager_pid, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            return;
        }

        let fixture = TestControlFixture::new("queued-cancel");
        let pid_file = fixture.root.join("manager-pids");
        let test_binary = std::env::current_exe().unwrap();
        let started = Instant::now();
        let mut consumer = Command::new(test_binary)
            .args([
                "queued_mutation_cancellation_is_group_safe_without_subreaper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_MODE, "true")
            .env(PID_FILE, &pid_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(3);
        while !pid_file.is_file() && Instant::now() < ready_deadline {
            assert!(
                consumer.try_wait().unwrap().is_none(),
                "consumer exited before ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(pid_file.is_file(), "queued manager did not become ready");

        let identities = fs::read_to_string(&pid_file)
            .unwrap()
            .split_ascii_whitespace()
            .map(|value| value.parse::<libc::pid_t>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(identities.len(), 3);
        let manager_pid = identities[0];
        let manager_group = identities[1];
        let descendant_pid = identities[2];
        assert_eq!(manager_pid, manager_group);

        // SAFETY: the subprocess installed the production TERM/HUP/INT handler
        // before publishing its manager PID file.
        assert_eq!(
            unsafe { libc::kill(consumer.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        let consumer_deadline = Instant::now() + Duration::from_secs(5);
        let consumer_status = loop {
            if let Some(status) = consumer.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < consumer_deadline,
                "consumer did not finish cooperative process-group cleanup"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(consumer_status.success());
        assert!(started.elapsed() < Duration::from_secs(5));

        let disappearance_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: signal zero only probes the recorded process identifiers.
            let manager_gone = unsafe { libc::kill(manager_pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            // SAFETY: signal zero only probes the recorded process identifiers.
            let descendant_gone = unsafe { libc::kill(descendant_pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if manager_gone && descendant_gone {
                break;
            }
            assert!(
                Instant::now() < disappearance_deadline,
                "queued manager process group survived cancellation"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unix_socket_bind_recovers_only_verified_stale_sockets_and_rejects_symlinks() {
        // SAFETY: these identity calls have no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        // SAFETY: these identity calls have no pointer arguments or preconditions.
        let gid = unsafe { libc::getegid() };
        if uid == 0 || gid == 0 {
            return;
        }
        let fixture = TestControlFixture::new("socket-stale");
        let socket = fixture.root.join("api.sock");
        let pid_file = fixture.root.join("api.pid");
        let listener = linux_socket::bind(&socket, uid).unwrap();
        let metadata = fs::symlink_metadata(&socket).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        assert_eq!(metadata.st_uid(), uid);
        assert_eq!(metadata.st_gid(), gid);
        assert_eq!(
            linux_socket::bind(&socket, uid).unwrap_err().kind,
            ErrorKind::Conflict
        );
        let (conflict_probe, _) = listener.accept().unwrap();
        drop(conflict_probe);
        // Other parallel tests fork helpers. CLOEXEC closes an inherited
        // listener only after exec, so disable the shared socket description
        // before dropping our descriptor and constructing the stale fixture.
        // SAFETY: the listener descriptor is live and owned by this test.
        let shutdown_status =
            unsafe { libc::shutdown(std::os::fd::AsRawFd::as_raw_fd(&listener), libc::SHUT_RDWR) };
        assert_eq!(shutdown_status, 0);
        drop(listener);

        // This is the safe crash window after bind(2) under umask 0777 but
        // before the final package-only mode contract has been applied. An
        // ordinary bind must never interpret EACCES as stale without a
        // validated dead service PID.
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(
            linux_socket::bind(&socket, uid).unwrap_err().kind,
            ErrorKind::UnsafeRuntime
        );
        assert!(socket.exists());
        fixture.write_private(&pid_file, b"2147483647\n");
        linux_socket::cleanup_stale_service_socket(&socket, &pid_file, uid, None).unwrap();
        assert!(!socket.exists());
        assert!(!pid_file.exists());

        let recovered = linux_socket::bind(&socket, uid).unwrap();
        assert_eq!(
            fs::symlink_metadata(&socket).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        drop(recovered);
        fixture.write_private(&pid_file, format!("{}\n", std::process::id()).as_bytes());
        assert_eq!(
            linux_socket::cleanup_stale_service_socket(&socket, &pid_file, uid, None)
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert!(socket.exists());
        assert!(pid_file.exists());
        let own_start = fs::read_to_string(format!("/proc/{}/stat", std::process::id()))
            .unwrap()
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_ascii_whitespace()
            .nth(19)
            .unwrap()
            .parse::<u64>()
            .unwrap();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .to_owned();
        let own_identity = linux_socket::TerminalProcessIdentity {
            pid: std::process::id(),
            start: own_start,
            boot: boot.clone(),
        };
        assert_eq!(
            linux_socket::exact_process_state(&own_identity, uid).unwrap(),
            linux_socket::ExactProcessState::Live
        );
        assert_eq!(
            linux_socket::cleanup_stale_service_socket(
                &socket,
                &pid_file,
                uid,
                Some(&own_identity),
            )
            .unwrap_err()
            .kind,
            ErrorKind::Conflict
        );
        assert_eq!(
            linux_socket::exact_process_state(&own_identity, uid + 1)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        assert!(!linux_socket::proc_entry_absent(std::process::id()).unwrap());
        assert!(linux_socket::proc_entry_absent(libc::pid_t::MAX as u32).unwrap());
        let wrong_pid = linux_socket::TerminalProcessIdentity {
            pid: libc::pid_t::MAX as u32,
            start: own_start,
            boot: boot.clone(),
        };
        assert_eq!(
            linux_socket::cleanup_stale_service_socket(&socket, &pid_file, uid, Some(&wrong_pid),)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        let mut wrong_boot = boot.clone().into_bytes();
        wrong_boot[0] = if wrong_boot[0] == b'0' { b'1' } else { b'0' };
        let wrong_boot = linux_socket::TerminalProcessIdentity {
            pid: std::process::id(),
            start: own_start,
            boot: String::from_utf8(wrong_boot).unwrap(),
        };
        assert_eq!(
            linux_socket::cleanup_stale_service_socket(&socket, &pid_file, uid, Some(&wrong_boot),)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        let reused = linux_socket::TerminalProcessIdentity {
            pid: std::process::id(),
            start: own_start + 1,
            boot: boot.clone(),
        };
        assert_eq!(
            linux_socket::cleanup_stale_service_socket(&socket, &pid_file, uid, Some(&reused),)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        assert!(socket.exists());
        assert!(pid_file.exists());
        fs::remove_file(&pid_file).unwrap();
        fs::remove_file(&socket).unwrap();

        let zombie_socket = linux_socket::bind(&socket, uid).unwrap();
        drop(zombie_socket);
        let mut zombie = Command::new("/bin/sh")
            .args(["-c", "sleep 0.1"])
            .spawn()
            .unwrap();
        let zombie_pid = zombie.id();
        let zombie_start = fs::read_to_string(format!("/proc/{zombie_pid}/stat"))
            .unwrap()
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_ascii_whitespace()
            .nth(19)
            .unwrap()
            .parse::<u64>()
            .unwrap();
        let zombie_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let state = fs::read_to_string(format!("/proc/{zombie_pid}/stat"))
                .unwrap()
                .rsplit_once(") ")
                .unwrap()
                .1
                .split_ascii_whitespace()
                .next()
                .unwrap()
                .to_owned();
            if matches!(state.as_str(), "Z" | "X" | "x") {
                break;
            }
            assert!(
                Instant::now() < zombie_deadline,
                "child did not become a zombie"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture.write_private(&pid_file, format!("{zombie_pid}\n").as_bytes());
        let exact_zombie = linux_socket::TerminalProcessIdentity {
            pid: zombie_pid,
            start: zombie_start,
            boot,
        };
        linux_socket::cleanup_stale_service_socket(&socket, &pid_file, uid, Some(&exact_zombie))
            .unwrap();
        assert!(!socket.exists());
        assert!(!pid_file.exists());
        zombie.wait().unwrap();
        linux_socket::cleanup_stale_service_socket(&socket, &pid_file, uid, Some(&exact_zombie))
            .unwrap();

        let unbound_socket = linux_socket::bind(&socket, uid).unwrap();
        drop(unbound_socket);
        assert_eq!(
            linux_socket::cleanup_stale_service_socket(
                &socket,
                &pid_file,
                uid,
                Some(&exact_zombie),
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );
        assert!(socket.exists());
        fs::remove_file(&socket).unwrap();

        let outside = fixture.root.join("outside");
        fs::write(&outside, b"do not remove").unwrap();
        symlink(&outside, &socket).unwrap();
        assert_eq!(
            linux_socket::bind(&socket, uid).unwrap_err().kind,
            ErrorKind::UnsafeRuntime
        );
        assert!(
            fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&outside).unwrap(), b"do not remove");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_queue_key_secret_result_and_fifo_round_trip() {
        let fixture = TestControlFixture::new("round-trip");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let session = [7_u8; 32];

        let first_key = linux_files::load_or_create_csrf_key(&paths, package_uid).unwrap();
        assert_eq!(first_key.len(), 32);
        assert_eq!(
            fs::metadata(&fixture.csrf_key)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let second_key = linux_files::load_or_create_csrf_key(&paths, package_uid).unwrap();
        assert_eq!(&first_key[..], &second_key[..]);
        assert_eq!(linux_files::random_nonce().unwrap().len(), 16);

        let secret_mutation = Mutation::SetSecret(SecretJobArgs {
            profile: "nightly".to_owned(),
            kind: SecretKind::Password,
            mode: SecretMode::Replace,
        });
        let secret_fingerprint =
            mutation_request_fingerprint(&first_key[..], &secret_mutation, Some(b"queue-secret"))
                .unwrap();
        let first_id = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                package_uid,
                client_request_id: REQUEST_ID,
                requested_by: "admin",
                requested_uid: 1000,
                session_binding: &session,
                audit_transaction: "10060f5e12345678fedcba98765432100123456789abcdef",
                request_fingerprint: &secret_fingerprint,
                issued_at_epoch: 10_000,
                mutation: &secret_mutation,
                secret: Some(b"queue-secret"),
            },
            MAX_OUTSTANDING_JOBS,
            |_, _| Ok(()),
        )
        .unwrap()
        .job_id()
        .to_owned();
        let plain_mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let plain_fingerprint =
            mutation_request_fingerprint(&first_key[..], &plain_mutation, None).unwrap();
        let second_id = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                package_uid,
                client_request_id: "11111111111111111111111111111111",
                requested_by: "admin",
                requested_uid: 1000,
                session_binding: &session,
                audit_transaction: JOB_ID,
                request_fingerprint: &plain_fingerprint,
                issued_at_epoch: 10_001,
                mutation: &plain_mutation,
                secret: None,
            },
            MAX_OUTSTANDING_JOBS,
            |_, _| Ok(()),
        )
        .unwrap()
        .job_id()
        .to_owned();
        assert!(valid_server_job_id(&first_id));
        assert!(first_id < second_id);

        let first_request = fixture.requests.join(format!("{first_id}.json"));
        let first_secret = fixture.requests.join(format!("{first_id}.secret"));
        let first_job_bytes = fs::read(&first_request).unwrap();
        assert!(!contains_bytes(&first_job_bytes, b"queue-secret"));
        assert_eq!(
            fs::metadata(&first_request).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&first_secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read(&first_secret).unwrap(), b"queue-secret\n");
        assert!(
            linux_files::read_optional_pending_job(&paths, &first_id, package_uid, false)
                .unwrap()
                .is_some()
        );

        let claimed_request = fixture.processing.join(format!("{first_id}.json"));
        let claimed_secret = fixture.processing.join(format!("{first_id}.secret"));
        fs::rename(&first_request, &claimed_request).unwrap();
        fs::rename(&first_secret, &claimed_secret).unwrap();
        assert!(
            linux_files::read_optional_pending_job(&paths, &first_id, package_uid, false)
                .unwrap()
                .is_none()
        );
        assert!(
            linux_files::read_optional_pending_job(&paths, &first_id, package_uid, true)
                .unwrap()
                .is_some()
        );
        let claimed_job = linux_files::read_job(&paths, &claimed_request, package_uid).unwrap();
        let parsed_job = parse_job(&claimed_job).unwrap();
        assert_eq!(parsed_job.request_id, first_id);
        let claimed = linux_files::read_claimed_secret(&paths, &first_id, package_uid, true)
            .unwrap()
            .unwrap();
        assert_eq!(&claimed[..], b"queue-secret");
        assert!(!claimed_secret.exists());
        assert!(linux_files::reject_unexpected_secret(&paths, &first_id, package_uid).is_ok());

        let manager_result = parse_manager_result(
            br#"{"schema":"sdsync.dsm-result.v1","ok":true,"message":"secret stored"}"#,
            None,
        )
        .unwrap();
        let response_bytes =
            canonical_queued_response_bytes(&parsed_job, 10_005, &manager_result, false).unwrap();
        let response_path = fixture.responses.join(format!("{first_id}.json"));
        linux_files::write_response(
            &paths,
            &response_path,
            &first_id,
            package_uid,
            &response_bytes,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&response_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            linux_files::read_optional_response(&paths, &first_id, package_uid)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            linux_files::write_response(
                &paths,
                &response_path,
                &first_id,
                package_uid,
                &response_bytes,
            )
            .unwrap_err()
            .kind,
            ErrorKind::Conflict
        );

        let concealed = completed_result_response(
            &paths,
            &first_id,
            &[8_u8; 32],
            1000,
            package_uid,
            10_006,
            RESULT_RETENTION_SECONDS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(concealed.status, 202);
        let completed = completed_result_response(
            &paths,
            &first_id,
            &session,
            1000,
            package_uid,
            10_006,
            RESULT_RETENTION_SECONDS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(completed.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&completed.body).unwrap()["state"],
            "complete"
        );

        let pending = execute_result_action(
            &paths,
            &second_id,
            &session,
            1000,
            package_uid,
            10_002,
            RESULT_RETENTION_SECONDS,
        )
        .unwrap();
        assert_eq!(pending.status, 202);
        let expired_pending = execute_result_action(
            &paths,
            &second_id,
            &session,
            1000,
            package_uid,
            10_001 + MAX_JOB_AGE_SECONDS + 1,
            RESULT_RETENTION_SECONDS,
        )
        .unwrap();
        assert_eq!(expired_pending.status, 410);
        fs::remove_file(&response_path).unwrap();
        let audit_pending_bytes =
            canonical_queued_response_bytes(&parsed_job, 10_005, &manager_result, true).unwrap();
        linux_files::write_response(
            &paths,
            &response_path,
            &first_id,
            package_uid,
            &audit_pending_bytes,
        )
        .unwrap();
        let retained_pending_audit = completed_result_response(
            &paths,
            &first_id,
            &session,
            1000,
            package_uid,
            10_005 + RESULT_RETENTION_SECONDS + 1,
            RESULT_RETENTION_SECONDS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(retained_pending_audit.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&retained_pending_audit.body).unwrap()["audit_pending"],
            true
        );
        assert!(response_path.is_file());

        fs::remove_file(&response_path).unwrap();
        linux_files::write_response(
            &paths,
            &response_path,
            &first_id,
            package_uid,
            &response_bytes,
        )
        .unwrap();
        let expired_response = completed_result_response(
            &paths,
            &first_id,
            &session,
            1000,
            package_uid,
            10_005 + RESULT_RETENTION_SECONDS + 1,
            RESULT_RETENTION_SECONDS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(expired_response.status, 410);
        assert!(!response_path.exists());

        let missing_id = "ffffffffffffffffffffffffffffffffffffffffffffffff";
        let missing = execute_result_action(
            &paths,
            missing_id,
            &session,
            1000,
            package_uid,
            10_000,
            RESULT_RETENTION_SECONDS,
        )
        .unwrap();
        assert_eq!(missing.status, 410);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_queue_rejects_owner_mode_symlink_secret_and_capacity_tampering() {
        let fixture = TestControlFixture::new("tamper");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let other_uid = if package_uid == u32::MAX {
            package_uid - 1
        } else {
            package_uid + 1
        };
        assert_eq!(
            linux_files::load_or_create_csrf_key(&paths, other_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );

        fixture.write_private(&fixture.csrf_key, &[1_u8; 31]);
        assert_eq!(
            linux_files::load_or_create_csrf_key(&paths, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        fs::remove_file(&fixture.csrf_key).unwrap();
        fixture.write_private(&fixture.csrf_key, &[2_u8; 32]);
        fs::set_permissions(&fixture.csrf_key, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            linux_files::load_or_create_csrf_key(&paths, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );

        assert_eq!(
            linux_files::read_optional_response(&paths, REQUEST_ID, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );
        let outside = fixture.root.join("outside-response");
        fixture.write_private(&outside, b"not a response");
        let linked_response = fixture.responses.join(format!("{JOB_ID}.json"));
        symlink(&outside, &linked_response).unwrap();
        assert_eq!(
            linux_files::read_optional_response(&paths, JOB_ID, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        fs::remove_file(&linked_response).unwrap();

        assert_eq!(
            linux_files::read_claimed_secret(&paths, JOB_ID, package_uid, true)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );
        let claimed_secret = fixture.processing.join(format!("{JOB_ID}.secret"));
        fixture.write_private(&claimed_secret, b"missing-newline");
        assert_eq!(
            linux_files::read_claimed_secret(&paths, JOB_ID, package_uid, true)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );
        assert!(!claimed_secret.exists());
        fixture.write_private(&claimed_secret, b"unexpected\n");
        assert_eq!(
            linux_files::read_claimed_secret(&paths, JOB_ID, package_uid, false)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );
        assert!(!claimed_secret.exists());

        let mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let request_fingerprint =
            mutation_request_fingerprint(&[3_u8; 32], &mutation, None).unwrap();
        let unknown_entry = fixture.requests.join("unreviewed-entry");
        fixture.write_private(&unknown_entry, b"");
        assert_eq!(
            linux_files::enqueue(
                &paths,
                EnqueueRequest {
                    package_uid,
                    client_request_id: REQUEST_ID,
                    requested_by: "admin",
                    requested_uid: 1000,
                    session_binding: &[7_u8; 32],
                    audit_transaction: JOB_ID,
                    request_fingerprint: &request_fingerprint,
                    issued_at_epoch: 10_000,
                    mutation: &mutation,
                    secret: None,
                },
                MAX_OUTSTANDING_JOBS,
                |_, _| Ok(()),
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );
        fs::remove_file(&unknown_entry).unwrap();

        let wrong_mode_job = fixture.requests.join(format!("{JOB_ID}.json"));
        fixture.write_private(&wrong_mode_job, b"");
        fs::set_permissions(&wrong_mode_job, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            linux_files::enqueue(
                &paths,
                EnqueueRequest {
                    package_uid,
                    client_request_id: REQUEST_ID,
                    requested_by: "admin",
                    requested_uid: 1000,
                    session_binding: &[7_u8; 32],
                    audit_transaction: JOB_ID,
                    request_fingerprint: &request_fingerprint,
                    issued_at_epoch: 10_000,
                    mutation: &mutation,
                    secret: None,
                },
                MAX_OUTSTANDING_JOBS,
                |_, _| Ok(()),
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );
        fs::remove_file(&wrong_mode_job).unwrap();

        for index in 0..MAX_OUTSTANDING_JOBS {
            let request_id = format!("{index:048x}");
            let client_request_id = format!("{index:032x}");
            let job = canonical_job_bytes(
                &request_id,
                &client_request_id,
                "admin",
                1000,
                &[8_u8; 32],
                JOB_ID,
                &request_fingerprint,
                10_000,
                &mutation,
            )
            .unwrap();
            let path = fixture.requests.join(format!("{request_id}.json"));
            fixture.write_private(&path, &job);
        }
        assert_eq!(
            linux_files::enqueue(
                &paths,
                EnqueueRequest {
                    package_uid,
                    client_request_id: REQUEST_ID,
                    requested_by: "admin",
                    requested_uid: 1000,
                    session_binding: &[7_u8; 32],
                    audit_transaction: JOB_ID,
                    request_fingerprint: &request_fingerprint,
                    issued_at_epoch: 10_000,
                    mutation: &mutation,
                    secret: None,
                },
                MAX_OUTSTANDING_JOBS,
                |_, _| Ok(()),
            )
            .unwrap_err()
            .kind,
            ErrorKind::Conflict
        );

        let response_path = fixture.responses.join(format!("{JOB_ID}.json"));
        assert_eq!(
            linux_files::write_response(&paths, &response_path, JOB_ID, package_uid, b"")
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
        assert_eq!(
            linux_files::write_response(
                &paths,
                &response_path,
                JOB_ID,
                package_uid,
                &vec![0_u8; MAX_MANAGER_OUTPUT_BYTES + 1],
            )
            .unwrap_err()
            .kind,
            ErrorKind::Unavailable
        );

        fs::set_permissions(&fixture.responses, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            linux_files::read_optional_response(&paths, JOB_ID, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enqueue_recovery_removes_a_consumed_secret_staging_link_immediately() {
        let fixture = TestControlFixture::new("consumed-secret-staging");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let stale_staging = fixture.staging.join(format!("{JOB_ID}.secret.tmp"));
        let claimed_secret = fixture.processing.join(format!("{JOB_ID}.secret"));
        fixture.write_private(&stale_staging, b"must-not-linger\n");
        fs::hard_link(&stale_staging, &claimed_secret).unwrap();
        fs::remove_file(&claimed_secret).unwrap();
        assert_eq!(fs::metadata(&stale_staging).unwrap().st_nlink(), 1);

        let mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let request_fingerprint =
            mutation_request_fingerprint(&[3_u8; 32], &mutation, None).unwrap();
        let now = current_epoch().unwrap();
        let outcome = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                package_uid,
                client_request_id: REQUEST_ID,
                requested_by: "admin",
                requested_uid: 1000,
                session_binding: &[7_u8; 32],
                audit_transaction: JOB_ID,
                request_fingerprint: &request_fingerprint,
                issued_at_epoch: now,
                mutation: &mutation,
                secret: None,
            },
            MAX_OUTSTANDING_JOBS,
            |_, _| Ok(()),
        )
        .unwrap();
        assert!(matches!(outcome, EnqueueOutcome::Published { .. }));
        assert!(!stale_staging.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queue_recovery_removes_only_stable_orphan_canonical_secrets() {
        let fixture = TestControlFixture::new("orphan-canonical-secret");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let orphan = fixture.requests.join(format!("{JOB_ID}.secret"));
        fixture.write_private(&orphan, b"orphaned-before-job-publication\n");

        linux_files::recover_orphan_canonical_secrets(&paths, package_uid).unwrap();
        assert!(!orphan.exists());

        let mutation = Mutation::SetSecret(SecretJobArgs {
            profile: "nightly".to_owned(),
            kind: SecretKind::Password,
            mode: SecretMode::Replace,
        });
        let fingerprint =
            mutation_request_fingerprint(&[3_u8; 32], &mutation, Some(b"claimed")).unwrap();
        let job = canonical_job_bytes(
            JOB_ID,
            REQUEST_ID,
            "admin",
            1000,
            &[7_u8; 32],
            "10060f5e12345678fedcba98765432100123456789abcdef",
            &fingerprint,
            current_epoch().unwrap(),
            &mutation,
        )
        .unwrap();
        let active_job = fixture.processing.join(format!("{JOB_ID}.json"));
        let active_secret = fixture.processing.join(format!("{JOB_ID}.secret"));
        fixture.write_private(&active_job, &job);
        fixture.write_private(&active_secret, b"claimed\n");
        linux_files::recover_orphan_canonical_secrets(&paths, package_uid).unwrap();
        assert!(active_secret.is_file());

        fs::remove_file(&active_job).unwrap();
        let hostile_link = fixture.root.join("hostile-secret-link");
        fs::hard_link(&active_secret, &hostile_link).unwrap();
        assert_eq!(
            linux_files::recover_orphan_canonical_secrets(&paths, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        assert!(active_secret.is_file());
        assert!(hostile_link.is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn response_audit_rewrite_temp_does_not_break_concurrent_queue_scans() {
        let fixture = TestControlFixture::new("response-audit-rewrite");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let temporary = fixture
            .responses
            .join(format!(".{JOB_ID}.audit-reconciled.tmp"));
        fixture.write_private(&temporary, br#"{"audit_pending":false}"#);

        assert!(
            linux_files::collect_json_job_ids(&paths, package_uid)
                .unwrap()
                .is_empty()
        );
        // The legacy-temp recovery pass runs immediately before the stable
        // idempotency scan and must tolerate the same live private name.
        linux_files::recover_legacy_queue_temps(&paths, package_uid).unwrap();
        assert!(temporary.is_file());

        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            linux_files::collect_json_job_ids(&paths, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
        assert_eq!(
            linux_files::recover_legacy_queue_temps(&paths, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staging_recovery_accepts_only_one_exact_published_hard_link() {
        let fixture = TestControlFixture::new("published-staging-links");
        let package_uid = TestControlFixture::package_uid();
        let cases = [
            (
                format!("{JOB_ID}.job.tmp"),
                fixture.requests.join(format!("{JOB_ID}.json")),
                MAX_JOB_BYTES,
            ),
            (
                format!("{JOB_ID}.secret.tmp"),
                fixture.processing.join(format!("{JOB_ID}.secret")),
                MAX_CONNECTION_SECRET_BYTES + 1,
            ),
            (
                format!("{JOB_ID}.response.tmp"),
                fixture.responses.join(format!("{JOB_ID}.json")),
                MAX_MANAGER_OUTPUT_BYTES,
            ),
        ];
        for (name, companion, maximum) in cases {
            let staging = fixture.staging.join(name);
            fixture.write_private(&staging, b"private-staging\n");
            fs::hard_link(&staging, &companion).unwrap();
            let metadata = linux_files::private_file_metadata_with_companion(
                &staging,
                package_uid,
                maximum,
                std::slice::from_ref(&companion),
            )
            .unwrap();
            assert_eq!(metadata.st_nlink(), 2);
            fs::remove_file(&staging).unwrap();
            assert_eq!(fs::metadata(&companion).unwrap().st_nlink(), 1);
            fs::remove_file(companion).unwrap();
        }

        let hostile_staging = fixture.staging.join(format!("{JOB_ID}.job.tmp"));
        let expected = fixture.requests.join(format!("{JOB_ID}.json"));
        let unrelated = fixture.root.join("unrelated-hard-link");
        fixture.write_private(&hostile_staging, b"hostile\n");
        fs::hard_link(&hostile_staging, &unrelated).unwrap();
        assert_eq!(
            linux_files::private_file_metadata_with_companion(
                &hostile_staging,
                package_uid,
                MAX_JOB_BYTES,
                std::slice::from_ref(&expected),
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );
        fs::remove_file(&unrelated).unwrap();
        fs::hard_link(&hostile_staging, &expected).unwrap();
        let extra = fixture.root.join("third-hard-link");
        fs::hard_link(&hostile_staging, &extra).unwrap();
        assert_eq!(
            linux_files::private_file_metadata_with_companion(
                &hostile_staging,
                package_uid,
                MAX_JOB_BYTES,
                &[expected],
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unpublished_bridge_job_can_terminalize_failed_while_publisher_is_live() {
        let fixture = TestControlFixture::new("publishing-terminal-failure");
        let control_paths = fixture.paths();
        let paths = control_paths.audit_outbox();
        let package_uid = TestControlFixture::package_uid();
        let (owner_pid, owner_start, owner_boot) = linux_files::current_process_identity().unwrap();
        let record = AuditOutboxRecord {
            schema: "sdsync.dsm-audit-outbox.v1".to_owned(),
            transaction: format!("bridge-{JOB_ID}"),
            operation: "set-password".to_owned(),
            profile: "archive".to_owned(),
            actor: "admin".to_owned(),
            actor_uid: package_uid.max(1),
            origin: "bridge".to_owned(),
            client_request_id: Some(REQUEST_ID.to_owned()),
            job_id: Some(JOB_ID.to_owned()),
            owner_pid,
            owner_start,
            owner_boot,
            phase: AuditOutboxPhase::Prepared,
        };
        let states = std::cell::RefCell::new(Vec::new());
        linux_files::audit_transaction_begin(
            &paths,
            package_uid,
            record,
            AuditOutboxPhase::Publishing,
            |_, state| {
                states.borrow_mut().push(state.to_owned());
                Ok(())
            },
        )
        .unwrap();
        let pending = fixture
            .audit_outbox_directory
            .join(format!("bridge-{JOB_ID}.event"));
        assert!(pending.is_file());
        assert!(
            !linux_files::audit_transaction_complete(
                &paths,
                package_uid,
                &format!("bridge-{JOB_ID}"),
                AuditOutboxPhase::Failed,
                |_, state| {
                    states.borrow_mut().push(state.to_owned());
                    Ok(())
                },
            )
            .unwrap()
        );
        assert!(!pending.exists());
        assert_eq!(states.into_inner(), ["requested", "requested", "failed"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enqueue_terminalizes_failed_when_requested_audit_cannot_advance_to_publishing() {
        let fixture = TestControlFixture::new("enqueue-audit-ready-write-failure");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let session = [7_u8; 32];
        let mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let fingerprint = mutation_request_fingerprint(&[9_u8; 32], &mutation, None).unwrap();
        let states = std::cell::RefCell::new(Vec::new());
        linux_files::fail_next_audit_ready_write();
        let error = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                package_uid,
                client_request_id: REQUEST_ID,
                requested_by: "admin",
                requested_uid: package_uid.max(1),
                session_binding: &session,
                audit_transaction: JOB_ID,
                request_fingerprint: &fingerprint,
                issued_at_epoch: current_epoch().unwrap(),
                mutation: &mutation,
                secret: None,
            },
            1,
            |_, state| {
                states.borrow_mut().push(state.to_owned());
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::UnsafeRuntime);
        assert_eq!(states.into_inner(), ["requested", "requested", "failed"]);
        for directory in [
            &fixture.requests,
            &fixture.processing,
            &fixture.responses,
            &fixture.staging,
            &fixture.audit_outbox_directory,
        ] {
            assert_eq!(fs::read_dir(directory).unwrap().count(), 0, "{directory:?}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interactive_connection_budgets_are_operation_specific_and_bounded() {
        let connection = ConnectionJobArgs {
            profile: Some("nightly".to_owned()),
            url: "https://nas.example.invalid".to_owned(),
            username: "backup-user".to_owned(),
            allow_http: false,
            danger_accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout_seconds: 15,
            timeout_seconds: 120,
            retries: 5,
            password_source: CredentialSource::Stored,
            totp_source: CredentialSource::None,
        };
        let authentication = Mutation::TestProfileAuth(connection.clone());
        let browsing = Mutation::BrowseRemote(BrowseRemoteJobArgs {
            connection,
            parent: "/".to_owned(),
            connection_proof: format!("v1.10300.{}.{}", "b".repeat(64), "c".repeat(64)),
        });
        let serialized = Mutation::RemoveProfile(NameArgs {
            name: "nightly".to_owned(),
        });

        assert_eq!(
            interactive_connection_budget(&authentication),
            Some(InteractiveConnectionBudget {
                probe: Duration::from_secs(12),
                logout: Duration::from_secs(3),
            })
        );
        assert_eq!(
            interactive_connection_budget(&browsing),
            Some(InteractiveConnectionBudget {
                probe: Duration::from_secs(27),
                logout: Duration::from_secs(3),
            })
        );
        assert_eq!(interactive_connection_budget(&serialized), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn remote_connection_mutations_publish_authentication_audit_before_enqueue() {
        let fixture = TestControlFixture::new("remote-connection-authentication-audit");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let session = [7_u8; 32];
        let connection = ConnectionJobArgs {
            profile: Some("nightly".to_owned()),
            url: "https://nas.example.invalid".to_owned(),
            username: "backup-user".to_owned(),
            allow_http: false,
            danger_accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout_seconds: 15,
            timeout_seconds: 120,
            retries: 2,
            password_source: CredentialSource::Stored,
            totp_source: CredentialSource::None,
        };
        let proof = format!("v1.10300.{}.{}", "b".repeat(64), "c".repeat(64));
        let mutations = [
            Mutation::TestProfileAuth(connection.clone()),
            Mutation::BrowseRemote(BrowseRemoteJobArgs {
                connection,
                parent: "/".to_owned(),
                connection_proof: proof,
            }),
        ];

        for (index, mutation) in mutations.iter().enumerate() {
            let client_request_id = format!("{:032x}", index + 1);
            let audit_transaction = format!("{:048x}", index + 1);
            let fingerprint = mutation_request_fingerprint(&[9_u8; 32], mutation, None).unwrap();
            let expected_operation = mutation.operation_id();
            let outcome = linux_files::enqueue(
                &paths,
                EnqueueRequest {
                    package_uid,
                    client_request_id: &client_request_id,
                    requested_by: "admin",
                    requested_uid: package_uid.max(1),
                    session_binding: &session,
                    audit_transaction: &audit_transaction,
                    request_fingerprint: &fingerprint,
                    issued_at_epoch: current_epoch().unwrap(),
                    mutation,
                    secret: None,
                },
                mutations.len(),
                |record, state| {
                    assert_eq!(record.operation, expected_operation);
                    assert_eq!(state, "requested");
                    let audit_line = serde_json::to_vec(&json!({
                        "epoch": 10_000,
                        "level": "info",
                        "configured_level": "info",
                        "subject_level": "warn",
                        "mandatory": true,
                        "category": "audit",
                        "subject_category": "authentication",
                        "operation": record.operation,
                        "state": state,
                        "transaction": record.transaction,
                        "origin": record.origin,
                        "actor": record.actor,
                        "actor_uid": record.actor_uid,
                        "profile": record.profile,
                        "client_request_id": record.client_request_id,
                    }))
                    .unwrap();
                    linux_files::validate_audit_log_line(&audit_line).map(|_| ())
                },
            )
            .unwrap();
            assert!(matches!(outcome, EnqueueOutcome::Published { .. }));
        }

        assert_eq!(fs::read_dir(&fixture.requests).unwrap().count(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lost_202_replay_returns_original_job_and_keeps_audit_client_correlation() {
        let fixture = TestControlFixture::new("idempotent-replay");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let session = [7_u8; 32];
        let mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let fingerprint = mutation_request_fingerprint(&[9_u8; 32], &mutation, None).unwrap();
        let audit_calls = std::cell::Cell::new(0_u32);
        let audit_events = std::cell::RefCell::new(Vec::new());
        let enqueue_request = || EnqueueRequest {
            package_uid,
            client_request_id: REQUEST_ID,
            requested_by: "admin",
            requested_uid: 1000,
            session_binding: &session,
            audit_transaction: JOB_ID,
            request_fingerprint: &fingerprint,
            issued_at_epoch: current_epoch().unwrap(),
            mutation: &mutation,
            secret: None,
        };
        let first = linux_files::enqueue(&paths, enqueue_request(), 1, |record, state| {
            audit_calls.set(audit_calls.get() + 1);
            audit_events
                .borrow_mut()
                .push((state.to_owned(), record.clone()));
            Ok(())
        })
        .unwrap();
        let first_id = first.job_id().to_owned();
        assert_eq!(audit_calls.get(), 1);
        assert_eq!(
            audit_events.borrow()[0].1.client_request_id.as_deref(),
            Some(REQUEST_ID)
        );
        assert_eq!(
            linux_files::find_session_request(
                &paths,
                package_uid,
                REQUEST_ID,
                "admin",
                1000,
                &session,
            )
            .unwrap(),
            Some(SessionRequestStatus::Pending {
                job_id: first_id.clone(),
                operation: "remove-profile".to_owned(),
            })
        );
        let pending_status =
            execute_request_status_action(&paths, REQUEST_ID, "admin", &session, 1000, package_uid)
                .unwrap();
        assert_eq!(pending_status.status, 202);
        let pending_status: Value = serde_json::from_slice(&pending_status.body).unwrap();
        assert_eq!(pending_status["schema"], "sdsync.dsm-request-status.v1");
        assert_eq!(pending_status["request_id"], REQUEST_ID);
        assert_eq!(pending_status["job_id"], first_id);
        assert_eq!(pending_status["operation"], "remove-profile");
        assert_eq!(pending_status["state"], "pending");

        let replay = linux_files::enqueue(&paths, enqueue_request(), 1, |record, state| {
            audit_calls.set(audit_calls.get() + 1);
            audit_events
                .borrow_mut()
                .push((state.to_owned(), record.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!(replay, EnqueueOutcome::Existing(first_id.clone()));
        assert_eq!(audit_calls.get(), 1);

        let conflicting_mutation = Mutation::SetDefault(NameArgs {
            name: "archive".to_owned(),
        });
        let conflicting_fingerprint =
            mutation_request_fingerprint(&[9_u8; 32], &conflicting_mutation, None).unwrap();
        let conflict = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                request_fingerprint: &conflicting_fingerprint,
                mutation: &conflicting_mutation,
                ..enqueue_request()
            },
            1,
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert_eq!(conflict.kind, ErrorKind::Conflict);

        let queued_path = fixture.requests.join(format!("{first_id}.json"));
        let processing_path = fixture.processing.join(format!("{first_id}.json"));
        fs::rename(&queued_path, &processing_path).unwrap();
        assert_eq!(
            linux_files::find_session_request(
                &paths,
                package_uid,
                REQUEST_ID,
                "admin",
                1000,
                &session,
            )
            .unwrap(),
            Some(SessionRequestStatus::Pending {
                job_id: first_id.clone(),
                operation: "remove-profile".to_owned(),
            })
        );
        let job = parse_job(&fs::read(&processing_path).unwrap()).unwrap();
        let result = json!({
            "schema": "sdsync.dsm-result.v1",
            "ok": true,
            "message": "completed",
        });
        let response_bytes = canonical_queued_response_bytes(
            &job,
            job.issued_at_epoch.saturating_add(1),
            &result,
            false,
        )
        .unwrap();
        let response_path = fixture.responses.join(format!("{first_id}.json"));
        linux_files::write_response(
            &paths,
            &response_path,
            &first_id,
            package_uid,
            &response_bytes,
        )
        .unwrap();
        fs::remove_file(&processing_path).unwrap();

        assert_eq!(
            linux_files::find_session_request(
                &paths,
                package_uid,
                REQUEST_ID,
                "admin",
                1000,
                &session,
            )
            .unwrap(),
            Some(SessionRequestStatus::Complete {
                job_id: first_id.clone(),
                operation: "remove-profile".to_owned(),
            })
        );
        let complete_status =
            execute_request_status_action(&paths, REQUEST_ID, "admin", &session, 1000, package_uid)
                .unwrap();
        assert_eq!(complete_status.status, 200);
        let complete_status: Value = serde_json::from_slice(&complete_status.body).unwrap();
        assert_eq!(complete_status["request_id"], REQUEST_ID);
        assert_eq!(complete_status["job_id"], first_id);
        assert_eq!(complete_status["operation"], "remove-profile");
        assert_eq!(complete_status["state"], "complete");

        let completed_replay =
            linux_files::enqueue(&paths, enqueue_request(), 1, |record, state| {
                audit_calls.set(audit_calls.get() + 1);
                audit_events
                    .borrow_mut()
                    .push((state.to_owned(), record.clone()));
                Ok(())
            })
            .unwrap();
        assert_eq!(completed_replay, EnqueueOutcome::Existing(first_id.clone()));
        // Reconciliation replays the exact requested/terminal pair through an
        // idempotent sink before the completed idempotency lookup. It never
        // creates a second job or a second audit transaction.
        assert_eq!(audit_calls.get(), 3);
        assert!(audit_events.borrow().iter().all(|(_, record)| {
            record.client_request_id.as_deref() == Some(REQUEST_ID)
                && record.job_id.as_deref() == Some(first_id.as_str())
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn request_status_conceals_missing_and_foreign_session_records_identically() {
        assert_eq!(
            request_status_found_response(
                REQUEST_ID,
                JOB_ID,
                "untrusted-operation",
                "pending",
                false,
            )
            .unwrap_err()
            .kind,
            ErrorKind::Unavailable,
            "request-status must not echo an unvalidated operation"
        );

        let fixture = TestControlFixture::new("request-status-concealment");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let current_session = [7_u8; 32];
        let foreign_session = [8_u8; 32];

        let missing = execute_request_status_action(
            &paths,
            REQUEST_ID,
            "admin",
            &current_session,
            1000,
            package_uid,
        )
        .unwrap();
        assert_eq!(missing.status, 202);

        let mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let fingerprint = mutation_request_fingerprint(&[9_u8; 32], &mutation, None).unwrap();
        let foreign = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                package_uid,
                client_request_id: REQUEST_ID,
                requested_by: "other-admin",
                requested_uid: 2000,
                session_binding: &foreign_session,
                audit_transaction: JOB_ID,
                request_fingerprint: &fingerprint,
                issued_at_epoch: current_epoch().unwrap(),
                mutation: &mutation,
                secret: None,
            },
            1,
            |_, _| Ok(()),
        )
        .unwrap();

        let concealed = execute_request_status_action(
            &paths,
            REQUEST_ID,
            "admin",
            &current_session,
            1000,
            package_uid,
        )
        .unwrap();
        assert_eq!(concealed.status, missing.status);
        assert_eq!(concealed.body, missing.body);
        let unresolved: Value = serde_json::from_slice(&concealed.body).unwrap();
        assert_eq!(unresolved["schema"], "sdsync.dsm-request-status.v1");
        assert_eq!(unresolved["request_id"], REQUEST_ID);
        assert_eq!(unresolved["state"], "unresolved");
        assert!(unresolved.get("job_id").is_none());
        assert!(unresolved.get("operation").is_none());

        let owned = execute_request_status_action(
            &paths,
            REQUEST_ID,
            "other-admin",
            &foreign_session,
            2000,
            package_uid,
        )
        .unwrap();
        let owned: Value = serde_json::from_slice(&owned.body).unwrap();
        assert_eq!(owned["job_id"], foreign.job_id());
        assert_eq!(owned["operation"], "remove-profile");
        assert_eq!(owned["state"], "pending");

        let current_job_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let current_audit_transaction = "cccccccccccccccccccccccccccccccccccccccccccccccc";
        let current_job = canonical_job_bytes(
            current_job_id,
            REQUEST_ID,
            "admin",
            1000,
            &current_session,
            current_audit_transaction,
            &fingerprint,
            current_epoch().unwrap(),
            &mutation,
        )
        .unwrap();
        fixture.write_private(
            &fixture.requests.join(format!("{current_job_id}.json")),
            &current_job,
        );
        assert_eq!(
            linux_files::find_session_request(
                &paths,
                package_uid,
                REQUEST_ID,
                "admin",
                1000,
                &current_session,
            )
            .unwrap(),
            Some(SessionRequestStatus::Pending {
                job_id: current_job_id.to_owned(),
                operation: "remove-profile".to_owned(),
            }),
            "a foreign-session collision must not hide the current session's mapping"
        );

        let duplicate_job_id = "dddddddddddddddddddddddddddddddddddddddddddddddd";
        let duplicate_audit_transaction = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let duplicate_job = canonical_job_bytes(
            duplicate_job_id,
            REQUEST_ID,
            "admin",
            1000,
            &current_session,
            duplicate_audit_transaction,
            &fingerprint,
            current_epoch().unwrap(),
            &mutation,
        )
        .unwrap();
        fixture.write_private(
            &fixture.requests.join(format!("{duplicate_job_id}.json")),
            &duplicate_job,
        );
        assert_eq!(
            linux_files::find_session_request(
                &paths,
                package_uid,
                REQUEST_ID,
                "admin",
                1000,
                &current_session,
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime,
            "two owned mappings for one client request ID must fail closed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enqueue_sequence_recovers_private_partial_state_from_published_maximum() {
        let fixture = TestControlFixture::new("sequence-recovery");
        let paths = fixture.paths();
        let package_uid = TestControlFixture::package_uid();
        let mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let fingerprint = mutation_request_fingerprint(&[4_u8; 32], &mutation, None).unwrap();
        let published_prefix = 0xffff_ffff_ffff_ff00_u64;
        let published_id = format!("{published_prefix:016x}{}", "1".repeat(32));
        let published_job = canonical_job_bytes(
            &published_id,
            REQUEST_ID,
            "admin",
            1000,
            &[7_u8; 32],
            JOB_ID,
            &fingerprint,
            current_epoch().unwrap(),
            &mutation,
        )
        .unwrap();
        fixture.write_private(
            &fixture.requests.join(format!("{published_id}.json")),
            &published_job,
        );
        fixture.write_private(&fixture.enqueue_sequence, b"partial");

        let next_id = linux_files::next_job_id(&paths, package_uid).unwrap();
        assert_eq!(
            u64::from_str_radix(&next_id[..16], 16).unwrap(),
            published_prefix + 1
        );
        assert_eq!(
            fs::read_to_string(&fixture.enqueue_sequence).unwrap(),
            format!("{:016x}", published_prefix + 1)
        );
    }

    #[test]
    fn get_query_parsing_accepts_only_allowlisted_reads() {
        let request = validate_http_request(environment(
            "GET",
            "action=logs&lines=200&source=controller&SynoToken=abc123",
        ))
        .unwrap();
        assert!(matches!(
            request,
            ValidatedHttpRequest::Get {
                action: ReadAction::Logs {
                    lines: 200,
                    source: LogSource::Controller
                },
                ..
            }
        ));

        let api_logs = validate_http_request(environment(
            "GET",
            "action=logs&lines=10&source=api&SynoToken=abc123",
        ))
        .unwrap();
        assert!(matches!(
            api_logs,
            ValidatedHttpRequest::Get {
                action: ReadAction::Logs {
                    lines: 10,
                    source: LogSource::Api
                },
                ..
            }
        ));

        let result = validate_http_request(environment(
            "GET",
            &format!("action=result&job_id={JOB_ID}&SynoToken=abc123"),
        ))
        .unwrap();
        assert!(matches!(
            result,
            ValidatedHttpRequest::Get {
                action: ReadAction::Result { job_id },
                ..
            } if job_id == JOB_ID
        ));

        let request_status = validate_http_request(environment(
            "GET",
            &format!("action=request-status&request_id={REQUEST_ID}&SynoToken=abc123"),
        ))
        .unwrap();
        assert!(matches!(
            request_status,
            ValidatedHttpRequest::Get {
                action: ReadAction::RequestStatus { request_id },
                ..
            } if request_id == REQUEST_ID
        ));

        assert!(validate_http_request(environment("GET", "action=unknown&SynoToken=x")).is_err());
        assert!(
            validate_http_request(environment("GET", "action=snapshot&extra=x&SynoToken=x"))
                .is_err()
        );
        assert!(
            validate_http_request(environment("GET", "action=logs&lines=1001&SynoToken=x"))
                .is_err()
        );
        assert!(
            validate_http_request(environment(
                "GET",
                &format!("action=result&job_id={REQUEST_ID}&SynoToken=x")
            ))
            .is_err()
        );
        assert!(
            validate_http_request(environment(
                "GET",
                &format!(
                    "action=result&job_id={}&SynoToken=x",
                    JOB_ID.to_ascii_uppercase()
                )
            ))
            .is_err()
        );
        for query in [
            "action=request-status",
            &format!("action=request-status&request_id={JOB_ID}"),
            &format!(
                "action=request-status&request_id={}",
                REQUEST_ID.to_ascii_uppercase()
            ),
            &format!("action=request-status&request_id={REQUEST_ID}&extra=x"),
        ] {
            assert!(
                validate_http_request(environment("GET", &format!("{query}&SynoToken=x"))).is_err()
            );
        }
    }

    #[test]
    fn read_manager_log_arguments_forward_the_fixed_source_and_bound_scan_lines() {
        assert_eq!(
            read_manager_arguments(&ReadAction::Logs {
                lines: 10,
                source: LogSource::Api,
            })
            .unwrap(),
            ["api", "logs", "--lines", "160", "--source", "api"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            read_manager_arguments(&ReadAction::Logs {
                lines: 1000,
                source: LogSource::All,
            })
            .unwrap(),
            ["api", "logs", "--lines", "1000", "--source", "all"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn synology_token_is_optional_but_present_sources_remain_strict() {
        let cookie_only = validate_http_request(environment("GET", "action=snapshot")).unwrap();
        assert!(matches!(
            cookie_only,
            ValidatedHttpRequest::Get {
                authentication: AuthenticationInputs {
                    synology_token: None,
                    ..
                },
                ..
            }
        ));

        let mut matching = environment("GET", "action=snapshot&SynoToken=token");
        matching.synology_token_header = Some(Zeroizing::new("token".to_owned()));
        assert!(validate_http_request(matching).is_ok());

        let mut mismatch = environment("GET", "action=snapshot&SynoToken=token-a");
        mismatch.synology_token_header = Some(Zeroizing::new("token-b".to_owned()));
        assert_eq!(
            validate_http_request(mismatch).unwrap_err().kind,
            ErrorKind::Forbidden
        );

        for invalid in ["", "contains space", "line\nbreak"] {
            let mut request = environment("GET", "action=snapshot");
            request.synology_token_header = Some(Zeroizing::new(invalid.to_owned()));
            assert_eq!(
                validate_http_request(request).unwrap_err().kind,
                ErrorKind::Forbidden
            );
        }
        assert_eq!(
            validate_http_request(environment("GET", "action=snapshot&SynoToken="))
                .unwrap_err()
                .kind,
            ErrorKind::Forbidden
        );
    }

    #[test]
    fn browser_request_marker_is_required_exact_and_revalidated_after_relay() {
        for marker in [None, Some(""), Some("0"), Some("true"), Some("1 ")] {
            let mut request = environment("GET", "action=snapshot");
            request.request_marker = marker.map(str::to_owned);
            assert_eq!(
                validate_http_request(request).unwrap_err().kind,
                ErrorKind::Forbidden
            );
        }

        let environment = environment("GET", "action=snapshot");
        let encoded = encode_relay_request(&environment, None, &authenticated_session()).unwrap();
        let mut relay = decode_relay_request(&encoded).unwrap();
        relay.request_marker = Some("0".to_owned());
        assert_eq!(
            validate_relay_http_request(&relay).unwrap_err().kind,
            ErrorKind::Forbidden
        );
    }

    #[test]
    fn methods_content_type_and_transfer_encoding_are_strict() {
        assert_eq!(
            validate_http_request(environment("PUT", "SynoToken=x"))
                .unwrap_err()
                .kind,
            ErrorKind::MethodNotAllowed
        );
        let mut get_with_body = environment("GET", "action=snapshot&SynoToken=x");
        get_with_body.content_length = Some("1".to_owned());
        assert!(validate_http_request(get_with_body).is_err());

        let mut dsm_empty_get_metadata = environment("GET", "action=csrf");
        dsm_empty_get_metadata.content_length = Some(String::new());
        dsm_empty_get_metadata.content_type = Some(String::new());
        dsm_empty_get_metadata.csrf_header = Some(Zeroizing::new(String::new()));
        dsm_empty_get_metadata.transfer_encoding = Some(String::new());
        assert!(matches!(
            validate_http_request(dsm_empty_get_metadata),
            Ok(ValidatedHttpRequest::Get {
                action: ReadAction::Csrf,
                ..
            })
        ));

        let mut dsm_zero_length_get = environment("GET", "action=csrf");
        dsm_zero_length_get.content_length = Some("0".to_owned());
        assert!(matches!(
            validate_http_request(dsm_zero_length_get),
            Ok(ValidatedHttpRequest::Get {
                action: ReadAction::Csrf,
                ..
            })
        ));

        let mut get_with_content_type = environment("GET", "action=csrf");
        get_with_content_type.content_type = Some("application/json".to_owned());
        assert_eq!(
            validate_http_request(get_with_content_type)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );

        let mut get_with_transfer_encoding = environment("GET", "action=csrf");
        get_with_transfer_encoding.transfer_encoding = Some("chunked".to_owned());
        assert_eq!(
            validate_http_request(get_with_transfer_encoding)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );

        let mut get_with_csrf_header = environment("GET", "action=csrf");
        get_with_csrf_header.csrf_header = Some(Zeroizing::new("mutation-token".to_owned()));
        assert_eq!(
            validate_http_request(get_with_csrf_header)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );

        let mut get_without_cookie = environment("GET", "action=csrf");
        get_without_cookie.cookie = Zeroizing::new(String::new());
        assert_eq!(
            validate_http_request(get_without_cookie).unwrap_err().kind,
            ErrorKind::Unauthorized
        );

        let mut post = post_environment(10);
        post.content_type = Some("text/plain".to_owned());
        assert_eq!(
            validate_http_request(post).unwrap_err().kind,
            ErrorKind::UnsupportedMediaType
        );

        let mut post_without_length = post_environment(10);
        post_without_length.content_length = Some(String::new());
        assert_eq!(
            validate_http_request(post_without_length).unwrap_err().kind,
            ErrorKind::BadRequest
        );

        let mut post_without_type = post_environment(10);
        post_without_type.content_type = Some(String::new());
        assert_eq!(
            validate_http_request(post_without_type).unwrap_err().kind,
            ErrorKind::UnsupportedMediaType
        );

        let mut post_without_csrf = post_environment(10);
        post_without_csrf.csrf_header = Some(Zeroizing::new(String::new()));
        assert_eq!(
            validate_http_request(post_without_csrf).unwrap_err().kind,
            ErrorKind::Forbidden
        );

        let mut chunked = post_environment(10);
        chunked.transfer_encoding = Some("chunked".to_owned());
        assert!(validate_http_request(chunked).is_err());
    }

    #[test]
    fn content_length_is_canonical_and_bounded() {
        assert_eq!(parse_content_length(Some("1")).unwrap(), 1);
        for invalid in [
            None,
            Some(""),
            Some("0"),
            Some("01"),
            Some("-1"),
            Some("1x"),
        ] {
            assert!(parse_content_length(invalid).is_err());
        }
        assert_eq!(
            parse_content_length(Some(&(MAX_POST_BODY_BYTES + 1).to_string()))
                .unwrap_err()
                .kind,
            ErrorKind::PayloadTooLarge
        );
    }

    #[test]
    fn body_reader_rejects_short_long_and_oversized_input() {
        assert_eq!(
            read_exact_body(&mut Cursor::new(b"abcd"), 4)
                .unwrap()
                .as_slice(),
            b"abcd"
        );
        assert!(read_exact_body(&mut Cursor::new(b"abc"), 4).is_err());
        assert!(read_exact_body(&mut Cursor::new(b"abcde"), 4).is_err());
        assert_eq!(
            read_exact_body(&mut Cursor::new(Vec::<u8>::new()), MAX_POST_BODY_BYTES + 1)
                .unwrap_err()
                .kind,
            ErrorKind::PayloadTooLarge
        );
    }

    #[test]
    fn relay_framing_is_length_prefixed_bounded_and_single_message() {
        let payload = br#"{"schema":"sdsync.test.v1","secret":"socket-only"}"#;
        let mut framed = Cursor::new(Vec::new());
        write_frame(&mut framed, payload, payload.len()).unwrap();
        assert_eq!(
            &framed.get_ref()[..4],
            &(payload.len() as u32).to_be_bytes()
        );
        framed.set_position(0);
        assert_eq!(
            read_single_frame(&mut framed, payload.len(), ErrorKind::BadRequest)
                .unwrap()
                .as_slice(),
            payload
        );

        for malformed in [
            vec![0, 0, 0],
            vec![0, 0, 0, 4, b'a', b'b', b'c'],
            vec![0, 0, 0, 1, b'a', b'b'],
            vec![0, 0, 0, 0],
        ] {
            assert!(
                read_single_frame(
                    &mut Cursor::new(malformed),
                    MAX_RELAY_REQUEST_BYTES,
                    ErrorKind::BadRequest,
                )
                .is_err()
            );
        }
        let oversized = ((MAX_RELAY_REQUEST_BYTES + 1) as u32)
            .to_be_bytes()
            .to_vec();
        assert_eq!(
            read_single_frame(
                &mut Cursor::new(oversized),
                MAX_RELAY_REQUEST_BYTES,
                ErrorKind::BadRequest,
            )
            .unwrap_err()
            .kind,
            ErrorKind::PayloadTooLarge
        );
        assert!(
            write_frame(
                &mut Cursor::new(Vec::new()),
                &vec![0_u8; MAX_RELAY_REQUEST_BYTES + 1],
                MAX_RELAY_REQUEST_BYTES,
            )
            .is_err()
        );

        let response = CgiResponse::accepted(br#"{"ok":true}"#.to_vec());
        let encoded = encode_relay_response(&response).unwrap();
        let decoded = decode_relay_response(&encoded).unwrap();
        assert_eq!(decoded.status, 202);
        assert_eq!(decoded.body, br#"{"ok":true}"#);
        let mut invalid_status = encoded.to_vec();
        invalid_status[..2].copy_from_slice(&201_u16.to_be_bytes());
        assert!(decode_relay_response(&invalid_status).is_err());
    }

    #[test]
    fn relay_reconstructs_and_revalidates_raw_http_fields_and_secret_body() {
        let secret = "socket-secret-value";
        let body = request(
            "set-secret",
            json!({"profile":"nightly","kind":"password","mode":"replace","value":secret}),
        );
        let environment = post_environment(body.len());
        let encoded =
            encode_relay_request(&environment, Some(&body), &authenticated_session()).unwrap();
        assert!(contains_bytes(&encoded, secret.as_bytes()));
        let relay = decode_relay_request(&encoded).unwrap();
        let debug = format!("{relay:?}");
        for sensitive in [secret, "authenticated-session", "dsm-token", "csrf-token"] {
            assert!(!debug.contains(sensitive));
        }
        let (validated, relayed_body) = validate_relay_http_request(&relay).unwrap();
        assert!(matches!(validated, ValidatedHttpRequest::Post { .. }));
        assert_eq!(relayed_body, Some(body.as_slice()));
        let parsed = parse_mutation_request(relayed_body.unwrap()).unwrap();
        assert_eq!(
            parsed.secret.as_ref().map(|value| value.as_slice()),
            Some(secret.as_bytes())
        );

        assert_eq!(
            encode_relay_request(
                &post_environment(1),
                Some(&[0xff]),
                &authenticated_session(),
            )
            .unwrap_err()
            .kind,
            ErrorKind::BadRequest
        );

        let mut oversized_field = decode_relay_request(&encoded).unwrap();
        oversized_field.cookie = "x".repeat(MAX_COOKIE_BYTES + 1);
        assert_eq!(
            oversized_field.validate_fields().unwrap_err().kind,
            ErrorKind::BadRequest
        );
        for control in ['\n', '\t', '\u{001b}', '\u{007f}'] {
            let mut control_field = decode_relay_request(&encoded).unwrap();
            control_field.server_name = Some(format!("nas{control}forged"));
            assert_eq!(
                control_field.validate_fields().unwrap_err().kind,
                ErrorKind::BadRequest
            );
        }
        let mut length_mismatch = decode_relay_request(&encoded).unwrap();
        length_mismatch.content_length = Some((body.len() + 1).to_string());
        assert_eq!(
            validate_relay_http_request(&length_mismatch)
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );

        let mut unknown = serde_json::from_slice::<Value>(&encoded).unwrap();
        unknown["unreviewed"] = json!(true);
        let unknown = Zeroizing::new(serde_json::to_vec(&unknown).unwrap());
        assert_eq!(
            decode_relay_request(&unknown).unwrap_err().kind,
            ErrorKind::BadRequest
        );
    }

    #[test]
    fn relayed_assertion_must_match_independent_uid_and_recomputed_binding() {
        const AUTHENTICATED_UID: u32 = 2000;
        let query_environment = environment("GET", "action=snapshot&SynoToken=dsm-token");
        let session = bound_authenticated_session(&query_environment, AUTHENTICATED_UID);
        let encoded = encode_relay_request(&query_environment, None, &session).unwrap();
        let relay = decode_relay_request(&encoded).unwrap();
        let validate = |relay: &RelayRequest| {
            let (request, _) = validate_relay_http_request(relay)?;
            let authentication = match &request {
                ValidatedHttpRequest::Get { authentication, .. }
                | ValidatedHttpRequest::Post { authentication, .. } => authentication,
            };
            validate_relay_authenticated_session(relay, authentication, AUTHENTICATED_UID)
        };
        let validated = validate(&relay).unwrap();
        assert_eq!(validated.username, "admin");
        assert_eq!(validated.uid, AUTHENTICATED_UID);

        let mut header_environment = environment("GET", "action=snapshot");
        header_environment.synology_token_header = Some(Zeroizing::new("header-token".to_owned()));
        let header_session = bound_authenticated_session(&header_environment, AUTHENTICATED_UID);
        let header_encoded =
            encode_relay_request(&header_environment, None, &header_session).unwrap();
        let header_relay = decode_relay_request(&header_encoded).unwrap();
        let validated_header = validate(&header_relay).unwrap();
        assert_eq!(validated_header.username, "admin");
        assert_eq!(validated_header.uid, AUTHENTICATED_UID);

        let mut swapped_username = decode_relay_request(&encoded).unwrap();
        swapped_username.authenticated_username = "other-admin".to_owned();
        assert_eq!(
            validate(&swapped_username).err().unwrap().kind,
            ErrorKind::Unauthorized
        );

        let mut forged_uid = decode_relay_request(&encoded).unwrap();
        forged_uid.authenticated_uid = AUTHENTICATED_UID + 1;
        forged_uid.session_binding = hex_encode(
            &session_binding(
                &forged_uid.authenticated_username,
                AUTHENTICATED_UID + 1,
                &query_environment.cookie,
                Some("dsm-token"),
            )
            .unwrap(),
        );
        assert_eq!(
            validate(&forged_uid).err().unwrap().kind,
            ErrorKind::Unauthorized
        );

        let mut changed_binding = decode_relay_request(&encoded).unwrap();
        changed_binding.session_binding.replace_range(..1, "0");
        if changed_binding.session_binding == relay.session_binding {
            changed_binding.session_binding.replace_range(..1, "1");
        }
        assert_eq!(
            validate(&changed_binding).err().unwrap().kind,
            ErrorKind::Unauthorized
        );

        let mut changed_ancillary_cookies = decode_relay_request(&encoded).unwrap();
        changed_ancillary_cookies.cookie =
            "did=rotated-device; io=rotated-socket; id=authenticated-session; stay_login=0"
                .to_owned();
        let validated_ancillary = validate(&changed_ancillary_cookies).unwrap();
        assert_eq!(validated_ancillary.binding, session.binding);

        let mut changed_cookie = decode_relay_request(&encoded).unwrap();
        changed_cookie.cookie = "id=different-session".to_owned();
        assert_eq!(
            validate(&changed_cookie).err().unwrap().kind,
            ErrorKind::Unauthorized
        );
    }

    #[test]
    fn relay_rejects_invalid_assertion_username_and_binding_shapes() {
        const AUTHENTICATED_UID: u32 = 2000;
        let environment = environment("GET", "action=snapshot");
        let session = bound_authenticated_session(&environment, AUTHENTICATED_UID);
        let encoded = encode_relay_request(&environment, None, &session).unwrap();

        let mut invalid_username = serde_json::from_slice::<Value>(&encoded).unwrap();
        invalid_username["authenticated_username"] = json!("x".repeat(257));
        assert_eq!(
            decode_relay_request(&serde_json::to_vec(&invalid_username).unwrap())
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );

        let mut short_binding = serde_json::from_slice::<Value>(&encoded).unwrap();
        short_binding["session_binding"] = json!("00");
        assert_eq!(
            decode_relay_request(&serde_json::to_vec(&short_binding).unwrap())
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );

        let mut non_hex_binding = serde_json::from_slice::<Value>(&encoded).unwrap();
        non_hex_binding["session_binding"] = json!("z".repeat(64));
        assert_eq!(
            decode_relay_request(&serde_json::to_vec(&non_hex_binding).unwrap())
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );
    }

    #[test]
    fn url_decoder_rejects_duplicates_nul_and_malformed_escapes() {
        assert_eq!(percent_decode("a%2Fb+c").unwrap(), "a/b c");
        assert!(percent_decode("%0").is_err());
        assert!(percent_decode("%00").is_err());
        assert!(parse_urlencoded("a=1&a=2").is_err());
        assert!(parse_urlencoded("missing-equals").is_err());
    }

    #[test]
    fn authentication_output_is_exactly_one_safe_username() {
        assert_eq!(parse_authentication_output(b"admin\n").unwrap(), "admin");
        assert_eq!(
            parse_authentication_output(b"DOMAIN\\operator\r\n").unwrap(),
            "DOMAIN\\operator"
        );
        assert_eq!(
            parse_authentication_output("María Silva\n".as_bytes()).unwrap(),
            "María Silva"
        );
        let long_directory_identity = format!("DOMAIN\\{}@directory.example.test", "a".repeat(96));
        assert!(long_directory_identity.len() > 64);
        assert!(valid_authenticated_username(&long_directory_identity));
        let four_byte_identity = "𐐀".repeat(64);
        assert_eq!(four_byte_identity.len(), 256);
        assert!(valid_authenticated_username(&four_byte_identity));
        assert!(!valid_authenticated_username(&(four_byte_identity + "a")));
        assert!(valid_authenticated_username("مدير التخزين"));
        for spoofing_control in ['\u{061c}', '\u{200e}', '\u{202e}', '\u{2066}', '\u{feff}'] {
            assert!(!valid_authenticated_username(&format!(
                "admin{spoofing_control}root"
            )));
        }
        for invalid in [
            b"".as_slice(),
            b"admin\nother\n",
            b"bad\0user\n",
            b"tab\tuser\n",
            b"carriage\ruser\n",
        ] {
            assert!(parse_authentication_output(invalid).is_err());
        }
        assert!(parse_authentication_output(&[0xff, b'\n']).is_err());
        assert!(!valid_authenticated_username("DOMAIN|administrator"));
        assert!(!valid_authenticated_username(&"x".repeat(257)));
    }

    #[test]
    fn administrator_membership_is_independent_and_root_is_refused() {
        assert!(authorize_admin_membership(1000, 100, 200, &[50, 200]).is_ok());
        assert!(authorize_admin_membership(1000, 200, 200, &[]).is_ok());
        assert_eq!(
            authorize_admin_membership(1000, 100, 200, &[50])
                .unwrap_err()
                .kind,
            ErrorKind::Forbidden
        );
        assert!(authorize_admin_membership(0, 200, 200, &[]).is_err());
    }

    #[test]
    fn named_group_membership_accepts_primary_or_supplementary_gid_only() {
        assert!(identity_belongs_to_group(200, 200, &[]));
        assert!(identity_belongs_to_group(100, 200, &[50, 200]));
        assert!(!identity_belongs_to_group(100, 200, &[50, 300]));
    }

    #[test]
    fn trusted_legacy_root_helper_mode_allows_set_id_but_rejects_mutable_files() {
        assert!(trusted_executable_mode(0o100_755, (0, 0)));
        assert!(trusted_executable_mode(0o104_755, (0, 0)));
        assert!(trusted_executable_mode(0o102_755, (0, 0)));
        for mode in [0o100_775, 0o100_757, 0o040_755, 0o100_644] {
            assert!(
                !trusted_executable_mode(mode, (0, 0)),
                "accepted root-owned mode {mode:o}"
            );
        }
    }

    #[test]
    fn trusted_standard_dsm_helper_mode_rejects_set_id_bits() {
        assert!(trusted_executable_mode(0o100_755, (1, 1)));
        for mode in [0o104_755, 0o102_755] {
            assert!(
                !trusted_executable_mode(mode, (1, 1)),
                "accepted system:system mode {mode:o}"
            );
        }
    }

    #[test]
    fn cgi_identity_requires_framework_owned_package_execution() {
        let valid = IdentityState {
            real_uid: 1060,
            effective_uid: 1060,
            executable_uid: 1060,
            executable_mode: 0o100_000 | 0o755,
        };
        assert_eq!(validate_cgi_identity(&valid).unwrap(), 1060);
        for invalid in [
            IdentityState {
                real_uid: 0,
                effective_uid: 0,
                ..valid.clone()
            },
            IdentityState {
                real_uid: 1023,
                effective_uid: 1023,
                ..valid.clone()
            },
            IdentityState {
                real_uid: 1023,
                ..valid.clone()
            },
            IdentityState {
                effective_uid: 1023,
                ..valid.clone()
            },
            IdentityState {
                executable_uid: 0,
                ..valid.clone()
            },
            IdentityState {
                executable_mode: 0o100_000 | 0o4755,
                ..valid.clone()
            },
            IdentityState {
                executable_mode: 0o100_000 | 0o775,
                ..valid.clone()
            },
        ] {
            assert!(validate_cgi_identity(&invalid).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rootless_daemon_socket_gate_requires_the_exact_nonroot_package_uid() {
        let old_web_uid = 1023;
        let package_uid = 1060;
        assert!(linux_socket::validate_peer_uid(package_uid, package_uid).is_ok());
        assert!(linux_socket::validate_peer_uid(old_web_uid, package_uid).is_err());
        assert!(linux_socket::validate_peer_uid(0, package_uid).is_err());
        assert!(linux_socket::validate_peer_uid(package_uid, 0).is_err());
    }

    #[test]
    fn server_and_consumer_derive_uid_from_plain_package_owned_binary() {
        let valid = IdentityState {
            real_uid: 1060,
            effective_uid: 1060,
            executable_uid: 1060,
            executable_mode: 0o100_000 | 0o755,
        };
        assert_eq!(validate_package_identity(&valid).unwrap(), 1060);
        assert!(
            validate_package_identity(&IdentityState {
                executable_mode: 0o100_000 | 0o4755,
                ..valid.clone()
            })
            .is_err()
        );
        assert!(
            validate_package_identity(&IdentityState {
                real_uid: 1023,
                ..valid.clone()
            })
            .is_err()
        );
        assert!(
            validate_package_identity(&IdentityState {
                effective_uid: 1023,
                ..valid.clone()
            })
            .is_err()
        );
        assert!(
            validate_package_identity(&IdentityState {
                real_uid: 0,
                effective_uid: 0,
                executable_uid: 0,
                ..valid
            })
            .is_err()
        );
    }

    #[test]
    fn synology_token_query_normalization_is_exactly_once_and_header_preserving() {
        let command_environment = |token: &str| -> BTreeMap<String, String> {
            let mut request = environment("GET", "action=snapshot");
            request.synology_token_header = Some(Zeroizing::new(token.to_owned()));
            let authentication = match validate_http_request(request).unwrap() {
                ValidatedHttpRequest::Get { authentication, .. } => authentication,
                _ => unreachable!(),
            };
            authentication_command_environment(&authentication)
                .into_iter()
                .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
                .collect()
        };

        for (header, expected_query) in [
            ("native-token%2B%2F%3D", "SynoToken=native-token%2B%2F%3D"),
            ("native-token+/=", "SynoToken=native-token%2B%2F%3D"),
            ("literal+plus", "SynoToken=literal%2Bplus"),
            ("literal%25percent", "SynoToken=literal%25percent"),
            ("literal%percent", "SynoToken=literal%25percent"),
            ("trailing%", "SynoToken=trailing%25"),
            ("short%2", "SynoToken=short%252"),
            ("bad%GG", "SynoToken=bad%25GG"),
            ("lower%2b", "SynoToken=lower%2B"),
            ("%252B", "SynoToken=%252B"),
        ] {
            let environment = command_environment(header);
            assert_eq!(environment["HTTP_X_SYNO_TOKEN"], header);
            assert_eq!(environment["QUERY_STRING"], expected_query);
        }

        let cookie = "id=authenticated-session";
        let encoded = "native-token%2B%2F%3D";
        let raw = "native-token+/=";
        assert_eq!(
            command_environment(encoded)["QUERY_STRING"],
            command_environment(raw)["QUERY_STRING"]
        );
        assert_ne!(
            session_binding("admin", 1000, cookie, Some(encoded)).unwrap(),
            session_binding("admin", 1000, cookie, Some(raw)).unwrap(),
            "session binding must retain the exact package header representation"
        );
    }

    #[test]
    fn child_environments_are_allowlists_without_request_secrets_for_manager() {
        let cookie_request = validate_http_request(environment("GET", "action=snapshot")).unwrap();
        let cookie_authentication = match cookie_request {
            ValidatedHttpRequest::Get { authentication, .. } => authentication,
            _ => unreachable!(),
        };
        let cookie_auth_environment = authentication_command_environment(&cookie_authentication)
            .into_iter()
            .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
            .collect::<BTreeMap<_, _>>();
        let expected_cookie_authentication_names = [
            "PATH",
            "LANG",
            "LC_ALL",
            "REQUEST_METHOD",
            "QUERY_STRING",
            "HTTP_COOKIE",
            "REMOTE_ADDR",
            "SERVER_ADDR",
            "SERVER_NAME",
            "SERVER_PORT",
            "HTTPS",
            "GATEWAY_INTERFACE",
            "HTTP_HOST",
            "REMOTE_PORT",
            "REQUEST_SCHEME",
            "SERVER_PROTOCOL",
            "SCRIPT_NAME",
            "SCRIPT_FILENAME",
            "DOCUMENT_ROOT",
            "SCGI",
            "SOCKET",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        assert_eq!(
            cookie_auth_environment
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected_cookie_authentication_names
        );
        assert_eq!(cookie_auth_environment["REQUEST_METHOD"], "GET");
        assert_eq!(cookie_auth_environment["QUERY_STRING"], "");
        assert_eq!(cookie_auth_environment["GATEWAY_INTERFACE"], "CGI/1.1");
        assert_eq!(
            cookie_auth_environment["HTTP_HOST"],
            "nas.example.invalid:5001"
        );
        assert_eq!(cookie_auth_environment["REMOTE_PORT"], "54321");
        assert_eq!(cookie_auth_environment["REQUEST_SCHEME"], "https");
        assert_eq!(cookie_auth_environment["SERVER_PROTOCOL"], "HTTP/2.0");
        assert_eq!(
            cookie_auth_environment["SCRIPT_NAME"],
            "/webman/3rdparty/synology-drive-sync/api.cgi"
        );
        assert_eq!(
            cookie_auth_environment["SCRIPT_FILENAME"],
            "/var/packages/synology-drive-sync/target/ui/api.cgi"
        );
        assert_eq!(
            cookie_auth_environment["DOCUMENT_ROOT"],
            "/usr/syno/synoman"
        );
        assert_eq!(cookie_auth_environment["SCGI"], "1");
        assert_eq!(cookie_auth_environment["SOCKET"], "/run/synoscgi.sock");

        let token_request =
            validate_http_request(environment("GET", "action=snapshot&SynoToken=dsm-token"))
                .unwrap();
        let token_authentication = match token_request {
            ValidatedHttpRequest::Get { authentication, .. } => authentication,
            _ => unreachable!(),
        };
        let token_auth_environment = authentication_command_environment(&token_authentication)
            .into_iter()
            .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
            .collect::<BTreeMap<_, _>>();
        let mut expected_token_authentication_names = expected_cookie_authentication_names;
        expected_token_authentication_names.insert("HTTP_X_SYNO_TOKEN".to_owned());
        assert_eq!(
            token_auth_environment
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected_token_authentication_names
        );
        assert_eq!(
            token_auth_environment["QUERY_STRING"],
            "SynoToken=dsm-token"
        );
        assert_eq!(token_auth_environment["HTTP_X_SYNO_TOKEN"], "dsm-token");
        assert!(!token_auth_environment["QUERY_STRING"].contains("action="));

        for excluded in [
            "CONTENT_LENGTH",
            "CONTENT_TYPE",
            "HTTP_TRANSFER_ENCODING",
            "HTTP_X_SDSYNC_REQUEST",
            "HTTP_X_SDSYNC_CSRF",
            "LD_LIBRARY_PATH",
            "LD_PRELOAD",
        ] {
            assert!(!token_auth_environment.contains_key(excluded));
        }
        for detected in [
            "GATEWAY_INTERFACE",
            "HTTP_HOST",
            "REMOTE_PORT",
            "REQUEST_SCHEME",
            "SERVER_PROTOCOL",
            "SCRIPT_NAME",
            "SCRIPT_FILENAME",
            "DOCUMENT_ROOT",
            "SCGI",
            "SOCKET",
        ] {
            assert!(CGI_ORIGIN_VARIABLES.contains(&detected));
        }

        let manager_names = manager_command_environment()
            .into_iter()
            .map(|(name, _)| name.into_string().unwrap())
            .collect::<BTreeSet<_>>();
        assert!(!manager_names.contains("HTTP_COOKIE"));
        assert!(!manager_names.contains("HTTP_X_SYNO_TOKEN"));
        assert!(!manager_names.contains("REQUEST_METHOD"));
        assert_eq!(manager_names.len(), 7);
    }

    #[test]
    fn session_binding_uses_only_one_exact_dsm_id_cookie() {
        let expected = session_binding("admin", 1000, "id=session-a", Some("token-a")).unwrap();
        for cookie in [
            "id=session-a",
            "_SSID=transport-a; id=session-a; did=device-a; stay_login=1; io=socket-a",
            "io=socket-b; stay_login=0; did=device-b; id=session-a; _CrPoSt=proxy-b",
            " did=device-c;  id=session-a; io=socket-c",
        ] {
            assert_eq!(
                session_binding("admin", 1000, cookie, Some("token-a")).unwrap(),
                expected,
                "ancillary cookie order and churn must not change DSM session identity"
            );
        }
        assert_ne!(
            session_binding("admin", 1000, "did=device-a; id=session-b", Some("token-a")).unwrap(),
            expected,
            "changing DSM's id cookie must change the session binding"
        );

        for cookie in [
            "",
            "did=device-a; stay_login=1",
            "id=",
            "id=session-a; id=session-a",
            "id=session-a; id=session-b",
            "id=session-a; ID=session-a",
            "ID=session-a",
            "id =session-a",
            "id=\"session-a\"",
            "id=session a",
        ] {
            assert_eq!(
                session_binding("admin", 1000, cookie, Some("token-a"))
                    .unwrap_err()
                    .kind,
                ErrorKind::Unauthorized,
                "missing, duplicate, ambiguous, or invalid DSM id cookies must fail closed"
            );
            let mut request = environment("GET", "action=snapshot&SynoToken=token-a");
            request.cookie = Zeroizing::new(cookie.to_owned());
            assert_eq!(
                validate_http_request(request).unwrap_err().kind,
                ErrorKind::Unauthorized,
                "invalid DSM session identity must be rejected before authentication"
            );
        }
    }

    #[test]
    fn csrf_is_session_bound_short_lived_and_tamper_evident() {
        let key = [7_u8; 32];
        let first = session_binding("admin", 1000, "id=session-a", Some("token-a")).unwrap();
        let second = session_binding("admin", 1000, "id=session-b", Some("token-a")).unwrap();
        let cookie_only = session_binding("admin", 1000, "id=session-a", None).unwrap();
        assert_ne!(first, cookie_only);
        assert_eq!(
            hex_encode(&first),
            "766459af09183f12f60c47bcd079757cec914923c2c960ffdd3552e32924692c"
        );
        let cookie_token = issue_csrf_token(
            &key,
            &cookie_only,
            10_000,
            &[8_u8; 16],
            CSRF_LIFETIME_SECONDS,
        )
        .unwrap();
        assert!(
            verify_csrf_token(
                &cookie_token,
                &key,
                &cookie_only,
                10_001,
                CSRF_LIFETIME_SECONDS,
            )
            .is_ok()
        );
        assert_eq!(
            verify_csrf_token(&cookie_token, &key, &first, 10_001, CSRF_LIFETIME_SECONDS,)
                .unwrap_err()
                .kind,
            ErrorKind::CsrfRejected
        );
        let token =
            issue_csrf_token(&key, &first, 10_000, &[9_u8; 16], CSRF_LIFETIME_SECONDS).unwrap();
        assert!(verify_csrf_token(&token, &key, &first, 10_001, CSRF_LIFETIME_SECONDS).is_ok());
        assert_eq!(
            verify_csrf_token(&token, &key, &second, 10_001, CSRF_LIFETIME_SECONDS)
                .unwrap_err()
                .kind,
            ErrorKind::CsrfRejected
        );
        assert!(
            verify_csrf_token(&token, &[8_u8; 32], &first, 10_001, CSRF_LIFETIME_SECONDS,).is_err()
        );
        assert!(verify_csrf_token(&token, &key, &first, 10_300, CSRF_LIFETIME_SECONDS).is_err());
        let tampered = token.replacen("v1.", "v2.", 1);
        let csrf_error =
            verify_csrf_token(&tampered, &key, &first, 10_001, CSRF_LIFETIME_SECONDS).unwrap_err();
        assert_eq!(csrf_error.kind, ErrorKind::CsrfRejected);
        let response = CgiResponse::error(csrf_error);
        assert_eq!(response.status, 403);
        let payload: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(payload["code"], "csrf_rejected");
        let forbidden = CgiResponse::error(BridgeError::new(ErrorKind::Forbidden));
        let forbidden_payload: Value = serde_json::from_slice(&forbidden.body).unwrap();
        assert_eq!(forbidden_payload["code"], "forbidden");
    }

    #[test]
    fn constant_time_comparison_has_fixed_length_mac_semantics() {
        assert!(constant_time_equal(&[1, 2, 3], &[1, 2, 3]));
        assert!(!constant_time_equal(&[1, 2, 3], &[1, 2, 4]));
        assert!(!constant_time_equal(&[1, 2, 3], &[1, 2]));
    }

    #[test]
    fn security_policy_file_requires_version_and_all_28_editable_canonical_fields() {
        let document = security_policy_document();
        assert_eq!(
            parse_security_policy_file(document.as_bytes()).unwrap(),
            SecurityPolicyArgs::default()
        );

        let malformed = [
            document.replace("policy_version=1\n", ""),
            document.replace("policy_version=1", "policy_version=2"),
            document.replace('\n', "\r\n"),
            document.replace("allow_empty_source=true", "allow_empty_source=tr\rue"),
            document.replace("allow_empty_source=true", "allow_empty_source=true\0"),
            document.trim_end_matches('\n').to_owned(),
            document.replace("allow_empty_source=true\n", ""),
            format!("{document}allow_empty_source=true\n"),
            format!("{document}unreviewed_key=true\n"),
            document.replace("csrf_lifetime_seconds=300", "csrf_lifetime_seconds=0300"),
            document.replace(
                "result_retention_seconds=3600",
                "result_retention_seconds=299",
            ),
            document.replace("max_outstanding_jobs=256", "max_outstanding_jobs=257"),
            document.replace("audit_log_level=info", "audit_log_level=verbose"),
            document.replace("allow_profile_changes=true", "allow_profile_changes=1"),
        ];
        for candidate in malformed {
            assert_eq!(
                parse_security_policy_file(candidate.as_bytes())
                    .unwrap_err()
                    .kind,
                ErrorKind::UnsafeRuntime
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn policy_v1_migration_is_atomic_idempotent_and_fail_closed() {
        let fixture = TestControlFixture::new("policy-v1-migration");
        let package_uid = TestControlFixture::package_uid();
        let policy_path = fixture.root.join("security.conf");
        let versioned = security_policy_document();
        let legacy = versioned.strip_prefix("policy_version=1\n").unwrap();
        fixture.write_private(&policy_path, legacy.as_bytes());

        assert!(
            linux_files::security_policy_migration_required_at(&policy_path, package_uid).unwrap()
        );
        assert!(linux_files::migrate_security_policy_at(&policy_path, package_uid).unwrap());
        assert_eq!(fs::read_to_string(&policy_path).unwrap(), versioned);
        assert!(
            !linux_files::security_policy_migration_required_at(&policy_path, package_uid).unwrap()
        );
        assert!(!linux_files::migrate_security_policy_at(&policy_path, package_uid).unwrap());

        let policy_link = fixture.root.join("security-policy-hardlink");
        fs::hard_link(&policy_path, &policy_link).unwrap();
        for result in [
            linux_files::load_security_policy_at(&policy_path, package_uid),
            linux_files::migrate_security_policy_at(&policy_path, package_uid)
                .map(|_| SecurityPolicyArgs::default()),
        ] {
            assert_eq!(result.unwrap_err().kind, ErrorKind::UnsafeRuntime);
        }
        fs::remove_file(&policy_link).unwrap();

        for unsafe_mode in [0o1600, 0o2600, 0o4600] {
            fs::set_permissions(&policy_path, fs::Permissions::from_mode(unsafe_mode)).unwrap();
            assert_eq!(
                linux_files::load_security_policy_at(&policy_path, package_uid)
                    .unwrap_err()
                    .kind,
                ErrorKind::UnsafeRuntime
            );
            assert_eq!(
                linux_files::migrate_security_policy_at(&policy_path, package_uid)
                    .unwrap_err()
                    .kind,
                ErrorKind::UnsafeRuntime
            );
        }
        fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o600)).unwrap();

        let invalid_documents = [
            "broken\n".to_owned(),
            versioned.replace("policy_version=1", "policy_version=2"),
        ];
        for invalid in invalid_documents {
            fixture.write_private(&policy_path, invalid.as_bytes());
            assert_eq!(
                linux_files::migrate_security_policy_at(&policy_path, package_uid)
                    .unwrap_err()
                    .kind,
                ErrorKind::UnsafeRuntime
            );
            assert_eq!(fs::read_to_string(&policy_path).unwrap(), invalid);
        }

        fs::remove_file(&policy_path).unwrap();
        std::os::unix::fs::symlink("missing-policy", &policy_path).unwrap();
        assert_eq!(
            linux_files::migrate_security_policy_at(&policy_path, package_uid)
                .unwrap_err()
                .kind,
            ErrorKind::UnsafeRuntime
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn durable_audit_verification_accepts_legacy_history_and_binds_exact_identity() {
        let fixture = TestControlFixture::new("durable-audit-history");
        let package_uid = TestControlFixture::package_uid();
        let log_root = fixture.root.join("logs");
        fs::create_dir(&log_root).unwrap();
        fs::set_permissions(&log_root, fs::Permissions::from_mode(0o700)).unwrap();
        let audit_log = log_root.join("audit.log");
        let activity_log = log_root.join("activity.log");
        let (owner_pid, owner_start, owner_boot) = linux_files::current_process_identity().unwrap();
        let record = AuditOutboxRecord {
            schema: "sdsync.dsm-audit-outbox.v1".to_owned(),
            transaction: JOB_ID.to_owned(),
            operation: "remove-profile".to_owned(),
            profile: "archive".to_owned(),
            actor: "مدير التخزين".to_owned(),
            actor_uid: package_uid.max(1),
            origin: "manager".to_owned(),
            client_request_id: None,
            job_id: None,
            owner_pid,
            owner_start,
            owner_boot,
            phase: AuditOutboxPhase::Succeeded,
        };
        let audit = serde_json::to_string(&json!({
            "epoch": 10_000,
            "level": "info",
            "configured_level": "info",
            "subject_level": "info",
            "mandatory": true,
            "category": "audit",
            "subject_category": "configuration",
            "operation": record.operation,
            "state": "succeeded",
            "transaction": record.transaction,
            "origin": record.origin,
            "actor": record.actor,
            "actor_uid": record.actor_uid,
            "profile": record.profile,
        }))
        .unwrap();
        fixture.write_private(&audit_log, format!("{audit}\n").as_bytes());
        fixture.write_private(
            &activity_log,
            format!(
                "9000|run.succeeded|legacy|succeeded|Released history\n10000|audit.succeeded|archive|succeeded|audit|info|{}|{}|Module remove-profile succeeded [{JOB_ID}]\n",
                record.actor_uid, record.actor
            )
            .as_bytes(),
        );
        linux_files::durably_verify_audit_event_at(
            &record,
            "succeeded",
            package_uid,
            &log_root,
            &audit_log,
            &activity_log,
        )
        .unwrap();

        let mut wrong_uid = record.clone();
        wrong_uid.actor_uid = record.actor_uid.saturating_add(1);
        assert_eq!(
            linux_files::durably_verify_audit_event_at(
                &wrong_uid,
                "succeeded",
                package_uid,
                &log_root,
                &audit_log,
                &activity_log,
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );

        let mut bridge_expectation = record.clone();
        bridge_expectation.origin = "bridge".to_owned();
        bridge_expectation.transaction = format!("bridge-{JOB_ID}");
        bridge_expectation.client_request_id = Some(REQUEST_ID.to_owned());
        bridge_expectation.job_id = None;
        let bridge_audit = serde_json::to_string(&json!({
            "epoch": 10_001,
            "level": "info",
            "configured_level": "info",
            "subject_level": "info",
            "mandatory": true,
            "category": "audit",
            "subject_category": "configuration",
            "operation": bridge_expectation.operation,
            "state": "requested",
            "transaction": bridge_expectation.transaction,
            "origin": bridge_expectation.origin,
            "actor": bridge_expectation.actor,
            "actor_uid": bridge_expectation.actor_uid,
            "profile": bridge_expectation.profile,
            "client_request_id": bridge_expectation.client_request_id.as_deref(),
        }))
        .unwrap();
        fixture.write_private(&audit_log, format!("{bridge_audit}\n").as_bytes());
        fixture.write_private(
            &activity_log,
            format!(
                "10001|audit.requested|archive|requested|audit|info|{}|{}|Module remove-profile requested [{}] request_id={}\n",
                bridge_expectation.actor_uid,
                bridge_expectation.actor,
                bridge_expectation.transaction,
                REQUEST_ID,
            )
            .as_bytes(),
        );
        linux_files::durably_verify_audit_event_at(
            &bridge_expectation,
            "requested",
            package_uid,
            &log_root,
            &audit_log,
            &activity_log,
        )
        .unwrap();

        let mut wrong_client_request = bridge_expectation.clone();
        wrong_client_request.client_request_id = Some("f".repeat(32));
        assert_eq!(
            linux_files::durably_verify_audit_event_at(
                &wrong_client_request,
                "requested",
                package_uid,
                &log_root,
                &audit_log,
                &activity_log,
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn durable_log_tail_recovery_is_exact_bounded_and_fail_closed() {
        let fixture = TestControlFixture::new("durable-log-tail-recovery");
        let package_uid = TestControlFixture::package_uid();
        let log_root = fixture.root.join("logs");
        fs::create_dir(&log_root).unwrap();
        fs::set_permissions(&log_root, fs::Permissions::from_mode(0o700)).unwrap();
        let audit_log = log_root.join("audit.log");
        let activity_log = log_root.join("activity.log");
        let audit = serde_json::to_string(&json!({
            "epoch": 10_000,
            "level": "info",
            "configured_level": "info",
            "subject_level": "info",
            "mandatory": true,
            "category": "audit",
            "subject_category": "configuration",
            "operation": "remove-profile",
            "state": "succeeded",
            "transaction": JOB_ID,
            "origin": "manager",
            "actor": "DSM Administrator",
            "actor_uid": package_uid.max(1),
            "profile": "archive",
        }))
        .unwrap();
        let activity = format!(
            "10000|audit.succeeded|archive|succeeded|audit|info|{}|DSM Administrator|Module remove-profile succeeded [{JOB_ID}]",
            package_uid.max(1)
        );

        fixture.write_private(&audit_log, format!("{audit}\n{{\"partial\"").as_bytes());
        assert!(
            linux_files::repair_durable_log_tail_at(
                &log_root,
                &audit_log,
                package_uid,
                5,
                11 * 1024 * 1024,
                |line| linux_files::validate_audit_log_line(line).map(|_| ()),
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(&audit_log).unwrap(),
            format!("{audit}\n")
        );

        fixture.write_private(&activity_log, activity.as_bytes());
        assert!(
            linux_files::repair_durable_log_tail_at(
                &log_root,
                &activity_log,
                package_uid,
                3,
                2 * 1024 * 1024,
                |line| linux_files::validate_activity_log_line(line).map(|_| ()),
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(&activity_log).unwrap(),
            format!("{activity}\n")
        );

        fixture.write_private(&audit_log, format!("{audit}\n\n").as_bytes());
        assert_eq!(
            linux_files::repair_durable_log_tail_at(
                &log_root,
                &audit_log,
                package_uid,
                5,
                11 * 1024 * 1024,
                |line| linux_files::validate_audit_log_line(line).map(|_| ()),
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );

        fixture.write_private(&audit_log, format!("{audit}\n").as_bytes());
        let rotated = log_root.join("audit.log.1");
        fixture.write_private(&rotated, b"{\"partial\"");
        assert_eq!(
            linux_files::repair_durable_log_tail_at(
                &log_root,
                &audit_log,
                package_uid,
                5,
                11 * 1024 * 1024,
                |line| linux_files::validate_audit_log_line(line).map(|_| ()),
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn durable_log_validators_reject_malformed_identity_and_activity_codes() {
        for code in [
            "audit.requested",
            "security.log_tail_recovered",
            "routine.retry_scheduled",
        ] {
            assert!(linux_files::valid_activity_code(code));
        }
        for code in [
            "audit",
            ".requested",
            "audit.",
            "audit..requested",
            "Audit.requested",
        ] {
            assert!(!linux_files::valid_activity_code(code));
        }

        let package_uid = TestControlFixture::package_uid().max(1);
        let base = json!({
            "epoch": 10_000,
            "level": "info",
            "configured_level": "info",
            "subject_level": "info",
            "mandatory": true,
            "category": "audit",
            "subject_category": "configuration",
            "operation": "remove-profile",
            "state": "succeeded",
            "transaction": JOB_ID,
            "origin": "manager",
            "actor": "DSM Administrator",
            "actor_uid": package_uid,
            "profile": "archive",
        });
        let mut malformed = Vec::new();
        for (field, value) in [
            ("operation", json!("unknown")),
            ("state", json!("complete")),
            ("transaction", json!("bad transaction")),
            ("origin", json!("browser")),
            ("profile", json!("../escape")),
            ("actor", json!("spoof\u{202e}")),
            ("actor_uid", json!(0)),
            ("subject_category", json!("secrets")),
            ("level", json!("debug")),
            ("client_request_id", json!("short")),
        ] {
            let mut candidate = base.clone();
            candidate[field] = value;
            malformed.push(candidate);
        }
        for candidate in malformed {
            let encoded = serde_json::to_vec(&candidate).unwrap();
            assert_eq!(
                linux_files::validate_audit_log_line(&encoded)
                    .unwrap_err()
                    .kind,
                ErrorKind::UnsafeRuntime
            );
        }

        let mut rejected = base.clone();
        rejected["operation"] = json!("rejected-post");
        rejected["subject_category"] = json!("bridge");
        rejected["state"] = json!("failed");
        rejected["level"] = json!("error");
        rejected["origin"] = json!("bridge");
        assert!(
            linux_files::validate_audit_log_line(&serde_json::to_vec(&rejected).unwrap()).is_ok()
        );
        rejected["client_request_id"] = json!(REQUEST_ID);
        assert!(
            linux_files::validate_audit_log_line(&serde_json::to_vec(&rejected).unwrap()).is_err()
        );

        let mut uncorrelated_bridge = base;
        uncorrelated_bridge["origin"] = json!("bridge");
        assert!(
            linux_files::validate_audit_log_line(
                &serde_json::to_vec(&uncorrelated_bridge).unwrap()
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn durable_audit_verifier_accepts_remote_connection_authentication_category() {
        let actor_uid = TestControlFixture::package_uid().max(1);
        for operation in ["test-profile-auth", "browse-remote"] {
            let audit = serde_json::to_vec(&json!({
                "epoch": 10_000,
                "level": "info",
                "configured_level": "info",
                "subject_level": "warn",
                "mandatory": true,
                "category": "audit",
                "subject_category": "authentication",
                "operation": operation,
                "state": "requested",
                "transaction": JOB_ID,
                "origin": "bridge",
                "actor": "DSM Administrator",
                "actor_uid": actor_uid,
                "profile": "nightly",
                "client_request_id": REQUEST_ID,
            }))
            .unwrap();
            let parsed = linux_files::validate_audit_log_line(&audit).unwrap();
            assert_eq!(parsed.operation, operation);
            assert_eq!(parsed.subject_category, "authentication");
        }
    }

    #[test]
    fn legacy_existing_profile_names_remain_removable_but_new_names_stay_bounded() {
        let legacy = "p".repeat(255);
        assert!(validate_existing_name(&legacy).is_ok());
        assert!(valid_audit_profile(&legacy));
        assert!(validate_name(&legacy).is_err());
        assert!(validate_existing_name(&"p".repeat(256)).is_err());

        assert!(
            parse_mutation_request(&request("remove-profile", json!({ "name": legacy }),)).is_ok()
        );
        let mut configure = configure_arguments();
        configure["name"] = json!("p".repeat(65));
        assert!(parse_mutation_request(&request("configure-profile", configure)).is_err());
    }

    #[test]
    fn tightened_policy_revokes_every_risky_queued_mutation_before_execution() {
        fn parsed(operation: &str, arguments: Value) -> Mutation {
            parse_mutation_request(&request(operation, arguments))
                .unwrap()
                .mutation
        }
        fn rejected(mutation: &Mutation, policy: &SecurityPolicyArgs) {
            assert_eq!(
                validate_mutation_against_security_policy(mutation, policy)
                    .unwrap_err()
                    .kind,
                ErrorKind::Forbidden
            );
        }

        let configure = parsed("configure-profile", configure_arguments());
        assert!(
            validate_mutation_against_security_policy(&configure, &SecurityPolicyArgs::default())
                .is_ok()
        );
        let mut policy = SecurityPolicyArgs {
            allow_profile_changes: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(&configure, &policy);

        let mut http_arguments = configure_arguments();
        http_arguments["url"] = json!("http://nas.example.invalid");
        http_arguments["allow_http"] = json!(true);
        policy = SecurityPolicyArgs {
            allow_http_targets: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(&parsed("configure-profile", http_arguments), &policy);

        let mut invalid_tls_arguments = configure_arguments();
        invalid_tls_arguments["danger_accept_invalid_certs"] = json!(true);
        policy = SecurityPolicyArgs {
            allow_invalid_tls: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(&parsed("configure-profile", invalid_tls_arguments), &policy);

        let mut destructive_arguments = configure_arguments();
        destructive_arguments["delete"] = json!(true);
        policy = SecurityPolicyArgs {
            allow_destructive_sync: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(&parsed("configure-profile", destructive_arguments), &policy);

        let mut empty_source_arguments = configure_arguments();
        empty_source_arguments["allow_empty_source"] = json!(true);
        empty_source_arguments["delete"] = json!(true);
        policy = SecurityPolicyArgs {
            allow_empty_source: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(
            &parsed("configure-profile", empty_source_arguments),
            &policy,
        );

        let mut remote_log_arguments = configure_arguments();
        remote_log_arguments["remote_log_url"] = json!("https://logs.example.invalid");
        policy = SecurityPolicyArgs {
            allow_remote_logging: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(&parsed("configure-profile", remote_log_arguments), &policy);

        let replace_remote_token = parsed(
            "set-secret",
            json!({"profile":"nightly","kind":"remote-log-token","mode":"replace","value":"token"}),
        );
        rejected(&replace_remote_token, &policy);
        let clear_remote_token = parsed(
            "set-secret",
            json!({"profile":"nightly","kind":"remote-log-token","mode":"clear","value":null}),
        );
        assert!(validate_mutation_against_security_policy(&clear_remote_token, &policy).is_ok());
        policy.allow_secret_changes = false;
        rejected(&clear_remote_token, &policy);

        let stored_connection = json!({
            "profile": "nightly",
            "url": "https://nas.example.invalid",
            "username": "browser-user",
            "allow_http": false,
            "danger_accept_invalid_certs": false,
            "ca_certificate": null,
            "connect_timeout_seconds": 15,
            "timeout_seconds": 120,
            "retries": 2,
            "password_source": "stored",
            "password": null,
            "totp_source": "stored",
            "totp": null
        });
        let auth_test = parsed("test-profile-auth", stored_connection.clone());
        let mut browse_arguments = stored_connection.clone();
        browse_arguments["parent"] = json!("/");
        browse_arguments["connection_proof"] =
            json!(format!("v1.10300.{}.{}", "b".repeat(64), "c".repeat(64)));
        let browse = parsed("browse-remote", browse_arguments);
        policy = SecurityPolicyArgs {
            allow_profile_changes: false,
            allow_secret_changes: false,
            allow_operational_actions: true,
            ..SecurityPolicyArgs::default()
        };
        assert!(validate_mutation_against_security_policy(&auth_test, &policy).is_ok());
        assert!(validate_mutation_against_security_policy(&browse, &policy).is_ok());
        rejected(
            &parsed(
                "set-secret",
                json!({"profile":"nightly","kind":"password","mode":"clear","value":null}),
            ),
            &policy,
        );
        policy.allow_operational_actions = false;
        rejected(&auth_test, &policy);
        rejected(&browse, &policy);

        let mut insecure_connection = stored_connection;
        insecure_connection["url"] = json!("http://nas.example.invalid");
        insecure_connection["allow_http"] = json!(true);
        rejected(
            &parsed("test-profile-auth", insecure_connection),
            &SecurityPolicyArgs {
                allow_http_targets: false,
                ..SecurityPolicyArgs::default()
            },
        );

        policy = SecurityPolicyArgs {
            allow_routine_changes: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(&parsed("routine", routine_arguments()), &policy);
        policy = SecurityPolicyArgs {
            allow_notification_changes: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(
            &parsed(
                "alert-policy",
                json!({"enabled":true,"on_success":false,"on_failure":true,"failure_threshold":2,"cooldown_seconds":3600}),
            ),
            &policy,
        );
        rejected(
            &parsed("client-event", json!({"event":"session-notifications"})),
            &policy,
        );
        policy = SecurityPolicyArgs {
            allow_interface_changes: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(
            &parsed("client-event", json!({"event":"interface-settings"})),
            &policy,
        );
        policy = SecurityPolicyArgs {
            allow_operational_actions: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(
            &parsed(
                "action",
                json!({"kind":"plan","scope":"all","write_test":null,"allow_delete":false,"max_total_delete":100}),
            ),
            &policy,
        );
        policy = SecurityPolicyArgs {
            allow_doctor_write_test: false,
            ..SecurityPolicyArgs::default()
        };
        rejected(
            &parsed(
                "action",
                json!({"kind":"doctor","scope":"nightly","write_test":true,"allow_delete":null,"max_total_delete":null}),
            ),
            &policy,
        );

        // The policy document itself remains a recovery path even after every
        // configurable mutation ceiling has been disabled.
        let locked_down = SecurityPolicyArgs {
            allow_interface_changes: false,
            allow_profile_changes: false,
            allow_secret_changes: false,
            allow_routine_changes: false,
            allow_notification_changes: false,
            allow_operational_actions: false,
            ..SecurityPolicyArgs::default()
        };
        assert!(
            validate_mutation_against_security_policy(
                &parsed("security-policy", security_policy_arguments()),
                &locked_down,
            )
            .is_ok()
        );
    }

    #[test]
    fn every_category_threshold_and_off_state_governs_real_optional_events() {
        const CATEGORIES: [&str; 12] = [
            "audit",
            "bridge",
            "authentication",
            "security",
            "configuration",
            "secrets",
            "routines",
            "operations",
            "notifications",
            "sync",
            "controller",
            "scheduler",
        ];
        let mut policy = SecurityPolicyArgs::default();
        for category in CATEGORIES {
            set_category_level(&mut policy, category, PolicyLogLevel::Off);
            assert!(!event_visible_at_threshold(
                &policy, category, "error", false
            ));
            set_category_level(&mut policy, category, PolicyLogLevel::Warn);
            assert!(event_visible_at_threshold(
                &policy, category, "error", false
            ));
            assert!(event_visible_at_threshold(&policy, category, "warn", false));
            assert!(!event_visible_at_threshold(
                &policy, category, "info", false
            ));
        }
        policy.audit_log_level = PolicyLogLevel::Off;
        assert!(event_visible_at_threshold(&policy, "audit", "info", true));

        policy.controller_log_level = PolicyLogLevel::Error;
        assert!(log_line_visible_at_threshold(
            &policy,
            "controller",
            r#"{"level":"error","event":"control_consumer_failed"}"#,
        ));
        assert!(!log_line_visible_at_threshold(
            &policy,
            "controller",
            r#"{"level":"info","event":"scheduled_run"}"#,
        ));
        policy.scheduler_log_level = PolicyLogLevel::Error;
        assert!(log_line_visible_at_threshold(
            &policy,
            "scheduler",
            r#"{"level":"error","event":"run_finished","exit":1}"#,
        ));
        policy.sync_log_level = PolicyLogLevel::Warn;
        assert!(log_line_visible_at_threshold(
            &policy,
            "sync",
            r#"{"level":"error","message":"failed"}"#,
        ));
        assert!(log_line_visible_at_threshold(
            &policy,
            "audit",
            r#"{"mandatory":true,"level":"info"}"#,
        ));
    }

    #[test]
    fn consume_results_preserve_truthful_outcome_while_terminal_audit_is_pending() {
        let mut calls = 0;
        let result =
            terminalize_consume_result(Err(BridgeError::new(ErrorKind::Unavailable)), |state| {
                calls += 1;
                assert_eq!(state, "failed");
                Ok(false)
            });
        assert_eq!(calls, 1);
        assert_eq!(result.value["ok"], false);
        assert_eq!(result.value["code"], "operation_failed");
        assert_eq!(result.state, "failed");
        assert!(!result.audit_pending);

        let pending =
            terminalize_consume_result(Err(BridgeError::new(ErrorKind::BadRequest)), |state| {
                assert_eq!(state, "failed");
                Err(BridgeError::new(ErrorKind::Unavailable))
            });
        assert_eq!(pending.value["ok"], false);
        assert_eq!(pending.state, "failed");
        assert!(pending.audit_pending);

        let success = json!({"schema":"sdsync.dsm-result.v1","ok":true,"message":"done"});
        let mut success_calls = 0;
        let untouched = terminalize_consume_result(Ok(success.clone()), |state| {
            success_calls += 1;
            assert_eq!(state, "succeeded");
            Ok(false)
        });
        assert_eq!(success_calls, 1);
        assert_eq!(untouched.value, success);
        assert_eq!(untouched.state, "succeeded");
        assert!(!untouched.audit_pending);

        let operation_failure = json!({
            "schema":"sdsync.dsm-result.v1",
            "ok":false,
            "code":"operation_failed",
            "message":"failed",
            "status":"failed",
            "exit_code":12,
            "scope":"nightly",
            "output":"diagnostic"
        });
        let terminal = terminalize_consume_result(Ok(operation_failure.clone()), |state| {
            assert_eq!(state, "failed");
            Ok(true)
        });
        assert_eq!(terminal.value, operation_failure);
        assert_eq!(terminal.state, "failed");
        assert!(terminal.audit_pending);
    }

    #[test]
    fn audit_attribution_uses_exact_allowlisted_operation_and_target() {
        let profile = parse_mutation_request(&request("configure-profile", configure_arguments()))
            .unwrap()
            .mutation;
        assert_eq!(mutation_audit_operation(&profile), "configure-profile");
        assert_eq!(mutation_audit_profile(&profile), "nightly");

        let doctor = parse_mutation_request(&request(
            "action",
            json!({"kind":"doctor","scope":"nightly","write_test":false,"allow_delete":null,"max_total_delete":null}),
        ))
        .unwrap()
        .mutation;
        assert_eq!(mutation_audit_operation(&doctor), "doctor");
        assert_eq!(mutation_audit_profile(&doctor), "nightly");

        let client = parse_mutation_request(&request(
            "client-event",
            json!({"event":"interface-settings"}),
        ))
        .unwrap()
        .mutation;
        assert_eq!(mutation_audit_operation(&client), "interface-settings");
        assert_eq!(mutation_audit_profile(&client), "all");

        for (kind, mode, expected) in [
            ("password", "replace", "set-password"),
            ("password", "clear", "remove-password"),
            ("totp", "replace", "set-totp"),
            ("totp", "clear", "remove-totp"),
            ("remote-log-token", "replace", "set-remote-log-token"),
            ("remote-log-token", "clear", "remove-remote-log-token"),
        ] {
            let mut arguments = json!({
                "profile":"nightly",
                "kind":kind,
                "mode":mode,
                "value":null
            });
            if mode == "replace" {
                arguments["value"] = json!("safe-test-value");
            }
            let mutation = parse_mutation_request(&request("set-secret", arguments))
                .unwrap()
                .mutation;
            assert_eq!(mutation_audit_operation(&mutation), expected);
            assert_eq!(mutation_audit_profile(&mutation), "nightly");
        }
    }

    #[test]
    fn configure_profile_schema_is_strict_and_dispatch_is_fixed() {
        let parsed =
            parse_mutation_request(&request("configure-profile", configure_arguments())).unwrap();
        let arguments = argument_strings(&parsed.mutation);
        assert_eq!(&arguments[..2], ["api", "configure-profile"]);
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--source", "/volume1/source"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--log-format", "human"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--progress", "auto"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--output", "json"])
        );
        assert!(!arguments.iter().any(|argument| argument == MANAGER_PATH));

        let mut unknown = configure_arguments();
        unknown["executable"] = json!("/tmp/evil");
        assert!(parse_mutation_request(&request("configure-profile", unknown)).is_err());

        for (field, value) in [
            ("log_format", "yaml"),
            ("progress", "sometimes"),
            ("output", "xml"),
        ] {
            let mut invalid = configure_arguments();
            invalid[field] = json!(value);
            assert!(parse_mutation_request(&request("configure-profile", invalid)).is_err());
        }

        let mut unsafe_empty_source = configure_arguments();
        unsafe_empty_source["allow_empty_source"] = json!(true);
        assert!(
            parse_mutation_request(&request("configure-profile", unsafe_empty_source)).is_err()
        );
        let mut bounded_empty_source = configure_arguments();
        bounded_empty_source["allow_empty_source"] = json!(true);
        bounded_empty_source["delete"] = json!(true);
        assert!(
            parse_mutation_request(&request("configure-profile", bounded_empty_source)).is_ok()
        );

        let mut portable_boundaries = configure_arguments();
        portable_boundaries["max_delete"] = json!(2_147_483_647_u64);
        portable_boundaries["max_rate_bytes_per_second"] = json!(9_007_199_254_740_991_u64);
        portable_boundaries["quiet"] = json!(true);
        portable_boundaries["verbosity"] = json!(2);
        portable_boundaries["output"] = json!("ndjson");
        let parsed =
            parse_mutation_request(&request("configure-profile", portable_boundaries)).unwrap();
        let arguments = argument_strings(&parsed.mutation);
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--max-delete", "2147483647"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--max-rate", "9007199254740991"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--output", "ndjson"])
        );

        for (field, value) in [
            ("max_delete", 2_147_483_648_u64),
            ("max_rate_bytes_per_second", 9_007_199_254_740_992_u64),
        ] {
            let mut outside_portable_bound = configure_arguments();
            outside_portable_bound[field] = json!(value);
            assert!(
                parse_mutation_request(&request("configure-profile", outside_portable_bound))
                    .is_err(),
                "{field} must remain exactly representable on every DSM target"
            );
        }

        for source in [
            "/",
            "/etc",
            "/volume01/source",
            "/volumeUSB0/source",
            "/volumeSATA01/source",
            "/volume1/../etc",
            "/volume1/@appdata/source",
            "/volume1/source/",
        ] {
            let mut invalid = configure_arguments();
            invalid["source"] = json!(source);
            assert!(
                parse_mutation_request(&request("configure-profile", invalid)).is_err(),
                "source escaped the canonical DSM volume boundary: {source}"
            );
        }
        for source in [
            "/volume1/source",
            "/volumeUSB1/usbshare",
            "/volumeSATA2/satashare",
        ] {
            let mut valid = configure_arguments();
            valid["source"] = json!(source);
            assert!(
                parse_mutation_request(&request("configure-profile", valid)).is_ok(),
                "recognized DSM storage root must remain configurable: {source}"
            );
        }
    }

    #[test]
    fn connection_drafts_are_stripped_from_jobs_and_browse_requires_a_proof() {
        let connection = json!({
            "profile": null,
            "url": "https://nas.example.invalid",
            "username": "browser-user",
            "allow_http": false,
            "danger_accept_invalid_certs": false,
            "ca_certificate": null,
            "connect_timeout_seconds": 15,
            "timeout_seconds": 120,
            "retries": 2,
            "password_source": "provided",
            "password": "fixture-password-not-in-job",
            "totp_source": "provided",
            "totp": "JBSWY3DPEHPK3PXP"
        });
        let parsed =
            parse_mutation_request(&request("test-profile-auth", connection.clone())).unwrap();
        let envelope = parsed
            .secret
            .as_ref()
            .expect("provided secrets need an envelope");
        let (password, totp) =
            decode_connection_secret_envelope(Some(Zeroizing::new(envelope.to_vec()))).unwrap();
        assert_eq!(
            password.as_ref().map(|value| value.as_slice()),
            Some(b"fixture-password-not-in-job".as_slice())
        );
        assert_eq!(
            totp.as_ref().map(|value| value.as_slice()),
            Some(b"JBSWY3DPEHPK3PXP".as_slice())
        );

        let job = canonical_job_bytes(
            JOB_ID,
            &parsed.request_id,
            "admin",
            1000,
            &[7_u8; 32],
            JOB_ID,
            &"a".repeat(64),
            10_000,
            &parsed.mutation,
        )
        .unwrap();
        assert!(!contains_bytes(&job, b"fixture-password-not-in-job"));
        assert!(!contains_bytes(&job, b"JBSWY3DPEHPK3PXP"));
        let parsed_job = parse_job(&job).unwrap();
        assert!(matches!(parsed_job.mutation, Mutation::TestProfileAuth(_)));
        assert_eq!(queued_job_class(&parsed_job), QueuedJobClass::Connection);

        let mut browse = connection;
        browse["parent"] = json!("/home/Drive");
        browse["connection_proof"] =
            json!(format!("v1.10300.{}.{}", "b".repeat(64), "c".repeat(64)));
        let parsed = parse_mutation_request(&request("browse-remote", browse)).unwrap();
        assert!(matches!(parsed.mutation, Mutation::BrowseRemote(_)));

        let mut no_proof = json!({
            "profile": null,
            "url": "https://nas.example.invalid",
            "username": "browser-user",
            "allow_http": false,
            "danger_accept_invalid_certs": false,
            "ca_certificate": null,
            "connect_timeout_seconds": 15,
            "timeout_seconds": 120,
            "retries": 2,
            "password_source": "provided",
            "password": "fixture-password-not-in-job",
            "totp_source": "none",
            "totp": null,
            "parent": "/"
        });
        assert!(parse_mutation_request(&request("browse-remote", no_proof.clone())).is_err());
        no_proof["connection_proof"] = json!("not-a-proof");
        assert!(parse_mutation_request(&request("browse-remote", no_proof)).is_err());
    }

    #[test]
    fn connection_proof_is_short_lived_and_bound_to_session_and_exact_draft() {
        let key = [11_u8; 32];
        let binding = [22_u8; 32];
        let fingerprint = "ab".repeat(32);
        let now = 10_000;
        let (proof, expires) = issue_connection_proof(&key, &binding, &fingerprint, now).unwrap();
        assert_eq!(expires, now + CONNECTION_PROOF_LIFETIME_SECONDS);
        verify_connection_proof(&proof, &key, &binding, &fingerprint, now).unwrap();
        assert!(verify_connection_proof(&proof, &key, &[23_u8; 32], &fingerprint, now).is_err());
        assert!(verify_connection_proof(&proof, &key, &binding, &"cd".repeat(32), now).is_err());
        assert!(verify_connection_proof(&proof, &key, &binding, &fingerprint, expires).is_err());
        let mut tampered = proof.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'a' { b'b' } else { b'a' };
        assert!(
            verify_connection_proof(
                std::str::from_utf8(&tampered).unwrap(),
                &key,
                &binding,
                &fingerprint,
                now
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn logout_is_always_attempted_and_proof_issuance_requires_logout_success() {
        let mut logout_calls = 0;
        let listing = logout_result_with::<(), _>(Err(RemoteConnectionFailure::Listing), || {
            logout_calls += 1;
            Err(RemoteConnectionFailure::Logout)
        });
        assert_eq!(logout_calls, 1);
        assert_eq!(listing, Err(RemoteConnectionFailure::ListingAndLogout));
        let listing_result = connection_failure_result(listing.unwrap_err());
        assert_eq!(listing_result["code"], "file_station_listing_logout_failed");
        validate_connection_manager_result(&listing_result, "browse-remote").unwrap();
        assert!(validate_connection_manager_result(&listing_result, "test-profile-auth").is_err());

        let permission =
            logout_result_with::<(), _>(Err(RemoteConnectionFailure::Permission), || {
                logout_calls += 1;
                Err(RemoteConnectionFailure::Logout)
            });
        assert_eq!(logout_calls, 2);
        assert_eq!(
            permission,
            Err(RemoteConnectionFailure::PermissionAndLogout)
        );
        let permission_result = connection_failure_result(permission.unwrap_err());
        assert_eq!(
            permission_result["code"],
            "file_station_denied_logout_failed"
        );
        validate_connection_manager_result(&permission_result, "browse-remote").unwrap();

        let logout = logout_result_with(Ok("listing"), || {
            logout_calls += 1;
            Err(RemoteConnectionFailure::Logout)
        });
        assert_eq!(logout_calls, 3);
        assert_eq!(logout, Err(RemoteConnectionFailure::Logout));

        let mut proof_calls = 0;
        let rejected =
            authentication_test_result_after_logout(Err(RemoteConnectionFailure::Logout), || {
                proof_calls += 1;
                Ok(("must-not-be-issued".to_owned(), 1))
            })
            .unwrap();
        assert_eq!(proof_calls, 0);
        assert_eq!(rejected["ok"], false);
        assert_eq!(rejected["code"], "file_station_logout_failed");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_source_browser_exposes_only_canonical_readable_volume_directories() {
        let fixture = TestControlFixture::new("source-browser");
        let volume1 = fixture.root.join("volume1");
        let volume2 = fixture.root.join("volume2");
        let volume_usb = fixture.root.join("volumeUSB1");
        let volume_sata = fixture.root.join("volumeSATA2");
        let invalid_volume = fixture.root.join("volume01");
        fs::create_dir(&volume1).unwrap();
        fs::create_dir(&volume2).unwrap();
        fs::create_dir(&volume_usb).unwrap();
        fs::create_dir(&volume_sata).unwrap();
        fs::create_dir(&invalid_volume).unwrap();
        fs::create_dir(volume1.join("alpha")).unwrap();
        fs::create_dir(volume1.join("zeta")).unwrap();
        fs::create_dir(volume1.join("@appdata")).unwrap();
        fs::write(volume1.join("plain-file"), b"not a directory").unwrap();
        let non_utf8 = OsString::from_vec(vec![b'n', b'o', b'n', 0xff]);
        fs::create_dir(volume1.join(non_utf8)).unwrap();
        symlink(&volume2, volume1.join("linked-volume")).unwrap();
        symlink(&volume2, fixture.root.join("volume3")).unwrap();

        let root: Value =
            serde_json::from_slice(&source_directories_document(&fixture.root, "/").unwrap())
                .unwrap();
        assert_eq!(
            root["directories"],
            json!([
                {"name":"volume1","path":"/volume1"},
                {"name":"volume2","path":"/volume2"},
                {"name":"volumeSATA2","path":"/volumeSATA2"},
                {"name":"volumeUSB1","path":"/volumeUSB1"}
            ])
        );
        assert_eq!(root["parent"], Value::Null);

        let children: Value = serde_json::from_slice(
            &source_directories_document(&fixture.root, "/volume1").unwrap(),
        )
        .unwrap();
        assert_eq!(
            children["directories"],
            json!([
                {"name":"alpha","path":"/volume1/alpha"},
                {"name":"zeta","path":"/volume1/zeta"}
            ])
        );
        assert_eq!(children["parent"], "/");
        let exact: Value =
            serde_json::from_slice(&source_path_document(&fixture.root, "/volume1").unwrap())
                .unwrap();
        assert_eq!(exact["schema"], "sdsync.dsm-source-path.v1");
        assert_eq!(exact["path"], "/volume1");
        assert_eq!(exact["valid"], true);
        assert!(source_directories_document(&fixture.root, "/volume3").is_err());
        assert!(source_path_document(&fixture.root, "/volume3").is_err());
        assert!(source_path_document(&fixture.root, "/volume1/linked-volume").is_err());
        assert!(source_path_document(&fixture.root, "/volumeUSB1").is_ok());
        assert!(source_path_document(&fixture.root, "/volumeSATA2").is_ok());
        assert!(source_directories_document(&fixture.root, "/etc").is_err());
        assert!(source_directories_document(&fixture.root, "/volume1/../volume2").is_err());
    }

    #[test]
    fn every_mutation_action_has_a_typed_allowlist_and_manager_mapping() {
        let cases = [
            (
                "remove-profile",
                json!({"name":"nightly"}),
                "remove-profile",
            ),
            ("set-default", json!({"name":"nightly"}), "set-default"),
            (
                "schedule",
                json!({"enabled":true,"interval_seconds":3600,"allow_delete":false,"max_total_delete":100}),
                "schedule",
            ),
            ("routine", routine_arguments(), "routine"),
            (
                "remove-routine",
                json!({"name":"nightly"}),
                "remove-routine",
            ),
            (
                "alert-policy",
                json!({"enabled":true,"on_success":false,"on_failure":true,"failure_threshold":2,"cooldown_seconds":3600}),
                "alert-policy",
            ),
            (
                "security-policy",
                security_policy_arguments(),
                "security-policy",
            ),
            (
                "client-event",
                json!({"event":"interface-settings"}),
                "client-event",
            ),
            (
                "action",
                json!({"kind":"doctor","scope":"nightly","write_test":true,"allow_delete":null,"max_total_delete":null}),
                "action",
            ),
            (
                "action",
                json!({"kind":"plan","scope":"all","write_test":null,"allow_delete":false,"max_total_delete":100}),
                "action",
            ),
            (
                "action",
                json!({"kind":"run","scope":"nightly","write_test":null,"allow_delete":true,"max_total_delete":null}),
                "action",
            ),
        ];
        for (operation, payload, manager_action) in cases {
            let parsed = parse_mutation_request(&request(operation, payload)).unwrap();
            let arguments = argument_strings(&parsed.mutation);
            assert_eq!(arguments[0], "api");
            assert_eq!(arguments[1], manager_action);
        }
    }

    #[test]
    fn secret_is_removed_from_job_json_and_only_stdin_mode_is_dispatched() {
        let parsed = parse_mutation_request(&request(
            "set-secret",
            json!({"profile":"nightly","kind":"password","mode":"replace","value":"not-in-job"}),
        ))
        .unwrap();
        assert_eq!(
            parsed.secret.as_ref().map(|secret| secret.as_slice()),
            Some(b"not-in-job".as_slice())
        );
        let job = canonical_job_bytes(
            JOB_ID,
            &parsed.request_id,
            "admin",
            1000,
            &[7_u8; 32],
            JOB_ID,
            &"a".repeat(64),
            10_000,
            &parsed.mutation,
        )
        .unwrap();
        assert!(!contains_bytes(&job, b"not-in-job"));
        let arguments = argument_strings(&parsed.mutation);
        assert_eq!(
            arguments,
            [
                "api",
                "set-secret",
                "--profile",
                "nightly",
                "--kind",
                "password",
                "--mode",
                "replace"
            ]
        );
        assert!(parse_mutation_request(&request(
            "set-secret",
            json!({"profile":"nightly","kind":"password","mode":"replace","value":"two\nlines"}),
        ))
        .is_err());
    }

    #[test]
    fn clear_secret_rejects_a_value_and_replace_requires_one() {
        assert!(
            parse_mutation_request(&request(
                "set-secret",
                json!({"profile":"nightly","kind":"totp","mode":"clear","value":null}),
            ))
            .is_ok()
        );
        assert!(
            parse_mutation_request(&request(
                "set-secret",
                json!({"profile":"nightly","kind":"totp","mode":"clear","value":"secret"}),
            ))
            .is_err()
        );
        assert!(
            parse_mutation_request(&request(
                "set-secret",
                json!({"profile":"nightly","kind":"totp","mode":"replace","value":null}),
            ))
            .is_err()
        );
    }

    #[test]
    fn action_cross_field_constraints_prevent_ambiguous_dispatch() {
        assert!(parse_mutation_request(&request(
            "action",
            json!({"kind":"doctor","scope":"all","write_test":false,"allow_delete":true,"max_total_delete":100}),
        ))
        .is_err());
        assert!(parse_mutation_request(&request(
            "action",
            json!({"kind":"run","scope":"nightly","write_test":null,"allow_delete":true,"max_total_delete":100}),
        ))
        .is_err());
        assert!(parse_mutation_request(&request(
            "action",
            json!({"kind":"plan","scope":"all","write_test":null,"allow_delete":false,"max_total_delete":null}),
        ))
        .is_err());
    }

    #[test]
    fn doctor_rejects_write_test_at_quick_and_standard_levels() {
        for level in ["quick", "standard"] {
            let error = parse_mutation_request(&request(
                "action",
                json!({
                    "kind": "doctor",
                    "scope": "nightly",
                    "level": level,
                    "write_test": true,
                    "allow_delete": null,
                    "max_total_delete": null
                }),
            ))
            .err()
            .expect("write testing below extensive must be rejected");
            assert_eq!(error.kind, ErrorKind::BadRequest, "level={level}");
        }
    }

    #[test]
    fn plan_and_run_reject_doctor_levels() {
        for arguments in [
            json!({
                "kind": "plan",
                "scope": "all",
                "level": "standard",
                "write_test": null,
                "allow_delete": false,
                "max_total_delete": 100
            }),
            json!({
                "kind": "run",
                "scope": "nightly",
                "level": "extensive",
                "write_test": null,
                "allow_delete": true,
                "max_total_delete": null
            }),
        ] {
            let error = parse_mutation_request(&request("action", arguments))
                .err()
                .expect("non-Doctor actions must reject Doctor levels");
            assert_eq!(error.kind, ErrorKind::BadRequest);
        }
    }

    #[test]
    fn explicit_quick_doctor_dispatches_quick_without_write_test() {
        let parsed = parse_mutation_request(&request(
            "action",
            json!({
                "kind": "doctor",
                "scope": "nightly",
                "level": "quick",
                "write_test": false,
                "allow_delete": null,
                "max_total_delete": null
            }),
        ))
        .unwrap();
        assert_eq!(
            argument_strings(&parsed.mutation),
            [
                "api",
                "action",
                "--kind",
                "doctor",
                "--scope",
                "nightly",
                "--level",
                "quick",
                "--write-test",
                "false",
            ]
        );
    }

    #[test]
    fn legacy_doctor_write_test_dispatches_extensive_level() {
        let parsed = parse_mutation_request(&request(
            "action",
            json!({
                "kind": "doctor",
                "scope": "nightly",
                "write_test": true,
                "allow_delete": null,
                "max_total_delete": null
            }),
        ))
        .unwrap();
        assert_eq!(
            argument_strings(&parsed.mutation),
            [
                "api",
                "action",
                "--kind",
                "doctor",
                "--scope",
                "nightly",
                "--level",
                "extensive",
                "--write-test",
                "true",
            ]
        );
    }

    #[test]
    fn aggregate_delete_bounds_are_portable_and_dispatch_exactly() {
        let boundary = MAX_DSM_DELETE_BOUND;
        for (operation, mut arguments) in [
            (
                "schedule",
                json!({"enabled":true,"interval_seconds":3600,"allow_delete":true,"max_total_delete":boundary}),
            ),
            ("routine", routine_arguments()),
            (
                "action",
                json!({"kind":"run","scope":"all","write_test":null,"allow_delete":true,"max_total_delete":boundary}),
            ),
        ] {
            arguments["max_total_delete"] = json!(boundary);
            let parsed = parse_mutation_request(&request(operation, arguments.clone())).unwrap();
            let manager_arguments = argument_strings(&parsed.mutation);
            assert!(
                manager_arguments
                    .windows(2)
                    .any(|pair| pair == ["--max-total-delete", "2147483647"]),
                "{operation} must dispatch the exact portable boundary"
            );

            arguments["max_total_delete"] = json!(boundary + 1);
            assert!(
                parse_mutation_request(&request(operation, arguments)).is_err(),
                "{operation} must reject the first value outside the portable boundary"
            );
        }
    }

    #[test]
    fn routine_ranges_dependencies_and_unknown_fields_fail_closed() {
        let mut duplicate = routine_arguments();
        duplicate["weekdays"] = json!([1, 1]);
        assert!(parse_mutation_request(&request("routine", duplicate)).is_err());
        let mut self_dependency = routine_arguments();
        self_dependency["depends_on"] = json!(["nightly"]);
        assert!(parse_mutation_request(&request("routine", self_dependency)).is_err());
        let mut unknown = routine_arguments();
        unknown["command"] = json!("sync --evil");
        assert!(parse_mutation_request(&request("routine", unknown)).is_err());
    }

    #[test]
    fn routine_mode_contract_rejects_irrelevant_timing_fields() {
        let common = json!({
            "profile": "nightly",
            "enabled": true,
            "action": "sync",
            "retry_count": 5,
            "retry_backoff_seconds": 60,
            "retry_exponential": true,
            "allow_delete": false,
            "max_total_delete": 100,
            "depends_on": []
        });
        let mut interval = common.clone();
        interval["mode"] = json!("interval");
        interval["interval_seconds"] = json!(3600);
        assert!(parse_mutation_request(&request("routine", interval.clone())).is_ok());
        interval["weekdays"] = json!([1]);
        assert!(parse_mutation_request(&request("routine", interval)).is_err());

        let mut daily = common.clone();
        daily["mode"] = json!("daily");
        daily["weekdays"] = json!([1, 3, 5]);
        daily["time_window_start"] = json!("01:30");
        daily["time_window_end"] = json!("04:00");
        assert!(parse_mutation_request(&request("routine", daily.clone())).is_ok());
        daily["interval_seconds"] = json!(3600);
        assert!(parse_mutation_request(&request("routine", daily)).is_err());

        let mut realtime = common;
        realtime["mode"] = json!("realtime");
        realtime["debounce_seconds"] = json!(45);
        realtime["poll_seconds"] = json!(30);
        assert!(parse_mutation_request(&request("routine", realtime.clone())).is_ok());
        realtime["time_window_start"] = json!("00:00");
        assert!(parse_mutation_request(&request("routine", realtime)).is_err());
    }

    #[test]
    fn routine_retry_contract_requires_toggle_and_caps_new_base() {
        let mut missing_toggle = routine_arguments();
        missing_toggle
            .as_object_mut()
            .unwrap()
            .remove("retry_exponential");
        assert!(parse_mutation_request(&request("routine", missing_toggle)).is_err());

        let mut too_large = routine_arguments();
        too_large["retry_backoff_seconds"] = json!(301);
        assert!(parse_mutation_request(&request("routine", too_large)).is_err());
    }

    #[test]
    fn outer_request_and_job_schemas_reject_unknown_or_stale_data() {
        let unknown = serde_json::to_vec(&json!({
            "schema":"sdsync.dsm-request.v1",
            "request_id":REQUEST_ID,
            "operation":"remove-profile",
            "arguments":{"name":"nightly"},
            "extra":true
        }))
        .unwrap();
        assert!(parse_mutation_request(&unknown).is_err());

        let parsed =
            parse_mutation_request(&request("remove-profile", json!({"name":"nightly"}))).unwrap();
        let job = canonical_job_bytes(
            JOB_ID,
            REQUEST_ID,
            "admin",
            1000,
            &[7_u8; 32],
            JOB_ID,
            &"a".repeat(64),
            10_000,
            &parsed.mutation,
        )
        .unwrap();
        let parsed_job = parse_job(&job).unwrap();
        assert_eq!(parsed_job.request_id, JOB_ID);
        assert_eq!(parsed_job.client_request_id, REQUEST_ID);
        assert_eq!(parsed_job.requested_by, "admin");
        assert_eq!(parsed_job.session_binding, [7_u8; 32]);
        assert_eq!(parsed_job.mutation.operation_id(), "remove-profile");
        assert_eq!(queued_job_class(&parsed_job), QueuedJobClass::Serialized);
        assert!(validate_job_freshness(parsed_job.issued_at_epoch, 10_001).is_ok());
        assert!(
            validate_job_freshness(parsed_job.issued_at_epoch, 10_000 + MAX_JOB_AGE_SECONDS + 1)
                .is_err()
        );
    }

    #[test]
    fn manager_result_schema_allows_only_documented_terminal_variants() {
        let accepted = [
            json!({
                "schema":"sdsync.dsm-result.v1",
                "ok":true,
                "message":"configuration updated"
            }),
            json!({
                "schema":"sdsync.dsm-result.v1",
                "ok":false,
                "code":"unsafe_state",
                "message":"operation failed",
                "exit_code":73
            }),
            json!({
                "schema":"sdsync.dsm-result.v1",
                "ok":true,
                "message":"secret state updated",
                "has_password":true,
                "has_totp":false,
                "has_remote_log_token":true
            }),
            json!({
                "schema":"sdsync.dsm-result.v1",
                "ok":true,
                "message":"doctor completed",
                "status":"succeeded",
                "exit_code":0,
                "scope":"all",
                "output":"healthy"
            }),
            json!({
                "schema":"sdsync.dsm-result.v1",
                "ok":false,
                "code":"operation_failed",
                "message":"run failed",
                "status":"failed",
                "exit_code":1,
                "scope":"nightly",
                "output":"safe failure"
            }),
        ];
        for value in accepted {
            assert!(parse_manager_result(&serde_json::to_vec(&value).unwrap(), None).is_ok());
        }

        let rejected = [
            json!({"schema":"wrong","ok":true,"message":"ok"}),
            json!({"schema":"sdsync.dsm-result.v1","ok":true,"message":"ok","command":"/bin/sh"}),
            json!({"schema":"sdsync.dsm-result.v1","ok":true,"message":"ok","code":"operation_failed"}),
            json!({"schema":"sdsync.dsm-result.v1","ok":false,"message":"failed"}),
            json!({"schema":"sdsync.dsm-result.v1","ok":true,"message":"ok","password":"leak"}),
            json!({"schema":"sdsync.dsm-result.v1","ok":true,"message":"ok","has_password":true}),
            json!({"schema":"sdsync.dsm-result.v1","ok":true,"message":"ok","status":"failed","exit_code":0,"scope":"all","output":"bad"}),
        ];
        for value in rejected {
            assert!(parse_manager_result(&serde_json::to_vec(&value).unwrap(), None).is_err());
        }
        assert!(
            parse_manager_result(
                br#"{"schema":"sdsync.dsm-result.v1","ok":true,"message":"exact-secret"}"#,
                Some(b"exact-secret")
            )
            .is_err()
        );

        let constant_secret_result = br#"{"schema":"sdsync.dsm-result.v1","ok":true,"message":"secret state updated","has_password":true,"has_totp":false,"has_remote_log_token":false}"#;
        for short_secret in [b"a".as_slice(), b"true".as_slice(), b"schema".as_slice()] {
            let parsed = parse_manager_result(constant_secret_result, Some(short_secret)).unwrap();
            validate_set_secret_manager_result(&parsed).unwrap();
        }
        for invalid in [
            json!({
                "schema":"sdsync.dsm-result.v1", "ok":true,
                "message":"secret a stored", "has_password":true,
                "has_totp":false, "has_remote_log_token":false
            }),
            json!({
                "schema":"sdsync.dsm-result.v1", "ok":true,
                "message":"secret state updated", "has_password":true,
                "has_totp":false, "has_remote_log_token":false,
                "profile":"nightly"
            }),
        ] {
            assert!(validate_set_secret_manager_result(&invalid).is_err());
        }
    }

    #[test]
    fn queued_response_is_strict_and_preserves_private_session_binding() {
        let mutation =
            parse_mutation_request(&request("remove-profile", json!({"name":"nightly"}))).unwrap();
        let job_bytes = canonical_job_bytes(
            JOB_ID,
            REQUEST_ID,
            "admin",
            1000,
            &[9_u8; 32],
            JOB_ID,
            &"a".repeat(64),
            10_000,
            &mutation.mutation,
        )
        .unwrap();
        let job = parse_job(&job_bytes).unwrap();
        let result = parse_manager_result(
            br#"{"schema":"sdsync.dsm-result.v1","ok":true,"message":"removed"}"#,
            None,
        )
        .unwrap();
        let response = canonical_queued_response_bytes(&job, 10_005, &result, false).unwrap();
        let parsed = parse_queued_response(&response, JOB_ID).unwrap();
        assert_eq!(parsed.operation.as_deref(), Some("remove-profile"));
        assert_eq!(parsed.session_binding, [9_u8; 32]);
        assert!(session_binding_matches(
            &parsed.session_binding,
            &[9_u8; 32]
        ));
        assert!(!session_binding_matches(
            &parsed.session_binding,
            &[8_u8; 32]
        ));
        assert_eq!(parsed.completed_at_epoch, 10_005);
        assert_eq!(parsed.result["ok"], true);
        let mut retained_v1: Value = serde_json::from_slice(&response).unwrap();
        retained_v1["schema"] = json!("sdsync.dsm-queued-response.v1");
        retained_v1.as_object_mut().unwrap().remove("operation");
        let parsed_v1 =
            parse_queued_response(&serde_json::to_vec(&retained_v1).unwrap(), JOB_ID).unwrap();
        assert_eq!(parsed_v1.operation.as_deref(), None);
        retained_v1["operation"] = json!("remove-profile");
        assert!(parse_queued_response(&serde_json::to_vec(&retained_v1).unwrap(), JOB_ID).is_err());
        assert!(
            !String::from_utf8(response.clone())
                .unwrap()
                .contains("id=authenticated")
        );
        assert!(
            parse_queued_response(
                &response,
                "00060f5e12345678fedcba98765432100123456789abcdee"
            )
            .is_err()
        );

        let mut unknown: Value = serde_json::from_slice(&response).unwrap();
        unknown["path"] = json!("/bin/sh");
        assert!(parse_queued_response(&serde_json::to_vec(&unknown).unwrap(), JOB_ID).is_err());
    }

    #[test]
    fn queued_connection_results_round_trip_only_their_exact_operation_schema() {
        let connection = json!({
            "profile": null,
            "url": "https://nas.example.invalid",
            "username": "browser-user",
            "allow_http": false,
            "danger_accept_invalid_certs": false,
            "ca_certificate": null,
            "connect_timeout_seconds": 10,
            "timeout_seconds": 30,
            "retries": 2,
            "password_source": "provided",
            "password": "fixture-password-not-in-result",
            "totp_source": "none",
            "totp": null
        });
        let auth = parse_mutation_request(&request("test-profile-auth", connection.clone()))
            .unwrap()
            .mutation;
        let auth_job = parse_job(
            &canonical_job_bytes(
                JOB_ID,
                REQUEST_ID,
                "admin",
                1000,
                &[9_u8; 32],
                JOB_ID,
                &"a".repeat(64),
                10_000,
                &auth,
            )
            .unwrap(),
        )
        .unwrap();
        let proof = format!("v1.10300.{}.{}", "a".repeat(64), "b".repeat(64));
        let auth_result = json!({
            "schema": "sdsync.dsm-result.v1",
            "ok": true,
            "message": "Authentication succeeded and the temporary File Station session was closed.",
            "connection_proof": proof,
            "connection_proof_expires_at_epoch": 10_300,
        });
        let auth_response =
            canonical_queued_response_bytes(&auth_job, 10_005, &auth_result, false).unwrap();
        let parsed_auth = parse_queued_response(&auth_response, JOB_ID).unwrap();
        assert_eq!(parsed_auth.result, auth_result);
        let auth_text = String::from_utf8(auth_response).unwrap();
        assert!(!auth_text.contains("fixture-password-not-in-result"));
        let mut mismatched_expiry = auth_result.clone();
        mismatched_expiry["connection_proof_expires_at_epoch"] = json!(10_301);
        assert!(
            canonical_queued_response_bytes(&auth_job, 10_005, &mismatched_expiry, false).is_err()
        );

        let mut browse = connection;
        browse["profile"] = json!("nightly");
        browse["password"] = Value::Null;
        browse["password_source"] = json!("stored");
        browse["connection_proof"] =
            json!(format!("v1.10300.{}.{}", "c".repeat(64), "d".repeat(64)));
        browse["parent"] = json!("/homes");
        let browse = parse_mutation_request(&request("browse-remote", browse))
            .unwrap()
            .mutation;
        let browse_job = parse_job(
            &canonical_job_bytes(
                JOB_ID,
                REQUEST_ID,
                "admin",
                1000,
                &[9_u8; 32],
                JOB_ID,
                &"b".repeat(64),
                10_000,
                &browse,
            )
            .unwrap(),
        )
        .unwrap();
        let browse_result = json!({
            "schema": "sdsync.dsm-result.v1",
            "ok": true,
            "message": "File Station directories loaded.",
            "directory_schema": "sdsync.dsm-remote-directories.v1",
            "current": "/homes",
            "directories": [{"name":"alice","path":"/homes/alice"}],
            "truncated": false,
        });
        let browse_response =
            canonical_queued_response_bytes(&browse_job, 10_006, &browse_result, false).unwrap();
        assert_eq!(
            parse_queued_response(&browse_response, JOB_ID)
                .unwrap()
                .result,
            browse_result
        );

        let mut wrong_shape = browse_result;
        wrong_shape["directories"][0]["path"] = json!("/homes/../etc");
        assert!(canonical_queued_response_bytes(&browse_job, 10_006, &wrong_shape, false).is_err());
        assert!(canonical_queued_response_bytes(&auth_job, 10_006, &wrong_shape, false).is_err());

        for job in [&auth_job, &browse_job] {
            let terminal =
                terminalize_consume_result(Err(BridgeError::new(ErrorKind::Forbidden)), |_| {
                    Ok(false)
                });
            let response = canonical_queued_response_bytes(job, 10_007, &terminal.value, false)
                .expect("connection-internal failure remains a retrievable terminal result");
            let parsed = parse_queued_response(&response, JOB_ID).unwrap();
            assert_eq!(parsed.result["code"], "operation_failed");
            assert_eq!(
                parsed.result["message"],
                "Operation could not be completed."
            );
        }
    }

    #[test]
    fn result_status_envelopes_are_bounded_and_expiry_is_explicit() {
        let pending = queued_pending_response(JOB_ID).unwrap();
        assert_eq!(pending.status, 202);
        let pending_json: Value = serde_json::from_slice(&pending.body).unwrap();
        assert_eq!(pending_json["state"], "pending");
        assert!(pending_json.get("result").is_none());

        let complete = queued_complete_response(
            JOB_ID,
            REQUEST_ID,
            1000,
            &generic_manager_result_value(),
            false,
        )
        .unwrap();
        assert_eq!(complete.status, 200);
        let complete_json: Value = serde_json::from_slice(&complete.body).unwrap();
        assert_eq!(complete_json["state"], "complete");
        assert_eq!(complete_json["result"]["schema"], "sdsync.dsm-result.v1");

        let expired = queued_expired_response(JOB_ID).unwrap();
        assert_eq!(expired.status, 410);
        let expired_json: Value = serde_json::from_slice(&expired.body).unwrap();
        assert_eq!(expired_json["state"], "expired_or_missing");
        assert_eq!(expired_json["result"]["ok"], false);
        assert_eq!(expired_json["result"]["code"], "expired_or_missing");
    }

    #[test]
    fn manager_json_redacts_sensitive_keys_and_exact_secret_values() {
        let output = br#"{"schema":"sdsync.dsm-activity.v1","has_password":true,"password":"leak","nested":{"token":"leak"},"message":"exact-secret"}"#;
        let sanitized = parse_and_sanitize_manager_json(
            output,
            &ReadAction::Activity { lines: 10 },
            Some(b"exact-secret"),
            None,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&sanitized).unwrap();
        assert_eq!(value["has_password"], true);
        assert_eq!(value["password"], "[redacted]");
        assert_eq!(value["nested"]["token"], "[redacted]");
        assert_eq!(value["message"], "[redacted]");
        assert!(parse_manager_result(output, Some(b"exact-secret")).is_err());
    }

    #[test]
    fn log_source_filter_and_snapshot_capabilities_are_bridge_owned() {
        let logs = br#"{"schema":"sdsync.dsm-logs.v1","logs":[{"source":"api","lines":["{\"level\":\"warn\"}"]},{"source":"controller","lines":[]},{"source":"sync","lines":[]}]}"#;
        let filtered = parse_and_sanitize_manager_json(
            logs,
            &ReadAction::Logs {
                lines: 10,
                source: LogSource::Sync,
            },
            None,
            None,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&filtered).unwrap();
        assert_eq!(value["logs"].as_array().unwrap().len(), 1);
        assert_eq!(value["logs"][0]["source"], "sync");

        let api_filtered = parse_and_sanitize_manager_json(
            logs,
            &ReadAction::Logs {
                lines: 10,
                source: LogSource::Api,
            },
            None,
            Some(&SecurityPolicyArgs::default()),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&api_filtered).unwrap();
        assert_eq!(value["logs"].as_array().unwrap().len(), 1);
        assert_eq!(value["logs"][0]["source"], "api");
        assert_eq!(value["logs"][0]["lines"].as_array().unwrap().len(), 1);

        let authentication_off = SecurityPolicyArgs {
            authentication_log_level: PolicyLogLevel::Off,
            ..SecurityPolicyArgs::default()
        };
        let hidden = parse_and_sanitize_manager_json(
            br#"{"schema":"sdsync.dsm-logs.v1","logs":[{"source":"api","lines":["{\"level\":\"warn\",\"category\":\"authentication\"}","{\"level\":\"warn\",\"category\":\"security\"}","{\"level\":\"info\",\"category\":\"bridge\"}"]}]}"#,
            &ReadAction::Logs {
                lines: 10,
                source: LogSource::Api,
            },
            None,
            Some(&authentication_off),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&hidden).unwrap();
        let lines = value["logs"][0]["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|line| !line.as_str().unwrap().contains("authentication"))
        );

        let snapshot = parse_and_sanitize_manager_json(
            br#"{"schema":"sdsync.dsm-api.v1","capabilities":{"mutations":false}}"#,
            &ReadAction::Snapshot,
            None,
            Some(&SecurityPolicyArgs::default()),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&snapshot).unwrap();
        assert_eq!(value["package"]["version"], env!("SDSYNC_VERSION"));
        assert_eq!(value["capabilities"]["mutations"], true);
        assert_eq!(value["capabilities"]["private_queue"], true);
        assert_eq!(value["capabilities"]["request_reconciliation"], true);
        assert_eq!(
            value["security_policy"]["queue_limits"],
            json!({
                "active_request_and_processing_jobs": MAX_OUTSTANDING_JOBS,
                "retained_terminal_responses": MAX_OUTSTANDING_JOBS,
                "worst_case_total_job_records": MAX_OUTSTANDING_JOBS * 2,
            })
        );
    }

    #[test]
    fn consumer_paths_are_fixed_corresponding_queue_children() {
        let request = PathBuf::from(format!("{PROCESSING_DIR}/{JOB_ID}.json"));
        let response = PathBuf::from(format!("{RESPONSES_DIR}/{JOB_ID}.json"));
        assert_eq!(
            validate_consumer_paths(&request, &response).unwrap(),
            JOB_ID
        );
        assert!(
            validate_consumer_paths(
                &PathBuf::from(format!("{PROCESSING_DIR}/../{JOB_ID}.json")),
                &response
            )
            .is_err()
        );
        assert!(
            validate_consumer_paths(
                &request,
                &PathBuf::from(format!(
                    "{RESPONSES_DIR}/ffffffffffffffffffffffffffffffffffffffffffffffff.json"
                ))
            )
            .is_err()
        );
        assert!(
            validate_consumer_paths(
                &request,
                &PathBuf::from(format!("{RESPONSES_DIR}/.{JOB_ID}.json.1234"))
            )
            .is_err()
        );
    }

    #[test]
    fn sortable_job_ids_remain_fifo_when_clock_stalls_or_regresses() {
        let first = next_enqueue_sequence(0, 1_000).unwrap();
        let second = next_enqueue_sequence(first, 1_000).unwrap();
        let third = next_enqueue_sequence(second, 900).unwrap();
        assert_eq!((first, second, third), (1_000, 1_001, 1_002));
        let first_id = sortable_job_id(first, &[0xaa; 16]);
        let second_id = sortable_job_id(second, &[0x00; 16]);
        assert_eq!(first_id.len(), 48);
        assert!(valid_server_job_id(&first_id));
        assert!(first_id < second_id);
        assert!(next_enqueue_sequence(u64::MAX, 1).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fresh_publications_and_exact_replays_request_an_idempotent_controller_wake() {
        let existing = EnqueueOutcome::Existing(JOB_ID.to_owned());
        let published = EnqueueOutcome::Published {
            job_id: JOB_ID.to_owned(),
            durability_uncertain: false,
        };
        let uncertain = EnqueueOutcome::Published {
            job_id: JOB_ID.to_owned(),
            durability_uncertain: true,
        };
        assert!(existing.should_wake_controller());
        assert!(published.should_wake_controller());
        assert!(uncertain.should_wake_controller());
    }

    #[test]
    fn client_and_server_request_identifiers_have_distinct_exact_bounds() {
        assert!(!valid_client_request_id(&"a".repeat(31)));
        assert!(valid_client_request_id(&"a".repeat(32)));
        assert!(!valid_client_request_id(&"a".repeat(33)));
        assert!(!valid_client_request_id(&"A".repeat(32)));

        assert!(!valid_server_job_id(&"b".repeat(47)));
        assert!(valid_server_job_id(&"b".repeat(48)));
        assert!(!valid_server_job_id(&"b".repeat(49)));
        assert!(!valid_server_job_id(&"B".repeat(48)));
    }

    #[test]
    fn audit_transactions_are_server_nonce_unique_and_job_bound() {
        let binding = [7_u8; 32];
        let first = audit_transaction_id(&binding, REQUEST_ID, 10_000, &[1_u8; 16]).unwrap();
        let second = audit_transaction_id(&binding, REQUEST_ID, 10_000, &[2_u8; 16]).unwrap();
        assert_ne!(first, second);
        assert!(valid_server_job_id(&first));
        assert!(valid_server_job_id(&second));

        let mutation =
            parse_mutation_request(&request("remove-profile", json!({"name":"nightly"}))).unwrap();
        let job_bytes = canonical_job_bytes(
            JOB_ID,
            REQUEST_ID,
            "admin",
            1000,
            &binding,
            &first,
            &"a".repeat(64),
            10_000,
            &mutation.mutation,
        )
        .unwrap();
        assert_eq!(parse_job(&job_bytes).unwrap().audit_transaction, first);

        let mut tampered: Value = serde_json::from_slice(&job_bytes).unwrap();
        tampered["audit_transaction"] = json!("client-selected");
        assert!(parse_job(&serde_json::to_vec(&tampered).unwrap()).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn consumer_subreaper_supports_old_kernel_fallback_only() {
        assert!(classify_consumer_subreaper_result(0, None).unwrap());
        assert!(!classify_consumer_subreaper_result(-1, Some(libc::ENOSYS)).unwrap());
        assert!(!classify_consumer_subreaper_result(-1, Some(libc::EINVAL)).unwrap());
        assert_eq!(
            classify_consumer_subreaper_result(-1, Some(libc::EPERM))
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn random_nonce_uses_validated_urandom_when_getrandom_is_unavailable() {
        let mut output = [0_u8; 32];
        linux_files::fill_random_with(&mut output, |_| {
            Err(io::Error::from_raw_os_error(libc::ENOSYS))
        })
        .unwrap();
        assert_ne!(output, [0_u8; 32]);

        let mut failed = [7_u8; 32];
        assert!(
            linux_files::fill_random_with(&mut failed, |_| {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            })
            .is_err()
        );
        assert_eq!(failed, [0_u8; 32]);
    }

    #[test]
    fn generic_errors_never_echo_internal_or_secret_values() {
        let response = CgiResponse::error(BridgeError::unsafe_runtime());
        let text = String::from_utf8(response.body).unwrap();
        assert_eq!(response.status, 503);
        assert!(!text.contains(MANAGER_PATH));
        assert!(!text.contains("secret"));
        assert!(text.contains("Request could not be completed"));
        assert!(
            !String::from_utf8(generic_manager_result())
                .unwrap()
                .contains("not-in-job")
        );

        let service = CgiResponse::service_unavailable();
        let service_text = String::from_utf8(service.body).unwrap();
        assert_eq!(service.status, 503);
        assert!(service_text.contains("service_unavailable"));
        assert!(service_text.contains("Package Center"));
        assert!(service_text.contains("controller log"));
        assert!(!service_text.contains(API_SOCKET_PATH));
        assert!(!service_text.contains(PACKAGE_HOME));
    }

    #[test]
    fn cgi_diagnostics_distinguish_authentication_identity_and_bridge_failures() {
        let cases = [
            (
                CgiResponse::staged_error(
                    CgiFailureStage::Authentication,
                    BridgeError::new(ErrorKind::Unauthorized),
                ),
                401,
                "unauthorized",
                "dsm_authentication",
            ),
            (
                CgiResponse::staged_error(CgiFailureStage::Identity, BridgeError::unsafe_runtime()),
                503,
                "cgi_identity_unsafe",
                "cgi_identity",
            ),
            (
                CgiResponse::service_unavailable(),
                503,
                "service_unavailable",
                "bridge_connect",
            ),
            (
                CgiResponse::staged_error(
                    CgiFailureStage::BridgeIo,
                    BridgeError::new(ErrorKind::Unavailable),
                ),
                503,
                "bridge_io_unavailable",
                "bridge_io",
            ),
            (
                CgiResponse::staged_error(
                    CgiFailureStage::BridgeProtocol,
                    BridgeError::new(ErrorKind::Unavailable),
                ),
                503,
                "bridge_protocol_unavailable",
                "bridge_protocol",
            ),
        ];
        for (response, status, code, stage) in cases {
            assert_eq!(response.status, status);
            let payload: Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(payload["status"], status);
            assert_eq!(payload["code"], code);
            assert_eq!(payload["stage"], stage);
            let text = String::from_utf8(response.body).unwrap();
            for secret in [
                "id=session-secret",
                "SynoToken",
                API_SOCKET_PATH,
                PACKAGE_HOME,
            ] {
                assert!(!text.contains(secret));
            }
        }

        for (error, status, code) in [
            (
                BridgeError::unsafe_runtime(),
                503,
                "dsm_authentication_helper_unsafe",
            ),
            (
                BridgeError::new(ErrorKind::Unavailable),
                503,
                "dsm_authentication_helper_unavailable",
            ),
            (
                BridgeError::new(ErrorKind::Unavailable),
                503,
                "dsm_authentication_webapi_unavailable",
            ),
            (
                BridgeError::new(ErrorKind::Unauthorized),
                401,
                "dsm_authentication_rejected",
            ),
            (
                BridgeError::new(ErrorKind::Unauthorized),
                401,
                "dsm_authentication_quickconnect_unsupported",
            ),
            (
                BridgeError::new(ErrorKind::Unauthorized),
                401,
                "dsm_authentication_webapi_rejected",
            ),
            (
                BridgeError::new(ErrorKind::Forbidden),
                403,
                "dsm_authentication_forbidden",
            ),
            (
                BridgeError::new(ErrorKind::Forbidden),
                403,
                "dsm_authentication_webapi_forbidden",
            ),
        ] {
            let response = CgiResponse::failure(CgiFailure::coded(
                CgiFailureStage::Authentication,
                error,
                code,
            ));
            let payload: Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(response.status, status);
            assert_eq!(payload["status"], status);
            assert_eq!(payload["code"], code);
            assert_eq!(payload["stage"], "dsm_authentication");
        }
    }

    #[test]
    fn get_error_envelopes_survive_webman_as_successful_process_transports() {
        let unauthorized = CgiResponse::staged_error(
            CgiFailureStage::Authentication,
            BridgeError::new(ErrorKind::Unauthorized),
        );
        let unavailable = CgiResponse::service_unavailable();
        assert_eq!(unauthorized.status, 401);
        assert_eq!(unavailable.status, 503);

        let get_unauthorized = unauthorized.for_cgi_transport(true);
        let get_unavailable = unavailable.for_cgi_transport(true);
        for (response, application_status, stage) in [
            (get_unauthorized, 401, "dsm_authentication"),
            (get_unavailable, 503, "bridge_connect"),
        ] {
            assert_eq!(response.status, 200);
            let payload: Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(payload["schema"], "sdsync.dsm-error.v1");
            assert_eq!(payload["ok"], false);
            assert_eq!(payload["status"], application_status);
            assert_eq!(payload["stage"], stage);
        }

        let post_unavailable = CgiResponse::service_unavailable().for_cgi_transport(false);
        assert_eq!(post_unavailable.status, 503);
        assert_eq!(
            serde_json::from_slice::<Value>(&post_unavailable.body).unwrap()["status"],
            503
        );
        let expired_read = queued_expired_response("aabbcc").unwrap();
        assert_eq!(expired_read.status, 410);
        assert_eq!(expired_read.for_cgi_transport(true).status, 410);

        for body in [
            br#"not-json"#.to_vec(),
            br#"{"schema":"foreign.error.v1","ok":false,"status":503,"code":"unavailable","stage":"bridge_connect","message":"No."}"#.to_vec(),
            br#"{"schema":"sdsync.dsm-error.v1","ok":false,"status":503,"code":"unavailable","stage":"bridge_connect","message":"No.","foreign":true}"#.to_vec(),
        ] {
            let untrusted = CgiResponse { status: 503, body }.for_cgi_transport(true);
            assert_eq!(untrusted.status, 503);
        }
        assert_eq!(cgi_exit_code(true), ExitCode::SUCCESS);
        assert_eq!(cgi_exit_code(false), ExitCode::FAILURE);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_runtime_compiles_and_fails_closed() {
        let failure = run_cgi().unwrap_err();
        assert_eq!(failure.error.kind, ErrorKind::UnsafeRuntime);
        assert_eq!(failure.stage, CgiFailureStage::Identity);
        let request = Path::new("request");
        let response = Path::new("response");
        assert_eq!(
            run_consumer(request, response).unwrap_err().kind,
            ErrorKind::UnsafeRuntime
        );
    }
}
