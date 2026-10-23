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
    /// What a run that failed part-way through execution had done first. Boxed, so the common
    /// error stays small.
    partial: Option<Box<PartialExecution>>,
}

/// What execution completed, and deferred, before the run failed.
#[derive(Debug)]
struct PartialExecution {
    completed: ExecutionSummary,
    deferred: Vec<DeferredPath>,
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

    /// What execution completed before the run failed part-way through, or `None` when it failed
    /// before executing anything. Mutations counted here did happen on the NAS.
    pub fn completed(&self) -> Option<ExecutionSummary> {
        self.partial.as_ref().map(|partial| partial.completed)
    }

    /// Paths the run had deferred, each with its reason, before it failed.
    pub fn deferred(&self) -> &[DeferredPath] {
        self.partial
            .as_ref()
            .map_or(&[], |partial| partial.deferred.as_slice())
    }

    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            partial: None,
        }
    }

    fn with_partial_execution(
        mut self,
        completed: ExecutionSummary,
        deferred: Vec<DeferredPath>,
    ) -> Self {
        self.partial = Some(Box::new(PartialExecution {
            completed,
            deferred,
        }));
        self
    }

    fn from_core(error: Error) -> Self {
        let code = match &error {
            Error::InvalidUrl(_)
            | Error::HttpsRequired
            | Error::UnsafeRemotePath { .. }
            | Error::ScopeNotFound(_)
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
            | Error::ResyncTicketStale { .. }
            | Error::RemoteSnapshotChanged(_) => ErrorCode::Safety,
            Error::Cancelled => ErrorCode::Cancelled,
            Error::ReconciliationPending { .. } => ErrorCode::Reconciliation,
            Error::Message(_) => ErrorCode::Internal,
            Error::InvalidResponse { .. }
            | Error::Api { .. }
            | Error::RemoteRefused { .. }
            | Error::UnsupportedApiVersion { .. }
            | Error::MissingApi(_)
            | Error::ShareNotWritable(_)
            | Error::RemoteEscape(_)
            | Error::RemoteMountRoot { .. }
            | Error::InvalidContentHash
            | Error::ContentVerificationFailed(_)
            | Error::ServerCopyNotStarted
            | Error::ServerCopyFailed { .. }
            | Error::RemoteDigestsFailing { .. }
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
    /// Compare size, File Station-resolution mtime, MD5, IEEE CRC32, and SHA-256 content.
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
    /// A NAS entry renamed in place to the local spelling (a case-only rename). Carried out last.
    RenameCaseOnly,
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
    renames: usize,
    skipped: Vec<SkippedPath>,
}

/// A path the plan neither carries nor deletes, and why.
///
/// Each of these used to fail the whole run. Now only the path is left alone, and an embedding
/// application must be able to show it: a link, a SYSTEM-attribute file or a case collision is a
/// file that is not on the NAS.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkippedPath {
    relative: String,
    reason: &'static str,
    detail: String,
}

impl SkippedPath {
    pub fn relative(&self) -> &str {
        &self.relative
    }

    /// Stable tag: `windows-attributes`, `link-or-reparse-point`, `dehydrated-placeholder`,
    /// `local-case-collision`, `remote-case-collision`, `case-rename-needs-delete`, `deferred`,
    /// `type-conflict`, `remote-link` or `protected-descendant`.
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// True for a skip that recurs on every run until someone acts on it, as opposed to a
    /// `deferred` path the next run picks up.
    pub fn is_lasting(&self) -> bool {
        self.reason != "deferred"
    }
}

impl PlanSummary {
    /// Paths this plan leaves out, lasting and deferred alike, sorted by path.
    pub fn skipped(&self) -> &[SkippedPath] {
        &self.skipped
    }

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

    /// Case-only renames this plan carries out on the NAS. Each is also in [`Self::changes`].
    pub fn renames(&self) -> usize {
        self.renames
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

/// Why a mirror deletion was kept rather than performed.
///
/// Declared here rather than re-exported, for the reason [`MutationEvent`] is: the SDK surface is
/// stable and free of internal types, so an engine enum gaining a variant is a decision this layer
/// makes explicitly instead of one that leaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteKept {
    /// The local path the deletion was scheduled against exists again.
    LocalPathExists,
    /// A deletion beneath this directory was kept, so the directory is not empty and a
    /// non-recursive delete of it could only fail.
    KeptDescendant,
}

/// One mutation exposed without internal plan types.
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
    /// A mirror deletion that did not happen, and why. Reported so an embedding caller's counts
    /// distinguish a mirror that converged from one that declined to.
    RemoteExtraDeleteSkipped {
        remote_path: String,
        relative: String,
        reason: DeleteKept,
    },
    /// A file whose planned deletion was kept, uploaded later in the same run so the mirror
    /// converges rather than deferring to the next run.
    KeptDeleteUploaded {
        relative: String,
        bytes: u64,
    },
    /// A path this run planned for and left for the next run. Not a mutation: reported so an
    /// embedding caller can name what did not converge, and why.
    PathDeferred {
        relative: String,
        reason: DeferredReason,
    },
    /// A NAS entry renamed in place to the local spelling (a case-only rename).
    Renamed {
        from_remote_path: String,
        to_remote_path: String,
    },
}

/// Why a path was left for the next run.
///
/// Declared here rather than re-exported, for the reason [`MutationEvent`] is. Each of these used
/// to fail the whole run; now only the path is skipped, by name, and the run ends
/// [partial](SyncOutcome::partial).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "kebab-case")]
pub enum DeferredReason {
    /// The local file changed while the run was reading or sending it.
    SourceChanged,
    /// The local file disappeared while the run was working.
    SourceVanished,
    /// The local file could not be opened or read (locked); retried next run.
    SourceUnreadable,
    /// The local entry is not a regular file or directory the sync carries. Lasting.
    UnsupportedLocalEntry,
    /// File Station could not compute the remote file's checksum; compared again next run.
    RemoteDigestUnavailable,
    /// The account may not read this remote file; lasting until its permissions change.
    RemoteUnreadable,
    /// The remote entry changed after planning and was left as found; planned again next run.
    RemoteChanged,
    /// The remote name cannot exist on this host's filesystem; lasting, never deleted on that
    /// account.
    UnrepresentableName,
    /// The listing of the containing remote folder was incomplete, so no deletion was made there.
    /// Lasting while DSM keeps listing it short.
    ListingIncomplete,
    /// The NAS-side copy finished with an error; uploaded next run.
    ServerCopyFailed,
    /// The NAS refused the deletion; retried next run.
    DeleteFailed,
    /// The NAS refuses this one path (its name, this account's access to it, its size for the
    /// destination filesystem); lasting until it is renamed or its permissions change.
    RemoteRefused,
    /// This account may not read the local file or folder; lasting until its permissions change.
    LocalPermissionDenied,
    /// A case-only rename on the NAS did not finish (File Station refused it, or the old spelling
    /// is still there beside the new one). Nothing was deleted on its account; lasting, and named
    /// on every run until someone resolves the two spellings.
    CaseRenamePending,
    /// A NAS entry the remote walk will not carry (a name this tool cannot address, a link out of
    /// the destination, a folder seen twice); never deleted, overwritten or descended. Lasting.
    RemoteSkipped,
    /// A local name, path or entry kind this tool refuses to carry; lasting until it is renamed.
    UnsupportedEntry,
    /// The NAS refuses this account the deletion (no permission); lasting until that changes.
    DeleteNotPermitted,
}

impl DeferredReason {
    /// Whether the path stays left out of every run until someone changes it. Such a path is
    /// reported, but does not make a run [partial](SyncOutcome::partial). The command line's rule.
    ///
    /// Answered by the engine's own rule, so the SDK cannot disagree with the command line.
    pub fn is_lasting(self) -> bool {
        self.core().is_lasting()
    }

    fn core(self) -> sync::DeferReason {
        match self {
            Self::SourceChanged => sync::DeferReason::SourceChanged,
            Self::SourceVanished => sync::DeferReason::SourceVanished,
            Self::SourceUnreadable => sync::DeferReason::SourceUnreadable,
            Self::UnsupportedLocalEntry => sync::DeferReason::UnsupportedLocalEntry,
            Self::RemoteDigestUnavailable => sync::DeferReason::RemoteDigestUnavailable,
            Self::RemoteUnreadable => sync::DeferReason::RemoteUnreadable,
            Self::RemoteChanged => sync::DeferReason::RemoteChanged,
            Self::UnrepresentableName => sync::DeferReason::UnrepresentableName,
            Self::ListingIncomplete => sync::DeferReason::ListingIncomplete,
            Self::ServerCopyFailed => sync::DeferReason::ServerCopyFailed,
            Self::DeleteFailed => sync::DeferReason::DeleteFailed,
            Self::RemoteRefused => sync::DeferReason::RemoteRefused,
            Self::LocalPermissionDenied => sync::DeferReason::LocalPermissionDenied,
            Self::CaseRenamePending => sync::DeferReason::CaseRenamePending,
            Self::RemoteSkipped => sync::DeferReason::RemoteSkipped,
            Self::UnsupportedEntry => sync::DeferReason::UnsupportedEntry,
            Self::DeleteNotPermitted => sync::DeferReason::DeleteNotPermitted,
        }
    }
}

impl From<sync::DeferReason> for DeferredReason {
    fn from(reason: sync::DeferReason) -> Self {
        match reason {
            sync::DeferReason::SourceChanged => Self::SourceChanged,
            sync::DeferReason::SourceVanished => Self::SourceVanished,
            sync::DeferReason::SourceUnreadable => Self::SourceUnreadable,
            sync::DeferReason::UnsupportedLocalEntry => Self::UnsupportedLocalEntry,
            sync::DeferReason::RemoteDigestUnavailable => Self::RemoteDigestUnavailable,
            sync::DeferReason::RemoteUnreadable => Self::RemoteUnreadable,
            sync::DeferReason::RemoteChanged => Self::RemoteChanged,
            sync::DeferReason::UnrepresentableName => Self::UnrepresentableName,
            sync::DeferReason::ListingIncomplete => Self::ListingIncomplete,
            sync::DeferReason::ServerCopyFailed => Self::ServerCopyFailed,
            sync::DeferReason::DeleteFailed => Self::DeleteFailed,
            sync::DeferReason::RemoteRefused => Self::RemoteRefused,
            sync::DeferReason::LocalPermissionDenied => Self::LocalPermissionDenied,
            sync::DeferReason::CaseRenamePending => Self::CaseRenamePending,
            sync::DeferReason::RemoteSkipped => Self::RemoteSkipped,
            sync::DeferReason::UnsupportedEntry => Self::UnsupportedEntry,
            sync::DeferReason::DeleteNotPermitted => Self::DeleteNotPermitted,
        }
    }
}

/// One path a run left for the next run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeferredPath {
    relative: String,
    reason: DeferredReason,
    detail: String,
}

impl DeferredPath {
    /// Source-relative, `/`-separated.
    pub fn relative(&self) -> &str {
        &self.relative
    }

    pub fn reason(&self) -> DeferredReason {
        self.reason
    }

    /// The underlying observation, for an operator. Never a credential.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

fn sdk_deferred(records: &[sync::DeferredPath]) -> Vec<DeferredPath> {
    records
        .iter()
        .map(|record| DeferredPath {
            relative: record.relative.clone(),
            reason: record.reason.into(),
            detail: record.detail.clone(),
        })
        .collect()
}

/// Append `extra` to `records`, keeping the first record for any path named twice.
fn merge_deferred(records: &mut Vec<sync::DeferredPath>, extra: Vec<sync::DeferredPath>) {
    for record in extra {
        if !records
            .iter()
            .any(|existing| existing.relative == record.relative)
        {
            records.push(record);
        }
    }
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
///
/// Carries the same signals the command line reports, so an embedding application is never the
/// one caller left without them: before these were added, a DSM that ignored the upload `mtime`
/// made every SDK run re-upload the whole tree with nothing in the outcome to say why (ADV-R-4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ExecutionSummary {
    deleted: usize,
    created: usize,
    copied: usize,
    uploaded: usize,
    uploaded_bytes: u64,
    mtime_not_kept: usize,
    copy_fallbacks: usize,
    deletes_skipped: usize,
    kept_deletes_uploaded: usize,
    renamed: usize,
}

impl ExecutionSummary {
    fn from_report(report: &sync::ExecutionReport, client: &ApiClient) -> Self {
        Self {
            deleted: report.deleted,
            created: report.created,
            copied: report.copied,
            uploaded: report.uploaded,
            uploaded_bytes: report.uploaded_bytes,
            // The command line's sum: copies counted by the executor, uploads by the client,
            // which is the only place that sees the time DSM kept.
            mtime_not_kept: report
                .mtime_not_kept
                .saturating_add(client.remote_mtime_unhonoured_count()),
            copy_fallbacks: report.copy_fallbacks,
            deletes_skipped: report.deletes_skipped,
            kept_deletes_uploaded: report.kept_deletes_uploaded,
            renamed: report.renamed,
        }
    }
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

    /// Files written whose modification time DSM did not keep within tolerance of the one sent.
    /// Non-zero means `metadata` and `content` comparison will see them as changed next run: the
    /// NAS is ignoring the upload `mtime`, and every run re-uploads them.
    pub fn mtime_not_kept(self) -> usize {
        self.mtime_not_kept
    }

    /// Planned server-side copies DSM refused to start, uploaded instead (also in `uploaded`).
    pub fn copy_fallbacks(self) -> usize {
        self.copy_fallbacks
    }

    /// Planned mirror deletions kept rather than performed.
    pub fn deletes_skipped(self) -> usize {
        self.deletes_skipped
    }

    /// Files uploaded because their planned deletion was kept (not in `uploaded`).
    pub fn kept_deletes_uploaded(self) -> usize {
        self.kept_deletes_uploaded
    }

    /// Case-only renames carried out on the NAS.
    pub fn renamed(self) -> usize {
        self.renamed
    }
}

/// Result of one complete, logged-out engine run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SyncOutcome {
    plan: PlanSummary,
    applied: bool,
    reconciled: bool,
    execution: Option<ExecutionSummary>,
    deferred: Vec<DeferredPath>,
    /// Serialised, so FFI callers see retries, re-logins, keepalive failures and orphaned server
    /// tasks too; skipping it left them the only callers who could not (ADV-R-4).
    recovered: crate::api::TransportCounters,
    /// Root-relative NAS folders whose listing DSM returned incomplete (`""` is the destination
    /// itself). No deletion is made inside them; reported even when none was planned there.
    incomplete_listings: Vec<String>,
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

    /// Paths this run left for the next run, each with its reason.
    pub fn deferred(&self) -> &[DeferredPath] {
        &self.deferred
    }

    /// True when the run completed everything except the paths in [`Self::deferred`].
    ///
    /// Distinct from success and from failure: every other path converged, nothing was deleted on
    /// the evidence that failed, and the next run picks the deferred paths up.
    pub fn partial(&self) -> bool {
        // The command line's rule: a path that will be left out of every run (a name this host
        // cannot hold, an entry the sync never carries) is reported, but is not "partial".
        self.deferred
            .iter()
            .any(|record| !record.reason.is_lasting())
    }

    /// Retries, re-logins and abandoned server tasks the run recovered from, or could not.
    pub fn recovered(&self) -> crate::api::TransportCounters {
        self.recovered
    }

    /// NAS folders whose listing was incomplete, so nothing in them was deleted. See the field.
    pub fn incomplete_listings(&self) -> &[String] {
        &self.incomplete_listings
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
        // Read before the first scan, so "was this written while the run was working" has an
        // answer that predates every observation the run made.
        let started_at = std::time::SystemTime::now();
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
        let mut local =
            local::scan(&request.source, &rules, cancellation).map_err(SdkError::from_core)?;
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
        .map(|client| {
            client
                .with_max_upload_rate(request.max_upload_rate)
                .with_cancellation(cancellation)
        })
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
            let scanned = client
                .remote_inventory_scoped(
                    &root,
                    &plan::Scope::root(),
                    &rules,
                    usize::MAX,
                    cancellation,
                )
                .map_err(SdkError::from_core)?;
            let incomplete_directories = scanned.incomplete_directories;
            let skipped_remote = scanned.skipped;
            let mut remote = scanned.inventory;
            // The command line's breaker: link boundaries on most folders mean DSM's real paths
            // are not what the link guard assumes, and the run fails by name rather than
            // skipping the tree folder by folder.
            sync::check_link_boundaries(&remote).map_err(SdkError::from_core)?;
            emit(
                cancellation,
                &mut on_event,
                SdkEvent::PhaseCompleted {
                    phase: Phase::RemoteScan,
                },
            )?;

            let options = PlanOptions {
                delete: request.deletion.enabled,
                allow_empty_source: request.deletion.allow_empty_source,
                max_delete: request.deletion.max_delete,
                compare: request.comparison.into(),
                server_copy,
                // The SDK always plans the whole tree; scoping is a command-line concern.
                scope: plan::Scope::root(),
            };
            // The same digest pass, with the same per-path deferral, as the command line.
            let content = request.comparison == Comparison::Content;
            if content {
                emit(
                    cancellation,
                    &mut on_event,
                    SdkEvent::PhaseStarted {
                        phase: Phase::ContentHashing,
                    },
                )?;
            }
            let mut deferred = sync::populate_plan_digests_with_listings(
                &client,
                &mut local,
                &mut remote,
                &rules,
                options.compare,
                server_copy,
                options.delete,
                &incomplete_directories,
                cancellation,
            )
            .map_err(SdkError::from_core)?;
            if content {
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
            let mut plan = plan::build_plan(&root, &local, &remote, &rules, &options)
                .map_err(SdkError::from_core)?;
            merge_deferred(
                &mut deferred,
                sync::withhold_deletions_in_incomplete_listings(&mut plan, &incomplete_directories),
            );
            // The command line's guards, in its order: a NAS entry the walk would not carry keeps
            // its folders, and a deferral the plan names among its skips is a record too.
            merge_deferred(
                &mut deferred,
                sync::protect_skipped_remote_entries(&mut plan, &skipped_remote),
            );
            let named_in_plan = sync::deferrals_in_plan_skips(&plan, &deferred);
            merge_deferred(&mut deferred, named_in_plan);
            // This plan is executed, so its uploads need the strong digest the upload path
            // verifies against. See `plan::promote_upload_fingerprints`.
            merge_deferred(
                &mut deferred,
                sync::promote_upload_fingerprints_deferring(&client, &mut plan, cancellation)
                    .map_err(SdkError::from_core)?,
            );
            sync::record_deferrals_in_plan(&mut plan, &deferred);
            let plan = plan;
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
                    deferred: sdk_deferred(&deferred),
                    recovered: client.transport_counters(),
                    incomplete_listings: incomplete_directories.clone(),
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
            let mut report = crate::sync::ExecutionReport::default();
            if !plan.is_empty() {
                // Long local phases are over; touch the session before the first upload so an
                // idle timeout that expired during hashing is renewed rather than failing it.
                client.touch_session();
                let observer_factory: UploadObserverFactory = Arc::new(|_| None);
                // Reporting what completed even when the run then fails: a mid-run failure used
                // to lose every deferral and count collected so far from the outcome.
                let executed = sync::execute_observed_reporting_partial(
                    &client,
                    &root,
                    &plan,
                    ExecuteOptions {
                        jobs: request.jobs,
                        dry_run: false,
                    },
                    cancellation.clone(),
                    observer_factory,
                    &mut report,
                    |event| {
                        if on_event(&SdkEvent::Mutation {
                            mutation: mutation_event(event),
                        }) == EventControl::Cancel
                        {
                            cancellation.cancel();
                        }
                    },
                );
                if let Err(error) = executed {
                    merge_deferred(&mut deferred, report.deferred.clone());
                    return Err(SdkError::from_core(error).with_partial_execution(
                        ExecutionSummary::from_report(&report, &client),
                        sdk_deferred(&deferred),
                    ));
                }
            }
            merge_deferred(&mut deferred, report.deferred.clone());
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
            if !plan.is_empty() {
                let (reconciliation, reconciliation_deferred) =
                    reconciliation_plan(&client, request, &root, &rules, &options, cancellation)?;
                merge_deferred(&mut deferred, reconciliation_deferred);
                // The command line's rules exactly, so an embedding application cannot fail a run
                // the CLI reports as a success, or the reverse.
                sync::ensure_reconciled(
                    &plan.source_root,
                    &plan,
                    &reconciliation,
                    started_at,
                    &sync::DeferralIndex::from_records(&deferred),
                )
                .map_err(SdkError::from_core)?;
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
                execution: Some(ExecutionSummary::from_report(&report, &client)),
                deferred: sdk_deferred(&deferred),
                recovered: client.transport_counters(),
                incomplete_listings: incomplete_directories,
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

/// Rebuild the plan after execution, through the same digest pass and the same deletion
/// withholding as the run, under the shared reconciliation options.
fn reconciliation_plan(
    client: &ApiClient,
    request: &SyncRequest,
    root: &RemoteRoot,
    rules: &IgnoreRules,
    run_options: &PlanOptions,
    cancellation: &CancellationToken,
) -> SdkResult<(SyncPlan, Vec<sync::DeferredPath>)> {
    check_cancellation(cancellation)?;
    let mut local =
        local::scan(&request.source, rules, cancellation).map_err(SdkError::from_core)?;
    check_cancellation(cancellation)?;
    let scanned = client
        .remote_inventory_scoped(root, &plan::Scope::root(), rules, usize::MAX, cancellation)
        .map_err(SdkError::from_core)?;
    let mut remote = scanned.inventory;
    check_cancellation(cancellation)?;
    // No promotion: a reconciliation plan is inspected for emptiness, never executed, so nothing
    // consumes an upload's digest here.
    let mut deferred = sync::populate_plan_digests_with_listings(
        client,
        &mut local,
        &mut remote,
        rules,
        run_options.compare,
        run_options.server_copy,
        run_options.delete,
        &scanned.incomplete_directories,
        cancellation,
    )
    .map_err(SdkError::from_core)?;
    let mut plan = plan::build_plan(
        root,
        &local,
        &remote,
        rules,
        &sync::reconciliation_options(run_options, run_options.compare),
    )
    .map_err(SdkError::from_core)?;
    deferred.extend(sync::withhold_deletions_in_incomplete_listings(
        &mut plan,
        &scanned.incomplete_directories,
    ));
    deferred.extend(sync::protect_skipped_remote_entries(
        &mut plan,
        &scanned.skipped,
    ));
    let named_in_plan = sync::deferrals_in_plan_skips(&plan, &deferred);
    deferred.extend(named_in_plan);
    Ok((plan, deferred))
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
    // A rename changes the NAS as surely as an upload does, so a caller previewing the plan has
    // to see it before it is applied, not only in the execution count afterwards.
    changes.extend(plan.renames.iter().map(|action| PlannedChange {
        operation: PlanOperation::RenameCaseOnly,
        remote_path: action.to_remote_path.clone(),
        source: Some(action.from_remote_path.clone()),
        bytes: 0,
        reason: "case-only-rename".to_owned(),
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
        renames: plan.renames.len(),
        skipped: plan
            .skipped
            .iter()
            .map(|entry| SkippedPath {
                relative: entry.relative.clone(),
                reason: entry.reason.as_str(),
                detail: entry.reason.detail(),
            })
            .collect(),
    }
}

fn operation_count(plan: &SyncPlan) -> usize {
    plan.pre_deletes.len()
        + plan.creates.len()
        + plan.copies.len()
        + plan.uploads.len()
        + plan.post_deletes.len()
        + plan.renames.len()
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
        ExecutionEvent::RemoteExtraDeleteSkipped {
            remote_path,
            relative,
            reason,
        } => MutationEvent::RemoteExtraDeleteSkipped {
            remote_path,
            relative,
            reason: match reason {
                sync::DeleteKeptReason::LocalPathExists => DeleteKept::LocalPathExists,
                sync::DeleteKeptReason::KeptDescendant => DeleteKept::KeptDescendant,
            },
        },
        ExecutionEvent::KeptDeleteUploaded { relative, bytes } => {
            MutationEvent::KeptDeleteUploaded { relative, bytes }
        }
        ExecutionEvent::PathDeferred { relative, reason } => MutationEvent::PathDeferred {
            relative,
            reason: reason.into(),
        },
        ExecutionEvent::Renamed {
            from_remote_path,
            to_remote_path,
        } => MutationEvent::Renamed {
            from_remote_path,
            to_remote_path,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every engine reason has an SDK reason that maps back to it, so `is_lasting` is the engine's.
    #[test]
    fn every_deferral_reason_round_trips_through_the_sdk() {
        for reason in sync::DeferReason::ALL {
            let sdk = DeferredReason::from(reason);
            assert_eq!(sdk.core(), reason);
            assert_eq!(sdk.is_lasting(), reason.is_lasting(), "{}", reason.as_str());
        }
    }

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
