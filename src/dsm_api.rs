//! Security boundary for the DSM dashboard CGI and its private controller queue.
//!
//! This module intentionally belongs only to the dedicated `sdsync-dsm-api`
//! binary.  It is not exported by the library and is never selected through
//! the main CLI's argv[0] or command dispatch.

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
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

const PACKAGE_ROOT: &str = "/var/packages/synology-drive-sync/target";
const PACKAGE_HOME: &str = "/var/packages/synology-drive-sync/home";
const PACKAGE_VAR: &str = "/var/packages/synology-drive-sync/var";
const MANAGER_PATH: &str = "/var/packages/synology-drive-sync/target/bin/sdsync-dsm";
const AUTHENTICATE_PATH: &str = "/usr/syno/synoman/webman/modules/authenticate.cgi";
const CONTROL_ROOT: &str = "/var/packages/synology-drive-sync/var/control";
const REQUESTS_DIR: &str = "/var/packages/synology-drive-sync/var/control/requests";
const PROCESSING_DIR: &str = "/var/packages/synology-drive-sync/var/control/processing";
const RESPONSES_DIR: &str = "/var/packages/synology-drive-sync/var/control/responses";
const CSRF_KEY_PATH: &str = "/var/packages/synology-drive-sync/var/control/csrf.key";
const ENQUEUE_LOCK_PATH: &str = "/var/packages/synology-drive-sync/var/control/enqueue.lock";
const WEB_IDENTITY: &str = "http";
const ADMINISTRATORS_GROUP: &str = "administrators";
const CGI_ORIGIN_VARIABLES: &[&str] = &[
    "REQUEST_METHOD",
    "GATEWAY_INTERFACE",
    "QUERY_STRING",
    "CONTENT_LENGTH",
    "CONTENT_TYPE",
    "HTTP_COOKIE",
    "HTTP_X_SYNO_TOKEN",
    "HTTP_X_SDSYNC_CSRF",
    "REMOTE_ADDR",
    "SERVER_ADDR",
    "SERVER_NAME",
    "SERVER_PORT",
    "SCRIPT_NAME",
    "SCRIPT_FILENAME",
    "DOCUMENT_ROOT",
];

const MAX_QUERY_BYTES: usize = 4 * 1024;
const MAX_COOKIE_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 1024;
const MAX_CSRF_BYTES: usize = 256;
const MAX_POST_BODY_BYTES: usize = 64 * 1024;
const MAX_JOB_BYTES: usize = 64 * 1024;
const MAX_MANAGER_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_AUTH_OUTPUT_BYTES: usize = 512;
const MAX_SECRET_BYTES: usize = 4096;
const MAX_JOB_AGE_SECONDS: u64 = 24 * 60 * 60;
const RESULT_RETENTION_SECONDS: u64 = 60 * 60;
const MAX_OUTSTANDING_JOBS: usize = 256;
const CSRF_LIFETIME_SECONDS: u64 = 5 * 60;
const CLOCK_SKEW_SECONDS: u64 = 30;
const SERVER_JOB_ID_BYTES: usize = 24;

type HmacSha256 = Hmac<Sha256>;
type BridgeResult<T> = Result<T, BridgeError>;

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct ControlPaths<'a> {
    root: &'a Path,
    requests: &'a Path,
    processing: &'a Path,
    responses: &'a Path,
    csrf_key: &'a Path,
    enqueue_lock: &'a Path,
}

#[cfg(target_os = "linux")]
struct EnqueueRequest<'a> {
    package_uid: u32,
    client_request_id: &'a str,
    requested_by: &'a str,
    session_binding: &'a [u8; 32],
    issued_at_epoch: u64,
    mutation: &'a Mutation,
    secret: Option<&'a [u8]>,
}

#[cfg(target_os = "linux")]
impl ControlPaths<'static> {
    fn production() -> Self {
        Self {
            root: Path::new(CONTROL_ROOT),
            requests: Path::new(REQUESTS_DIR),
            processing: Path::new(PROCESSING_DIR),
            responses: Path::new(RESPONSES_DIR),
            csrf_key: Path::new(CSRF_KEY_PATH),
            enqueue_lock: Path::new(ENQUEUE_LOCK_PATH),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorKind {
    BadRequest,
    Unauthorized,
    Forbidden,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct IdentityState {
    real_uid: u32,
    effective_uid: u32,
    executable_uid: u32,
    executable_mode: u32,
}

struct CgiEnvironment {
    method: String,
    content_length: Option<String>,
    content_type: Option<String>,
    query: Zeroizing<String>,
    cookie: Zeroizing<String>,
    synology_token_header: Option<Zeroizing<String>>,
    csrf_header: Option<Zeroizing<String>>,
    remote_address: Option<String>,
    server_address: Option<String>,
    server_name: Option<String>,
    server_port: Option<String>,
    https: Option<String>,
    transfer_encoding: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReadAction {
    Csrf,
    Snapshot,
    Logs { lines: u16, source: LogSource },
    Activity { lines: u16 },
    Result { job_id: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogSource {
    All,
    Controller,
    Scheduler,
    Sync,
}

impl LogSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Controller => "controller",
            Self::Scheduler => "scheduler",
            Self::Sync => "sync",
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
    method: String,
    query: Zeroizing<String>,
    cookie: Zeroizing<String>,
    synology_token: Zeroizing<String>,
    remote_address: Option<String>,
    server_address: Option<String>,
    server_name: Option<String>,
    server_port: Option<String>,
    https: Option<String>,
}

struct AuthenticatedSession {
    username: String,
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
    session_binding: &'a str,
    issued_at_epoch: u64,
    operation: &'a str,
    #[serde(borrow)]
    arguments: &'a RawValue,
}

struct ParsedJob {
    request_id: String,
    client_request_id: String,
    requested_by: String,
    session_binding: [u8; 32],
    issued_at_epoch: u64,
    mutation: Mutation,
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
    client_request_id: &'a str,
    requested_by: &'a str,
    session_binding: &'a str,
    issued_at_epoch: u64,
    completed_at_epoch: u64,
    #[serde(borrow)]
    result: &'a RawValue,
}

struct ParsedQueuedResponse {
    session_binding: [u8; 32],
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
    interval_seconds: u32,
    weekdays: Vec<u8>,
    time_window_start: String,
    time_window_end: String,
    debounce_seconds: u32,
    retry_count: u8,
    retry_backoff_seconds: u32,
    poll_seconds: u32,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OperationalActionArgs {
    kind: OperationalActionKind,
    scope: String,
    write_test: Option<bool>,
    allow_delete: Option<bool>,
    max_total_delete: Option<u64>,
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
    Schedule(ScheduleArgs),
    Routine(RoutineArgs),
    RemoveRoutine(NameArgs),
    AlertPolicy(AlertPolicyArgs),
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
            Self::Schedule(_) => "schedule",
            Self::Routine(_) => "routine",
            Self::RemoveRoutine(_) => "remove-routine",
            Self::AlertPolicy(_) => "alert-policy",
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
            Self::Schedule(value) => serde_json::to_value(value),
            Self::Routine(value) => serde_json::to_value(value),
            Self::AlertPolicy(value) => serde_json::to_value(value),
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
        cookie: Zeroizing::new(required("HTTP_COOKIE", MAX_COOKIE_BYTES)?),
        synology_token_header: optional("HTTP_X_SYNO_TOKEN", MAX_TOKEN_BYTES)?.map(Zeroizing::new),
        csrf_header: optional("HTTP_X_SDSYNC_CSRF", MAX_CSRF_BYTES)?.map(Zeroizing::new),
        remote_address: optional("REMOTE_ADDR", 128)?,
        server_address: optional("SERVER_ADDR", 128)?,
        server_name: optional("SERVER_NAME", 255)?,
        server_port: optional("SERVER_PORT", 8)?,
        https: optional("HTTPS", 16)?,
        transfer_encoding: optional("HTTP_TRANSFER_ENCODING", 64)?,
    })
}

fn validate_environment_value(value: &str, maximum: usize) -> BridgeResult<()> {
    if value.len() > maximum
        || value
            .bytes()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_http_request(mut environment: CgiEnvironment) -> BridgeResult<ValidatedHttpRequest> {
    if environment.transfer_encoding.is_some() {
        return Err(BridgeError::bad_request());
    }
    if environment.cookie.is_empty() {
        return Err(BridgeError::new(ErrorKind::Unauthorized));
    }

    let original_query = Zeroizing::new(environment.query.to_string());
    let mut query = parse_urlencoded(&environment.query)?;
    let query_token = query.remove("SynoToken").map(Zeroizing::new);
    let synology_token =
        choose_synology_token(environment.synology_token_header.take(), query_token)?;

    let authentication = AuthenticationInputs {
        method: environment.method.clone(),
        query: original_query,
        cookie: environment.cookie,
        synology_token,
        remote_address: environment.remote_address,
        server_address: environment.server_address,
        server_name: environment.server_name,
        server_port: environment.server_port,
        https: environment.https,
    };

    match environment.method.as_str() {
        "GET" => {
            if environment
                .content_length
                .as_deref()
                .is_some_and(|value| value != "0")
                || environment.content_type.is_some()
                || environment.csrf_header.is_some()
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
) -> BridgeResult<Zeroizing<String>> {
    let selected = match (header, query) {
        (Some(header), Some(query)) => {
            if !constant_time_equal(header.as_bytes(), query.as_bytes()) {
                return Err(BridgeError::new(ErrorKind::Forbidden));
            }
            header
        }
        (Some(header), None) => header,
        (None, Some(query)) => query,
        (None, None) => return Err(BridgeError::new(ErrorKind::Forbidden)),
    };
    if selected.is_empty()
        || selected.len() > MAX_TOKEN_BYTES
        || selected
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    Ok(selected)
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
        "logs" => {
            let lines = parse_lines(query.remove("lines"))?;
            let source = match query.remove("source").as_deref().unwrap_or("all") {
                "all" => LogSource::All,
                "controller" => LogSource::Controller,
                "scheduler" => LogSource::Scheduler,
                "sync" => LogSource::Sync,
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
    if request.schema != "sdsync.dsm-request.v1" || !valid_request_id(request.request_id) {
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
            validate_name(&arguments.name)?;
            (Mutation::RemoveProfile(arguments), None)
        }
        "set-default" => {
            let arguments: NameArgs = parse_arguments(request.arguments)?;
            validate_name(&arguments.name)?;
            (Mutation::SetDefault(arguments), None)
        }
        "set-secret" => {
            let mut arguments: SecretRequestArgs = parse_arguments(request.arguments)?;
            validate_name(&arguments.profile)?;
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
            validate_name(&arguments.name)?;
            (Mutation::RemoveRoutine(arguments), None)
        }
        "alert-policy" => {
            let arguments: AlertPolicyArgs = parse_arguments(request.arguments)?;
            validate_alert_policy(&arguments)?;
            (Mutation::AlertPolicy(arguments), None)
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
        || !valid_request_id(job.client_request_id)
        || !valid_authenticated_username(job.requested_by)
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
            validate_name(&value.name)?;
            Mutation::RemoveProfile(value)
        }
        "set-default" => {
            let value: NameArgs = parse_arguments(job.arguments)?;
            validate_name(&value.name)?;
            Mutation::SetDefault(value)
        }
        "set-secret" => {
            let value: SecretJobArgs = parse_arguments(job.arguments)?;
            validate_name(&value.profile)?;
            Mutation::SetSecret(value)
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
            validate_name(&value.name)?;
            Mutation::RemoveRoutine(value)
        }
        "alert-policy" => {
            let value: AlertPolicyArgs = parse_arguments(job.arguments)?;
            validate_alert_policy(&value)?;
            Mutation::AlertPolicy(value)
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
        session_binding,
        issued_at_epoch: job.issued_at_epoch,
        mutation,
    })
}

fn valid_request_id(value: &str) -> bool {
    (32..=64).contains(&value.len())
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

fn validate_configure_profile(value: &ConfigureProfileArgs) -> BridgeResult<()> {
    validate_name(&value.name)?;
    validate_bounded_text(&value.source, 4096, false)?;
    if !value.source.starts_with('/') || contains_dot_segment(&value.source) {
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
        || value.retries > 5
        || value.timeout_seconds == 0
        || value.timeout_seconds > 86_400
        || value.connect_timeout_seconds == 0
        || value.connect_timeout_seconds > 600
        || value.max_rate_bytes_per_second == Some(0)
        || value.verbosity > 2
        || (value.quiet && value.verbosity != 0)
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
    if !(60..=2_592_000).contains(&value.interval_seconds) {
        return Err(BridgeError::bad_request());
    }
    Ok(())
}

fn validate_routine(value: &RoutineArgs) -> BridgeResult<()> {
    validate_name(&value.profile)?;
    if !(60..=2_592_000).contains(&value.interval_seconds)
        || !(1..=3600).contains(&value.debounce_seconds)
        || value.retry_count > 5
        || !(10..=86_400).contains(&value.retry_backoff_seconds)
        || !(5..=3600).contains(&value.poll_seconds)
        || value.weekdays.is_empty()
        || value.weekdays.len() > 7
        || value.depends_on.len() > 64
        || !valid_clock_time(&value.time_window_start)
        || !valid_clock_time(&value.time_window_end)
    {
        return Err(BridgeError::bad_request());
    }
    let mut weekdays = BTreeSet::new();
    if value
        .weekdays
        .iter()
        .any(|weekday| !(1..=7).contains(weekday) || !weekdays.insert(*weekday))
    {
        return Err(BridgeError::bad_request());
    }
    let mut dependencies = BTreeSet::new();
    for dependency in &value.depends_on {
        validate_name(dependency)?;
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

fn validate_operational_action(value: &OperationalActionArgs) -> BridgeResult<()> {
    if value.scope != "all" {
        validate_name(&value.scope)?;
    }
    match value.kind {
        OperationalActionKind::Doctor => {
            if value.allow_delete.is_some() || value.max_total_delete.is_some() {
                return Err(BridgeError::bad_request());
            }
        }
        OperationalActionKind::Plan | OperationalActionKind::Run => {
            if value.write_test.is_some() {
                return Err(BridgeError::bad_request());
            }
            let _allow_delete = value.allow_delete.ok_or_else(BridgeError::bad_request)?;
            if value.scope == "all" {
                if value.max_total_delete.is_none() {
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
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@' | b'\\')
        })
}

fn validate_cgi_identity(state: &IdentityState, web_uid: u32) -> BridgeResult<()> {
    let regular_file = state.executable_mode & 0o170_000 == 0o100_000;
    if state.real_uid == 0
        || state.effective_uid == 0
        || state.real_uid != web_uid
        || state.effective_uid == state.real_uid
        || state.executable_uid != state.effective_uid
        || !regular_file
        || state.executable_mode & 0o4000 == 0
        || state.executable_mode & 0o022 != 0
    {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(())
}

fn validate_consumer_identity(state: &IdentityState) -> BridgeResult<()> {
    let regular_file = state.executable_mode & 0o170_000 == 0o100_000;
    if state.real_uid == 0
        || state.real_uid != state.effective_uid
        || state.executable_uid != state.effective_uid
        || !regular_file
        || state.executable_mode & 0o6000 != 0
        || state.executable_mode & 0o022 != 0
    {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(())
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
    if primary_gid != administrator_gid && !supplementary_groups.contains(&administrator_gid) {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    Ok(())
}

trait UidTransition {
    fn set_all_uids(&self, uid: u32) -> io::Result<()>;
    fn all_uids(&self) -> io::Result<(u32, u32, u32)>;
}

fn permanently_drop_with(transition: &impl UidTransition, package_uid: u32) -> BridgeResult<()> {
    if package_uid == 0 {
        return Err(BridgeError::unsafe_runtime());
    }
    transition
        .set_all_uids(package_uid)
        .map_err(|_| BridgeError::unsafe_runtime())?;
    let (real, effective, saved) = transition
        .all_uids()
        .map_err(|_| BridgeError::unsafe_runtime())?;
    if real != package_uid || effective != package_uid || saved != package_uid {
        return Err(BridgeError::unsafe_runtime());
    }
    Ok(())
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
    let mut variables = vec![
        (
            OsString::from("PATH"),
            OsString::from("/usr/sbin:/usr/bin:/sbin:/bin"),
        ),
        (OsString::from("LANG"), OsString::from("C")),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (
            OsString::from("REQUEST_METHOD"),
            OsString::from(&inputs.method),
        ),
        (
            OsString::from("QUERY_STRING"),
            OsString::from(inputs.query.as_str()),
        ),
        (
            OsString::from("HTTP_COOKIE"),
            OsString::from(inputs.cookie.as_str()),
        ),
        (
            OsString::from("HTTP_X_SYNO_TOKEN"),
            OsString::from(inputs.synology_token.as_str()),
        ),
    ];
    for (name, value) in [
        ("REMOTE_ADDR", inputs.remote_address.as_ref()),
        ("SERVER_ADDR", inputs.server_address.as_ref()),
        ("SERVER_NAME", inputs.server_name.as_ref()),
        ("SERVER_PORT", inputs.server_port.as_ref()),
        ("HTTPS", inputs.https.as_ref()),
    ] {
        if let Some(value) = value {
            variables.push((OsString::from(name), OsString::from(value)));
        }
    }
    variables
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

fn session_binding(username: &str, uid: u32, cookie: &str, synology_token: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"sdsync-dsm-session-v1\0");
    update_length_prefixed(&mut digest, username.as_bytes());
    digest.update(uid.to_be_bytes());
    update_length_prefixed(&mut digest, cookie.as_bytes());
    update_length_prefixed(&mut digest, synology_token.as_bytes());
    digest.finalize().into()
}

fn update_length_prefixed(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

#[cfg(target_os = "linux")]
mod linux_runtime {
    use super::*;
    use std::os::linux::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    use std::ptr;

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

    pub(super) fn web_uid() -> BridgeResult<u32> {
        lookup_user(WEB_IDENTITY).map(|entry| entry.0)
    }

    pub(super) fn authenticate_and_authorize(
        inputs: &AuthenticationInputs,
        state: &IdentityState,
    ) -> BridgeResult<AuthenticatedSession> {
        validate_trusted_executable(Path::new(AUTHENTICATE_PATH), 0)?;
        let mut command = Command::new(AUTHENTICATE_PATH);
        command
            .env_clear()
            .envs(authentication_command_environment(inputs))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let caller_uid = state.real_uid;
        // SAFETY: pre_exec performs only the async-signal-safe setresuid call.
        // The requested UID is the process's current real UID, so this is a
        // permanent privilege drop in the authentication child.
        unsafe {
            command.pre_exec(move || {
                if libc::setresuid(caller_uid, caller_uid, caller_uid) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = capture_stdout(&mut command, MAX_AUTH_OUTPUT_BYTES)?;
        if !output.status_success {
            return Err(BridgeError::new(ErrorKind::Unauthorized));
        }
        let username = parse_authentication_output(&output.stdout)?;
        let (uid, primary_gid) = lookup_user(&username)?;
        let administrator_gid = lookup_group(ADMINISTRATORS_GROUP)?;
        let groups = lookup_groups(&username, primary_gid)?;
        authorize_admin_membership(uid, primary_gid, administrator_gid, &groups)?;
        let binding = session_binding(&username, uid, &inputs.cookie, &inputs.synology_token);
        Ok(AuthenticatedSession { username, binding })
    }

    pub(super) fn permanently_drop_to_package_uid(package_uid: u32) -> BridgeResult<()> {
        permanently_drop_with(&LibcUidTransition, package_uid)
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

    fn validate_trusted_executable(path: &Path, expected_uid: u32) -> BridgeResult<()> {
        let metadata = fs::symlink_metadata(path).map_err(|_| BridgeError::unsafe_runtime())?;
        if !metadata.file_type().is_file()
            || metadata.st_uid() != expected_uid
            || metadata.st_mode() & 0o022 != 0
            || metadata.st_mode() & 0o111 == 0
        {
            return Err(BridgeError::unsafe_runtime());
        }
        Ok(())
    }

    struct LibcUidTransition;

    impl UidTransition for LibcUidTransition {
        fn set_all_uids(&self, uid: u32) -> io::Result<()> {
            // SAFETY: all requested IDs equal the current setuid effective or
            // saved package UID; the kernel validates that invariant.
            if unsafe { libc::setresuid(uid, uid, uid) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn all_uids(&self) -> io::Result<(u32, u32, u32)> {
            let (mut real, mut effective, mut saved) = (0, 0, 0);
            // SAFETY: getresuid writes to three valid local uid_t objects.
            if unsafe { libc::getresuid(&mut real, &mut effective, &mut saved) } == 0 {
                Ok((real, effective, saved))
            } else {
                Err(io::Error::last_os_error())
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
    stdout: Vec<u8>,
}

fn capture_stdout(command: &mut Command, maximum: usize) -> BridgeResult<CapturedOutput> {
    let mut child = command
        .spawn()
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    let mut stdout = child.stdout.take().ok_or_else(BridgeError::internal)?;
    let mut bytes = Vec::with_capacity(maximum.min(8192));
    stdout
        .by_ref()
        .take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    if bytes.len() > maximum {
        let _ = child.kill();
        let _ = child.wait();
        bytes.zeroize();
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let status = child
        .wait()
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    Ok(CapturedOutput {
        status_success: status.success(),
        stdout: bytes,
    })
}

fn issue_csrf_token(
    key: &[u8],
    session_binding: &[u8; 32],
    now: u64,
    nonce: &[u8; 16],
) -> BridgeResult<String> {
    if key.len() != 32 {
        return Err(BridgeError::unsafe_runtime());
    }
    let expires = now
        .checked_add(CSRF_LIFETIME_SECONDS)
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
) -> BridgeResult<()> {
    if token.len() > MAX_CSRF_BYTES || key.len() != 32 {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let components: Vec<&str> = token.split('.').collect();
    if components.len() != 5 || components[0] != "v1" {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let issued =
        parse_canonical_u64(components[1]).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    let expires =
        parse_canonical_u64(components[2]).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    if expires.checked_sub(issued) != Some(CSRF_LIFETIME_SECONDS)
        || issued > now.saturating_add(CLOCK_SKEW_SECONDS)
        || expires <= now
    {
        return Err(BridgeError::new(ErrorKind::Forbidden));
    }
    let nonce = hex_decode_exact::<16>(components[3])
        .ok_or_else(|| BridgeError::new(ErrorKind::Forbidden))?;
    let supplied_signature = hex_decode_exact::<32>(components[4])
        .ok_or_else(|| BridgeError::new(ErrorKind::Forbidden))?;
    let nonce_hex = hex_encode(&nonce);
    let message = csrf_message(issued, expires, &nonce_hex, session_binding);
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| BridgeError::new(ErrorKind::Forbidden))?;
    mac.update(message.as_bytes());
    let expected = mac.finalize().into_bytes();
    if !constant_time_equal(&expected, &supplied_signature) {
        return Err(BridgeError::new(ErrorKind::Forbidden));
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
        ReadAction::Logs { lines, .. } => vec![
            "api".into(),
            "logs".into(),
            "--lines".into(),
            lines.to_string().into(),
        ],
        ReadAction::Activity { lines } => vec![
            "api".into(),
            "activity".into(),
            "--lines".into(),
            lines.to_string().into(),
        ],
        ReadAction::Csrf | ReadAction::Result { .. } => return Err(BridgeError::internal()),
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
            // These values are deliberately fixed by the bridge: callers may
            // not redirect logs or select a format/path interpreted by a shell.
            push_pair(&mut arguments, "--log-format", "json");
            push_pair(&mut arguments, "--progress", "never");
            push_pair(&mut arguments, "--output", "human");
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
            push_pair(
                &mut arguments,
                "--interval",
                &value.interval_seconds.to_string(),
            );
            let weekdays = value
                .weekdays
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(",");
            push_pair(&mut arguments, "--weekdays", &weekdays);
            push_pair(
                &mut arguments,
                "--time-window-start",
                &value.time_window_start,
            );
            push_pair(&mut arguments, "--time-window-end", &value.time_window_end);
            push_pair(
                &mut arguments,
                "--debounce-seconds",
                &value.debounce_seconds.to_string(),
            );
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
                "--poll-seconds",
                &value.poll_seconds.to_string(),
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
        Mutation::Action(value) => {
            arguments.push("action".into());
            push_pair(&mut arguments, "--kind", value.kind.as_str());
            push_pair(&mut arguments, "--scope", &value.scope);
            match value.kind {
                OperationalActionKind::Doctor => push_pair(
                    &mut arguments,
                    "--write-test",
                    bool_text(value.write_test.unwrap_or(false)),
                ),
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
) -> BridgeResult<Vec<u8>> {
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    let expected_schema = match action {
        ReadAction::Snapshot => "sdsync.dsm-api.v1",
        ReadAction::Logs { .. } => "sdsync.dsm-logs.v1",
        ReadAction::Activity { .. } => "sdsync.dsm-activity.v1",
        ReadAction::Csrf | ReadAction::Result { .. } => return Err(BridgeError::internal()),
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
            root.insert(
                "capabilities".to_owned(),
                json!({
                    "mutations": true,
                    "secrets": true,
                    "write_test": true,
                    "private_queue": true,
                }),
            );
        }
        ReadAction::Logs { source, .. } if *source != LogSource::All => {
            let logs = value
                .get_mut("logs")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
            logs.retain(|entry| {
                entry.get("source").and_then(Value::as_str) == Some(source.as_str())
            });
        }
        ReadAction::Logs { .. }
        | ReadAction::Activity { .. }
        | ReadAction::Csrf
        | ReadAction::Result { .. } => {}
    }
    serde_json::to_vec(&value).map_err(|_| BridgeError::internal())
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

fn canonical_job_bytes(
    request_id: &str,
    client_request_id: &str,
    requested_by: &str,
    session_binding: &[u8; 32],
    issued_at_epoch: u64,
    mutation: &Mutation,
) -> BridgeResult<Vec<u8>> {
    let value = json!({
        "schema": "sdsync.dsm-job.v1",
        "request_id": request_id,
        "client_request_id": client_request_id,
        "requested_by": requested_by,
        "session_binding": hex_encode(session_binding),
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

fn validate_consumer_paths(request: &Path, response: &Path) -> BridgeResult<String> {
    if request.parent() != Some(Path::new(PROCESSING_DIR))
        || response.parent() != Some(Path::new(RESPONSES_DIR))
    {
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
    let request_id = &request_name[..request_name.len() - ".json".len()];
    if !valid_server_job_id(request_id) {
        return Err(BridgeError::bad_request());
    }
    Ok(request_id.to_owned())
}

fn parse_manager_result(bytes: &[u8], exact_secret: Option<&[u8]>) -> BridgeResult<Value> {
    if bytes.is_empty()
        || bytes.len() > MAX_MANAGER_OUTPUT_BYTES
        || exact_secret.is_some_and(|secret| contains_bytes(bytes, secret))
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    if json_contains_sensitive_value(&value, exact_secret) {
        return Err(BridgeError::new(ErrorKind::Unavailable));
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
        if scope != "all" && validate_name(scope).is_err() {
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
) -> BridgeResult<Vec<u8>> {
    if completed_at_epoch < job.issued_at_epoch {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let bytes = serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-queued-response.v1",
        "job_id": job.request_id,
        "client_request_id": job.client_request_id,
        "requested_by": job.requested_by,
        "session_binding": hex_encode(&job.session_binding),
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
    if response.schema != "sdsync.dsm-queued-response.v1"
        || response.job_id != expected_job_id
        || !valid_server_job_id(response.job_id)
        || !valid_request_id(response.client_request_id)
        || !valid_authenticated_username(response.requested_by)
        || response.completed_at_epoch < response.issued_at_epoch
    {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let session_binding = hex_decode_exact::<32>(response.session_binding)
        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
    let result = parse_manager_result(response.result.get().as_bytes(), None)?;
    Ok(ParsedQueuedResponse {
        session_binding,
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

fn run_manager(
    arguments: &[OsString],
    mut secret: Option<&mut Zeroizing<Vec<u8>>>,
) -> BridgeResult<CapturedOutput> {
    #[cfg(target_os = "linux")]
    linux_runtime::validate_package_manager()?;
    #[cfg(not(target_os = "linux"))]
    return Err(BridgeError::unsafe_runtime());

    let mut command = Command::new(MANAGER_PATH);
    command
        .args(arguments)
        .env_clear()
        .envs(manager_command_environment())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if secret.is_some() {
        command
            .env("SDSYNC_DSM_EXACT_SECRET_INPUT", "true")
            .stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    let mut child = command
        .spawn()
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    if let Some(secret) = secret.as_mut() {
        let mut stdin = child.stdin.take().ok_or_else(BridgeError::internal)?;
        let write_result = stdin
            .write_all(secret)
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush());
        drop(stdin);
        if write_result.is_err() {
            secret.zeroize();
            let _ = child.kill();
            let _ = child.wait();
            return Err(BridgeError::new(ErrorKind::Unavailable));
        }
    }
    let mut stdout = child.stdout.take().ok_or_else(BridgeError::internal)?;
    let mut bytes = Vec::with_capacity(8192);
    stdout
        .by_ref()
        .take((MAX_MANAGER_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    if bytes.len() > MAX_MANAGER_OUTPUT_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        bytes.zeroize();
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let status = child
        .wait()
        .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
    Ok(CapturedOutput {
        status_success: status.success(),
        stdout: bytes,
    })
}

#[cfg(target_os = "linux")]
mod linux_files {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::fd::AsRawFd;
    use std::os::linux::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;

    const NOFOLLOW_CLOEXEC: i32 = libc::O_NOFOLLOW | libc::O_CLOEXEC;

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

    pub(super) fn enqueue(
        paths: &ControlPaths<'_>,
        request: EnqueueRequest<'_>,
    ) -> BridgeResult<String> {
        let EnqueueRequest {
            package_uid,
            client_request_id,
            requested_by,
            session_binding,
            issued_at_epoch,
            mutation,
            secret,
        } = request;
        validate_private_directory(paths.root, package_uid)?;
        validate_private_directory(paths.requests, package_uid)?;
        let mut enqueue_lock = open_enqueue_lock(paths, package_uid)?;
        validate_outstanding_queue_capacity(paths, package_uid)?;
        let request_id = next_job_id(&mut enqueue_lock)?;
        let job = canonical_job_bytes(
            &request_id,
            client_request_id,
            requested_by,
            session_binding,
            issued_at_epoch,
            mutation,
        )?;
        if job.is_empty() || job.len() > MAX_JOB_BYTES {
            return Err(BridgeError::bad_request());
        }
        let final_job = paths.requests.join(format!("{request_id}.json"));
        let temporary_job = paths
            .requests
            .join(format!(".{request_id}.{}.job", std::process::id()));
        let secret_path = paths.requests.join(format!("{request_id}.secret"));
        if final_job.exists() || secret_path.exists() {
            return Err(BridgeError::new(ErrorKind::Conflict));
        }

        if let Some(secret) = secret {
            if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
                return Err(BridgeError::bad_request());
            }
            let mut secret_line = Zeroizing::new(Vec::with_capacity(secret.len() + 1));
            secret_line.extend_from_slice(secret);
            secret_line.push(b'\n');
            create_private_file(&secret_path, package_uid, &secret_line)?;
        }

        let publish_result = (|| {
            create_private_file(&temporary_job, package_uid, &job)?;
            fs::hard_link(&temporary_job, &final_job).map_err(|error| map_create_error(&error))?;
            fs::remove_file(&temporary_job).map_err(|_| BridgeError::unsafe_runtime())?;
            sync_directory(paths.requests)
        })();
        if publish_result.is_err() {
            let _ = fs::remove_file(&temporary_job);
            if secret.is_some() {
                let _ = fs::remove_file(&secret_path);
            }
        }
        publish_result?;
        Ok(request_id)
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
        let temporary = paths
            .responses
            .join(format!(".{request_id}.{}.response", std::process::id()));
        if path.exists() {
            return Err(BridgeError::new(ErrorKind::Conflict));
        }
        let result = (|| {
            create_private_file(&temporary, package_uid, response)?;
            fs::hard_link(&temporary, path).map_err(|error| map_create_error(&error))?;
            fs::remove_file(&temporary).map_err(|_| BridgeError::unsafe_runtime())?;
            sync_directory(paths.responses)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
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

    fn validate_outstanding_queue_capacity(
        paths: &ControlPaths<'_>,
        package_uid: u32,
    ) -> BridgeResult<()> {
        let mut jobs = 0_usize;
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
                    (MAX_SECRET_BYTES + 1) as u64
                };
                if !metadata.file_type().is_file()
                    || metadata.st_uid() != package_uid
                    || metadata.st_mode() & 0o777 != 0o600
                    || metadata.len() > maximum
                {
                    return Err(BridgeError::unsafe_runtime());
                }
                if is_job {
                    jobs = jobs
                        .checked_add(1)
                        .ok_or_else(|| BridgeError::new(ErrorKind::Unavailable))?;
                }
            }
        }
        if jobs >= MAX_OUTSTANDING_JOBS {
            return Err(BridgeError::new(ErrorKind::Conflict));
        }
        Ok(())
    }

    fn next_job_id(lock: &mut File) -> BridgeResult<String> {
        lock.seek(SeekFrom::Start(0))
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let mut previous_text = String::new();
        Read::by_ref(lock)
            .take(17)
            .read_to_string(&mut previous_text)
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let previous = if previous_text.is_empty() {
            0
        } else if previous_text.len() == 16
            && previous_text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            u64::from_str_radix(&previous_text, 16).map_err(|_| BridgeError::unsafe_runtime())?
        } else {
            return Err(BridgeError::unsafe_runtime());
        };
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        let wall_clock = u64::try_from(elapsed.as_micros())
            .map_err(|_| BridgeError::new(ErrorKind::Unavailable))?;
        let sequence = next_enqueue_sequence(previous, wall_clock)?;
        let encoded_sequence = format!("{sequence:016x}");
        lock.seek(SeekFrom::Start(0))
            .and_then(|_| lock.set_len(0))
            .and_then(|_| lock.write_all(encoded_sequence.as_bytes()))
            .and_then(|_| lock.sync_all())
            .map_err(|_| BridgeError::unsafe_runtime())?;
        let mut random = [0_u8; 16];
        fill_random(&mut random)?;
        Ok(sortable_job_id(sequence, &random))
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

    fn read_exact_private_file(
        path: &Path,
        package_uid: u32,
        maximum: usize,
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
            || metadata.st_mode() & 0o777 != 0o600
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
        let mut written = 0;
        while written < output.len() {
            // SAFETY: the pointer targets the unwritten portion of a valid
            // mutable slice and its remaining length is supplied exactly.
            let result = unsafe {
                libc::getrandom(
                    output[written..].as_mut_ptr().cast(),
                    output.len() - written,
                    0,
                )
            };
            if result > 0 {
                written += result as usize;
            } else if result == -1
                && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
            {
                continue;
            } else {
                output.zeroize();
                return Err(BridgeError::new(ErrorKind::Unavailable));
            }
        }
        Ok(())
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

    fn error(error: BridgeError) -> Self {
        let (status, code) = match error.kind {
            ErrorKind::BadRequest => (400, "invalid_request"),
            ErrorKind::Unauthorized => (401, "unauthorized"),
            ErrorKind::Forbidden => (403, "forbidden"),
            ErrorKind::MethodNotAllowed => (405, "method_not_allowed"),
            ErrorKind::UnsupportedMediaType => (415, "unsupported_media_type"),
            ErrorKind::PayloadTooLarge => (413, "payload_too_large"),
            ErrorKind::Conflict => (409, "conflict"),
            ErrorKind::UnsafeRuntime | ErrorKind::Unavailable => (503, "unavailable"),
            ErrorKind::Internal => (500, "internal_error"),
        };
        let body = serde_json::to_vec(&json!({
            "schema": "sdsync.dsm-error.v1",
            "ok": false,
            "code": code,
            "message": "Request could not be completed.",
        }))
        .unwrap_or_else(|_| {
            br#"{"schema":"sdsync.dsm-error.v1","ok":false,"code":"internal_error","message":"Request could not be completed."}"#
                .to_vec()
        });
        Self { status, body }
    }
}

pub(crate) fn main_entry() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() {
        let response = match run_cgi() {
            Ok(response) => response,
            Err(error) => CgiResponse::error(error),
        };
        let success = response.status < 400;
        if write_cgi_response(&response).is_err() || !success {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    } else if arguments.len() == 3 && arguments[0] == "--consume-job" {
        let request = PathBuf::from(&arguments[1]);
        let response = PathBuf::from(&arguments[2]);
        match run_consumer(&request, &response) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        }
    } else if std::env::var_os("REQUEST_METHOD").is_some() {
        let response = CgiResponse::error(BridgeError::bad_request());
        let _ = write_cgi_response(&response);
        ExitCode::FAILURE
    } else {
        ExitCode::FAILURE
    }
}

fn run_cgi() -> BridgeResult<CgiResponse> {
    #[cfg(not(target_os = "linux"))]
    return Err(BridgeError::unsafe_runtime());

    #[cfg(target_os = "linux")]
    {
        let environment = process_environment()?;
        let identity = linux_runtime::identity_state()?;
        let web_uid = linux_runtime::web_uid()?;
        validate_cgi_identity(&identity, web_uid)?;
        let request = validate_http_request(environment)?;
        linux_runtime::clear_environment()?;

        let authentication = match &request {
            ValidatedHttpRequest::Get { authentication, .. }
            | ValidatedHttpRequest::Post { authentication, .. } => authentication,
        };
        let session = linux_runtime::authenticate_and_authorize(authentication, &identity)?;
        // The parent CGI permanently adopts the package UID after DSM auth.
        // Supplementary web groups cannot be cleared by a non-root setuid
        // helper, so this process invokes only read-only manager API commands;
        // every operation that can inspect a source is placed on the clean
        // package controller's private queue instead.
        linux_runtime::permanently_drop_to_package_uid(identity.effective_uid)?;
        let now = current_epoch()?;
        let control_paths = ControlPaths::production();

        match request {
            ValidatedHttpRequest::Get { action, .. } => match action {
                ReadAction::Csrf => {
                    let key = linux_files::load_or_create_csrf_key(
                        &control_paths,
                        identity.effective_uid,
                    )?;
                    let nonce = linux_files::random_nonce()?;
                    let token = issue_csrf_token(&key[..], &session.binding, now, &nonce)?;
                    let body = serde_json::to_vec(&json!({
                        "schema": "sdsync.dsm-csrf.v1",
                        "csrf_token": token,
                        "expires_at_epoch": now + CSRF_LIFETIME_SECONDS,
                    }))
                    .map_err(|_| BridgeError::internal())?;
                    Ok(CgiResponse::success(body))
                }
                ReadAction::Result { job_id } => execute_result_action(
                    &control_paths,
                    &job_id,
                    &session.binding,
                    identity.effective_uid,
                    now,
                ),
                action => execute_read_action(&action),
            },
            ValidatedHttpRequest::Post {
                content_length,
                csrf_token,
                ..
            } => {
                let key =
                    linux_files::load_or_create_csrf_key(&control_paths, identity.effective_uid)?;
                verify_csrf_token(&csrf_token, &key[..], &session.binding, now)?;
                let body = read_exact_body(&mut io::stdin().lock(), content_length)?;
                let parsed = parse_mutation_request(&body)?;
                let job_id = linux_files::enqueue(
                    &control_paths,
                    EnqueueRequest {
                        package_uid: identity.effective_uid,
                        client_request_id: &parsed.request_id,
                        requested_by: &session.username,
                        session_binding: &session.binding,
                        issued_at_epoch: now,
                        mutation: &parsed.mutation,
                        secret: parsed.secret.as_ref().map(|secret| secret.as_slice()),
                    },
                )?;
                let response = serde_json::to_vec(&json!({
                    "schema": "sdsync.dsm-queued.v1",
                    "ok": true,
                    "request_id": parsed.request_id,
                    "job_id": job_id,
                    "state": "queued",
                }))
                .map_err(|_| BridgeError::internal())?;
                Ok(CgiResponse::accepted(response))
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn execute_result_action(
    paths: &ControlPaths<'_>,
    job_id: &str,
    session_binding: &[u8; 32],
    package_uid: u32,
    now: u64,
) -> BridgeResult<CgiResponse> {
    if !valid_server_job_id(job_id) {
        return Err(BridgeError::bad_request());
    }
    if let Some(response) =
        completed_result_response(paths, job_id, session_binding, package_uid, now)?
    {
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
        if !session_binding_matches(&job.session_binding, session_binding) {
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
    if let Some(response) =
        completed_result_response(paths, job_id, session_binding, package_uid, now)?
    {
        return Ok(response);
    }
    queued_expired_response(job_id)
}

#[cfg(target_os = "linux")]
fn completed_result_response(
    paths: &ControlPaths<'_>,
    job_id: &str,
    session_binding: &[u8; 32],
    package_uid: u32,
    now: u64,
) -> BridgeResult<Option<CgiResponse>> {
    let Some(bytes) = linux_files::read_optional_response(paths, job_id, package_uid)? else {
        return Ok(None);
    };
    let response = parse_queued_response(&bytes, job_id)?;
    if !session_binding_matches(&response.session_binding, session_binding) {
        return queued_pending_response(job_id).map(Some);
    }
    if response.completed_at_epoch > now.saturating_add(CLOCK_SKEW_SECONDS) {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    if now.saturating_sub(response.completed_at_epoch) > RESULT_RETENTION_SECONDS {
        linux_files::remove_expired_response(paths, job_id, package_uid)?;
        return queued_expired_response(job_id).map(Some);
    }
    queued_complete_response(job_id, &response.result).map(Some)
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

fn queued_complete_response(job_id: &str, result: &Value) -> BridgeResult<CgiResponse> {
    let body = serde_json::to_vec(&json!({
        "schema": "sdsync.dsm-result-status.v1",
        "job_id": job_id,
        "state": "complete",
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
fn execute_read_action(action: &ReadAction) -> BridgeResult<CgiResponse> {
    let arguments = read_manager_arguments(action)?;
    let output = run_manager(&arguments, None)?;
    if !output.status_success {
        return Err(BridgeError::new(ErrorKind::Unavailable));
    }
    let body = parse_and_sanitize_manager_json(&output.stdout, action, None)?;
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
        validate_consumer_identity(&identity)?;
        let request_id = validate_consumer_paths(request, response)?;
        linux_runtime::clear_environment()?;
        let control_paths = ControlPaths::production();

        let response_result = (|| {
            let job_bytes = linux_files::read_job(&control_paths, request, identity.effective_uid)?;
            let job = parse_job(&job_bytes)?;
            let now = current_epoch()?;
            validate_job_freshness(job.issued_at_epoch, now)?;
            if job.request_id != request_id {
                return Err(BridgeError::bad_request());
            }
            let result = consume_job_inner(&control_paths, &job, identity.effective_uid)
                .unwrap_or_else(|_| generic_manager_result_value());
            canonical_queued_response_bytes(&job, current_epoch()?, &result)
        })();
        linux_files::remove_claimed_secret(&control_paths, &request_id);
        let response_bytes = response_result?;
        linux_files::write_response(
            &control_paths,
            response,
            &request_id,
            identity.effective_uid,
            &response_bytes,
        )
    }
}

#[cfg(target_os = "linux")]
fn consume_job_inner(
    paths: &ControlPaths<'_>,
    job: &ParsedJob,
    package_uid: u32,
) -> BridgeResult<Value> {
    let mut secret = match &job.mutation {
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
    let output = run_manager(&arguments, secret.as_mut())?;
    let result = parse_manager_result(
        &output.stdout,
        secret.as_ref().map(|secret| secret.as_slice()),
    )?;
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
    use std::cell::Cell;
    use std::io::Cursor;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[cfg(target_os = "linux")]
    use std::sync::atomic::{AtomicU64, Ordering};

    const REQUEST_ID: &str = "0123456789abcdef0123456789abcdef";
    const JOB_ID: &str = "00060f5e12345678fedcba98765432100123456789abcdef";

    fn environment(method: &str, query: &str) -> CgiEnvironment {
        CgiEnvironment {
            method: method.to_owned(),
            content_length: None,
            content_type: None,
            query: Zeroizing::new(query.to_owned()),
            cookie: Zeroizing::new("id=authenticated-session".to_owned()),
            synology_token_header: None,
            csrf_header: None,
            remote_address: Some("192.0.2.8".to_owned()),
            server_address: Some("192.0.2.2".to_owned()),
            server_name: Some("nas.example.invalid".to_owned()),
            server_port: Some("5001".to_owned()),
            https: Some("on".to_owned()),
            transfer_encoding: None,
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
            "interval_seconds": 3600,
            "weekdays": [1, 2, 3, 4, 5],
            "time_window_start": "01:30",
            "time_window_end": "04:00",
            "debounce_seconds": 5,
            "retry_count": 2,
            "retry_backoff_seconds": 60,
            "poll_seconds": 30,
            "allow_delete": false,
            "max_total_delete": 100,
            "depends_on": ["upstream"]
        })
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
    struct TestControlFixture {
        root: PathBuf,
        requests: PathBuf,
        processing: PathBuf,
        responses: PathBuf,
        csrf_key: PathBuf,
        enqueue_lock: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl TestControlFixture {
        fn new(label: &str) -> Self {
            let sequence = NEXT_CONTROL_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "sdsync-dsm-api-{label}-{}-{sequence}",
                std::process::id()
            ));
            let requests = root.join("requests");
            let processing = root.join("processing");
            let responses = root.join("responses");
            for directory in [&root, &requests, &processing, &responses] {
                fs::create_dir(directory).unwrap();
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self {
                csrf_key: root.join("csrf.key"),
                enqueue_lock: root.join("enqueue.lock"),
                root,
                requests,
                processing,
                responses,
            }
        }

        fn paths(&self) -> ControlPaths<'_> {
            ControlPaths {
                root: &self.root,
                requests: &self.requests,
                processing: &self.processing,
                responses: &self.responses,
                csrf_key: &self.csrf_key,
                enqueue_lock: &self.enqueue_lock,
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
        assert_eq!(paths.csrf_key, Path::new(CSRF_KEY_PATH));
        assert_eq!(paths.enqueue_lock, Path::new(ENQUEUE_LOCK_PATH));
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
        let first_id = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                package_uid,
                client_request_id: REQUEST_ID,
                requested_by: "admin",
                session_binding: &session,
                issued_at_epoch: 10_000,
                mutation: &secret_mutation,
                secret: Some(b"queue-secret"),
            },
        )
        .unwrap();
        let plain_mutation = Mutation::RemoveProfile(NameArgs {
            name: "archive".to_owned(),
        });
        let second_id = linux_files::enqueue(
            &paths,
            EnqueueRequest {
                package_uid,
                client_request_id: "11111111111111111111111111111111",
                requested_by: "admin",
                session_binding: &session,
                issued_at_epoch: 10_001,
                mutation: &plain_mutation,
                secret: None,
            },
        )
        .unwrap();
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
            canonical_queued_response_bytes(&parsed_job, 10_005, &manager_result).unwrap();
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

        let concealed =
            completed_result_response(&paths, &first_id, &[8_u8; 32], package_uid, 10_006)
                .unwrap()
                .unwrap();
        assert_eq!(concealed.status, 202);
        let completed = completed_result_response(&paths, &first_id, &session, package_uid, 10_006)
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&completed.body).unwrap()["state"],
            "complete"
        );

        let pending =
            execute_result_action(&paths, &second_id, &session, package_uid, 10_002).unwrap();
        assert_eq!(pending.status, 202);
        let expired_pending = execute_result_action(
            &paths,
            &second_id,
            &session,
            package_uid,
            10_001 + MAX_JOB_AGE_SECONDS + 1,
        )
        .unwrap();
        assert_eq!(expired_pending.status, 410);
        let expired_response = completed_result_response(
            &paths,
            &first_id,
            &session,
            package_uid,
            10_005 + RESULT_RETENTION_SECONDS + 1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(expired_response.status, 410);
        assert!(!response_path.exists());

        let missing_id = "ffffffffffffffffffffffffffffffffffffffffffffffff";
        let missing =
            execute_result_action(&paths, missing_id, &session, package_uid, 10_000).unwrap();
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
        let unknown_entry = fixture.requests.join("unreviewed-entry");
        fixture.write_private(&unknown_entry, b"");
        assert_eq!(
            linux_files::enqueue(
                &paths,
                EnqueueRequest {
                    package_uid,
                    client_request_id: REQUEST_ID,
                    requested_by: "admin",
                    session_binding: &[7_u8; 32],
                    issued_at_epoch: 10_000,
                    mutation: &mutation,
                    secret: None,
                },
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
                    session_binding: &[7_u8; 32],
                    issued_at_epoch: 10_000,
                    mutation: &mutation,
                    secret: None,
                },
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsafeRuntime
        );
        fs::remove_file(&wrong_mode_job).unwrap();

        for index in 0..MAX_OUTSTANDING_JOBS {
            let path = fixture.requests.join(format!("{index:048x}.json"));
            fixture.write_private(&path, b"");
        }
        assert_eq!(
            linux_files::enqueue(
                &paths,
                EnqueueRequest {
                    package_uid,
                    client_request_id: REQUEST_ID,
                    requested_by: "admin",
                    session_binding: &[7_u8; 32],
                    issued_at_epoch: 10_000,
                    mutation: &mutation,
                    secret: None,
                },
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
    }

    #[test]
    fn synology_token_is_required_and_duplicate_sources_must_match() {
        assert_eq!(
            validate_http_request(environment("GET", "action=snapshot"))
                .unwrap_err()
                .kind,
            ErrorKind::Forbidden
        );
        let mut matching = environment("GET", "action=snapshot&SynoToken=token");
        matching.synology_token_header = Some(Zeroizing::new("token".to_owned()));
        assert!(validate_http_request(matching).is_ok());

        let mut mismatch = environment("GET", "action=snapshot&SynoToken=token-a");
        mismatch.synology_token_header = Some(Zeroizing::new("token-b".to_owned()));
        assert_eq!(
            validate_http_request(mismatch).unwrap_err().kind,
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

        let mut post = post_environment(10);
        post.content_type = Some("text/plain".to_owned());
        assert_eq!(
            validate_http_request(post).unwrap_err().kind,
            ErrorKind::UnsupportedMediaType
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
        for invalid in [b"".as_slice(), b"admin\nother\n", b"bad user\n", b"root:\n"] {
            assert!(parse_authentication_output(invalid).is_err());
        }
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
    fn cgi_identity_requires_non_root_http_real_uid_and_setuid_package_owner() {
        let valid = IdentityState {
            real_uid: 1023,
            effective_uid: 1060,
            executable_uid: 1060,
            executable_mode: 0o100_000 | 0o4755,
        };
        assert!(validate_cgi_identity(&valid, 1023).is_ok());
        for invalid in [
            IdentityState {
                real_uid: 0,
                ..valid.clone()
            },
            IdentityState {
                effective_uid: 0,
                executable_uid: 0,
                ..valid.clone()
            },
            IdentityState {
                real_uid: 1060,
                ..valid.clone()
            },
            IdentityState {
                executable_uid: 999,
                ..valid.clone()
            },
            IdentityState {
                executable_mode: 0o100_000 | 0o755,
                ..valid.clone()
            },
            IdentityState {
                executable_mode: 0o100_000 | 0o4775,
                ..valid.clone()
            },
        ] {
            assert!(validate_cgi_identity(&invalid, 1023).is_err());
        }
    }

    #[test]
    fn consumer_identity_requires_plain_package_owned_non_setuid_binary() {
        let valid = IdentityState {
            real_uid: 1060,
            effective_uid: 1060,
            executable_uid: 1060,
            executable_mode: 0o100_000 | 0o755,
        };
        assert!(validate_consumer_identity(&valid).is_ok());
        assert!(
            validate_consumer_identity(&IdentityState {
                executable_mode: 0o100_000 | 0o4755,
                ..valid.clone()
            })
            .is_err()
        );
        assert!(
            validate_consumer_identity(&IdentityState {
                real_uid: 1023,
                ..valid
            })
            .is_err()
        );
    }

    struct MockTransition {
        requested: Cell<Option<u32>>,
        reported: (u32, u32, u32),
        fail_set: bool,
    }

    impl UidTransition for MockTransition {
        fn set_all_uids(&self, uid: u32) -> io::Result<()> {
            self.requested.set(Some(uid));
            if self.fail_set {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "mock"))
            } else {
                Ok(())
            }
        }

        fn all_uids(&self) -> io::Result<(u32, u32, u32)> {
            Ok(self.reported)
        }
    }

    #[test]
    fn privilege_drop_is_mockable_and_verifies_real_effective_and_saved_ids() {
        let valid = MockTransition {
            requested: Cell::new(None),
            reported: (1060, 1060, 1060),
            fail_set: false,
        };
        assert!(permanently_drop_with(&valid, 1060).is_ok());
        assert_eq!(valid.requested.get(), Some(1060));

        let incomplete = MockTransition {
            requested: Cell::new(None),
            reported: (1023, 1060, 1060),
            fail_set: false,
        };
        assert!(permanently_drop_with(&incomplete, 1060).is_err());
        assert!(permanently_drop_with(&valid, 0).is_err());
    }

    #[test]
    fn child_environments_are_allowlists_without_request_secrets_for_manager() {
        let request =
            validate_http_request(environment("GET", "action=snapshot&SynoToken=dsm-token"))
                .unwrap();
        let authentication = match request {
            ValidatedHttpRequest::Get { authentication, .. } => authentication,
            _ => unreachable!(),
        };
        let auth_names = authentication_command_environment(&authentication)
            .into_iter()
            .map(|(name, _)| name.into_string().unwrap())
            .collect::<BTreeSet<_>>();
        assert!(auth_names.contains("HTTP_COOKIE"));
        assert!(auth_names.contains("HTTP_X_SYNO_TOKEN"));
        assert!(!auth_names.contains("LD_PRELOAD"));
        assert!(!auth_names.contains("HTTP_X_SDSYNC_CSRF"));

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
    fn csrf_is_session_bound_short_lived_and_tamper_evident() {
        let key = [7_u8; 32];
        let first = session_binding("admin", 1000, "id=session-a", "token-a");
        let second = session_binding("admin", 1000, "id=session-b", "token-a");
        let token = issue_csrf_token(&key, &first, 10_000, &[9_u8; 16]).unwrap();
        assert!(verify_csrf_token(&token, &key, &first, 10_001).is_ok());
        assert_eq!(
            verify_csrf_token(&token, &key, &second, 10_001)
                .unwrap_err()
                .kind,
            ErrorKind::Forbidden
        );
        assert!(verify_csrf_token(&token, &[8_u8; 32], &first, 10_001).is_err());
        assert!(verify_csrf_token(&token, &key, &first, 10_300).is_err());
        let tampered = token.replacen("v1.", "v2.", 1);
        assert!(verify_csrf_token(&tampered, &key, &first, 10_001).is_err());
    }

    #[test]
    fn constant_time_comparison_has_fixed_length_mac_semantics() {
        assert!(constant_time_equal(&[1, 2, 3], &[1, 2, 3]));
        assert!(!constant_time_equal(&[1, 2, 3], &[1, 2, 4]));
        assert!(!constant_time_equal(&[1, 2, 3], &[1, 2]));
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
                .any(|pair| pair == ["--log-format", "json"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--progress", "never"])
        );
        assert!(!arguments.iter().any(|argument| argument == MANAGER_PATH));

        let mut unknown = configure_arguments();
        unknown["executable"] = json!("/tmp/evil");
        assert!(parse_mutation_request(&request("configure-profile", unknown)).is_err());
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
            &[7_u8; 32],
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
            &[7_u8; 32],
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
    }

    #[test]
    fn queued_response_is_strict_and_preserves_private_session_binding() {
        let mutation =
            parse_mutation_request(&request("remove-profile", json!({"name":"nightly"}))).unwrap();
        let job_bytes = canonical_job_bytes(
            JOB_ID,
            REQUEST_ID,
            "admin",
            &[9_u8; 32],
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
        let response = canonical_queued_response_bytes(&job, 10_005, &result).unwrap();
        let parsed = parse_queued_response(&response, JOB_ID).unwrap();
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
    fn result_status_envelopes_are_bounded_and_expiry_is_explicit() {
        let pending = queued_pending_response(JOB_ID).unwrap();
        assert_eq!(pending.status, 202);
        let pending_json: Value = serde_json::from_slice(&pending.body).unwrap();
        assert_eq!(pending_json["state"], "pending");
        assert!(pending_json.get("result").is_none());

        let complete = queued_complete_response(JOB_ID, &generic_manager_result_value()).unwrap();
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
        let logs = br#"{"schema":"sdsync.dsm-logs.v1","logs":[{"source":"controller","lines":[]},{"source":"sync","lines":[]}]}"#;
        let filtered = parse_and_sanitize_manager_json(
            logs,
            &ReadAction::Logs {
                lines: 10,
                source: LogSource::Sync,
            },
            None,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&filtered).unwrap();
        assert_eq!(value["logs"].as_array().unwrap().len(), 1);
        assert_eq!(value["logs"][0]["source"], "sync");

        let snapshot = parse_and_sanitize_manager_json(
            br#"{"schema":"sdsync.dsm-api.v1","capabilities":{"mutations":false}}"#,
            &ReadAction::Snapshot,
            None,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&snapshot).unwrap();
        assert_eq!(value["capabilities"]["mutations"], true);
        assert_eq!(value["capabilities"]["private_queue"], true);
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
        assert!(valid_request_id(&first_id));
        assert!(first_id < second_id);
        assert!(next_enqueue_sequence(u64::MAX, 1).is_err());
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
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_runtime_compiles_and_fails_closed() {
        assert_eq!(run_cgi().unwrap_err().kind, ErrorKind::UnsafeRuntime);
        let request = Path::new("request");
        let response = Path::new("response");
        assert_eq!(
            run_consumer(request, response).unwrap_err().kind,
            ErrorKind::UnsafeRuntime
        );
    }
}
