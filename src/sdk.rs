//! Safe, synchronous embedding API for one complete synchronization run.
//!
//! The [`Engine`] entry point deliberately owns the operation ordering. Callers
//! using that high-level API can inspect a plan and decide whether to apply it,
//! but cannot construct an executable engine plan or invoke an engine mutation
//! without passing through the scanner and planner. The crate's public
//! low-level modules remain available for specialized integrations.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use zeroize::Zeroizing;

use crate::Error;
use crate::api::{ApiClient, ClientOptions};
use crate::cancel::CancellationToken;
use crate::local::{self, IgnoreRules};
use crate::path::RemoteRoot;
use crate::plan::{self, PlanOptions, SyncPlan};
use crate::sync::{self, ExecuteOptions, ExecutionEvent, UploadObserverFactory};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(7_200);
const DEFAULT_RETRIES: u32 = 2;
const DEFAULT_JOBS: usize = 2;
const DEFAULT_MAX_DELETE: usize = 100;
const MAX_RETRIES: u32 = 5;
const MAX_JOBS: usize = 16;

/// Product build version embedded by the approved release workflow. This is a
/// calendar release such as `26.1`; it is intentionally distinct from Cargo's
/// semantic package version and the C ABI major.
pub const fn build_version() -> &'static str {
    env!("SDSYNC_VERSION")
}

/// A secret that is zeroized when dropped and never reveals its value through
/// `Debug` formatting.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wrap a caller-owned secret string for short-lived SDK use.
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

/// Why the engine is requesting a one-time password.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OtpChallenge {
    /// DSM refused password-only authentication because the account requires
    /// a second factor.
    Required,
    /// DSM rejected the previously supplied OTP and permits one fresh value.
    Rejected,
}

/// A bounded failure returned by a caller's secret provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretProviderError {
    /// The requested secret is not available.
    Unavailable,
    /// The embedding application cancelled secret acquisition.
    Cancelled,
}

/// Supplies credentials only when the engine reaches the authentication step.
///
/// `otp` is never called speculatively. The engine first attempts password-only
/// login and requests an OTP only after DSM reports an OTP challenge.
pub trait SecretProvider {
    /// Return the DSM account password.
    fn password(&mut self) -> std::result::Result<Secret, SecretProviderError>;

    /// Return a current six-digit DSM OTP, or `None` when unavailable.
    fn otp(
        &mut self,
        challenge: OtpChallenge,
    ) -> std::result::Result<Option<Secret>, SecretProviderError>;
}

/// Stable, broad error categories for embedding applications.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    InvalidRequest,
    CredentialUnavailable,
    OtpRequired,
    Authentication,
    LocalFilesystem,
    Network,
    Remote,
    Safety,
    Cancelled,
    Reconciliation,
    Internal,
}

/// An SDK error with a stable category and a secret-free operator message.
#[derive(Debug)]
pub struct SdkError {
    code: ErrorCode,
    message: String,
}

impl SdkError {
    /// Stable category suitable for programmatic decisions.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// Bounded, secret-free diagnostic text.
    pub fn message(&self) -> &str {
        &self.message
    }

    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn from_core(error: Error) -> Self {
        let code = match &error {
            Error::InvalidUrl(_)
            | Error::HttpsRequired
            | Error::UnsafeRemotePath { .. }
            | Error::Configuration(_) => ErrorCode::InvalidRequest,
            Error::InvalidSource(_)
            | Error::UnsupportedLocalEntry { .. }
            | Error::FileIo { .. }
            | Error::SourceChanged(_) => ErrorCode::LocalFilesystem,
            Error::Http { .. } | Error::HttpBody { .. } | Error::HttpStatus { .. } => {
                ErrorCode::Network
            }
            Error::TypeConflict { .. }
            | Error::ProtectedConflict(_)
            | Error::EmptySourceDeletion
            | Error::DeleteLimit { .. }
            | Error::RemoteSnapshotChanged(_) => ErrorCode::Safety,
            Error::Cancelled => ErrorCode::Cancelled,
            Error::ReconciliationPending { .. } => ErrorCode::Reconciliation,
            Error::Message(_) => ErrorCode::Internal,
            Error::InvalidResponse { .. }
            | Error::Api { .. }
            | Error::UnsupportedApiVersion { .. }
            | Error::MissingApi(_)
            | Error::ShareNotWritable(_)
            | Error::RemoteEscape(_)
            | Error::RemoteMountRoot { .. }
            | Error::InvalidContentHash
            | Error::ContentVerificationFailed(_)
            | Error::ServerCopyNotStarted
            | Error::OperationTimedOut { .. }
            | Error::Vault { .. } => ErrorCode::Remote,
        };
        Self::new(code, error.to_string())
    }

    fn authentication(error: Error) -> Self {
        let code = match &error {
            Error::Http { .. } | Error::HttpBody { .. } | Error::HttpStatus { .. } => {
                ErrorCode::Network
            }
            Error::Cancelled => ErrorCode::Cancelled,
            _ => ErrorCode::Authentication,
        };
        Self::new(code, error.to_string())
    }

    fn cancelled() -> Self {
        Self::new(ErrorCode::Cancelled, "operation cancelled")
    }
}

impl fmt::Display for SdkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SdkError {}

/// SDK result alias.
pub type SdkResult<T> = std::result::Result<T, SdkError>;

/// File comparison strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Comparison {
    /// Compare size, File Station-resolution mtime, and MD5 content.
    Content,
    /// Compare size and File Station-resolution mtime.
    Metadata,
    /// Compare only byte size.
    SizeOnly,
}

impl From<Comparison> for plan::CompareMode {
    fn from(value: Comparison) -> Self {
        match value {
            Comparison::Content => Self::Content,
            Comparison::Metadata => Self::Metadata,
            Comparison::SizeOnly => Self::SizeOnly,
        }
    }
}

/// Explicit mirror-deletion policy. Deletion is disabled by default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeletionPolicy {
    enabled: bool,
    max_delete: usize,
    allow_empty_source: bool,
}

impl DeletionPolicy {
    /// Additive/update-only synchronization.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            max_delete: DEFAULT_MAX_DELETE,
            allow_empty_source: false,
        }
    }

    /// Enable bounded mirror deletion. A zero bound is rejected.
    pub fn bounded(max_delete: usize) -> SdkResult<Self> {
        if max_delete == 0 {
            return Err(SdkError::new(
                ErrorCode::InvalidRequest,
                "deletion requires a positive maximum",
            ));
        }
        Ok(Self {
            enabled: true,
            max_delete,
            allow_empty_source: false,
        })
    }

    /// Explicitly permit deletion when the source has no payload files.
    #[must_use]
    pub const fn allow_empty_source(mut self) -> Self {
        self.allow_empty_source = true;
        self
    }

    /// Whether mirror deletion is enabled.
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    /// Maximum entries that one run may delete.
    pub const fn max_delete(self) -> usize {
        self.max_delete
    }

    /// Whether the empty-source fuse is explicitly disabled.
    pub const fn empty_source_allowed(self) -> bool {
        self.allow_empty_source
    }
}

impl Default for DeletionPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// A validated, immutable synchronization request.
#[derive(Clone, Debug)]
pub struct SyncRequest {
    endpoint: String,
    username: String,
    source: PathBuf,
    remote: String,
    allow_http: bool,
    accept_invalid_certificates: bool,
    ca_certificate: Option<PathBuf>,
    connect_timeout: Duration,
    request_timeout: Duration,
    retries: u32,
    max_upload_rate: Option<u64>,
    exclusions: Vec<String>,
    comparison: Comparison,
    deletion: DeletionPolicy,
    jobs: usize,
}

impl SyncRequest {
    /// Start a request builder. The resulting request is additive, HTTPS-only,
    /// certificate-validating, content-comparing, and bounded by conservative
    /// timeout/retry defaults.
    pub fn builder(
        endpoint: impl Into<String>,
        username: impl Into<String>,
        source: impl Into<PathBuf>,
        remote: impl Into<String>,
    ) -> SyncRequestBuilder {
        SyncRequestBuilder {
            request: Self {
                endpoint: endpoint.into(),
                username: username.into(),
                source: source.into(),
                remote: remote.into(),
                allow_http: false,
                accept_invalid_certificates: false,
                ca_certificate: None,
                connect_timeout: DEFAULT_CONNECT_TIMEOUT,
                request_timeout: DEFAULT_REQUEST_TIMEOUT,
                retries: DEFAULT_RETRIES,
                max_upload_rate: None,
                exclusions: Vec::new(),
                comparison: Comparison::Content,
                deletion: DeletionPolicy::disabled(),
                jobs: DEFAULT_JOBS,
            },
        }
    }

    /// Configured endpoint, without credentials.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Configured DSM account name.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Local source path.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Remote File Station root.
    pub fn remote(&self) -> &str {
        &self.remote
    }
}

/// Builder for [`SyncRequest`].
#[derive(Clone, Debug)]
pub struct SyncRequestBuilder {
    request: SyncRequest,
}

impl SyncRequestBuilder {
    /// Permit plain HTTP for an explicitly trusted test/LAN endpoint.
    #[must_use]
    pub fn allow_http(mut self, allow: bool) -> Self {
        self.request.allow_http = allow;
        self
    }

    /// Disable TLS certificate validation. The deliberately alarming method
    /// name makes the trust reduction visible at the call site.
    #[must_use]
    pub fn danger_accept_invalid_certificates(mut self, accept: bool) -> Self {
        self.request.accept_invalid_certificates = accept;
        self
    }

    /// Trust an additional PEM CA certificate.
    #[must_use]
    pub fn ca_certificate(mut self, path: impl Into<PathBuf>) -> Self {
        self.request.ca_certificate = Some(path.into());
        self
    }

    /// Set the connection-establishment timeout.
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.request.connect_timeout = timeout;
        self
    }

    /// Set the complete operation timeout.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request.request_timeout = timeout;
        self
    }

    /// Set retry attempts after the initial request.
    #[must_use]
    pub fn retries(mut self, retries: u32) -> Self {
        self.request.retries = retries;
        self
    }

    /// Set a shared upload bandwidth limit in bytes per second.
    #[must_use]
    pub fn max_upload_rate(mut self, bytes_per_second: u64) -> Self {
        self.request.max_upload_rate = Some(bytes_per_second);
        self
    }

    /// Add one gitignore-style exclusion pattern.
    #[must_use]
    pub fn exclude(mut self, pattern: impl Into<String>) -> Self {
        self.request.exclusions.push(pattern.into());
        self
    }

    /// Select the file comparison strategy.
    #[must_use]
    pub fn comparison(mut self, comparison: Comparison) -> Self {
        self.request.comparison = comparison;
        self
    }

    /// Set the explicit mirror-deletion policy.
    #[must_use]
    pub fn deletion(mut self, deletion: DeletionPolicy) -> Self {
        self.request.deletion = deletion;
        self
    }

    /// Set the maximum number of concurrent mutation workers.
    #[must_use]
    pub fn jobs(mut self, jobs: usize) -> Self {
        self.request.jobs = jobs;
        self
    }

    /// Validate and freeze the request.
    pub fn build(self) -> SdkResult<SyncRequest> {
        let request = self.request;
        if request.username.is_empty() || request.username.chars().any(char::is_control) {
            return Err(SdkError::new(
                ErrorCode::InvalidRequest,
                "username must be non-empty and contain no control characters",
            ));
        }
        if request.source.as_os_str().is_empty() {
            return Err(SdkError::new(
                ErrorCode::InvalidRequest,
                "source path must not be empty",
            ));
        }
        if request.connect_timeout.is_zero() || request.request_timeout.is_zero() {
            return Err(SdkError::new(
                ErrorCode::InvalidRequest,
                "timeouts must be positive",
            ));
        }
        if request.jobs == 0 || request.jobs > MAX_JOBS {
            return Err(SdkError::new(
                ErrorCode::InvalidRequest,
                "jobs must be between 1 and 16",
            ));
        }
        if request.retries > MAX_RETRIES {
            return Err(SdkError::new(
                ErrorCode::InvalidRequest,
                "retries must be between 0 and 5",
            ));
        }
        if request.max_upload_rate == Some(0) {
            return Err(SdkError::new(
                ErrorCode::InvalidRequest,
                "maximum upload rate must be positive",
            ));
        }
        crate::api::normalize_base_url(&request.endpoint, request.allow_http)
            .map_err(SdkError::from_core)?;
        RemoteRoot::parse(&request.remote).map_err(SdkError::from_core)?;
        Ok(request)
    }
}

/// Ordered operation kinds in an immutable plan view.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanOperation {
    DeleteTypeConflict,
    CreateDirectory,
    CopyRemoteContent,
    Upload,
    DeleteRemoteExtra,
}

/// One read-only planned change.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedChange {
    operation: PlanOperation,
    remote_path: String,
    source: Option<String>,
    bytes: u64,
    reason: String,
}

impl PlannedChange {
    pub fn operation(&self) -> PlanOperation {
        self.operation
    }

    pub fn remote_path(&self) -> &str {
        &self.remote_path
    }

    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Immutable, serializable summary presented before any remote mutation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanSummary {
    changes: Vec<PlannedChange>,
    creates: usize,
    copies: usize,
    uploads: usize,
    deletes: usize,
    unchanged_files: usize,
    protected_entries: usize,
    upload_bytes: u64,
}

impl PlanSummary {
    pub fn changes(&self) -> &[PlannedChange] {
        &self.changes
    }

    pub fn creates(&self) -> usize {
        self.creates
    }

    pub fn copies(&self) -> usize {
        self.copies
    }

    pub fn uploads(&self) -> usize {
        self.uploads
    }

    pub fn deletes(&self) -> usize {
        self.deletes
    }

    pub fn unchanged_files(&self) -> usize {
        self.unchanged_files
    }

    pub fn protected_entries(&self) -> usize {
        self.protected_entries
    }

    pub fn upload_bytes(&self) -> u64 {
        self.upload_bytes
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Explicit decision made after inspecting the current plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanDecision {
    /// Return the plan without remote mutation.
    PreviewOnly,
    /// Apply exactly the guarded plan and perform final reconciliation.
    Apply,
}

/// High-level operation phases emitted to an observer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    LocalScan,
    ApiDiscovery,
    Authentication,
    RemoteScan,
    ContentHashing,
    Planning,
    Execution,
    Reconciliation,
    Logout,
}

/// One completed mutation exposed without internal plan types.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum MutationEvent {
    TypeConflictDeleted {
        remote_path: String,
    },
    DirectoryCreated {
        remote_path: String,
    },
    RemoteContentCopied {
        from_remote_path: String,
        to_remote_path: String,
        bytes: u64,
    },
    CopyFallbackUploaded {
        relative: String,
        bytes: u64,
    },
    Uploaded {
        relative: String,
        bytes: u64,
    },
    RemoteExtraDeleted {
        remote_path: String,
    },
}

/// Structured, secret-free SDK event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SdkEvent {
    PhaseStarted { phase: Phase },
    PhaseCompleted { phase: Phase },
    PlanReady { summary: PlanSummary },
    Mutation { mutation: MutationEvent },
}

/// Observer response. Cancellation is cooperative and checked before the next
/// safe remote operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventControl {
    Continue,
    Cancel,
}

/// Counts returned after an applied plan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ExecutionSummary {
    deleted: usize,
    created: usize,
    copied: usize,
    uploaded: usize,
    uploaded_bytes: u64,
}

impl ExecutionSummary {
    pub fn deleted(self) -> usize {
        self.deleted
    }

    pub fn created(self) -> usize {
        self.created
    }

    pub fn copied(self) -> usize {
        self.copied
    }

    pub fn uploaded(self) -> usize {
        self.uploaded
    }

    pub fn uploaded_bytes(self) -> u64 {
        self.uploaded_bytes
    }
}

/// Result of one complete, logged-out engine run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SyncOutcome {
    plan: PlanSummary,
    applied: bool,
    reconciled: bool,
    execution: Option<ExecutionSummary>,
}

impl SyncOutcome {
    pub fn plan(&self) -> &PlanSummary {
        &self.plan
    }

    pub fn applied(&self) -> bool {
        self.applied
    }

    pub fn reconciled(&self) -> bool {
        self.reconciled
    }

    pub fn execution(&self) -> Option<ExecutionSummary> {
        self.execution
    }
}

/// Stateless entry point for safe synchronous synchronization.
#[derive(Clone, Copy, Debug, Default)]
pub struct Engine;

impl Engine {
    /// Run one complete operation. `on_plan` is called exactly once after all
    /// inventories and required content hashes are current, and before any
    /// remote mutation.
    pub fn run<P, D, O>(
        &self,
        request: &SyncRequest,
        secrets: &mut P,
        cancellation: &CancellationToken,
        on_plan: D,
        mut on_event: O,
    ) -> SdkResult<SyncOutcome>
    where
        P: SecretProvider,
        D: FnOnce(&PlanSummary) -> PlanDecision,
        O: FnMut(&SdkEvent) -> EventControl,
    {
        check_cancellation(cancellation)?;
        let root = RemoteRoot::parse(&request.remote).map_err(SdkError::from_core)?;

        emit(
            cancellation,
            &mut on_event,
            SdkEvent::PhaseStarted {
                phase: Phase::LocalScan,
            },
        )?;
        let rules = IgnoreRules::build(&request.source, &request.exclusions)
            .map_err(SdkError::from_core)?;
        let mut local = local::scan(&request.source, &rules).map_err(SdkError::from_core)?;
        emit(
            cancellation,
            &mut on_event,
            SdkEvent::PhaseCompleted {
                phase: Phase::LocalScan,
            },
        )?;

        emit(
            cancellation,
            &mut on_event,
            SdkEvent::PhaseStarted {
                phase: Phase::ApiDiscovery,
            },
        )?;
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: request.endpoint.clone(),
            allow_http: request.allow_http,
            accept_invalid_certs: request.accept_invalid_certificates,
            ca_certificate: request.ca_certificate.clone(),
            connect_timeout: request.connect_timeout,
            request_timeout: request.request_timeout,
            retries: request.retries,
        })
        .map(|client| client.with_max_upload_rate(request.max_upload_rate))
        .map_err(SdkError::from_core)?;
        let server_copy = client.supports_server_copy();
        if request.deletion.enabled {
            client.require_delete_api().map_err(SdkError::from_core)?;
        }
        emit(
            cancellation,
            &mut on_event,
            SdkEvent::PhaseCompleted {
                phase: Phase::ApiDiscovery,
            },
        )?;

        emit(
            cancellation,
            &mut on_event,
            SdkEvent::PhaseStarted {
                phase: Phase::Authentication,
            },
        )?;
        authenticate(&mut client, &request.username, secrets, cancellation)?;

        let operation = catch_unwind(AssertUnwindSafe(|| {
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseCompleted {
                    phase: Phase::Authentication,
                },
            )?;
            client
                .verify_destination_writable(&root)
                .map_err(SdkError::from_core)?;
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseStarted {
                    phase: Phase::RemoteScan,
                },
            )?;
            let mut remote = client
                .remote_inventory(&root)
                .map_err(SdkError::from_core)?;
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseCompleted {
                    phase: Phase::RemoteScan,
                },
            )?;

            if request.comparison == Comparison::Content {
                emit(
                    cancellation,
                    &mut on_event,
                    SdkEvent::PhaseStarted {
                        phase: Phase::ContentHashing,
                    },
                )?;
                client.require_content_api().map_err(SdkError::from_core)?;
                local::populate_content_md5(&mut local, cancellation)
                    .map_err(SdkError::from_core)?;
                let selected = plan::select_remote_content_hashes_for_plan(
                    &local,
                    &remote,
                    &rules,
                    server_copy,
                    request.deletion.enabled,
                );
                client
                    .populate_remote_content_md5(&mut remote, &selected, cancellation)
                    .map_err(SdkError::from_core)?;
                emit(
                    cancellation,
                    &mut on_event,
                    SdkEvent::PhaseCompleted {
                        phase: Phase::ContentHashing,
                    },
                )?;
            }

            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseStarted {
                    phase: Phase::Planning,
                },
            )?;
            let plan = plan::build_plan(
                &root,
                &local,
                &remote,
                &rules,
                &PlanOptions {
                    delete: request.deletion.enabled,
                    allow_empty_source: request.deletion.allow_empty_source,
                    max_delete: request.deletion.max_delete,
                    compare: request.comparison.into(),
                    server_copy,
                },
            )
            .map_err(SdkError::from_core)?;
            let summary = summarize_plan(&plan);
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PlanReady {
                    summary: summary.clone(),
                },
            )?;
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseCompleted {
                    phase: Phase::Planning,
                },
            )?;

            let decision = on_plan(&summary);
            check_cancellation(cancellation)?;
            if decision == PlanDecision::PreviewOnly {
                return Ok(SyncOutcome {
                    plan: summary,
                    applied: false,
                    reconciled: false,
                    execution: None,
                });
            }
            check_cancellation(cancellation)?;

            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseStarted {
                    phase: Phase::Execution,
                },
            )?;
            let report = if plan.is_empty() {
                crate::sync::ExecutionReport::default()
            } else {
                let observer_factory: UploadObserverFactory = Arc::new(|_| None);
                sync::execute_observed(
                    &client,
                    &root,
                    &plan,
                    ExecuteOptions {
                        jobs: request.jobs,
                        dry_run: false,
                    },
                    cancellation.clone(),
                    observer_factory,
                    |event| {
                        if on_event(&SdkEvent::Mutation {
                            mutation: mutation_event(event),
                        }) == EventControl::Cancel
                        {
                            cancellation.cancel();
                        }
                    },
                )
                .map_err(SdkError::from_core)?
            };
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseCompleted {
                    phase: Phase::Execution,
                },
            )?;

            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseStarted {
                    phase: Phase::Reconciliation,
                },
            )?;
            let reconciliation =
                reconciliation_plan(&client, request, &root, &rules, server_copy, cancellation)?;
            if !reconciliation.is_empty() {
                return Err(SdkError::new(
                    ErrorCode::Reconciliation,
                    format!(
                        "final reconciliation found {} pending in-scope operations",
                        operation_count(&reconciliation)
                    ),
                ));
            }
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseCompleted {
                    phase: Phase::Reconciliation,
                },
            )?;

            Ok(SyncOutcome {
                plan: summary,
                applied: true,
                reconciled: true,
                execution: Some(ExecutionSummary {
                    deleted: report.deleted,
                    created: report.created,
                    copied: report.copied,
                    uploaded: report.uploaded,
                    uploaded_bytes: report.uploaded_bytes,
                }),
            })
        }));
        let operation = match operation {
            Ok(operation) => operation,
            Err(payload) => {
                // Authentication succeeded, so attempt session cleanup without
                // invoking caller code again. Preserve the original panic even
                // if the best-effort logout itself panics.
                let _ = catch_unwind(AssertUnwindSafe(|| client.logout()));
                resume_unwind(payload);
            }
        };

        let logout_start = catch_unwind(AssertUnwindSafe(|| {
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseStarted {
                    phase: Phase::Logout,
                },
            )
        }));
        let logout_start = match logout_start {
            Ok(logout_start) => logout_start,
            Err(payload) => {
                // A caller panic must not strand the authenticated session or
                // trigger another observer invocation during panic cleanup.
                let _ = catch_unwind(AssertUnwindSafe(|| client.logout()));
                resume_unwind(payload);
            }
        };
        let logout = client.logout().map_err(SdkError::from_core);
        let logout_end = emit_without_cancellation(
            &mut on_event,
            SdkEvent::PhaseCompleted {
                phase: Phase::Logout,
            },
        );

        match (operation, logout_start, logout, logout_end) {
            (Err(error), _, _, _) => Err(error),
            (Ok(_), Err(error), _, _) => Err(error),
            (Ok(_), Ok(()), Err(error), _) => Err(error),
            (Ok(_), Ok(()), Ok(()), Err(error)) => Err(error),
            (Ok(outcome), Ok(()), Ok(()), Ok(())) => Ok(outcome),
        }
    }
}

fn authenticate<P: SecretProvider>(
    client: &mut ApiClient,
    username: &str,
    secrets: &mut P,
    cancellation: &CancellationToken,
) -> SdkResult<()> {
    check_cancellation(cancellation)?;
    let password = secrets.password().map_err(provider_error)?;
    if password.expose().is_empty() {
        return Err(SdkError::new(
            ErrorCode::CredentialUnavailable,
            "credential provider returned an empty password",
        ));
    }

    match client.login(username, password.expose(), None) {
        Ok(()) => return Ok(()),
        Err(error) if matches!(error.api_code(), Some(403 | 406)) => {}
        Err(error) => return Err(SdkError::authentication(error)),
    }

    let mut otp = request_otp(secrets, OtpChallenge::Required, cancellation)?;
    match client.login(username, password.expose(), Some(otp.expose())) {
        Ok(()) => Ok(()),
        Err(error) if error.api_code() == Some(404) => {
            drop(otp);
            otp = request_otp(secrets, OtpChallenge::Rejected, cancellation)?;
            client
                .login(username, password.expose(), Some(otp.expose()))
                .map_err(SdkError::authentication)
        }
        Err(error) => Err(SdkError::authentication(error)),
    }
}

fn request_otp<P: SecretProvider>(
    secrets: &mut P,
    challenge: OtpChallenge,
    cancellation: &CancellationToken,
) -> SdkResult<Secret> {
    check_cancellation(cancellation)?;
    let otp = secrets
        .otp(challenge)
        .map_err(provider_error)?
        .ok_or_else(|| {
            SdkError::new(
                ErrorCode::OtpRequired,
                "DSM requires a one-time password, but the provider returned none",
            )
        })?;
    if otp.expose().len() != 6 || !otp.expose().bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SdkError::new(
            ErrorCode::CredentialUnavailable,
            "credential provider returned a malformed one-time password",
        ));
    }
    Ok(otp)
}

fn provider_error(error: SecretProviderError) -> SdkError {
    match error {
        SecretProviderError::Unavailable => SdkError::new(
            ErrorCode::CredentialUnavailable,
            "credential provider could not supply the requested secret",
        ),
        SecretProviderError::Cancelled => SdkError::cancelled(),
    }
}

fn emit<O: FnMut(&SdkEvent) -> EventControl>(
    cancellation: &CancellationToken,
    observer: &mut O,
    event: SdkEvent,
) -> SdkResult<()> {
    check_cancellation(cancellation)?;
    if observer(&event) == EventControl::Cancel {
        cancellation.cancel();
        return Err(SdkError::cancelled());
    }
    check_cancellation(cancellation)
}

fn emit_without_cancellation<O: FnMut(&SdkEvent) -> EventControl>(
    observer: &mut O,
    event: SdkEvent,
) -> SdkResult<()> {
    if observer(&event) == EventControl::Cancel {
        Err(SdkError::cancelled())
    } else {
        Ok(())
    }
}

fn check_cancellation(cancellation: &CancellationToken) -> SdkResult<()> {
    cancellation.check().map_err(SdkError::from_core)
}

fn reconciliation_plan(
    client: &ApiClient,
    request: &SyncRequest,
    root: &RemoteRoot,
    rules: &IgnoreRules,
    server_copy: bool,
    cancellation: &CancellationToken,
) -> SdkResult<SyncPlan> {
    check_cancellation(cancellation)?;
    let mut local = local::scan(&request.source, rules).map_err(SdkError::from_core)?;
    check_cancellation(cancellation)?;
    let mut remote = client.remote_inventory(root).map_err(SdkError::from_core)?;
    check_cancellation(cancellation)?;
    if request.comparison == Comparison::Content {
        client.require_content_api().map_err(SdkError::from_core)?;
        local::populate_content_md5(&mut local, cancellation).map_err(SdkError::from_core)?;
        let selected = plan::select_remote_content_hashes_for_plan(
            &local,
            &remote,
            rules,
            server_copy,
            request.deletion.enabled,
        );
        client
            .populate_remote_content_md5(&mut remote, &selected, cancellation)
            .map_err(SdkError::from_core)?;
    }
    plan::build_plan(
        root,
        &local,
        &remote,
        rules,
        &PlanOptions {
            delete: request.deletion.enabled,
            allow_empty_source: request.deletion.allow_empty_source,
            max_delete: request.deletion.max_delete,
            compare: request.comparison.into(),
            server_copy,
        },
    )
    .map_err(SdkError::from_core)
}

fn summarize_plan(plan: &SyncPlan) -> PlanSummary {
    let mut changes = Vec::with_capacity(operation_count(plan));
    changes.extend(plan.pre_deletes.iter().map(|action| PlannedChange {
        operation: PlanOperation::DeleteTypeConflict,
        remote_path: action.remote_path.clone(),
        source: None,
        bytes: action.snapshot.size,
        reason: "type-conflict".to_owned(),
    }));
    changes.extend(plan.creates.iter().map(|action| PlannedChange {
        operation: PlanOperation::CreateDirectory,
        remote_path: action.remote_path.clone(),
        source: None,
        bytes: 0,
        reason: action.reason.as_str().to_owned(),
    }));
    changes.extend(plan.copies.iter().map(|action| PlannedChange {
        operation: PlanOperation::CopyRemoteContent,
        remote_path: action.to_remote_path.clone(),
        source: Some(action.from_remote_path.clone()),
        bytes: action.expected_size,
        reason: "verified-remote-copy".to_owned(),
    }));
    changes.extend(plan.uploads.iter().map(|action| PlannedChange {
        operation: PlanOperation::Upload,
        remote_path: action.remote_path.clone(),
        source: Some(action.local.relative.clone()),
        bytes: action.local.size,
        reason: action.reason.as_str().to_owned(),
    }));
    changes.extend(plan.post_deletes.iter().map(|action| PlannedChange {
        operation: PlanOperation::DeleteRemoteExtra,
        remote_path: action.remote_path.clone(),
        source: None,
        bytes: action.snapshot.size,
        reason: "missing-local".to_owned(),
    }));
    PlanSummary {
        changes,
        creates: plan.creates.len(),
        copies: plan.copies.len(),
        uploads: plan.uploads.len(),
        deletes: plan.delete_count(),
        unchanged_files: plan.unchanged_files,
        protected_entries: plan.protected_entries,
        upload_bytes: plan.upload_bytes,
    }
}

fn operation_count(plan: &SyncPlan) -> usize {
    plan.pre_deletes.len()
        + plan.creates.len()
        + plan.copies.len()
        + plan.uploads.len()
        + plan.post_deletes.len()
}

fn mutation_event(event: ExecutionEvent) -> MutationEvent {
    match event {
        ExecutionEvent::TypeConflictDeleted { remote_path } => {
            MutationEvent::TypeConflictDeleted { remote_path }
        }
        ExecutionEvent::DirectoryCreated { remote_path } => {
            MutationEvent::DirectoryCreated { remote_path }
        }
        ExecutionEvent::RemoteContentCopied {
            from_remote_path,
            to_remote_path,
            bytes,
        } => MutationEvent::RemoteContentCopied {
            from_remote_path,
            to_remote_path,
            bytes,
        },
        ExecutionEvent::CopyFallbackUploaded { relative, bytes } => {
            MutationEvent::CopyFallbackUploaded { relative, bytes }
        }
        ExecutionEvent::Uploaded { relative, bytes } => MutationEvent::Uploaded { relative, bytes },
        ExecutionEvent::RemoteExtraDeleted { remote_path } => {
            MutationEvent::RemoteExtraDeleted { remote_path }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_defaults_are_safe_and_fields_are_validated() {
        let request = SyncRequest::builder(
            "https://files.example.invalid",
            "user",
            ".",
            "/home/Drive/backup",
        )
        .build()
        .expect("build safe defaults");
        assert!(!request.allow_http);
        assert!(!request.accept_invalid_certificates);
        assert_eq!(request.comparison, Comparison::Content);
        assert_eq!(request.deletion, DeletionPolicy::disabled());
        assert_eq!(request.jobs, DEFAULT_JOBS);

        SyncRequest::builder(
            "https://files.example.invalid",
            "user",
            ".",
            "/home/Drive/backup",
        )
        .jobs(MAX_JOBS)
        .retries(MAX_RETRIES)
        .build()
        .expect("public production limits are inclusive");

        for error in [
            SyncRequest::builder(
                "https://files.example.invalid",
                "user",
                ".",
                "/home/Drive/backup",
            )
            .jobs(MAX_JOBS + 1)
            .build()
            .expect_err("worker count above the production limit must fail"),
            SyncRequest::builder(
                "https://files.example.invalid",
                "user",
                ".",
                "/home/Drive/backup",
            )
            .retries(MAX_RETRIES + 1)
            .build()
            .expect_err("retry count above the production limit must fail"),
        ] {
            assert_eq!(error.code(), ErrorCode::InvalidRequest);
        }

        let error = SyncRequest::builder(
            "http://files.example.invalid",
            "user",
            ".",
            "/home/Drive/backup",
        )
        .build()
        .expect_err("HTTP must stay opt-in");
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
    }

    #[test]
    fn secrets_are_redacted_and_deletion_is_explicitly_bounded() {
        let secret = Secret::new("do-not-print-me");
        assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");
        assert!(!format!("{secret:?}").contains("do-not-print-me"));

        assert!(DeletionPolicy::bounded(0).is_err());
        let policy = DeletionPolicy::bounded(7)
            .expect("positive deletion bound")
            .allow_empty_source();
        assert!(policy.enabled());
        assert_eq!(policy.max_delete(), 7);
        assert!(policy.empty_source_allowed());
    }

    #[test]
    fn pre_cancelled_engine_never_requests_a_secret() {
        struct NoSecrets {
            calls: usize,
        }
        impl SecretProvider for NoSecrets {
            fn password(&mut self) -> std::result::Result<Secret, SecretProviderError> {
                self.calls += 1;
                Ok(Secret::new("secret"))
            }

            fn otp(
                &mut self,
                _challenge: OtpChallenge,
            ) -> std::result::Result<Option<Secret>, SecretProviderError> {
                self.calls += 1;
                Ok(None)
            }
        }

        let request = SyncRequest::builder(
            "https://files.example.invalid",
            "user",
            ".",
            "/home/Drive/backup",
        )
        .build()
        .expect("valid request");
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let mut secrets = NoSecrets { calls: 0 };
        let error = Engine
            .run(
                &request,
                &mut secrets,
                &cancellation,
                |_| PlanDecision::PreviewOnly,
                |_| EventControl::Continue,
            )
            .expect_err("pre-cancelled run");
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(secrets.calls, 0);
    }
}
