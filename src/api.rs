use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use reqwest::blocking::multipart::{Form, Part};
use reqwest::blocking::{Client as HttpClient, Response};
use reqwest::header::{COOKIE, HeaderMap, HeaderValue, LOCATION, SET_COOKIE};
use reqwest::redirect::Policy;
use reqwest::{Certificate, Client as AsyncHttpClient, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::cancel::CancellationToken;
use crate::integrity::{ContentHasher, ContentMd5};
use crate::local::{EntryKind, LocalEntry};
// Only the closed, secret-free value types are imported. Log levels, event codes, and the logger
// itself deliberately stay out of the transport layer: this module reports what happened on the
// wire, and the binary decides what that is worth logging.
use crate::observability::{
    ApiCallDetail, BoundedText, CdnMarker, CertificateVerification, ConnectionDetail, CookieFact,
    CookieFacts, CookiePersistence, CookieSameSite, DecodeFault, DecodeFaultKind,
    IntermediaryFacts, JsonKind, LoginFormat, RequestOutcome, RequestTransport, SessionShape,
    SessionTransport, ShortToken, UrlScheme,
};
use crate::path::{RemoteRoot, parent_and_name};
use crate::plan::Scope;
use crate::{Error, Result};

const DISCOVERY_APIS: &[&str] = &[
    "SYNO.API.Auth",
    "SYNO.FileStation.Info",
    "SYNO.FileStation.List",
    "SYNO.FileStation.CreateFolder",
    "SYNO.FileStation.Upload",
    "SYNO.FileStation.Delete",
    "SYNO.FileStation.MD5",
    "SYNO.FileStation.Download",
    "SYNO.FileStation.CopyMove",
    "SYNO.FileStation.CheckPermission",
];
const LIST_PAGE_SIZE: usize = 500;
const MAX_JSON_RESPONSE: u64 = 32 * 1024 * 1024;
const MAX_CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest a rate-limited reader sleeps before it looks at the cancellation token again.
const RATE_LIMIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// A single shared I/O runtime keeps synchronous SDK calls safe even when their caller is already
/// running on Tokio. Download futures never run on or block the caller's executor.
const DOWNLOAD_RUNTIME_THREADS: usize = 1;
/// Rate-limit tokens are held scaled by this factor so refill stays exact integer arithmetic.
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const WRITE_PROBE_PAYLOAD: &[u8] = b"synology-drive-sync disposable write probe v1\n";
const WRITE_PROBE_FILE_NAME: &str = "probe.bin";
const WRITE_PROBE_COPY_DIRECTORY: &str = "copy";
const DIAGNOSTIC_INVENTORY_SAMPLE_LIMIT: usize = 5;
const DIAGNOSTIC_INVENTORY_REQUEST_LIMIT: usize = DIAGNOSTIC_INVENTORY_SAMPLE_LIMIT + 1;
const DIAGNOSTIC_INVENTORY_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(5);
const DIAGNOSTIC_RELATIVE_PATH_MAX_CHARS: usize = 512;
const DIAGNOSTIC_NAME_MAX_CHARS: usize = 255;
const DISCOVERY_FAILURE_OPERATION: &str = "File Station API discovery";
const X_SYNO_TOKEN_HEADER: &str = "x-syno-token";
const MAX_SESSION_HEADER_BYTES: usize = 4 * 1024;
static WRITE_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether this client keeps a cookie jar and replays what a server sets.
///
/// It does not: no `reqwest` client built here enables `cookie_store`, so every `Set-Cookie` DSM
/// or an intermediary sends is observed and then discarded. The only cookie that ever goes back
/// out is the `id=<sid>` header synthesised from the login response. A diagnostic that reports on
/// cookie permanence has to state that, and reading the fact from here rather than asserting it
/// in prose means the report follows the code if a jar is ever enabled.
pub const CLIENT_MAINTAINS_COOKIE_JAR: bool = false;

/// Cookie names DSM itself is known to set. Anything else came from something in between.
///
/// `id` is the session identifier, `smid` its shared-folder sibling, `stay_login` the persistent
/// login opt-in, and `did`/`device_id` the trusted-device marker set after a 2FA login.
const DSM_COOKIE_NAMES: &[&str] = &[
    "id",
    "smid",
    "stay_login",
    "did",
    "device_id",
    "sharing_sid",
];

/// Whether a cookie name is one DSM sets itself, rather than one an intermediary added.
pub(crate) fn is_dsm_cookie_name(name: &str) -> bool {
    DSM_COOKIE_NAMES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(name))
}

/// Return whether a failed client connection still received an HTTP/DSM response from at least
/// one discovery route. Doctor uses this evidence to keep transport negotiation distinct from an
/// invalid or unavailable API-discovery response.
pub fn is_discovery_response_failure(error: &Error) -> bool {
    matches!(
        error,
        Error::InvalidResponse { operation, .. } if operation == DISCOVERY_FAILURE_OPERATION
    )
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub base_url: String,
    pub allow_http: bool,
    pub accept_invalid_certs: bool,
    pub ca_certificate: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub retries: u32,
}

#[derive(Clone)]
struct Session {
    sid: Zeroizing<String>,
    syno_token: Option<Zeroizing<String>>,
}

#[derive(Clone)]
struct SessionHeaders {
    cookie: HeaderValue,
    syno_token: Option<HeaderValue>,
}

#[derive(Clone, Debug, Deserialize)]
struct ApiSpec {
    path: String,
    #[serde(rename = "minVersion")]
    min_version: u32,
    #[serde(rename = "maxVersion")]
    max_version: u32,
    #[serde(default, rename = "requestFormat")]
    _request_format: Option<String>,
}

#[derive(Clone)]
pub struct ApiClient {
    http: HttpClient,
    download_http: AsyncHttpClient,
    base: Url,
    apis: HashMap<String, ApiSpec>,
    session: Option<Session>,
    retries: u32,
    control_timeout: Duration,
    /// Optional absolute budget shared by every control request made through
    /// this client. Interactive callers use this so discovery fallbacks,
    /// authentication challenges, and logout cannot each restart the clock.
    control_deadline: Option<Instant>,
    upload_timeout: Duration,
    operation_timeout: Duration,
    upload_rate_limit: Option<Arc<Mutex<TokenBucket>>>,
    /// The process cancellation token, shared by this client and every clone of it.
    ///
    /// Per-operation methods keep their explicit `&CancellationToken` parameter; this field
    /// exists only for the waits the client performs on a caller's behalf deep inside
    /// `send_form_with_retry`, where no per-operation token is in scope. Without it a control
    /// request could stay parked in a backoff sleep for seconds after Ctrl-C.
    cancellation: CancellationToken,
    /// Optional transport instrumentation, shared by this client and every clone of it.
    observer: Option<RequestObserver>,
}

#[derive(Clone, Debug)]
pub struct RemoteEntry {
    pub relative: String,
    pub remote_path: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_seconds: i64,
    /// File Station mount-point type (for example CIFS/NFS/ISO), when present.
    /// Mounted directories are inventory boundaries and are never traversed or deleted.
    pub mount_point_type: Option<String>,
    pub content_md5: Option<ContentMd5>,
}

/// One File Station directory that the authenticated account may browse.
///
/// This deliberately contains only display/path metadata. File Station session
/// identifiers, Synology tokens, credentials, and raw permission documents are
/// never exposed to callers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RemoteDirectory {
    pub name: String,
    pub path: String,
}

/// A single bounded page for an interactive File Station directory chooser.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RemoteDirectoryPage {
    pub parent: String,
    pub directories: Vec<RemoteDirectory>,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct RemoteInventory {
    pub root_exists: bool,
    pub entries: BTreeMap<String, RemoteEntry>,
}

/// A remote inventory walked under a scope, plus whether the budget let it finish.
#[derive(Debug)]
pub struct ScopedRemoteInventory {
    pub inventory: RemoteInventory,
    /// False when the entry budget stopped the walk, making the inventory a subset of the scope.
    pub complete: bool,
}

/// One bounded, display-safe entry returned by a target diagnostic.
///
/// An explicitly selected target uses a path relative to that target. Shared-folder discovery uses
/// the absolute File Station logical root (for example `/team`), never a local DSM volume path.
/// This deliberately contains no content digest, ACL document, session identifier, or
/// server-provided free-form detail. Long names are truncated on a character boundary and
/// explicitly marked so diagnostic output stays bounded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticRemoteEntry {
    pub relative_path: String,
    pub relative_path_truncated: bool,
    pub name: String,
    pub name_truncated: bool,
    pub kind: EntryKind,
    pub size_bytes: Option<u64>,
    pub mtime_seconds: Option<i64>,
    pub mount_boundary: bool,
}

/// One non-recursive, single-page File Station diagnostic inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticRemoteInventory {
    pub root_exists: bool,
    /// Total entries reported by File Station for the selected diagnostic scope.
    pub total_entries: usize,
    pub sample: Vec<DiagnosticRemoteEntry>,
    pub truncated: bool,
    pub truncated_count: usize,
    pub truncated_reason: Option<&'static str>,
    pub pages_requested: u8,
    pub traversal_depth: u8,
    pub deadline_ms: u64,
}

impl DiagnosticRemoteInventory {
    /// The verdict for a destination File Station says is not there.
    ///
    /// Named once because DSM reports a missing path two ways -- as an envelope error and as a
    /// per-entry status -- and both have to reach the same answer.
    fn absent_root() -> Self {
        Self {
            root_exists: false,
            total_entries: 0,
            sample: Vec::new(),
            truncated: false,
            truncated_count: 0,
            truncated_reason: None,
            pages_requested: 0,
            traversal_depth: 0,
            deadline_ms: duration_millis_saturating(DIAGNOSTIC_INVENTORY_TIMEOUT),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationWriteCheck {
    /// The existing directory whose child-create permission was checked.
    pub checked_directory: String,
    /// Whether the configured destination itself existed when it was checked.
    pub destination_exists: bool,
}

/// One entry of DSM's full `SYNO.API.Info` map, read leniently.
///
/// Every field is optional, unlike the strict `ApiSpec` that feeds `required_spec`. `query=all`
/// on a NAS with third-party packages returns hundreds of entries authored by whoever wrote those
/// packages; a strict type would let one of them fail the whole enumeration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DiscoveredApi {
    pub path: Option<String>,
    #[serde(rename = "minVersion")]
    pub min_version: Option<u32>,
    #[serde(rename = "maxVersion")]
    pub max_version: Option<u32>,
    #[serde(default, rename = "requestFormat")]
    pub request_format: Option<String>,
}

impl DiscoveredApi {
    /// Whether this entry advertises a version range containing `version`.
    ///
    /// An entry that reported neither bound cannot answer the question, and says so rather than
    /// defaulting to a reassuring `true`.
    #[must_use]
    pub fn offers_version(&self, version: u32) -> Option<bool> {
        let (min, max) = (self.min_version?, self.max_version?);
        Some((min..=max).contains(&version))
    }
}

/// Everything DSM advertises, plus the entries it advertised unusably.
#[derive(Clone, Debug, Default)]
pub struct ApiCatalogue {
    pub apis: BTreeMap<String, DiscoveredApi>,
    /// Entries whose value did not decode into [`DiscoveredApi`]. Counted, never dropped silently.
    pub unusable_entries: usize,
}

/// An API version this tool asks DSM for, and what depends on it.
///
/// Every member is a compile-time literal, so a requirement can be reported anywhere -- including
/// beside a server-supplied name -- without carrying server text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiRequirement {
    pub api: &'static str,
    pub version: u32,
    /// Whether the tool has a working fallback when DSM does not offer it.
    pub optional: bool,
    /// What stops working without it. Rendered verbatim beside the API name.
    pub purpose: &'static str,
}

/// The API versions this tool actually requests, with what each one buys.
///
/// This is the requirement half of the capability matrix. It deliberately mirrors the versions
/// the call sites pass rather than the allowlist alone, because "DSM offers this API" and "DSM
/// offers the version we ask for" are different questions and only the second one ambushes people
/// at runtime.
pub const API_REQUIREMENTS: &[ApiRequirement] = &[
    // Login negotiates `min(6, maxVersion)` and refuses anything below 3, so 3 is the real floor
    // and reporting 6 here would call a working DSM incompatible.
    ApiRequirement {
        api: "SYNO.API.Auth",
        version: 3,
        optional: false,
        purpose: "authentication; login negotiates up to version 6 when DSM offers it",
    },
    ApiRequirement {
        api: "SYNO.FileStation.Info",
        version: 2,
        optional: true,
        purpose: "host identity and per-account capability report",
    },
    ApiRequirement {
        api: "SYNO.FileStation.List",
        version: 2,
        optional: false,
        purpose: "remote inventory and path resolution",
    },
    ApiRequirement {
        api: "SYNO.FileStation.CreateFolder",
        version: 2,
        optional: false,
        purpose: "creating destination directories",
    },
    ApiRequirement {
        api: "SYNO.FileStation.Upload",
        version: 2,
        optional: false,
        purpose: "uploading files",
    },
    ApiRequirement {
        api: "SYNO.FileStation.CheckPermission",
        version: 3,
        optional: false,
        purpose: "the non-mutating write pre-check",
    },
    ApiRequirement {
        api: "SYNO.FileStation.Download",
        version: 2,
        optional: true,
        purpose: "content-mode fingerprint verification",
    },
    ApiRequirement {
        api: "SYNO.FileStation.MD5",
        version: 2,
        optional: true,
        purpose: "remote MD5 comparison",
    },
    ApiRequirement {
        api: "SYNO.FileStation.Delete",
        version: 2,
        optional: true,
        purpose: "--delete and write-probe cleanup",
    },
    ApiRequirement {
        api: "SYNO.FileStation.CopyMove",
        version: 3,
        optional: true,
        purpose: "server-side copy; verified upload is used without it",
    },
];

/// How many session-channel variants the ablation probe runs.
pub const SESSION_CHANNEL_VARIANTS: usize = 5;

/// Which channels carry the DSM session on one request.
///
/// DSM accepts a session through several channels at once and this client normally presents all
/// of them, so a rejection cannot be attributed to any one. Naming the combination explicitly is
/// what makes the attribution possible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionChannels {
    /// Everything this client normally sends: the synthesised cookie, the token header, and both
    /// request fields. Run first, so the probe reproduces today's behaviour before varying it.
    All,
    /// Only the documented `_sid` request field, which is what `format=sid` login promises.
    SidFieldOnly,
    /// Only the synthesised `Cookie: id=<sid>` header.
    CookieOnly,
    /// Only the `X-SYNO-TOKEN` header, with no session identifier at all.
    TokenHeaderOnly,
    /// Only the documented `_sid` request field, from a session established *without*
    /// `enable_syno_token`.
    ///
    /// The other four vary how one session is presented. This one varies how the session was
    /// created, because that is the other half of the hypothesis: logging in with
    /// `enable_syno_token=yes` asks DSM for browser-style cookie-and-header authentication, and a
    /// DSM that then refuses the documented `_sid` parameter path may be refusing it *for that
    /// session* rather than in general. A login without the flag settles which.
    SidFieldOnlyTokenlessLogin,
}

impl SessionChannels {
    /// Ablation order. `All` is first deliberately: a probe that changed a variable before
    /// reproducing the observed behaviour would have nothing to compare against.
    pub const ABLATION_ORDER: [Self; SESSION_CHANNEL_VARIANTS] = [
        Self::All,
        Self::SidFieldOnly,
        Self::CookieOnly,
        Self::TokenHeaderOnly,
        Self::SidFieldOnlyTokenlessLogin,
    ];

    #[must_use]
    pub fn sends_cookie_header(self) -> bool {
        matches!(self, Self::All | Self::CookieOnly)
    }

    #[must_use]
    pub fn sends_token_header(self) -> bool {
        matches!(self, Self::All | Self::TokenHeaderOnly)
    }

    #[must_use]
    pub fn sends_sid_field(self) -> bool {
        matches!(
            self,
            Self::All | Self::SidFieldOnly | Self::SidFieldOnlyTokenlessLogin
        )
    }

    #[must_use]
    pub fn sends_token_field(self) -> bool {
        matches!(self, Self::All | Self::SidFieldOnly)
    }

    /// Whether the variant needs a session established separately from the run's own.
    #[must_use]
    pub fn needs_tokenless_login(self) -> bool {
        self == Self::SidFieldOnlyTokenlessLogin
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::SidFieldOnly => "sid-field-only",
            Self::CookieOnly => "cookie-only",
            Self::TokenHeaderOnly => "token-header-only",
            Self::SidFieldOnlyTokenlessLogin => "sid-field-only-tokenless-login",
        }
    }

    /// How the variant reads in the report, in the same vocabulary the call lines use.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::All => "cookie + token-header + sid-field + token-field",
            Self::SidFieldOnly => "sid-field + token-field",
            Self::CookieOnly => "cookie only",
            Self::TokenHeaderOnly => "token-header only",
            Self::SidFieldOnlyTokenlessLogin => "sid-field only, second login without SynoToken",
        }
    }
}

/// One ablation variant's result. `Copy`, and carries no value of any channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelProbe {
    pub channels: SessionChannels,
    pub outcome: RequestOutcome,
    pub dsm_code: Option<i64>,
    pub http_status: Option<u16>,
    pub elapsed_ms: u64,
    /// Why this variant was not attempted, when it was not.
    ///
    /// A compile-time string, so no runtime text reaches the report through it. A variant that
    /// did not run is not evidence of anything, and reading it as a rejection would put a finding
    /// in the verdict that nothing observed.
    pub skipped: Option<&'static str>,
}

impl ChannelProbe {
    /// A placeholder for a variant that has not run yet.
    #[must_use]
    pub fn unrun(channels: SessionChannels) -> Self {
        Self {
            channels,
            outcome: RequestOutcome::Transport,
            dsm_code: None,
            http_status: None,
            elapsed_ms: 0,
            skipped: None,
        }
    }

    /// A variant that was deliberately not attempted, and the reason.
    #[must_use]
    pub fn skipped(channels: SessionChannels, reason: &'static str) -> Self {
        Self {
            skipped: Some(reason),
            ..Self::unrun(channels)
        }
    }

    /// Whether this variant produced an observation at all.
    #[must_use]
    pub fn ran(self) -> bool {
        self.skipped.is_none()
    }

    #[must_use]
    pub fn accepted(self) -> bool {
        self.outcome == RequestOutcome::Ok
    }

    /// Whether DSM rejected the session itself, rather than refusing the operation.
    #[must_use]
    pub fn session_rejected(self) -> bool {
        matches!(self.dsm_code, Some(106 | 107 | 119))
    }
}

/// One safe, read-only capability probe to make.
///
/// `api`, `method`, and `cgi_path` are supplied by the caller. The first two are compile-time
/// literals; the path is routed through [`endpoint_url`], which confines it to the configured
/// origin and `webapi/` prefix, so a server-supplied path cannot redirect a probe elsewhere.
#[derive(Clone, Debug)]
pub struct CapabilityProbeSpec {
    pub api: &'static str,
    pub method: &'static str,
    pub version: u32,
    pub cgi_path: &'static str,
    pub parameters: Vec<(String, String)>,
}

/// What one capability probe learned. `Copy`, and free of server-supplied text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityProbe {
    pub api: &'static str,
    pub method: &'static str,
    pub version: u32,
    pub outcome: RequestOutcome,
    pub dsm_code: Option<i64>,
    pub http_status: Option<u16>,
    pub elapsed_ms: u64,
}

/// File Station's own per-account capability report.
///
/// The two text fields are sanitized and bounded on the way in: `hostname` is DSM's own name for
/// the host that served the request, and `support_virtual_protocol` is a comma-separated list of
/// VFS types. Neither is a secret, and neither is trusted as free-form terminal output.
/// Each flag is an `Option` because "DSM did not say" and "DSM said no" are different answers,
/// and a capability report that renders the first as the second is worse than one that admits it
/// does not know.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileStationInfo {
    pub hostname: Option<BoundedText>,
    pub is_manager: Option<bool>,
    pub support_sharing: Option<bool>,
    pub support_virtual_protocol: Option<BoundedText>,
}

/// One component of a destination path, and what DSM said about it.
///
/// The path is built from the destination this run was given, never from a server response, so
/// nothing here can carry text DSM chose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSegmentProbe {
    pub path: String,
    pub depth: u8,
    pub exists: bool,
    pub is_directory: bool,
    pub mount_boundary: bool,
    pub dsm_code: Option<i64>,
}

/// The component-by-component walk of a destination path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DestinationPathResolution {
    pub segments: Vec<PathSegmentProbe>,
    /// How many components the destination has, including the share root.
    pub total_components: usize,
    /// The first component that does not exist, as a 1-based position.
    pub first_missing: Option<usize>,
}

impl DestinationPathResolution {
    /// Whether every component of the destination was walked and every one of them exists.
    ///
    /// The per-segment check is not redundant with `first_missing`: a walk stopped by something
    /// other than a 408 -- a rejected session, a refused permission -- records that component as
    /// not existing without setting `first_missing`, and must not be reported as fully resolved.
    #[must_use]
    pub fn fully_resolved(&self) -> bool {
        self.first_missing.is_none()
            && self.segments.len() == self.total_components
            && self.segments.iter().all(|segment| segment.exists)
    }

    /// Whether the share root itself -- the first component -- is absent or invisible.
    ///
    /// A different fault from a missing interior component, and it needs a different answer: the
    /// account cannot see the shared folder at all, so no amount of creating directories helps.
    #[must_use]
    pub fn share_root_missing(&self) -> bool {
        self.first_missing == Some(1)
    }

    /// The deepest component that does exist, when any does.
    #[must_use]
    pub fn nearest_existing(&self) -> Option<&PathSegmentProbe> {
        self.segments.iter().rev().find(|segment| segment.exists)
    }
}

/// Structured evidence from an explicitly requested, disposable remote write probe.
///
/// The probe is never run implicitly. A caller must invoke [`ApiClient::run_write_probe`] and
/// should make that mutation visible to the user before doing so.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteProbeReport {
    pub target_path: String,
    pub probe_path: String,
    pub target_verified: bool,
    pub directory_created: bool,
    pub upload_attempted: bool,
    pub upload_verified: bool,
    pub uploaded_size: u64,
    pub uploaded_md5: ContentMd5,
    pub uploaded_mtime_seconds: i64,
    pub server_copy_supported: bool,
    pub server_copy_attempted: bool,
    pub server_copy_verified: bool,
    pub cleanup_completed: bool,
    /// Conservatively set when cleanup could not prove that the probe directory is absent.
    pub leftover_remote_probe_path: Option<String>,
}

/// A probe failure preserves both its operational cause and any independent cleanup failure.
#[derive(Debug)]
pub struct WriteProbeFailure {
    pub cause: Error,
    pub cleanup_error: Option<Error>,
    pub report: WriteProbeReport,
}

pub type WriteProbeResult = std::result::Result<WriteProbeReport, Box<WriteProbeFailure>>;

impl fmt::Display for WriteProbeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "disposable write probe failed: {}", self.cause)?;
        if let Some(cleanup_error) = &self.cleanup_error {
            write!(formatter, "; cleanup also failed: {cleanup_error}")?;
        }
        if let Some(path) = &self.report.leftover_remote_probe_path {
            write!(
                formatter,
                "; inspect and remove leftover probe path {path:?}"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for WriteProbeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadTransferEvent {
    AttemptStarted { attempt: u32 },
    Advanced { bytes: u64 },
    Completed,
    Failed,
}

/// Return `false` from an observer to cancel the transfer at the next safe read boundary.
pub type UploadObserver = Arc<dyn Fn(UploadTransferEvent) -> bool + Send + Sync>;

/// One instrumentation record from the transport layer.
///
/// Every variant is `Copy` and closed: an enum, an integer, a boolean, a compile-time
/// `&'static str`, or a sanitized bounded token. A response body, header value, form-field value,
/// credential, or session identifier is not representable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiObservation {
    /// The transport identity of a freshly connected client, replayed when an observer is
    /// installed so the endpoint is named once per run rather than once per request.
    Connected(ConnectionDetail),
    /// A login response was accepted and produced a usable session.
    SessionEstablished(SessionShape),
    /// A request is about to go on the wire.
    CallStarted(ApiCallDetail),
    /// A request finished, successfully or not.
    CallCompleted(ApiCallDetail),
}

/// Receives one record per HTTP round trip this client makes.
///
/// The observer runs on the calling thread inside the request path and must not block. It is
/// shared by every clone of the client it was installed on.
pub type RequestObserver = Arc<dyn Fn(ApiObservation) + Send + Sync>;

/// Everything one request attempt needs to describe itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AttemptContext {
    api: &'static str,
    version: u32,
    method: &'static str,
    timeout: Duration,
    attempt: u32,
    max_attempts: u32,
}

impl AttemptContext {
    /// One unretried attempt, which is what most control requests make.
    fn single(api: &'static str, version: u32, method: &'static str, timeout: Duration) -> Self {
        Self {
            api,
            version,
            method,
            timeout,
            attempt: 1,
            max_attempts: 1,
        }
    }
}

/// Response facts that are only observable before the body is consumed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ResponseFacts {
    http_status: Option<u16>,
    /// How many cookies the response set, and under which names. Never their values.
    set_cookie_count: u16,
    set_cookie_names: BoundedText,
    /// The same cookies with their attributes and a salted digest of each value.
    cookies: CookieFacts,
    /// What the response headers revealed about proxies and relays on the path.
    intermediary: IntermediaryFacts,
    /// The host of a `Location` target, when the response redirected. Host only.
    redirect_host: Option<BoundedText>,
    response_bytes: u64,
    /// Why the body would not deserialize, in schema terms. Set only on a decode failure.
    decode: Option<DecodeFault>,
}

impl ApiClient {
    pub fn connect(options: &ClientOptions) -> Result<Self> {
        Self::connect_observed(options, None)
    }

    /// Connect while reporting every HTTP round trip, including API discovery.
    ///
    /// The observer is installed before discovery runs rather than afterwards, because discovery
    /// is itself two requests and its `entry.cgi` to `query.cgi` fallback is exactly the sort of
    /// reverse-proxy misrouting an operator needs to see.
    pub fn connect_observed(
        options: &ClientOptions,
        observer: Option<RequestObserver>,
    ) -> Result<Self> {
        Self::connect_with_requirements(
            options,
            &[
                ("SYNO.FileStation.List", 2),
                ("SYNO.FileStation.CreateFolder", 2),
                ("SYNO.FileStation.Upload", 2),
                ("SYNO.FileStation.CheckPermission", 3),
            ],
            None,
            observer,
        )
    }

    /// Connect a least-privilege client for authentication tests and the
    /// interactive directory chooser. Only Auth and List are required; sync
    /// mutation APIs are neither assumed nor invoked by this client.
    pub fn connect_for_browsing(options: &ClientOptions) -> Result<Self> {
        Self::connect_with_requirements(options, &[("SYNO.FileStation.List", 2)], None, None)
    }

    /// Connect a least-privilege browsing client whose discovery and all
    /// subsequent control requests share one absolute deadline.
    ///
    /// This is intended for bounded interactive probes. A discovery fallback,
    /// a second login carrying an OTP, and directory listing all consume the
    /// same budget; none of them can silently restart it. Callers that need a
    /// separately reserved cleanup slice must opt into [`Self::logout_bounded`].
    pub fn connect_for_browsing_bounded(
        options: &ClientOptions,
        total_timeout: Duration,
    ) -> Result<Self> {
        if total_timeout.is_zero() {
            return Err(Error::Message(
                "File Station control deadline must be greater than zero".to_owned(),
            ));
        }
        let deadline = Instant::now().checked_add(total_timeout).ok_or_else(|| {
            Error::Message("File Station control deadline is out of range".to_owned())
        })?;
        Self::connect_with_requirements(
            options,
            &[("SYNO.FileStation.List", 2)],
            Some(deadline),
            None,
        )
    }

    fn connect_with_requirements(
        options: &ClientOptions,
        required_apis: &[(&str, u32)],
        control_deadline: Option<Instant>,
        observer: Option<RequestObserver>,
    ) -> Result<Self> {
        let base = normalize_base_url(&options.base_url, options.allow_http)?;
        let mut builder = crate::blocking_client_builder()?
            .connect_timeout(options.connect_timeout)
            .timeout(control_request_timeout(options.request_timeout))
            .redirect(Policy::none())
            .user_agent(concat!("synology-drive-sync/", env!("SDSYNC_VERSION")));
        let mut download_builder = crate::async_client_builder()?
            .connect_timeout(options.connect_timeout)
            .read_timeout(control_request_timeout(options.request_timeout))
            .redirect(Policy::none())
            .user_agent(concat!("synology-drive-sync/", env!("SDSYNC_VERSION")));

        if options.accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
            download_builder = download_builder.danger_accept_invalid_certs(true);
        }
        if let Some(path) = &options.ca_certificate {
            let certificate = load_ca_certificate(path)?;
            builder = builder.add_root_certificate(certificate.clone());
            download_builder = download_builder.add_root_certificate(certificate);
        }
        let http = builder.build().map_err(|source| Error::Http {
            operation: "building HTTP client".to_owned(),
            source,
        })?;
        let download_http = download_builder.build().map_err(|source| Error::Http {
            operation: "building streaming HTTP client".to_owned(),
            source,
        })?;

        // Settled before `base` is moved into the client. The endpoint is named once per run
        // rather than once per request.
        let connection = connection_detail(&base, options);
        let mut client = Self {
            http,
            download_http,
            base,
            apis: HashMap::new(),
            session: None,
            retries: options.retries,
            control_timeout: control_request_timeout(options.request_timeout),
            control_deadline,
            upload_timeout: options.request_timeout,
            operation_timeout: options.request_timeout,
            upload_rate_limit: None,
            cancellation: CancellationToken::default(),
            observer,
        };
        // Announced before discovery so the endpoint is named ahead of the first request it
        // explains, which is the order a reader needs.
        client.observe(ApiObservation::Connected(connection));
        client.apis = client.discover()?;
        client.validate_api("SYNO.API.Auth", 3)?;
        for &(api, version) in required_apis {
            client.validate_api(api, version)?;
        }
        Ok(client)
    }

    /// Cap upload throughput at `bytes_per_second`, shared by this client and every clone of it
    /// so concurrent jobs divide one budget instead of each receiving the full rate.
    ///
    /// `None` (and a zero rate, which the configuration layer already rejects) leaves uploads
    /// unlimited, which is the default and the only behaviour that existed before.
    #[must_use]
    pub fn with_max_upload_rate(mut self, bytes_per_second: Option<u64>) -> Self {
        self.upload_rate_limit = upload_rate_bucket(bytes_per_second);
        self
    }

    /// Share `cancellation` with this client so its internal retry backoff wakes on Ctrl-C.
    ///
    /// Cancellation stays cooperative: this only shortens waits the client would otherwise sleep
    /// through, and a request already in flight is still bounded by its own timeout. Every clone
    /// of the returned client observes the same token.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: &CancellationToken) -> Self {
        self.cancellation = cancellation.clone();
        self
    }

    fn observe(&self, observation: ApiObservation) {
        if let Some(observer) = &self.observer {
            observer(observation);
        }
    }

    /// Report which session channels a request carries.
    ///
    /// The header half mirrors what `with_blocking_session_headers` and
    /// `with_async_session_headers` attach, derived from session state rather than by changing
    /// their signatures. The field half reads form-field **names** only; no value is inspected.
    fn session_transport(&self, fields: &[(String, String)]) -> SessionTransport {
        let session = self.session.as_ref();
        SessionTransport {
            cookie_header: session.is_some(),
            syno_token_header: session.is_some_and(|session| session.syno_token.is_some()),
            sid_field: fields.iter().any(|(name, _)| name == "_sid"),
            syno_token_field: fields.iter().any(|(name, _)| name == "SynoToken"),
        }
    }

    /// The limit this client is pacing uploads against, or `None` when unlimited. This exists so
    /// callers can prove the configured rate actually reached the client instead of being
    /// dropped on the way.
    #[must_use]
    pub fn max_upload_rate(&self) -> Option<u64> {
        self.upload_rate_limit.as_ref().map(|bucket| {
            bucket
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .bytes_per_second
        })
    }

    pub fn require_delete_api(&self) -> Result<()> {
        self.validate_api("SYNO.FileStation.Delete", 2)
    }

    pub fn require_content_api(&self) -> Result<()> {
        self.validate_api("SYNO.FileStation.MD5", 2)
    }

    /// Require the File Station Download API used for complete MD5/CRC32/SHA-256 fingerprints.
    ///
    /// [`Self::require_content_api`] retains its original MD5 capability contract for existing
    /// library callers; new content-mode execution must use this stronger gate.
    pub fn require_content_fingerprint_api(&self) -> Result<()> {
        self.validate_api("SYNO.FileStation.Download", 2)
    }

    pub fn supports_server_copy(&self) -> bool {
        self.validate_api("SYNO.FileStation.CopyMove", 3).is_ok()
    }

    pub fn populate_remote_content_md5(
        &self,
        inventory: &mut RemoteInventory,
        selected_relative_paths: &BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.require_content_api()?;
        for relative in selected_relative_paths {
            cancellation.check()?;
            let entry = inventory.entries.get_mut(relative).ok_or_else(|| {
                Error::Message(format!(
                    "remote content selection referenced missing inventory path {relative:?}"
                ))
            })?;
            if entry.kind != EntryKind::File {
                return Err(Error::Message(format!(
                    "remote content selection referenced non-file path {relative:?}"
                )));
            }
            entry.content_md5 = Some(self.remote_content_md5(&entry.remote_path, cancellation)?);
        }
        Ok(())
    }

    pub fn remote_content_md5(
        &self,
        remote_path: &str,
        cancellation: &CancellationToken,
    ) -> Result<ContentMd5> {
        self.require_content_api()?;
        cancellation.check()?;
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or_else(|| Error::Message("operation timeout is too large".to_owned()))?;
        let task: TaskStartData = self
            .call_bounded(
                "SYNO.FileStation.MD5",
                2,
                "start",
                vec![pair("file_path", json_string(remote_path)?)],
                self.control_timeout,
            )?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.FileStation.MD5.start".to_owned(),
                message: "successful response contained no task ID".to_owned(),
            })?;
        validate_task_id(&task.taskid, "SYNO.FileStation.MD5.start")?;

        loop {
            if cancellation.is_cancelled() {
                let _ = self.stop_task("SYNO.FileStation.MD5", 2, &task.taskid);
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                let _ = self.stop_task("SYNO.FileStation.MD5", 2, &task.taskid);
                return Err(Error::OperationTimedOut {
                    operation: "remote MD5 calculation",
                });
            }
            let request_timeout = self
                .control_timeout
                .min(deadline.saturating_duration_since(Instant::now()));
            let status: Md5StatusData = match self.call_bounded(
                "SYNO.FileStation.MD5",
                2,
                "status",
                vec![pair("taskid", json_string(&task.taskid)?)],
                request_timeout,
            ) {
                Ok(Some(status)) => status,
                Ok(None) => {
                    let _ = self.stop_task("SYNO.FileStation.MD5", 2, &task.taskid);
                    return Err(Error::InvalidResponse {
                        operation: "SYNO.FileStation.MD5.status".to_owned(),
                        message: "successful response contained no task status".to_owned(),
                    });
                }
                Err(error) => {
                    let _ = self.stop_task("SYNO.FileStation.MD5", 2, &task.taskid);
                    return Err(error);
                }
            };
            if status.finished {
                let digest = status.md5.ok_or_else(|| Error::InvalidResponse {
                    operation: "SYNO.FileStation.MD5.status".to_owned(),
                    message: "finished task contained no MD5 digest".to_owned(),
                })?;
                return ContentMd5::parse_hex(&digest);
            }
            if let Err(error) = sleep_cancellable(Duration::from_millis(100), cancellation) {
                let _ = self.stop_task("SYNO.FileStation.MD5", 2, &task.taskid);
                return Err(error);
            }
        }
    }

    /// Populate collision-resistant content fingerprints for the selected files.
    ///
    /// File Station exposes only MD5 as a server-side calculation. Content mode therefore
    /// streams each selected remote file through the documented Download API and computes MD5,
    /// IEEE CRC32, and SHA-256 together without retaining the downloaded payload.
    pub fn populate_remote_content_fingerprints(
        &self,
        inventory: &mut RemoteInventory,
        selected_relative_paths: &BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.require_content_fingerprint_api()?;
        for relative in selected_relative_paths {
            cancellation.check()?;
            let entry = inventory.entries.get_mut(relative).ok_or_else(|| {
                Error::Message(format!(
                    "remote content selection referenced missing inventory path {relative:?}"
                ))
            })?;
            if entry.kind != EntryKind::File {
                return Err(Error::Message(format!(
                    "remote content selection referenced non-file path {relative:?}"
                )));
            }
            entry.content_md5 = Some(self.remote_content_fingerprint(
                &entry.remote_path,
                entry.size,
                cancellation,
            )?);
        }
        Ok(())
    }

    pub fn remote_content_fingerprint(
        &self,
        remote_path: &str,
        expected_size: u64,
        cancellation: &CancellationToken,
    ) -> Result<ContentMd5> {
        self.require_content_fingerprint_api()?;
        for attempt in 0..=self.retries {
            cancellation.check()?;
            match self.remote_content_fingerprint_once(remote_path, expected_size, cancellation) {
                Err(error) if attempt < self.retries && retryable(&error) => {
                    retry_pause_cancellable(attempt, cancellation)?;
                }
                result => return result,
            }
        }
        unreachable!("retry loop always returns")
    }

    fn remote_content_fingerprint_once(
        &self,
        remote_path: &str,
        expected_size: u64,
        cancellation: &CancellationToken,
    ) -> Result<ContentMd5> {
        let fields = self.download_form_fields(remote_path)?;

        let url = self.api_url("SYNO.FileStation.Download")?;
        let request = self
            .with_async_session_headers(self.download_http.post(url))?
            .form(&*fields);
        // `form` has serialized its own request-body copy. Erase the caller-owned
        // SID/SynoToken strings before the potentially long streaming download.
        drop(fields);
        run_download_request(
            request,
            remote_path.to_owned(),
            expected_size,
            self.operation_timeout,
            cancellation.clone(),
        )
    }

    fn download_form_fields(&self, remote_path: &str) -> Result<Zeroizing<Vec<(String, String)>>> {
        let session = self.required_session()?;
        let mut fields = Zeroizing::new(vec![
            pair("api", "SYNO.FileStation.Download"),
            pair("version", "2"),
            pair("method", "download"),
            pair("path", json_array([remote_path])?),
            pair("mode", json_string("download")?),
            pair("_sid", session.sid.to_string()),
        ]);
        if let Some(token) = &session.syno_token {
            fields.push(pair("SynoToken", token.to_string()));
        }
        Ok(fields)
    }

    pub fn copy_file_verified(
        &self,
        root: &RemoteRoot,
        source_path: &str,
        destination_path: &str,
        expected_size: u64,
        expected: ContentMd5,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.validate_api("SYNO.FileStation.CopyMove", 3)
            .map_err(copy_start_error)?;
        validate_mutation_target(root, source_path)?;
        validate_mutation_target(root, destination_path)?;
        let (_, source_name) = parent_and_name(source_path)?;
        let (destination_parent, destination_name) = parent_and_name(destination_path)?;
        if source_name != destination_name || source_path == destination_path {
            return Err(Error::Message(
                "safe server-side copy requires different parents and an unchanged basename"
                    .to_owned(),
            ));
        }
        cancellation.check()?;
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or_else(|| Error::Message("operation timeout is too large".to_owned()))?;

        let task_result: Result<Option<TaskStartData>> = self.call_bounded(
            "SYNO.FileStation.CopyMove",
            3,
            "start",
            vec![
                pair("path", json_array([source_path])?),
                pair("dest_folder_path", json_string(destination_parent)?),
                pair("remove_src", "false"),
                pair("accurate_progress", "false"),
            ],
            self.control_timeout,
        );
        let task: TaskStartData =
            task_result
                .map_err(copy_start_error)?
                .ok_or_else(|| Error::InvalidResponse {
                    operation: "SYNO.FileStation.CopyMove.start".to_owned(),
                    message: "successful response contained no task ID".to_owned(),
                })?;
        validate_task_id(&task.taskid, "SYNO.FileStation.CopyMove.start")?;

        loop {
            if cancellation.is_cancelled() {
                let _ = self.stop_task("SYNO.FileStation.CopyMove", 3, &task.taskid);
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                let _ = self.stop_task("SYNO.FileStation.CopyMove", 3, &task.taskid);
                return Err(Error::OperationTimedOut {
                    operation: "server-side file copy",
                });
            }
            let request_timeout = self
                .control_timeout
                .min(deadline.saturating_duration_since(Instant::now()));
            let status: TaskStatusData = match self.call_bounded(
                "SYNO.FileStation.CopyMove",
                3,
                "status",
                vec![pair("taskid", json_string(&task.taskid)?)],
                request_timeout,
            ) {
                Ok(Some(status)) => status,
                Ok(None) => {
                    let _ = self.stop_task("SYNO.FileStation.CopyMove", 3, &task.taskid);
                    return Err(Error::InvalidResponse {
                        operation: "SYNO.FileStation.CopyMove.status".to_owned(),
                        message: "successful response contained no task status".to_owned(),
                    });
                }
                Err(error) => {
                    let _ = self.stop_task("SYNO.FileStation.CopyMove", 3, &task.taskid);
                    return Err(error);
                }
            };
            if status.finished {
                break;
            }
            if let Err(error) = sleep_cancellable(Duration::from_millis(100), cancellation) {
                let _ = self.stop_task("SYNO.FileStation.CopyMove", 3, &task.taskid);
                return Err(error);
            }
        }

        self.verify_remote_content(destination_path, expected_size, expected, cancellation)
    }

    fn stop_task(&self, api: &'static str, version: u32, taskid: &str) -> Result<()> {
        self.call_bounded::<Value>(
            api,
            version,
            "stop",
            vec![pair("taskid", json_string(taskid)?)],
            STOP_REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    pub fn verify_remote_content(
        &self,
        remote_path: &str,
        expected_size: u64,
        expected_md5: ContentMd5,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.remote_content_matches(remote_path, expected_size, expected_md5, cancellation)? {
            Ok(())
        } else {
            Err(Error::ContentVerificationFailed(remote_path.to_owned()))
        }
    }

    /// Re-read one path without retry immediately before a mutation and require its metadata to
    /// match the inventory snapshot. File content, when required, is checked separately through
    /// the complete streamed fingerprint (or the explicit legacy MD5 helper) so a delayed
    /// successful response cannot hide a replacement.
    pub fn verify_remote_metadata_snapshot(
        &self,
        remote_path: &str,
        expected_kind: EntryKind,
        expected_size: u64,
        expected_mtime_seconds: i64,
        require_mtime: bool,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        cancellation.check()?;
        let item = match self.get_info_with_retry(remote_path, false) {
            Ok(item) => item,
            Err(error) if error.api_code() == Some(408) => {
                return Err(Error::RemoteSnapshotChanged(remote_path.to_owned()));
            }
            Err(error) => return Err(error),
        };
        let actual_kind = if item.isdir {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        if actual_kind != expected_kind {
            return Err(Error::RemoteSnapshotChanged(remote_path.to_owned()));
        }
        let additional = item.additional.unwrap_or_default();
        let (actual_size, actual_mtime_seconds) =
            file_metadata("SYNO.FileStation.List.getinfo", actual_kind, &additional)?;
        if actual_size != expected_size
            || (require_mtime && actual_mtime_seconds != expected_mtime_seconds)
        {
            return Err(Error::RemoteSnapshotChanged(remote_path.to_owned()));
        }
        cancellation.check()?;
        Ok(())
    }

    fn remote_content_matches(
        &self,
        remote_path: &str,
        expected_size: u64,
        expected_md5: ContentMd5,
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        cancellation.check()?;
        if !expected_md5.has_full_proof() {
            return Ok(false);
        }
        let Some(actual_size) = self.remote_file_size(remote_path)? else {
            return Ok(false);
        };
        if actual_size != expected_size {
            return Ok(false);
        }
        let actual = self.remote_content_fingerprint(remote_path, expected_size, cancellation)?;
        Ok(content_evidence_matches(expected_md5, actual))
    }

    fn remote_file_size(&self, remote_path: &str) -> Result<Option<u64>> {
        let item = match self.get_info_with_retry(remote_path, false) {
            Ok(item) => item,
            Err(error) if error.api_code() == Some(408) => return Ok(None),
            Err(error) => return Err(error),
        };
        if item.isdir {
            return Ok(None);
        }
        item.additional
            .and_then(|additional| additional.size)
            .map(Some)
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.FileStation.List.getinfo".to_owned(),
                message: "file information contained no byte size".to_owned(),
            })
    }

    pub fn login(&mut self, username: &str, password: &str, otp: Option<&str>) -> Result<()> {
        self.login_negotiating_syno_token(username, password, otp, true)
    }

    /// Log in without asking DSM for a `SynoToken`.
    ///
    /// `enable_syno_token=yes` is what turns a login into the browser-style, cookie-and-header
    /// session DSM's own web UI uses, and a session created that way may be one DSM will not
    /// resolve from the `_sid` request parameter its guide documents. Establishing a second
    /// session without the flag is the only way to tell "this DSM rejects the parameter path"
    /// from "this DSM rejects the parameter path *for token-bound sessions*".
    ///
    /// Diagnostic use only. The session this produces has no `SynoToken`, so it cannot carry the
    /// header channel, and the caller is responsible for logging it out.
    pub fn login_without_syno_token(
        &mut self,
        username: &str,
        password: &str,
        otp: Option<&str>,
    ) -> Result<()> {
        self.login_negotiating_syno_token(username, password, otp, false)
    }

    fn login_negotiating_syno_token(
        &mut self,
        username: &str,
        password: &str,
        otp: Option<&str>,
        enable_syno_token: bool,
    ) -> Result<()> {
        // A failed re-login must never leave an older session usable.
        self.session = None;
        let spec = self.required_spec("SYNO.API.Auth")?;
        let auth_version = 6_u32.min(spec.max_version);
        if auth_version < 3 || auth_version < spec.min_version {
            return Err(Error::UnsupportedApiVersion {
                api: "SYNO.API.Auth".to_owned(),
                version: 6,
                min: spec.min_version,
                max: spec.max_version,
            });
        }

        let mut fields = vec![
            pair("api", "SYNO.API.Auth"),
            pair("version", auth_version.to_string()),
            pair("method", "login"),
            pair("account", username),
            pair("passwd", password),
            pair("session", "FileStation"),
            pair("format", "sid"),
        ];
        if auth_version >= 6 && enable_syno_token {
            fields.push(pair("enable_syno_token", "yes"));
        }
        if let Some(code) = otp {
            fields.push(pair("otp_code", code));
        }

        let url = self.api_url("SYNO.API.Auth")?;
        // The login response is the only place `Set-Cookie` can be observed, and whether the
        // server set one is exactly what distinguishes a session DSM expects us to carry in a
        // cookie from one it expects only in the `_sid` field.
        let mut facts = ResponseFacts::default();
        let data: LoginData = self
            .send_attempt(
                url,
                fields,
                AttemptContext::single(
                    "SYNO.API.Auth",
                    auth_version,
                    "login",
                    self.control_timeout,
                ),
                &mut facts,
            )?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.API.Auth.login".to_owned(),
                message: "successful response contained no session data".to_owned(),
            })?;
        if data.sid.is_empty() {
            return Err(Error::InvalidResponse {
                operation: "SYNO.API.Auth.login".to_owned(),
                message: "successful response contained an empty SID".to_owned(),
            });
        }
        let session = Session {
            sid: Zeroizing::new(data.sid),
            syno_token: data
                .synotoken
                .filter(|token| !token.is_empty())
                .map(Zeroizing::new),
        };
        // DSM's documented entry.cgi transport uses the returned SID and optional
        // SynoToken as sensitive headers. Validate that representation before a
        // caller can mistake a syntactically successful login for a usable session.
        session.request_headers()?;
        self.observe(ApiObservation::SessionEstablished(SessionShape {
            sid_length: u16::try_from(session.sid.len()).unwrap_or(u16::MAX),
            token_present: session.syno_token.is_some(),
            login_format: LoginFormat::Sid,
            server_set_cookie: facts.set_cookie_count > 0,
        }));
        self.session = Some(session);
        Ok(())
    }

    pub fn logout(&mut self) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        let version = 6_u32.min(self.required_spec("SYNO.API.Auth")?.max_version);
        let fields = self.authenticated_fields(
            "SYNO.API.Auth",
            version,
            "logout",
            vec![pair("session", "FileStation")],
        )?;
        let url = self.api_url("SYNO.API.Auth")?;
        let result = self.send_form_once::<Value>(url, fields, "SYNO.API.Auth", version, "logout");
        self.session = None;
        result.map(|_| ())
    }

    /// End the current session with a fresh, cleanup-only deadline.
    ///
    /// A bounded interactive caller can reserve this slice separately from its
    /// discovery/login budget. Replacing an exhausted probe deadline here is
    /// deliberate: once a login response has supplied a SID, attempting logout
    /// is more important than preserving unused probe time.
    pub fn logout_bounded(&mut self, timeout: Duration) -> Result<()> {
        if timeout.is_zero() {
            return Err(Error::Message(
                "File Station logout deadline must be greater than zero".to_owned(),
            ));
        }
        self.control_deadline = Some(Instant::now().checked_add(timeout).ok_or_else(|| {
            Error::Message("File Station logout deadline is out of range".to_owned())
        })?);
        self.logout()
    }

    /// Confirm that File Station accepts the session returned by DSM authentication.
    ///
    /// A successful `SYNO.API.Auth.login` response proves the credentials were accepted, but it
    /// does not prove that a reverse proxy will carry the returned session into File Station. This
    /// bounded, non-mutating request asks for at most one visible shared-folder root and discards
    /// it. Interactive authentication tests use it before reporting a usable target session.
    pub fn confirm_file_station_session(&self) -> Result<()> {
        let data: ListShareData = self
            .call_bounded(
                "SYNO.FileStation.List",
                2,
                "list_share",
                vec![pair("offset", "0"), pair("limit", "1")],
                SESSION_CONFIRMATION_TIMEOUT,
            )?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.FileStation.List.list_share".to_owned(),
                message: "session confirmation returned no shared-folder data".to_owned(),
            })?;
        if data.shares.len() > 1 || data.total.is_some_and(|total| total < data.shares.len()) {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.List.list_share".to_owned(),
                message: "session confirmation exceeded its one-item response bound".to_owned(),
            });
        }
        Ok(())
    }

    pub fn verify_share_writable(&self, root: &RemoteRoot) -> Result<()> {
        let parameters = vec![
            pair("offset", "0"),
            pair("limit", "0"),
            pair("onlywritable", "true"),
        ];
        let data: ListShareData = self
            .call("SYNO.FileStation.List", 2, "list_share", parameters, true)?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.FileStation.List.list_share".to_owned(),
                message: "successful response contained no share list".to_owned(),
            })?;
        let expected = root.share_path();
        if data.shares.iter().any(|share| share.path == expected) {
            Ok(())
        } else {
            Err(Error::ShareNotWritable(root.share_name().to_owned()))
        }
    }

    /// List directories visible to the authenticated File Station account.
    ///
    /// `/` maps to File Station's real shared-folder listing. Every other
    /// parent maps to `SYNO.FileStation.List.list`. The discovered API path and
    /// supported version are used by the internal API dispatcher; neither is hard-coded.
    /// Results are deliberately bounded for an interactive chooser and only
    /// direct, normalized child directories are returned.
    pub fn browse_directories(&self, parent: &str, maximum: usize) -> Result<RemoteDirectoryPage> {
        const MAXIMUM_CHOOSER_DIRECTORIES: usize = 500;
        if maximum == 0 || maximum > MAXIMUM_CHOOSER_DIRECTORIES {
            return Err(Error::Message(format!(
                "File Station directory browser limit must be between 1 and {MAXIMUM_CHOOSER_DIRECTORIES}"
            )));
        }
        self.required_session()?;
        let request_limit = maximum + 1;

        let (mut directories, total) = if parent == "/" {
            let parameters = vec![
                pair("offset", "0"),
                pair("limit", request_limit.to_string()),
                pair("sort_by", json_string("name")?),
                pair("sort_direction", json_string("asc")?),
                pair("additional", json_array(["perm"])?),
            ];
            let data: ListShareData = self
                .call("SYNO.FileStation.List", 2, "list_share", parameters, true)?
                .ok_or_else(|| Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list_share".to_owned(),
                    message: "successful response contained no share list".to_owned(),
                })?;
            let total = data.total.unwrap_or(data.shares.len());
            let mut output = Vec::new();
            for share in data.shares {
                if share.disable_list || permission_disables_listing(share.additional.as_ref()) {
                    continue;
                }
                let root = RemoteRoot::parse(&share.path)?;
                if root.as_str() != root.share_path() {
                    return Err(Error::InvalidResponse {
                        operation: "SYNO.FileStation.List.list_share".to_owned(),
                        message: "shared-folder result was not a normalized root path".to_owned(),
                    });
                }
                let name = share
                    .name
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| root.share_name().to_owned());
                if name != root.share_name() {
                    return Err(Error::InvalidResponse {
                        operation: "SYNO.FileStation.List.list_share".to_owned(),
                        message: "shared-folder name did not match its path".to_owned(),
                    });
                }
                output.push(RemoteDirectory {
                    name,
                    path: root.as_str().to_owned(),
                });
            }
            (output, total)
        } else {
            let normalized_parent = RemoteRoot::parse(parent)?;
            if normalized_parent.as_str() != parent {
                return Err(Error::Message(
                    "File Station directory browser parent was not normalized".to_owned(),
                ));
            }
            let parameters = vec![
                pair("folder_path", json_string(parent)?),
                pair("offset", "0"),
                pair("limit", request_limit.to_string()),
                pair("sort_by", json_string("name")?),
                pair("sort_direction", json_string("asc")?),
                pair("filetype", json_string("dir")?),
                pair("additional", json_array(["perm", "mount_point_type"])?),
            ];
            let data: ListData = self
                .call("SYNO.FileStation.List", 2, "list", parameters, true)?
                .ok_or_else(|| Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list".to_owned(),
                    message: "successful response contained no directory data".to_owned(),
                })?;
            let total = data.total;
            let prefix = format!("{parent}/");
            let mut output = Vec::new();
            for item in data.files {
                let item = item.into_item("SYNO.FileStation.List", "list")?;
                if !item.isdir
                    || item.disable_list
                    || permission_disables_listing(item.additional.as_ref())
                    || item
                        .additional
                        .as_ref()
                        .and_then(|additional| additional.mount_point_type.as_deref())
                        .is_some_and(|value| !value.is_empty())
                {
                    continue;
                }
                let expected_path = format!("{prefix}{}", item.name);
                if item.name.is_empty()
                    || item.name.contains('/')
                    || item.name.contains('\\')
                    || item.name.chars().any(char::is_control)
                    || item.path != expected_path
                {
                    return Err(Error::InvalidResponse {
                        operation: "SYNO.FileStation.List.list".to_owned(),
                        message: "directory result escaped its requested parent".to_owned(),
                    });
                }
                let normalized = RemoteRoot::parse(&item.path)?;
                output.push(RemoteDirectory {
                    name: item.name,
                    path: normalized.as_str().to_owned(),
                });
            }
            (output, total)
        };

        directories.sort_by(|left, right| left.name.cmp(&right.name));
        directories.dedup_by(|left, right| left.path == right.path);
        let truncated = total > maximum || directories.len() > maximum;
        directories.truncate(maximum);
        Ok(RemoteDirectoryPage {
            parent: parent.to_owned(),
            directories,
            truncated,
        })
    }

    /// Verify write permission at the configured destination without changing remote state.
    ///
    /// File Station's CheckPermission API checks permission to create a named child within an
    /// existing directory. For an existing destination, use a collision-resistant probe name in
    /// that exact directory. If the destination is absent, check creation of the first missing
    /// path component in its nearest existing ancestor; later components do not exist yet and
    /// therefore cannot have independent ACLs to inspect without mutating the NAS.
    pub fn verify_destination_writable(&self, root: &RemoteRoot) -> Result<DestinationWriteCheck> {
        self.verify_destination_writable_with_resolution(root).1
    }

    /// The same check, keeping the component-by-component walk it performs on the way.
    ///
    /// The walk is discarded by [`Self::verify_destination_writable`] because sync only needs the
    /// verdict, but it is the whole answer a diagnostic wants: "neither the destination nor any
    /// ancestor of it exists" and "only the last component is missing" are different problems with
    /// different fixes, and only the walk separates them. Returning both means the resolution
    /// costs no extra request -- these are the same `getinfo` calls, reported instead of dropped.
    pub fn verify_destination_writable_with_resolution(
        &self,
        root: &RemoteRoot,
    ) -> (DestinationPathResolution, Result<DestinationWriteCheck>) {
        let prefixes = absolute_prefixes(root.as_str());
        let mut resolution = DestinationPathResolution {
            segments: Vec::with_capacity(prefixes.len()),
            total_components: prefixes.len(),
            first_missing: None,
        };
        if let Err(error) = self.validate_api("SYNO.FileStation.CheckPermission", 3) {
            return (resolution, Err(error));
        }

        let mut nearest_existing = None;
        for (index, path) in prefixes.into_iter().enumerate() {
            let depth = u8::try_from(index + 1).unwrap_or(u8::MAX);
            match self.get_info(&path) {
                Ok(item) => {
                    let mount_type = item
                        .additional
                        .as_ref()
                        .and_then(|additional| additional.mount_point_type.as_deref())
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_owned);
                    resolution.segments.push(PathSegmentProbe {
                        path: path.clone(),
                        depth,
                        exists: true,
                        is_directory: item.isdir,
                        mount_boundary: mount_type.is_some(),
                        dsm_code: None,
                    });
                    if !item.isdir {
                        return (
                            resolution,
                            Err(Error::Message(format!(
                                "remote destination ancestor {path} exists but is not a directory"
                            ))),
                        );
                    }
                    if let Some(mount_type) = mount_type {
                        return (resolution, Err(Error::RemoteMountRoot { path, mount_type }));
                    }
                    nearest_existing = Some(path);
                }
                Err(error) if error.api_code() == Some(408) => {
                    resolution.segments.push(PathSegmentProbe {
                        path: path.clone(),
                        depth,
                        exists: false,
                        is_directory: false,
                        mount_boundary: false,
                        dsm_code: Some(408),
                    });
                    resolution.first_missing = Some(index + 1);
                    let Some(existing) = nearest_existing else {
                        return (
                            resolution,
                            Err(Error::ShareNotWritable(root.share_name().to_owned())),
                        );
                    };
                    let outcome = parent_and_name(&path).and_then(|(_, missing_name)| {
                        self.check_write_permission(&existing, missing_name)
                    });
                    return (
                        resolution,
                        outcome.map(|()| DestinationWriteCheck {
                            checked_directory: existing,
                            destination_exists: false,
                        }),
                    );
                }
                Err(error) => {
                    resolution.segments.push(PathSegmentProbe {
                        path,
                        depth,
                        exists: false,
                        is_directory: false,
                        mount_boundary: false,
                        dsm_code: error.api_code(),
                    });
                    return (resolution, Err(error));
                }
            }
        }

        let probe_name = permission_probe_name();
        let outcome = self.check_write_permission(root.as_str(), &probe_name);
        (
            resolution,
            outcome.map(|()| DestinationWriteCheck {
                checked_directory: root.as_str().to_owned(),
                destination_exists: true,
            }),
        )
    }

    fn check_write_permission(&self, directory: &str, filename: &str) -> Result<()> {
        let parameters = vec![
            pair("path", json_string(directory)?),
            pair("filename", json_string(filename)?),
            pair("create_only", "true"),
        ];
        self.call::<Value>(
            "SYNO.FileStation.CheckPermission",
            3,
            "write",
            parameters,
            true,
        )?;
        Ok(())
    }

    /// Exercise real File Station write operations inside a unique disposable child directory.
    ///
    /// This is deliberately opt-in and refuses an absent target. It never overwrites a remote
    /// item: both the upload and the optional server-side copy use create-only semantics. Cleanup
    /// ignores the caller's cancellation state, uses bounded control requests, and deletes only
    /// known strict children before deleting the unique probe directory non-recursively.
    pub fn run_write_probe(
        &self,
        root: &RemoteRoot,
        cancellation: &CancellationToken,
    ) -> WriteProbeResult {
        let probe_name = write_probe_name();
        let probe_path = format!("{}/{}", root.as_str(), probe_name);
        let expected_fingerprint = write_probe_fingerprint();
        let local = match ProbeLocalFile::create(expected_fingerprint) {
            Ok(local) => local,
            Err(cause) => {
                let mut report = initial_write_probe_report(
                    root,
                    probe_path,
                    WRITE_PROBE_PAYLOAD.len() as u64,
                    expected_fingerprint,
                    0,
                    self.supports_server_copy(),
                );
                report.cleanup_completed = true;
                return Err(Box::new(WriteProbeFailure {
                    cause,
                    cleanup_error: None,
                    report,
                }));
            }
        };
        self.run_write_probe_with_local(root, &probe_path, &local.entry, cancellation)
    }

    fn run_write_probe_with_local(
        &self,
        root: &RemoteRoot,
        probe_path: &str,
        local: &LocalEntry,
        cancellation: &CancellationToken,
    ) -> WriteProbeResult {
        let expected_md5 = local
            .content_md5
            .expect("write-probe local entry always has an MD5 digest");
        let expected_mtime_seconds = local.mtime_ms.div_euclid(1000);
        let copy_supported = self.supports_server_copy();
        let mut report = initial_write_probe_report(
            root,
            probe_path.to_owned(),
            local.size,
            expected_md5,
            expected_mtime_seconds,
            copy_supported,
        );
        let upload_path = format!("{probe_path}/{WRITE_PROBE_FILE_NAME}");
        let copy_directory = format!("{probe_path}/{WRITE_PROBE_COPY_DIRECTORY}");
        let copy_path = format!("{copy_directory}/{WRITE_PROBE_FILE_NAME}");
        let mut cleanup_required = false;

        let operation: Result<()> = (|| {
            self.required_session()?;
            self.require_delete_api()?;
            self.require_content_fingerprint_api()?;
            cancellation.check()?;
            self.verify_existing_write_probe_target(root)?;
            report.target_verified = true;
            self.require_remote_absent(probe_path)?;

            // Do not clean up a deterministic name collision: that directory is not ours. For a
            // lost/ambiguous response, cleanup is attempted because the create may have landed.
            match self.create_probe_folder(probe_path) {
                Ok(()) => {
                    cleanup_required = true;
                    report.directory_created = true;
                }
                Err(error) if error.api_code() == Some(414) => return Err(error),
                Err(error) => {
                    cleanup_required = true;
                    return Err(error);
                }
            }
            cancellation.check()?;
            self.verify_empty_probe_directory(probe_path, cancellation)?;
            cancellation.check()?;

            report.upload_attempted = true;
            self.upload_non_overwriting(local, &upload_path, cancellation)?;
            self.verify_remote_metadata_snapshot(
                &upload_path,
                EntryKind::File,
                local.size,
                expected_mtime_seconds,
                true,
                cancellation,
            )?;
            report.upload_verified = true;

            if copy_supported {
                cancellation.check()?;
                match self.create_probe_folder(&copy_directory) {
                    Ok(()) => {}
                    Err(error) => return Err(error),
                }
                self.verify_empty_probe_directory(&copy_directory, cancellation)?;
                self.require_remote_absent(&copy_path)?;
                report.server_copy_attempted = true;
                self.copy_file_verified(
                    root,
                    &upload_path,
                    &copy_path,
                    local.size,
                    expected_md5,
                    cancellation,
                )?;
                self.verify_remote_metadata_snapshot(
                    &copy_path,
                    EntryKind::File,
                    local.size,
                    expected_mtime_seconds,
                    true,
                    cancellation,
                )?;
                report.server_copy_verified = true;
            }
            Ok(())
        })();

        let cleanup = if cleanup_required {
            self.cleanup_write_probe(root, probe_path, &upload_path, &copy_directory, &copy_path)
        } else {
            ProbeCleanup::not_needed()
        };
        report.cleanup_completed = cleanup.completed;
        report.leftover_remote_probe_path = cleanup.leftover_remote_probe_path;

        match operation {
            Ok(()) if cleanup.error.is_none() => Ok(report),
            Ok(()) => Err(Box::new(WriteProbeFailure {
                cause: cleanup
                    .error
                    .expect("failed cleanup always retains its error"),
                cleanup_error: None,
                report,
            })),
            Err(cause) => Err(Box::new(WriteProbeFailure {
                cause,
                cleanup_error: cleanup.error,
                report,
            })),
        }
    }

    fn verify_existing_write_probe_target(&self, root: &RemoteRoot) -> Result<()> {
        for path in absolute_prefixes(root.as_str()) {
            let item = match self.get_info_with_retry(&path, false) {
                Ok(item) => item,
                Err(error) if error.api_code() == Some(408) => {
                    return Err(Error::Message(format!(
                        "write-probe target {:?} must already exist",
                        root.as_str()
                    )));
                }
                Err(error) => return Err(error),
            };
            if !item.isdir {
                return Err(Error::Message(format!(
                    "write-probe target ancestor {path:?} is not a directory"
                )));
            }
            if let Some(mount_type) = item
                .additional
                .and_then(|additional| additional.mount_point_type)
                .filter(|value| !value.trim().is_empty())
            {
                return Err(Error::RemoteMountRoot { path, mount_type });
            }
        }
        Ok(())
    }

    fn require_remote_absent(&self, path: &str) -> Result<()> {
        match self.get_info_with_retry(path, false) {
            Err(error) if error.api_code() == Some(408) => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(Error::Message(format!(
                "refusing write probe because unique path {path:?} already exists"
            ))),
        }
    }

    fn create_probe_folder(&self, remote_path: &str) -> Result<()> {
        let (parent, name) = parent_and_name(remote_path)?;
        self.call_bounded::<Value>(
            "SYNO.FileStation.CreateFolder",
            2,
            "create",
            vec![
                pair("folder_path", json_array([parent])?),
                pair("name", json_array([name])?),
                pair("force_parent", "false"),
            ],
            self.control_timeout,
        )?;
        Ok(())
    }

    fn verify_empty_probe_directory(
        &self,
        remote_path: &str,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let item = self.get_info_with_retry(remote_path, false)?;
        if !item.isdir {
            return Err(Error::Message(format!(
                "write-probe path {remote_path:?} was not created as a directory"
            )));
        }
        let children = self.list_directory(remote_path, cancellation)?;
        if !children.is_empty() {
            return Err(Error::Message(format!(
                "write-probe directory {remote_path:?} was not empty after creation"
            )));
        }
        Ok(())
    }

    fn upload_non_overwriting(
        &self,
        local: &LocalEntry,
        remote_file: &str,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.upload_observed_with_policy(local, remote_file, None, false, false, cancellation)
    }

    fn cleanup_write_probe(
        &self,
        root: &RemoteRoot,
        probe_path: &str,
        upload_path: &str,
        copy_directory: &str,
        copy_path: &str,
    ) -> ProbeCleanup {
        let mut first_error = None;
        for path in [copy_path, copy_directory, upload_path, probe_path] {
            if let Err(error) = self.delete_probe_path_bounded(root, path)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }

        match self.get_info_with_retry(probe_path, false) {
            Err(error) if error.api_code() == Some(408) => ProbeCleanup {
                completed: true,
                leftover_remote_probe_path: None,
                // A final absence check supersedes transient/missing-child cleanup errors.
                error: None,
            },
            Ok(_) => ProbeCleanup {
                completed: false,
                leftover_remote_probe_path: Some(probe_path.to_owned()),
                error: first_error.or_else(|| {
                    Some(Error::Message(format!(
                        "write-probe cleanup left remote path {probe_path:?}"
                    )))
                }),
            },
            Err(error) => ProbeCleanup {
                completed: false,
                // If the final check failed, conservatively tell the caller where to inspect.
                leftover_remote_probe_path: Some(probe_path.to_owned()),
                error: first_error.or(Some(error)),
            },
        }
    }

    fn delete_probe_path_bounded(&self, root: &RemoteRoot, remote_path: &str) -> Result<()> {
        validate_delete_target(root, remote_path)?;
        let result = self.call_bounded::<Value>(
            "SYNO.FileStation.Delete",
            2,
            "delete",
            vec![
                pair("path", json_array([remote_path])?),
                pair("recursive", "false"),
            ],
            STOP_REQUEST_TIMEOUT,
        );
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.api_code() == Some(408) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Walk the destination into a complete recursive inventory.
    ///
    /// The traversal is unbounded by design, so `cancellation` is consulted before each ancestor
    /// probe and before each directory in the breadth-first queue is listed. A cancelled walk
    /// returns [`Error::Cancelled`] and no partial inventory.
    pub fn remote_inventory(
        &self,
        root: &RemoteRoot,
        cancellation: &CancellationToken,
    ) -> Result<RemoteInventory> {
        let scope = Scope::root();
        Ok(self
            .remote_inventory_scoped(root, &scope, usize::MAX, cancellation)?
            .inventory)
    }

    /// Walk only the part of the destination that `scope` names.
    ///
    /// Entries keep their **root-relative** paths regardless of the scope, so ignore rules and
    /// plan comparison behave exactly as they do for a whole-tree inventory.
    ///
    /// The ancestor guard runs over the scope path rather than the root, so it covers the root's
    /// ancestors *and* the scope's own components in one pass: a mount point anywhere along the
    /// way is still rejected. Directories between the root and the scope are recorded, so a
    /// scoped plan does not propose re-creating parents that already exist.
    ///
    /// A scope may name a single file. A scope that does not exist remotely is not an error: it
    /// is the answer "nothing here is on the NAS yet".
    pub fn remote_inventory_scoped(
        &self,
        root: &RemoteRoot,
        scope: &Scope,
        budget: usize,
        cancellation: &CancellationToken,
    ) -> Result<ScopedRemoteInventory> {
        let mut entries = BTreeMap::new();
        let scope_path = root.join(scope.as_str())?;
        let mut pending = vec![scope_path.clone()];
        let mut root_exists = true;

        // Inspect every ancestor before traversing. This also catches a destination below
        // a mounted remote folder, including when the final destination does not exist yet.
        for path in absolute_prefixes(&scope_path) {
            cancellation.check()?;
            let info = match self.get_info(&path) {
                Ok(info) => info,
                Err(error) if error.api_code() == Some(408) => {
                    // Absent at or above the root means the root itself is missing. Absent below
                    // it means only the scope is missing, which is a legitimate answer.
                    if path.len() <= root.as_str().len() {
                        root_exists = false;
                    }
                    return Ok(ScopedRemoteInventory {
                        inventory: RemoteInventory {
                            root_exists,
                            entries,
                        },
                        complete: true,
                    });
                }
                Err(error) => return Err(error),
            };
            let is_scope_path = path == scope_path;
            // A scope is allowed to name one file; anything else on the path must be a directory.
            let scope_names_a_file = is_scope_path && !scope.is_root() && !info.isdir;
            if !info.isdir && !scope_names_a_file {
                return Err(Error::Message(format!(
                    "remote destination ancestor {path} exists but is not a directory"
                )));
            }
            let additional = info.additional.unwrap_or_default();
            if let Some(mount_type) = additional
                .mount_point_type
                .clone()
                .filter(|value| !value.trim().is_empty())
            {
                return Err(Error::RemoteMountRoot { path, mount_type });
            }
            if root.contains_child(&path) {
                let relative = root.relative(&path)?;
                let kind = if info.isdir {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                let (size, mtime_seconds) =
                    file_metadata("SYNO.FileStation.Getinfo.getinfo", kind, &additional)?;
                entries.insert(
                    relative.clone(),
                    RemoteEntry {
                        relative,
                        remote_path: path.clone(),
                        kind,
                        size,
                        mtime_seconds,
                        mount_point_type: None,
                        content_md5: None,
                    },
                );
            }
            if scope_names_a_file {
                // Nothing to list beneath a file.
                pending.clear();
            }
        }

        let mut complete = true;
        while let Some(folder) = pending.pop() {
            cancellation.check()?;
            if entries.len() >= budget {
                complete = false;
                break;
            }
            let files = match self.list_directory(&folder, cancellation) {
                Ok(files) => files,
                Err(error) if folder == root.as_str() && error.api_code() == Some(408) => {
                    root_exists = false;
                    break;
                }
                // The scope itself may simply not exist yet; that is an answer, not a failure.
                Err(error) if folder == scope_path && error.api_code() == Some(408) => break,
                Err(error) => return Err(error),
            };

            for item in files {
                let (actual_parent, actual_name) = parent_and_name(&item.path)?;
                if actual_parent != folder || actual_name != item.name {
                    return Err(Error::InvalidResponse {
                        operation: "SYNO.FileStation.List.list".to_owned(),
                        message: format!(
                            "server returned inconsistent child path {:?} while listing {:?}",
                            item.path, folder
                        ),
                    });
                }
                let relative = root.relative(&item.path)?;
                if relative.is_empty() {
                    return Err(Error::InvalidResponse {
                        operation: "SYNO.FileStation.List.list".to_owned(),
                        message: "directory listing unexpectedly included its own root".to_owned(),
                    });
                }
                let kind = if item.isdir {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                let additional = item.additional.unwrap_or_default();
                let (size, mtime_seconds) =
                    file_metadata("SYNO.FileStation.List.list", kind, &additional)?;
                let mount_point_type = additional
                    .mount_point_type
                    .filter(|value| !value.trim().is_empty());
                let entry = RemoteEntry {
                    relative: relative.clone(),
                    remote_path: item.path.clone(),
                    kind,
                    size,
                    mtime_seconds,
                    mount_point_type: mount_point_type.clone(),
                    content_md5: None,
                };
                if entries.insert(relative.clone(), entry).is_some() {
                    return Err(Error::InvalidResponse {
                        operation: "SYNO.FileStation.List.list".to_owned(),
                        message: format!("server returned duplicate path {relative:?}"),
                    });
                }
                if item.isdir && mount_point_type.is_none() {
                    pending.push(item.path);
                }
            }
        }

        Ok(ScopedRemoteInventory {
            inventory: RemoteInventory {
                root_exists,
                entries,
            },
            complete,
        })
    }

    /// Inspect one destination with a hard diagnostic budget.
    ///
    /// Unlike [`Self::remote_inventory`], this never recursively walks the target. It performs at
    /// most one bounded `getinfo` request and one bounded list page, examines at most six direct
    /// children, and returns at most five deterministic samples. Sync planning continues to use
    /// the complete recursive inventory; Doctor uses this narrow path so an unexpectedly large
    /// tree cannot turn an interactive health check into an unbounded scan.
    ///
    /// A caller that has just walked the destination itself wants
    /// [`Self::diagnostic_remote_inventory_of_existing_root`], which skips the opening `getinfo`
    /// as already answered rather than paying for it twice.
    pub fn diagnostic_remote_inventory(
        &self,
        root: &RemoteRoot,
    ) -> Result<DiagnosticRemoteInventory> {
        self.required_session()?;
        let started = Instant::now();
        let remaining = || {
            DIAGNOSTIC_INVENTORY_TIMEOUT
                .checked_sub(started.elapsed())
                .filter(|duration| !duration.is_zero())
                .ok_or(Error::OperationTimedOut {
                    operation: "bounded target diagnostic inventory",
                })
        };

        let info_parameters = vec![
            pair("path", json_array([root.as_str()])?),
            pair(
                "additional",
                json_array(["size", "time", "mount_point_type"])?,
            ),
        ];
        let mut info: GetInfoData = match self.call_bounded(
            "SYNO.FileStation.List",
            2,
            "getinfo",
            info_parameters,
            remaining()?,
        ) {
            Ok(Some(info)) => info,
            Ok(None) => {
                return Err(Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.getinfo".to_owned(),
                    message: "successful response contained no path information".to_owned(),
                });
            }
            Err(error) if error.api_code() == Some(408) => {
                return Ok(DiagnosticRemoteInventory::absent_root());
            }
            Err(error) => return Err(error),
        };
        if info.files.len() != 1 {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.List.getinfo".to_owned(),
                message: "target diagnostic expected exactly one path result".to_owned(),
            });
        }
        // File Station reports a missing path per entry as readily as it does at the envelope
        // level, so the same `408` has to be recognised in both places or the destination check
        // reports a malformed response where it means "that directory is not there".
        let root_item = match info
            .files
            .pop()
            .expect("length checked")
            .into_item("SYNO.FileStation.List", "getinfo")
        {
            Ok(item) => item,
            Err(error) if error.api_code() == Some(408) => {
                return Ok(DiagnosticRemoteInventory::absent_root());
            }
            Err(error) => return Err(error),
        };
        if root_item.path != root.as_str() || !root_item.isdir {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.List.getinfo".to_owned(),
                message: "target diagnostic result was not the requested directory".to_owned(),
            });
        }
        if let Some(mount_type) = root_item
            .additional
            .and_then(|additional| additional.mount_point_type)
            .filter(|value| !value.trim().is_empty())
        {
            return Err(Error::RemoteMountRoot {
                path: root.as_str().to_owned(),
                mount_type,
            });
        }

        self.diagnostic_inventory_page(root, remaining()?)
    }

    /// The same bounded inventory, for a destination the caller has already inspected.
    ///
    /// [`Self::diagnostic_remote_inventory`] opens with a `getinfo` that establishes three things:
    /// the path exists, it is a directory, and it is not a mount boundary. The doctor's write
    /// permission check walks the destination one component at a time immediately beforehand and
    /// establishes exactly those three, for exactly that path, from the same `getinfo` method with
    /// the same `additional` fields -- it returns `RemoteMountRoot` for a mount boundary and a
    /// plain error for a non-directory, so a walk that reports the destination as existing has
    /// already ruled both out. Repeating the request costs a full round trip to re-learn it.
    ///
    /// Only for a caller holding that evidence. Every other path keeps the opening `getinfo`,
    /// which is what turns an absent destination into `absent_root` rather than an error.
    pub fn diagnostic_remote_inventory_of_existing_root(
        &self,
        root: &RemoteRoot,
    ) -> Result<DiagnosticRemoteInventory> {
        self.required_session()?;
        // The whole budget goes to the listing: there is no `getinfo` here to share it with.
        self.diagnostic_inventory_page(root, DIAGNOSTIC_INVENTORY_TIMEOUT)
    }

    /// The one bounded direct-child page both diagnostic inventories return.
    fn diagnostic_inventory_page(
        &self,
        root: &RemoteRoot,
        timeout: Duration,
    ) -> Result<DiagnosticRemoteInventory> {
        let parameters = vec![
            pair("folder_path", json_string(root.as_str())?),
            pair("offset", "0"),
            pair("limit", DIAGNOSTIC_INVENTORY_REQUEST_LIMIT.to_string()),
            pair("sort_by", json_string("name")?),
            pair("sort_direction", json_string("asc")?),
            pair("filetype", json_string("all")?),
            pair(
                "additional",
                json_array(["size", "time", "mount_point_type"])?,
            ),
        ];
        let data: ListData = self
            .call_bounded("SYNO.FileStation.List", 2, "list", parameters, timeout)?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.FileStation.List.list".to_owned(),
                message: "successful response contained no directory data".to_owned(),
            })?;
        if data.files.len() > DIAGNOSTIC_INVENTORY_REQUEST_LIMIT || data.total < data.files.len() {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.List.list".to_owned(),
                message: "bounded diagnostic page exceeded its declared item budget".to_owned(),
            });
        }

        let mut files = data
            .files
            .into_iter()
            .map(|item| item.into_item("SYNO.FileStation.List", "list"))
            .collect::<Result<Vec<_>>>()?;
        files.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.path.cmp(&right.path))
        });
        let mut validated = Vec::with_capacity(files.len());
        let mut observed_paths = BTreeSet::new();
        for item in files {
            let (actual_parent, actual_name) = parent_and_name(&item.path)?;
            if actual_parent != root.as_str()
                || actual_name != item.name
                || item.name.is_empty()
                || item.name.contains('/')
                || item.name.contains('\\')
                || item.name.chars().any(char::is_control)
            {
                return Err(Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list".to_owned(),
                    message: "bounded diagnostic child escaped its requested parent".to_owned(),
                });
            }
            let relative = root.relative(&item.path)?;
            if relative.is_empty() {
                return Err(Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list".to_owned(),
                    message: "bounded diagnostic listing included its own root".to_owned(),
                });
            }
            if !observed_paths.insert(relative.clone()) {
                return Err(Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list".to_owned(),
                    message: "bounded diagnostic page contained a duplicate child".to_owned(),
                });
            }
            let kind = if item.isdir {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            let additional = item.additional.unwrap_or_default();
            let (size, mtime_seconds) =
                file_metadata("SYNO.FileStation.List.list", kind, &additional)?;
            let mount_boundary = additional
                .mount_point_type
                .is_some_and(|value| !value.trim().is_empty());
            let (relative_path, relative_path_truncated) =
                bounded_diagnostic_text(&relative, DIAGNOSTIC_RELATIVE_PATH_MAX_CHARS);
            let (name, name_truncated) =
                bounded_diagnostic_text(&item.name, DIAGNOSTIC_NAME_MAX_CHARS);
            validated.push(DiagnosticRemoteEntry {
                relative_path,
                relative_path_truncated,
                name,
                name_truncated,
                kind,
                size_bytes: (kind == EntryKind::File).then_some(size),
                mtime_seconds: (kind == EntryKind::File).then_some(mtime_seconds),
                mount_boundary,
            });
        }
        validated.truncate(DIAGNOSTIC_INVENTORY_SAMPLE_LIMIT);
        let sample = validated;
        let truncated_count = data.total.saturating_sub(sample.len());
        Ok(DiagnosticRemoteInventory {
            root_exists: true,
            total_entries: data.total,
            sample,
            truncated: truncated_count > 0,
            truncated_count,
            truncated_reason: (truncated_count > 0).then_some("sample_limit"),
            pages_requested: 1,
            traversal_depth: 1,
            deadline_ms: duration_millis_saturating(DIAGNOSTIC_INVENTORY_TIMEOUT),
        })
    }

    /// Discover shared-folder roots visible to the authenticated account with a hard budget.
    ///
    /// This is the default Standard/Extensive Doctor inventory when no destination was selected.
    /// It performs exactly one bounded `list_share` request, examines at most six wire entries,
    /// and returns at most five deterministic, validated directory samples. It does not select a
    /// destination and does not inspect or mutate any child of a returned shared folder.
    pub fn diagnostic_visible_shared_folders(&self) -> Result<DiagnosticRemoteInventory> {
        self.required_session()?;
        let parameters = vec![
            pair("offset", "0"),
            pair("limit", DIAGNOSTIC_INVENTORY_REQUEST_LIMIT.to_string()),
            pair("sort_by", json_string("name")?),
            pair("sort_direction", json_string("asc")?),
            pair("additional", json_array(["perm"])?),
        ];
        let data: ListShareData = self
            .call_bounded(
                "SYNO.FileStation.List",
                2,
                "list_share",
                parameters,
                DIAGNOSTIC_INVENTORY_TIMEOUT,
            )?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.FileStation.List.list_share".to_owned(),
                message: "successful response contained no shared-folder list".to_owned(),
            })?;
        let total = data.total.unwrap_or(data.shares.len());
        if data.shares.len() > DIAGNOSTIC_INVENTORY_REQUEST_LIMIT || total < data.shares.len() {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.List.list_share".to_owned(),
                message: "bounded diagnostic page exceeded its declared item budget".to_owned(),
            });
        }

        let mut validated = Vec::with_capacity(data.shares.len());
        let mut observed_paths = BTreeSet::new();
        for share in data.shares {
            let root = RemoteRoot::parse(&share.path)?;
            if root.as_str() != root.share_path() {
                return Err(Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list_share".to_owned(),
                    message: "shared-folder diagnostic result was not a normalized root path"
                        .to_owned(),
                });
            }
            let name = share
                .name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| root.share_name().to_owned());
            if name != root.share_name()
                || name.contains('/')
                || name.contains('\\')
                || name.chars().any(char::is_control)
            {
                return Err(Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list_share".to_owned(),
                    message: "shared-folder diagnostic name did not match its root path".to_owned(),
                });
            }
            if !observed_paths.insert(root.as_str().to_owned()) {
                return Err(Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list_share".to_owned(),
                    message: "bounded diagnostic page contained a duplicate shared-folder root"
                        .to_owned(),
                });
            }
            let (relative_path, relative_path_truncated) =
                bounded_diagnostic_text(root.as_str(), DIAGNOSTIC_RELATIVE_PATH_MAX_CHARS);
            let (name, name_truncated) = bounded_diagnostic_text(&name, DIAGNOSTIC_NAME_MAX_CHARS);
            validated.push(DiagnosticRemoteEntry {
                relative_path,
                relative_path_truncated,
                name,
                name_truncated,
                kind: EntryKind::Directory,
                size_bytes: None,
                mtime_seconds: None,
                mount_boundary: false,
            });
        }
        validated.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.relative_path.cmp(&right.relative_path))
        });
        validated.truncate(DIAGNOSTIC_INVENTORY_SAMPLE_LIMIT);
        let truncated_count = total.saturating_sub(validated.len());
        Ok(DiagnosticRemoteInventory {
            root_exists: true,
            total_entries: total,
            sample: validated,
            truncated: truncated_count > 0,
            truncated_count,
            truncated_reason: (truncated_count > 0).then_some("sample_limit"),
            pages_requested: 1,
            traversal_depth: 0,
            deadline_ms: duration_millis_saturating(DIAGNOSTIC_INVENTORY_TIMEOUT),
        })
    }

    /// Read everything DSM advertises, not merely the ten APIs this tool requires.
    ///
    /// Deliberately separate from [`Self::discover`]. Discovery feeds `required_spec` and
    /// therefore every call this client makes, and its map is decoded all-or-nothing: one
    /// third-party package advertising a malformed entry would break connection establishment
    /// itself. This read decodes entry by entry into an all-optional type instead, so a bad entry
    /// costs a line of output rather than the run.
    ///
    /// The `entry.cgi` to `query.cgi` fallback is reused rather than reimplemented: this request
    /// is subject to exactly the same reverse-proxy misrouting the fallback exists to survive.
    pub fn enumerate_all_apis(&self) -> Result<ApiCatalogue> {
        let raw = match self.enumerate_all_apis_at("entry.cgi") {
            Ok(raw) => raw,
            Err(first_error) => match self.enumerate_all_apis_at("query.cgi") {
                Ok(raw) => raw,
                Err(second_error) => {
                    return Err(Error::Message(format!(
                        "DSM capability enumeration failed through the reverse proxy; \
                         entry.cgi: {first_error}; query.cgi fallback: {second_error}"
                    )));
                }
            },
        };
        let mut catalogue = ApiCatalogue::default();
        for (name, value) in raw {
            match serde_json::from_value::<DiscoveredApi>(value) {
                Ok(api) => {
                    catalogue.apis.insert(name, api);
                }
                // Counted, never dropped silently: a DSM advertising an API entry this shape
                // cannot describe is itself worth a line of the report.
                Err(_) => catalogue.unusable_entries += 1,
            }
        }
        Ok(catalogue)
    }

    fn enumerate_all_apis_at(&self, cgi: &str) -> Result<BTreeMap<String, Value>> {
        let url = endpoint_url(&self.base, cgi)?;
        let fields = vec![
            pair("api", "SYNO.API.Info"),
            pair("version", "1"),
            pair("method", "query"),
            // Documented since DSM 4.0 and unauthenticated: the location of SYNO.API.Info is
            // fixed precisely so a client can always ask this question.
            pair("query", "all"),
        ];
        self.send_form_with_retry(url, fields, "SYNO.API.Info", 1, "query", true)?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.API.Info.query".to_owned(),
                message: "successful response contained no API map".to_owned(),
            })
    }

    /// Present the same read-only request once per variant, varying only how the session is
    /// carried.
    ///
    /// This is the one measurement that separates "the session is rejected because of how this
    /// client presents it" from "the session is rejected because the path does not carry it".
    /// Every variant is `list_share` with `limit=1` -- the same bounded, non-mutating call
    /// [`Self::confirm_file_station_session`] already makes -- so the only variable is the
    /// channel.
    ///
    /// The variants run in a fixed order with [`SessionChannels::All`] first, so the probe
    /// reproduces the run's own behaviour before it starts changing anything.
    /// The variant that needs its own session is left for [`Self::probe_tokenless_sid_field`],
    /// which the caller runs against a separately established client: this method has one session
    /// and cannot make another without the password, which it deliberately never sees.
    pub fn probe_session_channels(
        &self,
        tokenless_unavailable: &'static str,
    ) -> Result<[ChannelProbe; SESSION_CHANNEL_VARIANTS]> {
        self.required_session()?;
        self.validate_api("SYNO.FileStation.List", 2)?;
        let mut probes = [ChannelProbe::unrun(SessionChannels::All); SESSION_CHANNEL_VARIANTS];
        for (slot, channels) in SessionChannels::ABLATION_ORDER.iter().enumerate() {
            self.cancellation.check()?;
            probes[slot] = if channels.needs_tokenless_login() {
                ChannelProbe::skipped(*channels, tokenless_unavailable)
            } else {
                self.probe_one_session_channel(*channels)?
            };
        }
        Ok(probes)
    }

    /// The documented `_sid` request field alone, against this client's own session.
    ///
    /// Meant to be called on a client whose session was established by
    /// [`Self::login_without_syno_token`]; nothing here enforces that, because the point of the
    /// variant is what DSM answers rather than what the client believes it holds.
    pub fn probe_tokenless_sid_field(&self) -> Result<ChannelProbe> {
        self.validate_api("SYNO.FileStation.List", 2)?;
        self.probe_one_session_channel(SessionChannels::SidFieldOnlyTokenlessLogin)
    }

    /// One ablation variant: build the request by hand so the channel is explicit.
    ///
    /// The production header helpers are deliberately untouched. They derive what to attach from
    /// session state, which is exactly the behaviour under test, so a probe that reused them
    /// could not vary the thing it exists to vary.
    fn probe_one_session_channel(&self, channels: SessionChannels) -> Result<ChannelProbe> {
        let session = self.required_session()?;
        let url = self.api_url("SYNO.FileStation.List")?;
        let mut fields = vec![
            pair("api", "SYNO.FileStation.List"),
            pair("version", "2"),
            pair("method", "list_share"),
            pair("offset", "0"),
            pair("limit", "1"),
        ];
        if channels.sends_sid_field() {
            fields.push(pair("_sid", session.sid.to_string()));
        }
        if channels.sends_token_field()
            && let Some(token) = &session.syno_token
        {
            fields.push(pair("SynoToken", token.to_string()));
        }
        let fields = Zeroizing::new(fields);

        let mut record = ApiCallDetail::started(
            "SYNO.FileStation.List",
            "list_share",
            2,
            route_text(&url),
            RequestTransport::Form,
        );
        // Reported from the selector rather than from session state: for this request the two
        // deliberately disagree, and the selector is the truth on the wire.
        record.session = SessionTransport {
            cookie_header: channels.sends_cookie_header(),
            syno_token_header: channels.sends_token_header() && session.syno_token.is_some(),
            sid_field: channels.sends_sid_field(),
            syno_token_field: channels.sends_token_field() && session.syno_token.is_some(),
        };
        record.request_fields = u16::try_from(fields.len()).unwrap_or(u16::MAX);
        let timeout = self.remaining_control_timeout(SESSION_CONFIRMATION_TIMEOUT)?;
        record.timeout_ms = duration_millis_saturating(timeout);
        self.observe(ApiObservation::CallStarted(record));

        let mut facts = ResponseFacts::default();
        let started = Instant::now();
        let outcome = self
            .with_selected_session_headers(self.http.post(url), channels)?
            .timeout(timeout)
            .form(&*fields)
            .send()
            .map_err(|source| Error::Http {
                operation: "SYNO.FileStation.List.list_share".to_owned(),
                source,
            })
            .and_then(|response| {
                decode_response_observed::<ListShareData>(
                    response,
                    "SYNO.FileStation.List",
                    "list_share",
                    &mut facts,
                )
            });
        record.elapsed_ms = duration_millis_saturating(started.elapsed());
        apply_response_facts(&mut record, facts);
        apply_outcome(&mut record, &outcome, "SYNO.FileStation.List");
        self.observe(ApiObservation::CallCompleted(record));
        Ok(ChannelProbe {
            channels,
            outcome: record.outcome,
            dsm_code: record.dsm_code,
            http_status: record.http_status,
            elapsed_ms: record.elapsed_ms,
            skipped: None,
        })
    }

    /// Attach only the session headers a probe asked for.
    ///
    /// Separate from [`Self::with_blocking_session_headers`] on purpose: that one is the
    /// production path and stays exactly as it is, so nothing sync does changes because a
    /// diagnostic wanted a selector.
    fn with_selected_session_headers(
        &self,
        mut request: reqwest::blocking::RequestBuilder,
        channels: SessionChannels,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        if let Some(headers) = self.session_headers()? {
            if channels.sends_cookie_header() {
                request = request.header(COOKIE, headers.cookie);
            }
            if channels.sends_token_header()
                && let Some(token) = headers.syno_token
            {
                request = request.header(X_SYNO_TOKEN_HEADER, token);
            }
        }
        Ok(request)
    }

    /// Read File Station's own per-account capability report.
    ///
    /// `SYNO.FileStation.Info` version 2 `get` takes no parameters and is documented since
    /// DSM 6.0. It is the only documented, non-admin call that names the host serving the
    /// request, which is why the diagnostic makes it twice: a hostname that differs between two
    /// calls of one run is the only positive evidence of a path that does not reach one host.
    pub fn file_station_info(&self) -> Result<FileStationInfo> {
        let info: FileStationInfoWire = self
            .call_bounded(
                "SYNO.FileStation.Info",
                2,
                "get",
                Vec::new(),
                SESSION_CONFIRMATION_TIMEOUT,
            )?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.FileStation.Info.get".to_owned(),
                message: "successful response contained no File Station information".to_owned(),
            })?;
        let protocols = info
            .support_virtual_protocol
            .as_ref()
            .and_then(virtual_protocol_text)
            .or_else(|| {
                info.support_virtual
                    .as_ref()
                    .and_then(virtual_protocol_text)
            });
        Ok(FileStationInfo {
            hostname: info
                .hostname
                .filter(|hostname| !hostname.is_empty())
                .map(|hostname| BoundedText::sanitized(&hostname)),
            is_manager: info.is_manager,
            support_sharing: info.support_sharing,
            support_virtual_protocol: protocols
                .filter(|protocols| !protocols.is_empty())
                .map(|protocols| BoundedText::sanitized(&protocols)),
        })
    }

    /// Exercise one advertised, read-only capability and report only how DSM answered.
    ///
    /// The API name, method, and version are compile-time literals from the caller, and the CGI
    /// path is routed through [`endpoint_url`], which confines it to the configured origin and
    /// `webapi/` prefix. Nothing here can reach an API the caller did not name.
    pub fn probe_capability(&self, spec: CapabilityProbeSpec) -> Result<CapabilityProbe> {
        let session = self.required_session()?;
        let url = endpoint_url(&self.base, spec.cgi_path)?;
        let mut fields = vec![
            pair("api", spec.api),
            pair("version", spec.version.to_string()),
            pair("method", spec.method),
        ];
        fields.extend(spec.parameters.iter().cloned());
        fields.push(pair("_sid", session.sid.to_string()));
        if let Some(token) = &session.syno_token {
            fields.push(pair("SynoToken", token.to_string()));
        }
        let fields = Zeroizing::new(fields);

        let mut record = ApiCallDetail::started(
            spec.api,
            spec.method,
            spec.version,
            route_text(&url),
            RequestTransport::Form,
        );
        record.session = self.session_transport(&fields);
        record.request_fields = u16::try_from(fields.len()).unwrap_or(u16::MAX);
        let timeout = self.remaining_control_timeout(SESSION_CONFIRMATION_TIMEOUT)?;
        record.timeout_ms = duration_millis_saturating(timeout);
        self.observe(ApiObservation::CallStarted(record));

        let mut facts = ResponseFacts::default();
        let started = Instant::now();
        let outcome = self
            .with_blocking_session_headers(self.http.post(url))?
            .timeout(timeout)
            .form(&*fields)
            .send()
            .map_err(|source| Error::Http {
                operation: format!("{}.{}", spec.api, spec.method),
                source,
            })
            .and_then(|response| {
                decode_response_observed::<Value>(response, spec.api, spec.method, &mut facts)
            });
        record.elapsed_ms = duration_millis_saturating(started.elapsed());
        apply_response_facts(&mut record, facts);
        apply_outcome(&mut record, &outcome, spec.api);
        self.observe(ApiObservation::CallCompleted(record));
        Ok(CapabilityProbe {
            api: spec.api,
            method: spec.method,
            version: spec.version,
            outcome: record.outcome,
            dsm_code: record.dsm_code,
            http_status: record.http_status,
            elapsed_ms: record.elapsed_ms,
        })
    }

    pub fn create_folder(&self, remote_path: &str) -> Result<()> {
        let (parent, name) = parent_and_name(remote_path)?;
        let parameters = vec![
            pair("folder_path", json_array([parent])?),
            pair("name", json_array([name])?),
            pair("force_parent", "true"),
        ];
        self.call::<Value>(
            "SYNO.FileStation.CreateFolder",
            2,
            "create",
            parameters,
            true,
        )?;
        Ok(())
    }

    pub fn upload(&self, local: &LocalEntry, remote_file: &str) -> Result<()> {
        self.upload_observed(local, remote_file, None, &CancellationToken::default())
    }

    pub fn upload_observed(
        &self,
        local: &LocalEntry,
        remote_file: &str,
        observer: Option<UploadObserver>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.upload_observed_with_policy(local, remote_file, observer, true, true, cancellation)
    }

    fn upload_observed_with_policy(
        &self,
        local: &LocalEntry,
        remote_file: &str,
        observer: Option<UploadObserver>,
        overwrite: bool,
        create_parents: bool,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let result = self.upload_observed_inner(
            local,
            remote_file,
            observer.clone(),
            overwrite,
            create_parents,
            cancellation,
        );
        if let Some(observer) = observer {
            let _ = observer(if result.is_ok() {
                UploadTransferEvent::Completed
            } else {
                UploadTransferEvent::Failed
            });
        }
        result
    }

    fn upload_observed_inner(
        &self,
        local: &LocalEntry,
        remote_file: &str,
        observer: Option<UploadObserver>,
        overwrite: bool,
        create_parents: bool,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let (remote_parent, remote_name) = parent_and_name(remote_file)?;
        let observer_cancelled = Arc::new(AtomicBool::new(false));
        for attempt in 0..=self.retries {
            cancellation.check()?;
            if observer.as_ref().is_some_and(|observer| {
                !observer(UploadTransferEvent::AttemptStarted {
                    attempt: attempt + 1,
                })
            }) {
                return Err(Error::Cancelled);
            }
            verify_local_snapshot(local)?;
            let file = File::open(&local.full_path).map_err(|source| Error::FileIo {
                path: local.full_path.clone(),
                source,
            })?;
            verify_open_file_snapshot(local, &file)?;
            let reader = ObservedReader {
                inner: file,
                observer: observer.clone(),
                cancelled: Arc::clone(&observer_cancelled),
                throttle: upload_throttle(self.upload_rate_limit.as_ref(), cancellation),
            };
            let part = Part::reader_with_length(reader, local.size)
                .file_name(remote_name.to_owned())
                .mime_str("application/octet-stream")
                .map_err(|source| Error::Http {
                    operation: format!("preparing upload for {}", local.full_path.display()),
                    source,
                })?;
            let session = self.required_session()?;
            let mut form = Form::new()
                .text("api", "SYNO.FileStation.Upload")
                .text("version", "2")
                .text("method", "upload")
                .text("path", remote_parent.to_owned())
                .text("create_parents", create_parents.to_string())
                .text("overwrite", overwrite.to_string())
                .text("mtime", local.mtime_ms.to_string())
                .text("_sid", session.sid.to_string());
            let syno_token_attached = session.syno_token.is_some();
            if let Some(token) = &session.syno_token {
                form = form.text("SynoToken", token.to_string());
            }
            // Synology requires the binary part to be last.
            form = form.part("file", part);

            let url = self.api_url("SYNO.FileStation.Upload")?;
            let operation = format!("uploading {}", local.relative);
            let mut record = ApiCallDetail::started(
                "SYNO.FileStation.Upload",
                "upload",
                2,
                route_text(&url),
                RequestTransport::Multipart,
            );
            record.attempt = attempt + 1;
            record.max_attempts = self.retries + 1;
            // The multipart form carries the same session material the form path does, built
            // field by field just above; describe it the same way rather than re-deriving it.
            record.session = SessionTransport {
                cookie_header: true,
                syno_token_header: syno_token_attached,
                sid_field: true,
                syno_token_field: syno_token_attached,
            };
            record.request_bytes = local.size;
            record.timeout_ms = duration_millis_saturating(self.upload_timeout);
            self.observe(ApiObservation::CallStarted(record));
            let started = Instant::now();
            let request = self.with_blocking_session_headers(self.http.post(url))?;
            let mut facts = ResponseFacts::default();
            let result = match request.timeout(self.upload_timeout).multipart(form).send() {
                Ok(response) => decode_response_observed::<Value>(
                    response,
                    "SYNO.FileStation.Upload",
                    "upload",
                    &mut facts,
                )
                .map(|_| ()),
                Err(source) => Err(Error::Http {
                    operation: operation.clone(),
                    source,
                }),
            };
            record.elapsed_ms = duration_millis_saturating(started.elapsed());
            apply_response_facts(&mut record, facts);
            apply_outcome(&mut record, &result, "SYNO.FileStation.Upload");
            self.observe(ApiObservation::CallCompleted(record));
            let result = prioritize_observer_cancellation(&observer_cancelled, result);
            match result {
                Ok(()) => {
                    verify_local_snapshot(local)?;
                    if let Some(expected) = local.content_md5 {
                        let actual_local = crate::local::hash_file_snapshot(local, cancellation)?;
                        if !content_evidence_matches(expected, actual_local) {
                            return Err(Error::SourceChanged(local.full_path.clone()));
                        }
                        self.verify_remote_content(
                            remote_file,
                            local.size,
                            expected,
                            cancellation,
                        )?;
                    }
                    return Ok(());
                }
                Err(error) if attempt < self.retries && retryable(&error) => {
                    verify_local_snapshot(local)?;
                    if let Some(expected) = local.content_md5 {
                        let actual_local = crate::local::hash_file_snapshot(local, cancellation)?;
                        if !content_evidence_matches(expected, actual_local) {
                            return Err(Error::SourceChanged(local.full_path.clone()));
                        }
                        match self.remote_content_matches(
                            remote_file,
                            local.size,
                            expected,
                            cancellation,
                        ) {
                            Ok(true) => return Ok(()),
                            Ok(false) => {}
                            Err(probe_error) if retryable(&probe_error) => {}
                            Err(probe_error) => return Err(probe_error),
                        }
                    }
                    retry_pause_cancellable(attempt, cancellation)?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("retry loop always returns")
    }

    /// Validate and open an upload source before any destructive type replacement begins.
    pub fn preflight_upload_source(
        &self,
        local: &LocalEntry,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        verify_local_snapshot(local)?;
        let file = File::open(&local.full_path).map_err(|source| Error::FileIo {
            path: local.full_path.clone(),
            source,
        })?;
        verify_open_file_snapshot(local, &file)?;
        if let Some(expected) = local.content_md5 {
            let actual = crate::local::hash_file_snapshot(local, cancellation)?;
            if !content_evidence_matches(expected, actual) {
                return Err(Error::SourceChanged(local.full_path.clone()));
            }
        }
        Ok(())
    }

    pub fn delete_non_recursive(&self, root: &RemoteRoot, remote_path: &str) -> Result<()> {
        validate_delete_target(root, remote_path)?;
        let parameters = vec![
            pair("path", json_array([remote_path])?),
            pair("recursive", "false"),
        ];
        self.call::<Value>("SYNO.FileStation.Delete", 2, "delete", parameters, false)?;
        Ok(())
    }

    fn discover(&self) -> Result<HashMap<String, ApiSpec>> {
        let first = self.discover_at("entry.cgi");
        match first {
            Ok(apis) => Ok(apis),
            Err(first_error) => match self.discover_at("query.cgi") {
                Ok(apis) => Ok(apis),
                Err(second_error) => {
                    let message = format!(
                        "File Station API discovery failed through the reverse proxy; entry.cgi: {first_error}; query.cgi fallback: {second_error}"
                    );
                    if discovery_route_returned_response(&first_error)
                        || discovery_route_returned_response(&second_error)
                    {
                        Err(Error::InvalidResponse {
                            operation: DISCOVERY_FAILURE_OPERATION.to_owned(),
                            message,
                        })
                    } else {
                        Err(Error::Message(message))
                    }
                }
            },
        }
    }

    fn discover_at(&self, cgi: &str) -> Result<HashMap<String, ApiSpec>> {
        let url = endpoint_url(&self.base, cgi)?;
        let fields = vec![
            pair("api", "SYNO.API.Info"),
            pair("version", "1"),
            pair("method", "query"),
            pair("query", DISCOVERY_APIS.join(",")),
        ];
        self.send_form_with_retry(url, fields, "SYNO.API.Info", 1, "query", true)?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.API.Info.query".to_owned(),
                message: "successful response contained no API map".to_owned(),
            })
    }

    /// Read every page of one directory listing.
    ///
    /// A directory large enough to paginate is checked once per page so a Ctrl-C during the
    /// remote scan is not held until the whole directory has been drained.
    fn list_directory(
        &self,
        folder: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RemoteItem>> {
        let mut offset = 0_usize;
        let mut output = Vec::new();
        loop {
            cancellation.check()?;
            let parameters = vec![
                pair("folder_path", json_string(folder)?),
                pair("offset", offset.to_string()),
                pair("limit", LIST_PAGE_SIZE.to_string()),
                pair("sort_by", json_string("name")?),
                pair("sort_direction", json_string("asc")?),
                pair("filetype", json_string("all")?),
                pair(
                    "additional",
                    json_array(["size", "time", "mount_point_type"])?,
                ),
            ];
            let data: ListData = self
                .call("SYNO.FileStation.List", 2, "list", parameters, true)?
                .ok_or_else(|| Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list".to_owned(),
                    message: "successful response contained no directory data".to_owned(),
                })?;
            let count = data.files.len();
            for item in data.files {
                output.push(item.into_item("SYNO.FileStation.List", "list")?);
            }
            offset += count;
            if offset >= data.total {
                break;
            }
            if count == 0 {
                return Err(Error::InvalidResponse {
                    operation: "SYNO.FileStation.List.list".to_owned(),
                    message: format!(
                        "pagination stalled at offset {offset} while server reported {} entries",
                        data.total
                    ),
                });
            }
        }
        Ok(output)
    }

    fn get_info(&self, path: &str) -> Result<RemoteItem> {
        self.get_info_with_retry(path, true)
    }

    fn get_info_with_retry(&self, path: &str, allow_retry: bool) -> Result<RemoteItem> {
        let parameters = vec![
            pair("path", json_array([path])?),
            pair(
                "additional",
                json_array(["size", "time", "mount_point_type"])?,
            ),
        ];
        let mut data: GetInfoData = self
            .call(
                "SYNO.FileStation.List",
                2,
                "getinfo",
                parameters,
                allow_retry,
            )?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.FileStation.List.getinfo".to_owned(),
                message: "successful response contained no path information".to_owned(),
            })?;
        if data.files.len() != 1 {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.List.getinfo".to_owned(),
                message: format!(
                    "expected exactly one result for {path:?}, received {}",
                    data.files.len()
                ),
            });
        }
        let item = data.files.pop().expect("length checked");
        if item.path != path {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.List.getinfo".to_owned(),
                message: format!(
                    "server returned path {:?} while inspecting {path:?}",
                    item.path
                ),
            });
        }
        // The path is checked before the per-entry status is raised, so a `408` is never
        // attributed to a path other than the one that was asked about.
        item.into_item("SYNO.FileStation.List", "getinfo")
    }

    fn call<T: DeserializeOwned>(
        &self,
        api: &'static str,
        version: u32,
        method: &'static str,
        parameters: Vec<(String, String)>,
        allow_retry: bool,
    ) -> Result<Option<T>> {
        self.validate_api(api, version)?;
        let fields = self.authenticated_fields(api, version, method, parameters)?;
        let url = self.api_url(api)?;
        self.send_form_with_retry(url, fields, api, version, method, allow_retry)
    }

    fn call_bounded<T: DeserializeOwned>(
        &self,
        api: &'static str,
        version: u32,
        method: &'static str,
        parameters: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<Option<T>> {
        self.validate_api(api, version)?;
        let fields = self.authenticated_fields(api, version, method, parameters)?;
        let url = self.api_url(api)?;
        self.send_form_once_with_timeout(url, fields, api, version, method, timeout)
    }

    fn authenticated_fields(
        &self,
        api: &str,
        version: u32,
        method: &str,
        parameters: Vec<(String, String)>,
    ) -> Result<Vec<(String, String)>> {
        let session = self.required_session()?;
        let mut fields = vec![
            pair("api", api),
            pair("version", version.to_string()),
            pair("method", method),
        ];
        fields.extend(parameters);
        fields.push(pair("_sid", session.sid.to_string()));
        if let Some(token) = &session.syno_token {
            fields.push(pair("SynoToken", token.to_string()));
        }
        Ok(fields)
    }

    fn send_form_with_retry<T: DeserializeOwned>(
        &self,
        url: Url,
        fields: Vec<(String, String)>,
        api: &'static str,
        version: u32,
        method: &'static str,
        allow_retry: bool,
    ) -> Result<Option<T>> {
        // These owned copies can include SID/SynoToken values. Erase them after the final
        // attempt; each per-attempt clone is erased by `send_form_once` as well.
        let fields = Zeroizing::new(fields);
        let attempts = if allow_retry { self.retries } else { 0 };
        for attempt in 0..=attempts {
            let result = self.send_attempt(
                url.clone(),
                fields.to_vec(),
                AttemptContext {
                    api,
                    version,
                    method,
                    timeout: self.control_timeout,
                    attempt: attempt + 1,
                    max_attempts: attempts + 1,
                },
                &mut ResponseFacts::default(),
            );
            match result {
                Ok(value) => return Ok(value),
                // The pause is this loop's cancellation point, and the only one it may have.
                // `sleep_cancellable` checks the token before every slice and once more on the
                // way out, so a cancelled run never reaches the next attempt. The check
                // deliberately does not move to the top of the loop: `attempts` is zero whenever
                // a caller passed `allow_retry: false` -- the non-recursive delete and the
                // task-stop requests do -- and a guard there would refuse the single request
                // those paths depend on, turning cancellation into abandoned remote state.
                Err(error) if attempt < attempts && retryable(&error) => {
                    // Report the wait that is about to happen, not merely that one will: a log
                    // showing four attempts without their backoff cannot explain where the
                    // wall-clock time went.
                    let mut record = ApiCallDetail::started(
                        api,
                        method,
                        version,
                        route_text(&url),
                        RequestTransport::Form,
                    );
                    record.attempt = attempt + 1;
                    record.max_attempts = attempts + 1;
                    record.retry_backoff_ms =
                        Some(duration_millis_saturating(retry_backoff(attempt)));
                    record.outcome = RequestOutcome::Transport;
                    self.observe(ApiObservation::CallStarted(record));
                    retry_pause_cancellable(attempt, &self.cancellation)?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("retry loop always returns")
    }

    fn send_form_once<T: DeserializeOwned>(
        &self,
        url: Url,
        fields: Vec<(String, String)>,
        api: &'static str,
        version: u32,
        method: &'static str,
    ) -> Result<Option<T>> {
        self.send_form_once_with_timeout(url, fields, api, version, method, self.control_timeout)
    }

    fn send_form_once_with_timeout<T: DeserializeOwned>(
        &self,
        url: Url,
        fields: Vec<(String, String)>,
        api: &'static str,
        version: u32,
        method: &'static str,
        timeout: Duration,
    ) -> Result<Option<T>> {
        self.send_attempt(
            url,
            fields,
            AttemptContext::single(api, version, method, timeout),
            &mut ResponseFacts::default(),
        )
    }

    /// Send one form request and report it to any installed observer.
    ///
    /// This is the single choke point every DSM control request passes through, so instrumenting
    /// it here is what makes a whole failing run legible from one log instead of costing a round
    /// trip per unanswered question.
    fn send_attempt<T: DeserializeOwned>(
        &self,
        url: Url,
        fields: Vec<(String, String)>,
        context: AttemptContext,
        facts: &mut ResponseFacts,
    ) -> Result<Option<T>> {
        let AttemptContext {
            api,
            version,
            method,
            attempt,
            max_attempts,
            ..
        } = context;
        let timeout = self.remaining_control_timeout(context.timeout)?;
        // This blocking `send` is the one wait on the control plane that cancellation cannot
        // shorten. It is self-bounded by `MAX_CONTROL_REQUEST_TIMEOUT`, and because retries now
        // abandon their backoff on cancellation, a cancelled run waits out at most one of these
        // windows rather than accumulating one per attempt. Slicing it would mean either moving
        // every control call onto the async client or leaving a detached thread holding a live
        // session and its zeroizing field copy; a second Ctrl-C exits immediately instead.
        //
        // Passwords, OTPs, and session values enter this owned form field list. reqwest must
        // still serialize its own request-body copy, but this caller-owned copy is short-lived
        // and explicitly erased.
        let fields = Zeroizing::new(fields);

        let mut record = ApiCallDetail::started(
            api,
            method,
            version,
            route_text(&url),
            RequestTransport::Form,
        );
        record.attempt = attempt;
        record.max_attempts = max_attempts;
        record.session = self.session_transport(&fields);
        record.request_fields = u16::try_from(fields.len()).unwrap_or(u16::MAX);
        record.timeout_ms = duration_millis_saturating(timeout);
        self.observe(ApiObservation::CallStarted(record));

        let started = Instant::now();
        let outcome = self
            .with_blocking_session_headers(self.http.post(url))?
            .timeout(timeout)
            .form(&*fields)
            .send()
            .map_err(|source| Error::Http {
                operation: format!("{api}.{method}"),
                source,
            })
            .and_then(|response| decode_response_observed(response, api, method, facts));
        record.elapsed_ms = duration_millis_saturating(started.elapsed());
        apply_response_facts(&mut record, *facts);
        apply_outcome(&mut record, &outcome, api);
        self.observe(ApiObservation::CallCompleted(record));
        outcome
    }

    fn remaining_control_timeout(&self, requested: Duration) -> Result<Duration> {
        let Some(deadline) = self.control_deadline else {
            return Ok(requested);
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Message(
                "File Station control operation exceeded its total deadline".to_owned(),
            ));
        }
        Ok(requested.min(remaining))
    }

    fn validate_api(&self, api: &str, version: u32) -> Result<()> {
        let spec = self.required_spec(api)?;
        if version < spec.min_version || version > spec.max_version {
            return Err(Error::UnsupportedApiVersion {
                api: api.to_owned(),
                version,
                min: spec.min_version,
                max: spec.max_version,
            });
        }
        Ok(())
    }

    fn required_spec(&self, api: &str) -> Result<&ApiSpec> {
        self.apis
            .get(api)
            .ok_or_else(|| Error::MissingApi(api.to_owned()))
    }

    fn required_session(&self) -> Result<&Session> {
        self.session
            .as_ref()
            .ok_or_else(|| Error::Message("not authenticated to File Station".to_owned()))
    }

    fn session_headers(&self) -> Result<Option<SessionHeaders>> {
        self.session
            .as_ref()
            .map(Session::request_headers)
            .transpose()
    }

    fn with_blocking_session_headers(
        &self,
        mut request: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        if let Some(headers) = self.session_headers()? {
            request = request.header(COOKIE, headers.cookie);
            if let Some(token) = headers.syno_token {
                request = request.header(X_SYNO_TOKEN_HEADER, token);
            }
        }
        Ok(request)
    }

    fn with_async_session_headers(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        if let Some(headers) = self.session_headers()? {
            request = request.header(COOKIE, headers.cookie);
            if let Some(token) = headers.syno_token {
                request = request.header(X_SYNO_TOKEN_HEADER, token);
            }
        }
        Ok(request)
    }

    fn api_url(&self, api: &str) -> Result<Url> {
        endpoint_url(&self.base, &self.required_spec(api)?.path)
    }
}

impl Session {
    fn request_headers(&self) -> Result<SessionHeaders> {
        if self.sid.len() > MAX_SESSION_HEADER_BYTES || !valid_cookie_value(&self.sid) {
            return Err(invalid_session_header("SID"));
        }
        let cookie = Zeroizing::new(format!("id={}", self.sid.as_str()));
        let mut cookie =
            HeaderValue::from_str(&cookie).map_err(|_| invalid_session_header("SID"))?;
        cookie.set_sensitive(true);

        let syno_token = self
            .syno_token
            .as_ref()
            .map(|token| {
                if token.is_empty() || token.len() > MAX_SESSION_HEADER_BYTES {
                    return Err(invalid_session_header("SynoToken"));
                }
                let mut header = HeaderValue::from_str(token)
                    .map_err(|_| invalid_session_header("SynoToken"))?;
                header.set_sensitive(true);
                Ok(header)
            })
            .transpose()?;

        Ok(SessionHeaders { cookie, syno_token })
    }
}

#[derive(Debug, Deserialize)]
struct LoginData {
    sid: String,
    #[serde(default)]
    synotoken: Option<String>,
}

/// `SYNO.FileStation.Info` version 2 `get`.
///
/// Every member is optional, and the two virtual-protocol members are read as raw JSON, because
/// DSM 7 does not answer in the shape the guide documents. The guide describes
/// `support_virtual_protocol` as a comma-separated *string* and its worked example spells the
/// same idea `support_virtual`; a DSM 7.2 host answers with a JSON *array* under the documented
/// name and an unrelated *object* of mount toggles under the example's name. Reading either as a
/// `String` -- and reading both through one serde alias, as this once did -- fails on every real
/// DSM 7, which is what made a capability probe report `decode` against a healthy NAS.
#[derive(Debug, Deserialize)]
struct FileStationInfoWire {
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    is_manager: Option<bool>,
    #[serde(default)]
    support_sharing: Option<bool>,
    #[serde(default)]
    support_virtual_protocol: Option<Value>,
    /// The guide's worked-example spelling. Consulted only when it carries protocol names, which
    /// on DSM 7 it does not: there it is an object describing which mounts the account may make.
    #[serde(default)]
    support_virtual: Option<Value>,
}

/// The virtual-protocol list as text, in whichever of DSM's shapes it arrives.
///
/// A string is passed through, an array of strings is joined the way the guide's prose says the
/// value looks, and anything else -- DSM 7's `support_virtual` object, a number, a null -- is
/// reported as "not stated" rather than guessed at.
fn virtual_protocol_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let names: Vec<&str> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .collect();
            (!names.is_empty()).then(|| names.join(","))
        }
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct ListShareData {
    #[serde(default)]
    total: Option<usize>,
    #[serde(default)]
    shares: Vec<ShareWire>,
}

#[derive(Debug, Deserialize)]
struct ShareWire {
    path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    disable_list: bool,
    #[serde(default)]
    additional: Option<RemoteAdditionalWire>,
}

#[derive(Debug, Deserialize)]
struct ListData {
    total: usize,
    #[serde(default)]
    files: Vec<RemoteItemWire>,
}

#[derive(Debug, Deserialize)]
struct GetInfoData {
    #[serde(default)]
    files: Vec<RemoteItemWire>,
}

#[derive(Debug, Deserialize)]
struct TaskStartData {
    taskid: String,
}

#[derive(Debug, Deserialize)]
struct Md5StatusData {
    finished: bool,
    #[serde(default)]
    md5: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskStatusData {
    finished: bool,
}

#[derive(Debug, Deserialize)]
struct RemoteItemWire {
    path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    isdir: Option<bool>,
    /// File Station's per-entry status for this path.
    ///
    /// `getinfo` answers each requested path independently, so an envelope that succeeded can
    /// still report `408` for a path that does not exist -- and such an entry carries neither a
    /// `name` nor an `isdir`. Requiring those two members turned "the path is not there", which
    /// is the answer the write probe is asking for, into an undiagnosable decode failure.
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    disable_list: bool,
    #[serde(default)]
    additional: Option<RemoteAdditionalWire>,
}

/// One File Station entry whose shape has been checked, so the rest of the client need not.
///
/// Separate from [`RemoteItemWire`] on purpose: the wire type tolerates everything DSM sends and
/// this one holds only entries that actually describe a file or folder. Anything else has already
/// been turned into the DSM error it is.
#[derive(Debug)]
struct RemoteItem {
    path: String,
    name: String,
    isdir: bool,
    disable_list: bool,
    additional: Option<RemoteAdditionalWire>,
}

impl RemoteItemWire {
    /// Validate one entry, turning a per-entry status into the DSM error it stands for.
    ///
    /// A non-zero `code` becomes [`Error::Api`], so the `408` handling every caller already has
    /// keeps working whether DSM reports a missing path at the envelope level or per entry. Only
    /// after that is a missing `name` or `isdir` treated as a malformed response, because for an
    /// entry DSM is describing rather than rejecting, those two are the entry.
    fn into_item(self, api: &'static str, method: &'static str) -> Result<RemoteItem> {
        if let Some(code) = self.code.filter(|code| *code != 0) {
            return Err(Error::Api {
                api: api.to_owned(),
                operation: method.to_owned(),
                code,
                description: api_error_description(api, code)
                    .map(|description| format!(": {description}"))
                    .unwrap_or_default(),
                // The per-entry status is a code, not a payload; there is nothing else to carry.
                details: Vec::new(),
            });
        }
        let missing = match (&self.name, self.isdir) {
            (Some(_), Some(_)) => None,
            (None, Some(_)) => Some("name"),
            (Some(_), None) => Some("isdir"),
            (None, None) => Some("name or isdir"),
        };
        if let Some(missing) = missing {
            return Err(Error::InvalidResponse {
                operation: format!("{api}.{method}"),
                message: format!("entry described no {missing}"),
            });
        }
        Ok(RemoteItem {
            path: self.path,
            name: self.name.expect("name presence checked"),
            isdir: self.isdir.expect("isdir presence checked"),
            disable_list: self.disable_list,
            additional: self.additional,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct RemoteAdditionalWire {
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    time: Option<RemoteTimeWire>,
    #[serde(default)]
    mount_point_type: Option<String>,
    #[serde(default)]
    perm: Option<RemotePermissionWire>,
}

#[derive(Debug, Default, Deserialize)]
struct RemotePermissionWire {
    #[serde(default)]
    adv_right: Option<RemoteAdvancedRightWire>,
    #[serde(default)]
    acl: Option<RemoteAclWire>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteAdvancedRightWire {
    #[serde(default)]
    disable_list: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteAclWire {
    #[serde(default)]
    read: Option<bool>,
    #[serde(default)]
    exec: Option<bool>,
}

fn permission_disables_listing(additional: Option<&RemoteAdditionalWire>) -> bool {
    additional
        .and_then(|value| value.perm.as_ref())
        .is_some_and(|permission| {
            permission
                .adv_right
                .as_ref()
                .is_some_and(|rights| rights.disable_list)
                || permission
                    .acl
                    .as_ref()
                    .is_some_and(|acl| acl.read == Some(false) || acl.exec == Some(false))
        })
}

/// The `time` member of a `<file additional>` object.
///
/// `mtime` is optional because the object carries four timestamps and DSM is free to answer with
/// any subset of them. A caller that needs a modification time says so through [`file_metadata`],
/// which fails with a message naming what was missing rather than with a decode failure naming
/// nothing.
#[derive(Debug, Deserialize)]
struct RemoteTimeWire {
    #[serde(default)]
    mtime: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    success: bool,
    data: Option<T>,
    error: Option<ApiErrorWire>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorWire {
    code: i64,
    #[serde(default)]
    errors: Value,
}

static DOWNLOAD_RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, String>> =
    OnceLock::new();

fn download_runtime() -> Result<&'static tokio::runtime::Runtime> {
    match DOWNLOAD_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(DOWNLOAD_RUNTIME_THREADS)
            .thread_name("sdsync-download-io")
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(runtime) => Ok(runtime),
        Err(reason) => Err(Error::Message(format!(
            "failed to build the bounded download runtime: {reason}"
        ))),
    }
}

fn run_download_request(
    request: reqwest::RequestBuilder,
    remote_path: String,
    expected_size: u64,
    operation_timeout: Duration,
    cancellation: CancellationToken,
) -> Result<ContentMd5> {
    cancellation.check()?;
    let deadline = tokio::time::Instant::now()
        .checked_add(operation_timeout)
        .ok_or_else(|| Error::Message("operation timeout is too large".to_owned()))?;
    let runtime = download_runtime()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let _task = runtime.spawn(async move {
        let result = download_content_fingerprint(
            request,
            &remote_path,
            expected_size,
            deadline,
            &cancellation,
        )
        .await;
        let _ = sender.send(result);
    });
    receiver.recv().map_err(|_| {
        Error::Message("bounded download runtime stopped before returning a result".to_owned())
    })?
}

async fn download_content_fingerprint(
    request: reqwest::RequestBuilder,
    remote_path: &str,
    expected_size: u64,
    deadline: tokio::time::Instant,
    cancellation: &CancellationToken,
) -> Result<ContentMd5> {
    let mut response = await_download(request.send(), deadline, cancellation, remote_path).await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::HttpStatus {
            operation: "SYNO.FileStation.Download.download".to_owned(),
            status,
            message: withheld_response_message("SYNO.FileStation.Download").to_owned(),
        });
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.starts_with("application/json") {
        let body =
            read_download_json_body(&mut response, deadline, cancellation, remote_path).await?;
        // The download path has no `ApiCallDetail` to attach a fault to: this JSON body is a DSM
        // error standing in for file content, and the error it raises is what the caller sees.
        let decoded = decode_response_body::<Value>(
            status,
            &body,
            "SYNO.FileStation.Download",
            "download",
            &mut None,
        );
        return match decoded {
            Err(error) => Err(error),
            Ok(_) => Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.Download.download".to_owned(),
                message: "successful download returned a JSON envelope instead of file bytes"
                    .to_owned(),
            }),
        };
    }
    if !content_type.starts_with("application/octet-stream") {
        return Err(Error::InvalidResponse {
            operation: "SYNO.FileStation.Download.download".to_owned(),
            message: "download response was not an octet stream".to_owned(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length != expected_size)
    {
        return Err(Error::RemoteSnapshotChanged(remote_path.to_owned()));
    }

    let mut hasher = ContentHasher::new();
    let mut bytes_read = 0_u64;
    while let Some(chunk) =
        await_download(response.chunk(), deadline, cancellation, remote_path).await?
    {
        bytes_read = bytes_read
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| Error::RemoteSnapshotChanged(remote_path.to_owned()))?;
        if bytes_read > expected_size {
            return Err(Error::RemoteSnapshotChanged(remote_path.to_owned()));
        }
        hasher.update(&chunk);
    }
    cancellation.check()?;
    if tokio::time::Instant::now() >= deadline {
        return Err(Error::OperationTimedOut {
            operation: "remote content fingerprint download",
        });
    }
    if bytes_read != expected_size {
        return Err(Error::RemoteSnapshotChanged(remote_path.to_owned()));
    }
    Ok(hasher.finalize())
}

async fn read_download_json_body(
    response: &mut reqwest::Response,
    deadline: tokio::time::Instant,
    cancellation: &CancellationToken,
    remote_path: &str,
) -> Result<Zeroizing<Vec<u8>>> {
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) =
        await_download(response.chunk(), deadline, cancellation, remote_path).await?
    {
        let remaining = (MAX_JSON_RESPONSE + 1).saturating_sub(body.len() as u64);
        if chunk.len() as u64 > remaining {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.Download.download".to_owned(),
                message: "response exceeded the 32 MiB safety limit".to_owned(),
            });
        }
        body.extend_from_slice(&chunk);
        if body.len() as u64 > MAX_JSON_RESPONSE {
            return Err(Error::InvalidResponse {
                operation: "SYNO.FileStation.Download.download".to_owned(),
                message: "response exceeded the 32 MiB safety limit".to_owned(),
            });
        }
    }
    Ok(body)
}

async fn await_download<T, F>(
    future: F,
    deadline: tokio::time::Instant,
    cancellation: &CancellationToken,
    remote_path: &str,
) -> Result<T>
where
    F: std::future::Future<Output = reqwest::Result<T>>,
{
    tokio::pin!(future);
    loop {
        cancellation.check()?;
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(Error::OperationTimedOut {
                operation: "remote content fingerprint download",
            });
        }
        let poll_at = (now + RATE_LIMIT_POLL_INTERVAL).min(deadline);
        if let Ok(result) = tokio::time::timeout_at(poll_at, &mut future).await {
            cancellation.check()?;
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::OperationTimedOut {
                    operation: "remote content fingerprint download",
                });
            }
            return result.map_err(|source| Error::Http {
                operation: format!("downloading {remote_path:?} for content verification"),
                source,
            });
        }
    }
}

/// Read the size and modified time a snapshot comparison depends on.
///
/// File Station reports both for every file when `size` and `time` are requested, so an absent
/// value is a malformed response rather than a real zero. Coercing it to `0` would let a file
/// that was replaced after planning compare equal to its stored snapshot and be deleted anyway,
/// so files fail closed here exactly as [`ApiClient::remote_file_size`] does. Directories are
/// exempt: DSM omits both fields for them, and directory snapshots are additionally guarded by
/// the descendant check in the sync executor.
fn file_metadata(
    operation: &str,
    kind: EntryKind,
    additional: &RemoteAdditionalWire,
) -> Result<(u64, i64)> {
    let size = additional.size;
    let mtime_seconds = additional.time.as_ref().and_then(|time| time.mtime);
    if kind == EntryKind::Directory {
        return Ok((size.unwrap_or(0), mtime_seconds.unwrap_or(0)));
    }
    let missing = match (size, mtime_seconds) {
        (Some(size), Some(mtime_seconds)) => return Ok((size, mtime_seconds)),
        (None, Some(_)) => "byte size",
        (Some(_), None) => "modified time",
        (None, None) => "byte size or modified time",
    };
    Err(Error::InvalidResponse {
        operation: operation.to_owned(),
        message: format!("file information contained no {missing}"),
    })
}

fn bounded_diagnostic_text(value: &str, maximum_chars: usize) -> (String, bool) {
    let mut characters = value.chars();
    let bounded = characters.by_ref().take(maximum_chars).collect::<String>();
    let truncated = characters.next().is_some();
    (bounded, truncated)
}

fn duration_millis_saturating(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------------------------
// Decode diagnosis
// ---------------------------------------------------------------------------------------------

/// Describe a failed deserialization in schema terms, without repeating any response content.
///
/// serde's own message cannot be forwarded: `invalid type: string "…"` quotes the value it
/// rejected, and a DSM response body can carry a session identifier. Only four things are kept --
/// the classification, the member name serde named, serde's own description of what it wanted,
/// and the JSON *type* it found -- and the reported position is turned into a dotted member path
/// by walking the body's structure rather than by copying any part of it.
fn decode_fault(body: &[u8], error: &serde_json::Error) -> DecodeFault {
    let message = error.to_string();
    let (kind, field) = classify_decode_message(&message, error);
    let (container, member) = json_member_paths(body, error.line(), error.column());
    // A named member is described where it *should have been*, which for a missing or duplicated
    // one is inside its container rather than at the position the parser stopped at.
    let path = match kind {
        DecodeFaultKind::MissingField
        | DecodeFaultKind::UnknownField
        | DecodeFaultKind::DuplicateField => match (container.is_empty(), field.is_empty()) {
            (_, true) => container,
            (true, false) => field.clone(),
            (false, false) => format!("{container}.{field}"),
        },
        _ => member,
    };
    DecodeFault {
        kind,
        path: BoundedText::sanitized(&path),
        field: ShortToken::sanitized(&field),
        expected: ShortToken::sanitized(&expected_description(&message)),
        found: found_json_kind(&message, kind),
        line: u32::try_from(error.line()).unwrap_or(u32::MAX),
        column: u32::try_from(error.column()).unwrap_or(u32::MAX),
    }
}

/// Classify serde's message, and recover the member name it names.
///
/// Matching on the message text is deliberate: `serde_json::Error` exposes only a coarse
/// [`serde_json::error::Category`], and the difference between "the member is missing" and "the
/// member is the wrong type" is the whole value of this diagnostic. The category still decides
/// every case the message does not name, so an unrecognised message degrades to a coarse but
/// truthful classification rather than to a wrong one.
fn classify_decode_message(message: &str, error: &serde_json::Error) -> (DecodeFaultKind, String) {
    for (prefix, kind) in [
        ("missing field ", DecodeFaultKind::MissingField),
        ("unknown field ", DecodeFaultKind::UnknownField),
        ("duplicate field ", DecodeFaultKind::DuplicateField),
    ] {
        if let Some(rest) = message.strip_prefix(prefix) {
            return (kind, backtick_quoted(rest).unwrap_or_default());
        }
    }
    if message.starts_with("invalid type:")
        || message.starts_with("invalid value:")
        || message.starts_with("invalid length")
    {
        return (DecodeFaultKind::TypeMismatch, String::new());
    }
    let kind = match error.classify() {
        serde_json::error::Category::Syntax => DecodeFaultKind::Syntax,
        serde_json::error::Category::Eof => DecodeFaultKind::UnexpectedEnd,
        serde_json::error::Category::Data | serde_json::error::Category::Io => {
            DecodeFaultKind::Unspecified
        }
    };
    (kind, String::new())
}

/// The text between the first pair of backticks, which is how serde quotes a member name.
fn backtick_quoted(text: &str) -> Option<String> {
    let rest = text.strip_prefix('`')?;
    let end = rest.find('`')?;
    Some(rest[..end].to_owned())
}

/// Serde's own `expected` description, which comes from a `Visitor::expecting` implementation
/// and so is derived from the client's schema rather than from the server's bytes.
fn expected_description(message: &str) -> String {
    let Some(rest) = message.split(", expected ").nth(1) else {
        return String::new();
    };
    let end = rest.find(" at line ").unwrap_or(rest.len());
    rest[..end].trim().to_owned()
}

/// The JSON type serde named as *found*, taken from the type word alone.
///
/// `invalid type: string "abcdef"` yields [`JsonKind::String`] and nothing else: the word before
/// the value is the only part of that message this reads.
fn found_json_kind(message: &str, kind: DecodeFaultKind) -> JsonKind {
    if kind == DecodeFaultKind::MissingField {
        return JsonKind::Absent;
    }
    let Some(rest) = message.strip_prefix("invalid type: ") else {
        return JsonKind::Unknown;
    };
    let word = rest
        .split([' ', ',', '`'])
        .find(|word| !word.is_empty())
        .unwrap_or_default();
    match word {
        "null" | "unit" => JsonKind::Null,
        "boolean" => JsonKind::Bool,
        "integer" | "floating" => JsonKind::Number,
        "string" | "character" => JsonKind::String,
        "sequence" => JsonKind::Array,
        "map" => JsonKind::Object,
        _ => JsonKind::Unknown,
    }
}

/// The dotted paths of the member at a reported position, and of the container holding it.
///
/// Built by walking the body's structural bytes and keeping only object keys and array indices;
/// no scalar value is ever read, and a string is consumed for its span rather than its contents
/// unless it is in key position. Returns `(container, member)` so a caller can describe a member
/// that is *missing* -- which has a container but no position of its own -- as well as one that
/// is present under the wrong type.
fn json_member_paths(body: &[u8], line: usize, column: usize) -> (String, String) {
    enum Frame {
        Object { key: String, expecting_key: bool },
        Array { index: usize },
    }

    let target = decode_target_offset(body, line, column);
    let mut stack: Vec<Frame> = Vec::new();
    let mut index = 0_usize;
    while index < body.len() && index < target {
        match body[index] {
            b'"' => {
                let (text, next) = scan_json_string(body, index);
                if let Some(Frame::Object { key, expecting_key }) = stack.last_mut()
                    && *expecting_key
                {
                    *key = text;
                    *expecting_key = false;
                }
                index = next;
            }
            b'{' => {
                stack.push(Frame::Object {
                    key: String::new(),
                    expecting_key: true,
                });
                index += 1;
            }
            b'[' => {
                stack.push(Frame::Array { index: 0 });
                index += 1;
            }
            b'}' | b']' => {
                stack.pop();
                index += 1;
            }
            b',' => {
                match stack.last_mut() {
                    Some(Frame::Object { key, expecting_key }) => {
                        key.clear();
                        *expecting_key = true;
                    }
                    Some(Frame::Array { index }) => *index += 1,
                    None => {}
                }
                index += 1;
            }
            _ => index += 1,
        }
    }

    let mut container = String::new();
    let mut member = String::new();
    for frame in &stack {
        match frame {
            Frame::Object { key, .. } => {
                container = member.clone();
                if !key.is_empty() {
                    push_path_segment(&mut member, key);
                }
            }
            Frame::Array { index } => {
                container = member.clone();
                push_path_segment(&mut member, &index.to_string());
            }
        }
    }
    (container, member)
}

/// Where in the body the reported position falls, backed up onto a structural byte.
///
/// serde reports the position it had reached, which for a structural disagreement is at or just
/// past the bracket or brace that caused it. Stepping back onto that byte is what keeps a
/// container on the walk's stack at the moment the walk stops, and so keeps `data.files` from
/// reading as `data.files.0` merely because the parser had already consumed the `[`.
fn decode_target_offset(body: &[u8], line: usize, column: usize) -> usize {
    let mut offset = 0_usize;
    let mut current_line = 1_usize;
    while current_line < line && offset < body.len() {
        if body[offset] == b'\n' {
            current_line += 1;
        }
        offset += 1;
    }
    let mut target = offset
        .saturating_add(column.saturating_sub(1))
        .min(body.len());
    if target > 0 && matches!(body[target - 1], b'{' | b'[' | b'}' | b']') {
        target -= 1;
    }
    target
}

/// Consume one JSON string starting at `start`, returning its unescaped-enough text and the index
/// just past its closing quote.
///
/// Escapes are honoured only far enough to find the end of the string and to keep a key readable:
/// the text is used as a path segment and is sanitized before it reaches any record.
fn scan_json_string(body: &[u8], start: usize) -> (String, usize) {
    let mut text = String::new();
    let mut index = start + 1;
    while index < body.len() {
        match body[index] {
            b'"' => return (text, index + 1),
            b'\\' => {
                // The escaped byte cannot end the string, whatever it is.
                index += 2;
            }
            byte => {
                text.push(char::from(byte));
                index += 1;
            }
        }
    }
    (text, body.len())
}

fn push_path_segment(path: &mut String, segment: &str) {
    if !path.is_empty() {
        path.push('.');
    }
    path.push_str(segment);
}

/// Decode a response and record the facts that stop being observable once the body is read.
///
/// `Set-Cookie` and `Location` are read as presence and host respectively, before the body is
/// consumed. Neither header value is retained.
fn decode_response_observed<T: DeserializeOwned>(
    mut response: Response,
    api: &str,
    method: &str,
    facts: &mut ResponseFacts,
) -> Result<Option<T>> {
    let status = response.status();
    facts.http_status = Some(status.as_u16());
    // Read here rather than in `decode_response_body`, which receives only the status and the
    // body bytes: by then the headers are gone. Whether DSM rotates the session on a *successful*
    // response is the fact that distinguishes a stale client-held identifier from a rejected one.
    (facts.set_cookie_count, facts.set_cookie_names) = set_cookie_names(response.headers());
    facts.cookies = cookie_facts(response.headers());
    facts.intermediary = intermediary_facts(response.headers());
    facts.redirect_host = response
        .headers()
        .get(LOCATION)
        .and_then(|location| location.to_str().ok())
        .and_then(redirect_host);
    // Successful authentication responses contain the SID and may contain a SynoToken;
    // challenge responses can contain a short-lived challenge token. Erase the raw response
    // allocation after decoding. Deserialized and reqwest-owned intermediary allocations are
    // separate and cannot all be guaranteed zeroized by this layer.
    let mut body = Zeroizing::new(Vec::new());
    response
        .by_ref()
        .take(MAX_JSON_RESPONSE + 1)
        .read_to_end(&mut body)
        .map_err(|source| Error::HttpBody {
            operation: format!("{api}.{method}"),
            source,
        })?;
    facts.response_bytes = body.len() as u64;
    if body.len() as u64 > MAX_JSON_RESPONSE {
        return Err(Error::InvalidResponse {
            operation: format!("{api}.{method}"),
            message: "response exceeded the 32 MiB safety limit".to_owned(),
        });
    }
    decode_response_body(status, &body, api, method, &mut facts.decode)
}

/// Decode one DSM envelope, recording *why* it did not decode when it does not.
///
/// `fault` receives the schema-level description of a deserialization failure. It is separate
/// from the returned error because [`Error::InvalidResponse`] is raised from a dozen places that
/// have no serde error to describe, and because the record it feeds must stay `Copy` and free of
/// response content.
fn decode_response_body<T: DeserializeOwned>(
    status: StatusCode,
    body: &[u8],
    api: &str,
    method: &str,
    fault: &mut Option<DecodeFault>,
) -> Result<Option<T>> {
    // API discovery is the only unauthenticated response decoded here. Every other API either
    // receives login material or an authenticated SID/SynoToken. Default to withholding those
    // response bodies so a diagnostic proxy cannot reflect request secrets into user-visible
    // errors or logs.
    let withhold_response_body = api != "SYNO.API.Info";
    if !status.is_success() {
        return Err(Error::HttpStatus {
            operation: format!("{api}.{method}"),
            status,
            message: if withhold_response_body {
                withheld_response_message(api).to_owned()
            } else {
                http_status_hint(status, body)
            },
        });
    }

    let envelope: Envelope<T> = serde_json::from_slice(body).map_err(|error| {
        let diagnosis = decode_fault(body, &error);
        *fault = Some(diagnosis);
        let route_hint = if looks_like_html(body) {
            " (the proxy returned HTML, so /webapi/* is probably routed to the File Station UI instead of WebAPI)"
        } else {
            ""
        };
        // serde's own text quotes the value it rejected, so it is forwarded only for the one
        // unauthenticated API whose body is already reportable. Everywhere else the schema-level
        // diagnosis says the same thing about the *shape* without republishing the bytes.
        let detail = if withhold_response_body {
            format!(
                "{}; response: [{}]",
                diagnosis.describe(),
                withheld_response_message(api)
            )
        } else {
            format!("{error}; response: {}", response_snippet(body))
        };
        Error::InvalidResponse {
            operation: format!("{api}.{method}"),
            message: format!("expected a DSM JSON envelope: {detail}{route_hint}"),
        }
    })?;
    if envelope.success {
        return Ok(envelope.data);
    }
    let error = envelope.error.unwrap_or(ApiErrorWire {
        code: 100,
        errors: Value::Null,
    });
    let description = api_error_description(api, error.code)
        .map(|description| format!(": {description}"))
        .unwrap_or_default();
    Err(Error::Api {
        api: api.to_owned(),
        operation: method.to_owned(),
        code: error.code,
        description,
        // Auth challenges can contain short-lived challenge tokens, and a diagnostic proxy can
        // reflect authenticated request fields. Never retain either class of response detail in
        // an error that the CLI (or Debug logging) might print.
        details: if withhold_response_body {
            Vec::new()
        } else {
            error_details(error.errors)
        },
    })
}

fn withheld_response_message(api: &str) -> &'static str {
    if api == "SYNO.API.Auth" {
        "authentication response body withheld"
    } else {
        "authenticated API response body withheld"
    }
}

fn error_details(value: Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(values) => values,
        value => vec![value],
    }
}

pub fn normalize_base_url(input: &str, allow_http: bool) -> Result<Url> {
    let raw = input.trim();
    let normalized = if raw.ends_with('/') {
        raw.to_owned()
    } else {
        format!("{raw}/")
    };
    let url = Url::parse(&normalized).map_err(|error| Error::InvalidUrl(error.to_string()))?;
    if url.scheme() != "https" && !(allow_http && url.scheme() == "http") {
        return Err(Error::HttpsRequired);
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(Error::InvalidUrl(
            "URL must have a host and must not contain credentials".to_owned(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::InvalidUrl(
            "query strings and fragments are not allowed".to_owned(),
        ));
    }
    Ok(url)
}

/// Describe a connected client's transport identity.
///
/// `normalize_base_url` has already rejected credentials, a query, and a fragment, so only the
/// host and path remain, and both still pass the record sanitizer on the way in.
fn connection_detail(base: &Url, options: &ClientOptions) -> ConnectionDetail {
    ConnectionDetail {
        scheme: if base.scheme() == "http" {
            UrlScheme::Http
        } else {
            UrlScheme::Https
        },
        host: BoundedText::sanitized(base.host_str().unwrap_or_default()),
        port: base.port(),
        base_path: BoundedText::sanitized(base.path()),
        certificate_verification: if options.accept_invalid_certs {
            CertificateVerification::Disabled
        } else if options.ca_certificate.is_some() {
            CertificateVerification::CustomCa
        } else {
            CertificateVerification::Enabled
        },
    }
}

/// Load the operator's pinned CA, rejecting a file that pins nothing.
///
/// The rustls backend only stores the PEM bytes here and parses them when the client is built, so
/// a file holding no CERTIFICATE section at all -- an empty, truncated, or simply mistaken file --
/// would be accepted in silence and leave the operator believing a CA was pinned when nothing was
/// added to the trust store. Counting the sections up front makes that loud; an unreadable payload
/// is deliberately left to surface where reqwest actually rejects it, when the client is built.
fn load_ca_certificate(path: &std::path::Path) -> Result<Certificate> {
    let pem = fs::read(path).map_err(|source| Error::FileIo {
        path: path.to_path_buf(),
        source,
    })?;
    if Certificate::from_pem_bundle(&pem).is_ok_and(|certificates| certificates.is_empty()) {
        return Err(Error::Message(format!(
            "CA certificate file {path:?} contains no certificate; --ca-certificate must name a PEM file with at least one CERTIFICATE block"
        )));
    }
    Certificate::from_pem(&pem).map_err(|source| Error::Http {
        operation: format!("loading CA certificate {path:?}"),
        source,
    })
}

/// A blocking client for transport probes, trusting exactly what the control client trusts.
///
/// Connection reuse is switched off so every request this client makes pays for a fresh DNS
/// lookup, TCP connect, and TLS handshake. That is the opposite of what the control client wants
/// and precisely what a latency measurement needs: a pooled second request would report the cost
/// of an already-open socket and hide the very variance the probe exists to find.
///
/// `request_timeout` is the caller's ceiling, and it is *not* clamped to something small here.
/// It once was -- four seconds -- and against a QuickConnect relay whose unauthenticated
/// discovery request measurably takes 3.8 seconds, every probe sample timed out while the control
/// client on the same host succeeded. A probe that gives up sooner than the client it is meant to
/// explain reports a fault that does not exist; the budget's own ceiling bounds it instead.
pub(crate) fn probe_client(
    options: &ClientOptions,
    request_timeout: Duration,
) -> Result<HttpClient> {
    let timeout = request_timeout.min(control_request_timeout(options.request_timeout));
    let mut builder = crate::blocking_client_builder()?
        .connect_timeout(options.connect_timeout.min(timeout))
        .timeout(timeout)
        .redirect(Policy::none())
        .pool_max_idle_per_host(0)
        .user_agent(concat!("synology-drive-sync/", env!("SDSYNC_VERSION")));
    if options.accept_invalid_certs {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if let Some(path) = &options.ca_certificate {
        builder = builder.add_root_certificate(load_ca_certificate(path)?);
    }
    builder.build().map_err(|source| Error::Http {
        operation: "building transport probe client".to_owned(),
        source,
    })
}

/// The path component of a request URL, for instrumentation.
///
/// Deliberately the path alone: the host is reported once by [`connection_detail`], and a query
/// string is never included because DSM control requests carry their parameters in the body.
fn route_text(url: &Url) -> BoundedText {
    BoundedText::sanitized(url.path())
}

/// The cookie *names* a response set, comma separated, with how many it set.
///
/// Only the text before the first `=` of each `Set-Cookie` header is kept. A cookie value -- which
/// for DSM is a live session identifier -- is dropped with the rest of the header, along with every
/// attribute after the first `;`. The result still passes the record sanitizer.
fn set_cookie_names(headers: &HeaderMap) -> (u16, BoundedText) {
    let mut count = 0_u16;
    let mut names = String::new();
    for header in headers.get_all(SET_COOKIE) {
        count = count.saturating_add(1);
        let Ok(text) = header.to_str() else {
            continue;
        };
        let name = text
            .split(';')
            .next()
            .unwrap_or_default()
            .split('=')
            .next()
            .unwrap_or_default()
            .trim();
        if name.is_empty() {
            continue;
        }
        if !names.is_empty() {
            names.push(',');
        }
        names.push_str(name);
    }
    (count, BoundedText::sanitized(&names))
}

/// Per-process salt for cookie value digests.
///
/// Drawn once at first use from the wall clock and the process id, which is enough that a digest
/// printed by one run cannot be lined up against one printed by another. This is not the reason
/// the digest is safe -- 32 bits folded out of a keyed hash of a forty-character opaque token is
/// not invertible with or without a salt -- it simply removes the question entirely.
fn cookie_fingerprint_salt() -> u64 {
    static SALT: OnceLock<u64> = OnceLock::new();
    *SALT.get_or_init(|| {
        let nanos = UNIX_EPOCH
            .elapsed()
            .map(|elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        nanos.rotate_left(17).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ u64::from(std::process::id()).wrapping_mul(0xff51_afd7_ed55_8ccd)
    })
}

/// A salted digest of a cookie value, for equality comparison and nothing else.
///
/// The value is read, folded, and dropped: no part of it is stored, returned, or recoverable from
/// the result.
fn cookie_fingerprint(value: &str) -> u32 {
    // FNV-1a, seeded with the per-process salt rather than the published offset basis.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ cookie_fingerprint_salt();
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Folded to 32 bits because the report only compares digests for equality, and a short token
    // is what an operator can actually scan down a column of call records.
    (((hash >> 32) ^ hash) & 0xffff_ffff) as u32
}

/// Describe one `Set-Cookie` header: its name, its attributes, and a digest of its value.
///
/// The value is used to compute the digest and its length, and is then dropped. Attribute names
/// are matched case-insensitively, as RFC 6265 requires. `Path` and `Domain` are recorded as
/// presence only, because a `Domain` names the scope an intermediary claims and presence answers
/// the diagnostic question without publishing it.
fn describe_set_cookie(header: &str) -> Option<CookieFact> {
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.trim().split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let value = value.trim();
    let mut fact = CookieFact {
        name: ShortToken::sanitized(name),
        fingerprint: cookie_fingerprint(value),
        value_length: u16::try_from(value.len()).unwrap_or(u16::MAX),
        ..CookieFact::default()
    };
    let mut max_age: Option<i64> = None;
    for attribute in parts {
        let (key, attribute_value) = match attribute.trim().split_once('=') {
            Some((key, attribute_value)) => (key.trim(), attribute_value.trim()),
            None => (attribute.trim(), ""),
        };
        if key.eq_ignore_ascii_case("expires") {
            fact.expires_present = true;
        } else if key.eq_ignore_ascii_case("max-age") {
            fact.max_age_present = true;
            max_age = attribute_value.parse().ok();
        } else if key.eq_ignore_ascii_case("secure") {
            fact.secure = true;
        } else if key.eq_ignore_ascii_case("httponly") {
            fact.http_only = true;
        } else if key.eq_ignore_ascii_case("path") {
            fact.path_present = true;
        } else if key.eq_ignore_ascii_case("domain") {
            fact.domain_present = true;
        } else if key.eq_ignore_ascii_case("samesite") {
            fact.same_site = if attribute_value.eq_ignore_ascii_case("strict") {
                CookieSameSite::Strict
            } else if attribute_value.eq_ignore_ascii_case("lax") {
                CookieSameSite::Lax
            } else if attribute_value.eq_ignore_ascii_case("none") {
                CookieSameSite::None
            } else {
                CookieSameSite::Unrecognized
            };
        }
    }
    fact.persistence = if max_age.is_some_and(|seconds| seconds <= 0)
        || (value.is_empty() && (fact.expires_present || fact.max_age_present))
    {
        CookiePersistence::Deletion
    } else if fact.expires_present || fact.max_age_present {
        CookiePersistence::Persistent
    } else {
        CookiePersistence::Session
    };
    Some(fact)
}

/// Describe every cookie a response set, up to the record's fixed capacity.
pub(crate) fn cookie_facts(headers: &HeaderMap) -> CookieFacts {
    let mut facts = CookieFacts::default();
    for header in headers.get_all(SET_COOKIE) {
        match header.to_str().ok().and_then(describe_set_cookie) {
            Some(fact) => facts.push(fact),
            // A header that is not valid UTF-8, or carries no `name=`, is still evidence that
            // something set a cookie. Counting it keeps the record from claiming to be complete.
            None => facts.push_undescribed(),
        }
    }
    facts
}

/// Recognise a caching or content-delivery intermediary from the headers it adds.
fn cdn_marker(headers: &HeaderMap) -> CdnMarker {
    let present = |name: &str| headers.contains_key(name);
    let server_mentions = |needle: &str| {
        headers
            .get("server")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains(needle))
    };
    if present("cf-ray") || present("cf-cache-status") || server_mentions("cloudflare") {
        CdnMarker::Cloudflare
    } else if present("x-amz-cf-id") || present("x-amz-cf-pop") || server_mentions("cloudfront") {
        CdnMarker::CloudFront
    } else if present("x-served-by") || present("fastly-io-info") || server_mentions("fastly") {
        CdnMarker::Fastly
    } else if present("x-akamai-transformed") || present("akamai-grn") {
        CdnMarker::Akamai
    } else if present("x-varnish") || server_mentions("varnish") {
        CdnMarker::Varnish
    } else if present("x-cache") || present("x-cache-hits") || present("x-proxy-cache") {
        CdnMarker::Other
    } else {
        CdnMarker::None
    }
}

/// How many cookies a response set under a name DSM itself never uses.
///
/// A load balancer's affinity cookie is the clearest single sign that requests are being pinned
/// -- or, when it is absent from a relay, that nothing is pinning them at all.
fn foreign_cookie_count(headers: &HeaderMap) -> u16 {
    let mut count = 0_u16;
    for header in headers.get_all(SET_COOKIE) {
        let Ok(text) = header.to_str() else {
            continue;
        };
        let name = text
            .split(';')
            .next()
            .unwrap_or_default()
            .split('=')
            .next()
            .unwrap_or_default()
            .trim();
        if !name.is_empty()
            && !DSM_COOKIE_NAMES
                .iter()
                .any(|known| known.eq_ignore_ascii_case(name))
        {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Fingerprint the machinery between this client and DSM from one response's headers.
///
/// Banner values are kept, bounded and sanitized: naming the proxy is the entire point of the
/// check, and the same records already name a redirect's host. Nothing here reads a cookie value,
/// a body, or a request header.
pub(crate) fn intermediary_facts(headers: &HeaderMap) -> IntermediaryFacts {
    let banner = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(ShortToken::sanitized)
            .unwrap_or_default()
    };
    IntermediaryFacts {
        via: banner("via"),
        server: banner("server"),
        powered_by: banner("x-powered-by"),
        // These are request headers. Meeting one in a *response* means something on the path
        // echoed it back, which only a proxy that rewrites them does.
        forwarded_for_reflected: headers.contains_key("x-forwarded-for")
            || headers.contains_key("forwarded"),
        real_ip_reflected: headers.contains_key("x-real-ip") || headers.contains_key("x-client-ip"),
        cdn_marker: cdn_marker(headers),
        foreign_cookie_count: foreign_cookie_count(headers),
    }
}

/// The host of a redirect target, or `None` when the target is relative.
///
/// Only the host is retained. A relay's `Location` can carry query parameters, and those are
/// exactly the sort of reflected request state this crate withholds everywhere else.
fn redirect_host(location: &str) -> Option<BoundedText> {
    Url::parse(location)
        .ok()
        .and_then(|url| url.host_str().map(BoundedText::sanitized))
}

/// Fold the pre-body response facts into the record that will be reported.
fn apply_response_facts(record: &mut ApiCallDetail, facts: ResponseFacts) {
    record.http_status = facts.http_status;
    record.set_cookie_count = facts.set_cookie_count;
    record.set_cookie_names = facts.set_cookie_names;
    record.cookies = facts.cookies;
    record.intermediary = facts.intermediary;
    record.redirect_host = facts.redirect_host;
    record.response_bytes = facts.response_bytes;
    record.decode = facts.decode;
}

/// Fold a decoded result into the record that will be reported.
fn apply_outcome<T>(record: &mut ApiCallDetail, result: &Result<T>, api: &str) {
    let (outcome, dsm_code, dsm_description) = classify_outcome(result, api);
    record.outcome = outcome;
    record.dsm_code = dsm_code;
    record.dsm_description = dsm_description;
}

/// Classify a decoded response for instrumentation.
///
/// The DSM description is re-derived from [`api_error_description`] rather than parsed out of
/// `Error::Api`'s pre-formatted string, so the record holds a compile-time literal.
fn classify_outcome<T>(
    result: &Result<T>,
    api: &str,
) -> (RequestOutcome, Option<i64>, Option<&'static str>) {
    match result {
        Ok(_) => (RequestOutcome::Ok, None, None),
        Err(Error::Api { code, .. }) => (
            RequestOutcome::DsmError,
            Some(*code),
            api_error_description(api, *code),
        ),
        Err(Error::HttpStatus { status, .. }) => (
            if status.is_redirection() {
                RequestOutcome::Redirect
            } else {
                RequestOutcome::HttpStatus
            },
            None,
            None,
        ),
        Err(Error::InvalidResponse { .. }) => (RequestOutcome::Decode, None, None),
        Err(_) => (RequestOutcome::Transport, None, None),
    }
}

fn endpoint_url(base: &Url, discovered_path: &str) -> Result<Url> {
    if discovered_path.is_empty()
        || discovered_path.starts_with('/')
        || discovered_path.contains('\\')
        || discovered_path.contains('?')
        || discovered_path.contains('#')
        || discovered_path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(Error::InvalidResponse {
            operation: "API endpoint discovery".to_owned(),
            message: format!("server returned unsafe CGI path {discovered_path:?}"),
        });
    }
    let webapi = base
        .join("webapi/")
        .map_err(|error| Error::InvalidUrl(error.to_string()))?;
    let endpoint = webapi
        .join(discovered_path)
        .map_err(|error| Error::InvalidUrl(error.to_string()))?;
    if endpoint.origin() != base.origin() || !endpoint.path().starts_with(webapi.path()) {
        return Err(Error::InvalidResponse {
            operation: "API endpoint discovery".to_owned(),
            message: format!("server returned escaping CGI path {discovered_path:?}"),
        });
    }
    Ok(endpoint)
}

struct ObservedReader<R> {
    inner: R,
    observer: Option<UploadObserver>,
    cancelled: Arc<AtomicBool>,
    /// `None` leaves the reader on the unmetered path it has always taken.
    throttle: Option<UploadThrottle>,
}

/// A token bucket written as a pure state machine: every operation is handed the current
/// instant instead of reading the clock itself, so refill, burst and starvation are all
/// testable without a single sleep.
///
/// Tokens are stored scaled by [`NANOS_PER_SECOND`], which makes the arithmetic exact: one byte
/// costs `NANOS_PER_SECOND` scaled units and one elapsed nanosecond mints `bytes_per_second` of
/// them. Nothing is rounded away, so a slow trickle still accumulates into whole bytes.
#[derive(Debug)]
struct TokenBucket {
    bytes_per_second: u64,
    capacity_scaled: u128,
    available_scaled: u128,
    updated: Instant,
}

/// What the bucket permits a reader to do right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RateGrant {
    /// Read at most this many bytes immediately.
    Ready(u64),
    /// Nothing is available yet; wait no longer than this and ask again.
    Wait(Duration),
}

impl TokenBucket {
    /// The bucket starts full and holds one second of traffic. That burst size is also what
    /// bounds every wait it can report: a caller is never queued for more than a full bucket,
    /// so `Wait` never exceeds one second no matter how large the read or how small the rate.
    fn new(bytes_per_second: NonZeroU64, now: Instant) -> Self {
        let capacity_scaled = u128::from(bytes_per_second.get()) * NANOS_PER_SECOND;
        Self {
            bytes_per_second: bytes_per_second.get(),
            capacity_scaled,
            available_scaled: capacity_scaled,
            updated: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_nanos();
        if elapsed == 0 {
            return;
        }
        self.updated = now;
        let minted = elapsed.saturating_mul(u128::from(self.bytes_per_second));
        self.available_scaled = self
            .available_scaled
            .saturating_add(minted)
            .min(self.capacity_scaled);
    }

    /// Hand out as much of `requested` as the budget currently allows, or report how long the
    /// caller should wait before asking again. A grant is deducted immediately, so two workers
    /// sharing a bucket cannot both spend the same byte.
    fn take(&mut self, requested: u64, now: Instant) -> RateGrant {
        self.refill(now);
        // Never queue for more than one bucket's worth; that is what keeps waits short.
        let wanted = requested.min(self.bytes_per_second);
        let available = u64::try_from(self.available_scaled / NANOS_PER_SECOND).unwrap_or(u64::MAX);
        if wanted == 0 || available > 0 {
            let granted = available.min(wanted);
            self.available_scaled -= u128::from(granted) * NANOS_PER_SECOND;
            return RateGrant::Ready(granted);
        }
        let shortfall = u128::from(wanted) * NANOS_PER_SECOND - self.available_scaled;
        let nanos = shortfall.div_ceil(u128::from(self.bytes_per_second));
        RateGrant::Wait(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }
}

/// Upload pacing for one transfer: the byte budget shared with every other worker, plus the
/// token that lets a waiting reader give up.
#[derive(Clone)]
struct UploadThrottle {
    bucket: Arc<Mutex<TokenBucket>>,
    cancellation: CancellationToken,
}

impl UploadThrottle {
    /// Block until the shared budget allows at least one byte, then report how much of
    /// `requested` may be read now.
    ///
    /// The wait is served in [`RATE_LIMIT_POLL_INTERVAL`] slices rather than one long sleep, so
    /// a cancelled transfer is abandoned within that interval however slow the limit is. The
    /// bucket lock is deliberately released before each sleep; holding it would serialise every
    /// other worker behind this one.
    fn claim(&self, requested: usize) -> std::io::Result<usize> {
        loop {
            if self.cancellation.is_cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "upload cancelled",
                ));
            }
            let grant = {
                let mut bucket = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
                bucket.take(u64::try_from(requested).unwrap_or(u64::MAX), Instant::now())
            };
            match grant {
                RateGrant::Ready(allowance) => {
                    return Ok(usize::try_from(allowance).unwrap_or(requested));
                }
                RateGrant::Wait(remaining) => {
                    thread::sleep(remaining.min(RATE_LIMIT_POLL_INTERVAL));
                }
            }
        }
    }
}

fn upload_rate_bucket(bytes_per_second: Option<u64>) -> Option<Arc<Mutex<TokenBucket>>> {
    bytes_per_second
        .and_then(NonZeroU64::new)
        .map(|rate| Arc::new(Mutex::new(TokenBucket::new(rate, Instant::now()))))
}

fn upload_throttle(
    bucket: Option<&Arc<Mutex<TokenBucket>>>,
    cancellation: &CancellationToken,
) -> Option<UploadThrottle> {
    bucket.map(|bucket| UploadThrottle {
        bucket: Arc::clone(bucket),
        cancellation: cancellation.clone(),
    })
}

struct ProbeCleanup {
    completed: bool,
    leftover_remote_probe_path: Option<String>,
    error: Option<Error>,
}

impl ProbeCleanup {
    fn not_needed() -> Self {
        Self {
            completed: true,
            leftover_remote_probe_path: None,
            error: None,
        }
    }
}

struct ProbeLocalFile {
    entry: LocalEntry,
}

impl ProbeLocalFile {
    fn create(content_md5: ContentMd5) -> Result<Self> {
        for _ in 0..16 {
            let path = std::env::temp_dir().join(format!("{}.bin", write_probe_name()));
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(Error::FileIo { path, source }),
            };
            if let Err(source) = file
                .write_all(WRITE_PROBE_PAYLOAD)
                .and_then(|()| file.sync_all())
            {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(Error::FileIo { path, source });
            }
            drop(file);

            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(source) => {
                    let _ = fs::remove_file(&path);
                    return Err(Error::FileIo { path, source });
                }
            };
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(source) => {
                    let _ = fs::remove_file(&path);
                    return Err(Error::FileIo { path, source });
                }
            };
            let Some(mtime_ms) = modified
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            else {
                let _ = fs::remove_file(&path);
                return Err(Error::Message(format!(
                    "write-probe temporary file has an unsupported modification time: {path:?}"
                )));
            };
            return Ok(Self {
                entry: LocalEntry {
                    relative: WRITE_PROBE_FILE_NAME.to_owned(),
                    full_path: path,
                    kind: EntryKind::File,
                    size: metadata.len(),
                    mtime_ms,
                    content_md5: Some(content_md5),
                },
            });
        }
        Err(Error::Message(
            "could not allocate a unique local write-probe file".to_owned(),
        ))
    }
}

impl Drop for ProbeLocalFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.entry.full_path);
    }
}

fn initial_write_probe_report(
    root: &RemoteRoot,
    probe_path: String,
    uploaded_size: u64,
    uploaded_md5: ContentMd5,
    uploaded_mtime_seconds: i64,
    server_copy_supported: bool,
) -> WriteProbeReport {
    WriteProbeReport {
        target_path: root.as_str().to_owned(),
        probe_path,
        target_verified: false,
        directory_created: false,
        upload_attempted: false,
        upload_verified: false,
        uploaded_size,
        uploaded_md5,
        uploaded_mtime_seconds,
        server_copy_supported,
        server_copy_attempted: false,
        server_copy_verified: false,
        cleanup_completed: false,
        leftover_remote_probe_path: None,
    }
}

fn write_probe_name() -> String {
    let nanos = UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = WRITE_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        ".synology-drive-sync-probe-{}-{nanos}-{sequence}",
        std::process::id()
    )
}

fn write_probe_fingerprint() -> ContentMd5 {
    ContentMd5::from_content(WRITE_PROBE_PAYLOAD)
}

fn content_evidence_matches(expected: ContentMd5, actual: ContentMd5) -> bool {
    expected.full_match(&actual) == Some(true)
}

impl<R: Read> Read for ObservedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        // An unlimited upload takes exactly the path it always has: one full-buffer read.
        // A limited one is metered by shortening the read rather than by sleeping on bytes it
        // has already taken, which keeps the pacing smooth and every wait bounded.
        let count = match self.throttle.as_ref() {
            Some(throttle) => match throttle.claim(buffer.len()) {
                Ok(allowance) => self.inner.read(&mut buffer[..allowance])?,
                Err(error) => {
                    self.cancelled.store(true, Ordering::Release);
                    return Err(error);
                }
            },
            None => self.inner.read(buffer)?,
        };
        if count > 0
            && let Some(observer) = &self.observer
            && !observer(UploadTransferEvent::Advanced {
                bytes: count as u64,
            })
        {
            self.cancelled.store(true, Ordering::Release);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "upload cancelled",
            ));
        }
        Ok(count)
    }
}

fn prioritize_observer_cancellation(cancelled: &AtomicBool, result: Result<()>) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(Error::Cancelled)
    } else {
        result
    }
}

fn verify_local_snapshot(local: &LocalEntry) -> Result<()> {
    let metadata = fs::symlink_metadata(&local.full_path).map_err(|source| Error::FileIo {
        path: local.full_path.clone(),
        source,
    })?;
    let modified = metadata.modified().map_err(|source| Error::FileIo {
        path: local.full_path.clone(),
        source,
    })?;
    let millis = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() != local.size
        || millis != Some(local.mtime_ms)
    {
        return Err(Error::SourceChanged(local.full_path.clone()));
    }
    Ok(())
}

fn validate_delete_target(root: &RemoteRoot, remote_path: &str) -> Result<()> {
    let relative = root
        .relative(remote_path)
        .map_err(|_| Error::UnsafeRemotePath {
            path: remote_path.to_owned(),
            reason: "delete target must be a normalized strict child of the configured destination"
                .to_owned(),
        })?;
    if relative.is_empty() {
        return Err(Error::UnsafeRemotePath {
            path: remote_path.to_owned(),
            reason: "the configured destination itself cannot be deleted".to_owned(),
        });
    }
    Ok(())
}

fn validate_mutation_target(root: &RemoteRoot, remote_path: &str) -> Result<()> {
    validate_delete_target(root, remote_path)
}

fn validate_task_id(taskid: &str, operation: &str) -> Result<()> {
    if taskid.is_empty() || taskid.len() > 1024 || taskid.chars().any(char::is_control) {
        return Err(Error::InvalidResponse {
            operation: operation.to_owned(),
            message: "server returned an invalid task ID".to_owned(),
        });
    }
    Ok(())
}

fn sleep_cancellable(duration: Duration, cancellation: &CancellationToken) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(duration)
        .ok_or_else(|| Error::Message("sleep duration is too large".to_owned()))?;
    while Instant::now() < deadline {
        cancellation.check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(25)));
    }
    cancellation.check()
}

fn absolute_prefixes(path: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    for component in path.trim_start_matches('/').split('/') {
        current.push('/');
        current.push_str(component);
        output.push(current.clone());
    }
    output
}

fn permission_probe_name() -> String {
    // CheckPermission never creates this name. Process and wall-clock entropy make an accidental
    // collision with an existing child vanishingly unlikely without adding an RNG dependency or
    // exposing credential material.
    let nanos = UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        ".synology-drive-sync-write-check-{}-{nanos}",
        std::process::id()
    )
}

fn verify_open_file_snapshot(local: &LocalEntry, file: &File) -> Result<()> {
    let metadata = file.metadata().map_err(|source| Error::FileIo {
        path: local.full_path.clone(),
        source,
    })?;
    let modified = metadata.modified().map_err(|source| Error::FileIo {
        path: local.full_path.clone(),
        source,
    })?;
    let millis = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    if !metadata.is_file() || metadata.len() != local.size || millis != Some(local.mtime_ms) {
        return Err(Error::SourceChanged(local.full_path.clone()));
    }
    Ok(())
}

fn json_string(value: &str) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|error| Error::Message(format!("failed to JSON-encode API parameter: {error}")))
}

fn json_array<'a>(values: impl IntoIterator<Item = &'a str>) -> Result<String> {
    serde_json::to_string(&values.into_iter().collect::<Vec<_>>())
        .map_err(|error| Error::Message(format!("failed to JSON-encode API parameter: {error}")))
}

/// Encode values as the JSON array File Station's list-shaped parameters expect.
///
/// Exposed so a caller assembling a [`CapabilityProbeSpec`] encodes a `path` exactly the way
/// every other call site does, rather than hand-building a string that quotes differently.
pub fn json_array_of<'a>(values: impl IntoIterator<Item = &'a str>) -> Result<String> {
    json_array(values)
}

fn pair(key: impl Into<String>, value: impl Into<String>) -> (String, String) {
    (key.into(), value.into())
}

fn valid_cookie_value(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            matches!(
                byte,
                0x21 | 0x23..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E
            )
        })
}

fn invalid_session_header(component: &str) -> Error {
    Error::InvalidResponse {
        operation: "SYNO.API.Auth.login".to_owned(),
        message: format!(
            "successful response contained {component} data that cannot be used safely for authenticated DSM requests"
        ),
    }
}

fn discovery_route_returned_response(error: &Error) -> bool {
    matches!(
        error,
        Error::HttpBody { .. }
            | Error::HttpStatus { .. }
            | Error::InvalidResponse { .. }
            | Error::Api { .. }
    )
}

fn retryable(error: &Error) -> bool {
    match error {
        Error::Http { source, .. } => {
            source.is_connect() || source.is_timeout() || source.is_body()
        }
        Error::HttpBody { .. } => true,
        Error::HttpStatus { status, .. } => {
            matches!(status.as_u16(), 408 | 425 | 429 | 502 | 503 | 504)
        }
        Error::Api { code, .. } => matches!(*code, 109..=111 | 117..=118 | 402),
        _ => false,
    }
}

fn copy_start_error(error: Error) -> Error {
    let deterministic_rejection =
        matches!(
            &error,
            Error::MissingApi(_) | Error::UnsupportedApiVersion { .. }
        ) || matches!(error.api_code(), Some(102 | 103 | 104 | 105 | 407 | 409));
    if deterministic_rejection {
        Error::ServerCopyNotStarted
    } else {
        error
    }
}

fn control_request_timeout(upload_timeout: Duration) -> Duration {
    upload_timeout.min(MAX_CONTROL_REQUEST_TIMEOUT)
}

/// The backoff for one retry attempt.
///
/// Extracted from the pause itself so instrumentation can report the wait that is about to happen
/// without duplicating the schedule.
fn retry_backoff(attempt: u32) -> Duration {
    let multiplier = 1_u64 << attempt.min(4);
    Duration::from_millis(250 * multiplier)
}

fn retry_pause_cancellable(attempt: u32, cancellation: &CancellationToken) -> Result<()> {
    sleep_cancellable(retry_backoff(attempt), cancellation)
}

fn looks_like_html(body: &[u8]) -> bool {
    let prefix = String::from_utf8_lossy(&body[..body.len().min(256)]).to_ascii_lowercase();
    prefix.contains("<!doctype html") || prefix.contains("<html")
}

fn response_snippet(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().min(512)])
        .trim()
        .chars()
        .flat_map(char::escape_default)
        .collect()
}

/// Redirects are refused so credentials cannot cross an origin.
pub const REDIRECT_REFUSED_HINT: &str = "redirects are disabled to prevent credentials crossing origins; expose /webapi/* directly at the configured HTTPS URL";
/// The proxy rejected the body before File Station saw it.
pub const BODY_TOO_LARGE_HINT: &str =
    "request body is larger than the reverse proxy permits; raise its upload/body-size limit";
/// The proxy answered, but could not reach DSM behind it.
pub const BAD_GATEWAY_HINT: &str = "reverse proxy could not reach the File Station backend";
/// The proxy gave up before DSM answered.
pub const GATEWAY_TIMEOUT_HINT: &str =
    "reverse proxy timed out; raise its send/read timeout for large uploads";

fn http_status_hint(status: StatusCode, body: &[u8]) -> String {
    match status.as_u16() {
        301 | 302 | 303 | 307 | 308 => REDIRECT_REFUSED_HINT.to_owned(),
        413 => BODY_TOO_LARGE_HINT.to_owned(),
        502 => BAD_GATEWAY_HINT.to_owned(),
        504 => GATEWAY_TIMEOUT_HINT.to_owned(),
        _ => {
            let snippet = response_snippet(body);
            if snippet.is_empty() {
                "empty response body".to_owned()
            } else {
                snippet
            }
        }
    }
}

fn api_error_description(api: &str, code: i64) -> Option<&'static str> {
    if api == "SYNO.API.Auth" {
        return match code {
            400 => Some("account does not exist or password is incorrect"),
            401 => Some("account is disabled"),
            402 => Some("account is not permitted to sign in"),
            403 => Some("two-factor OTP is required"),
            404 => Some("two-factor OTP is invalid or expired"),
            406 => Some("two-factor OTP is enforced"),
            407 => Some("source IP is blocked"),
            408 | 409 => Some("password has expired"),
            410 => Some("password must be changed"),
            _ => None,
        };
    }
    match code {
        100 => Some("unknown error"),
        101 => Some("missing API, method, or version parameter"),
        102 => Some("requested API does not exist"),
        103 => Some("requested method does not exist"),
        104 => Some("requested API version is unsupported"),
        105 => Some("session does not have permission"),
        106 => Some("session timed out; rerun to authenticate again"),
        107 => Some("session was interrupted by a duplicate login"),
        119 => Some("session is invalid; rerun to authenticate again"),
        150 => Some("request source IP differs from login IP; fix reverse-proxy routing"),
        400 => Some("invalid file-operation parameter"),
        402 => Some("file subsystem is busy"),
        407 => Some("operation is not permitted"),
        408 => Some("remote file or directory does not exist"),
        411 => Some("remote filesystem is read-only"),
        414 => Some("remote item already exists"),
        415 => Some("disk quota exceeded"),
        416 => Some("no space left on the device"),
        417 => Some("remote input/output error"),
        418 => Some("illegal remote name or path"),
        421 => Some("remote resource is busy"),
        900 => Some("delete failed"),
        1100 => Some("folder creation failed"),
        1101 => Some("parent folder item-count limit exceeded"),
        1800 => Some("upload Content-Length is missing or mismatched"),
        1801 => Some("upload receive timeout"),
        1802 => Some("upload file part has no filename"),
        1803 => Some("upload was cancelled"),
        1804 => Some("file is too large for the destination filesystem"),
        1805 => Some("upload overwrite/skip policy is missing"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::thread::JoinHandle;
    use std::time::SystemTime;

    use super::*;
    use crate::observability::MAX_DESCRIBED_COOKIES;

    const SCRIPTED_SERVER_TIMEOUT: Duration = Duration::from_secs(5);

    /// Reporting that DSM rotated the session must never report *what it rotated it to*.
    ///
    /// A `Set-Cookie` value is a live session identifier, so this pins the split: names before the
    /// first `=` are kept, and everything from that `=` onwards is dropped with the attributes.
    #[test]
    fn set_cookie_reporting_keeps_names_and_drops_every_value() {
        let secret = "9c7f1d2e5b8a4f60ab3d";
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_str(&format!(
                "id={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age=3600"
            ))
            .unwrap(),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_str("stay_login=1; Path=/webapi").unwrap(),
        );

        let (count, names) = set_cookie_names(&headers);
        assert_eq!(count, 2);
        assert_eq!(names.as_str(), "id,stay_login");
        // The decisive assertion: neither the value nor any attribute survives.
        for leaked in [secret, "Path", "HttpOnly", "SameSite", "Max-Age", "="] {
            assert!(
                !names.as_str().contains(leaked),
                "cookie reporting leaked {leaked:?}: {names}"
            );
        }

        // A response that sets nothing reports nothing.
        let (count, names) = set_cookie_names(&HeaderMap::new());
        assert_eq!(count, 0);
        assert!(names.is_empty());

        // A malformed header still cannot smuggle a value through.
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_str("  spaced_name  =  value=with=equals  ").unwrap(),
        );
        let (count, names) = set_cookie_names(&headers);
        assert_eq!(count, 1);
        assert_eq!(names.as_str(), "spaced_name");
    }

    /// The described-cookie record must carry every attribute and none of the value.
    #[test]
    fn a_described_cookie_carries_its_attributes_and_never_its_value() {
        let secret = "hgU9_TnMzBqXwLpR7vKd2eFsA4Yj6Nc1QoZi8Wm3Xb5";
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_str(&format!(
                "id={secret}; Path=/; HttpOnly; Secure; SameSite=Lax; Domain=nas.example.test"
            ))
            .unwrap(),
        );

        let facts = cookie_facts(&headers);
        let described = facts.described().collect::<Vec<_>>();
        assert_eq!(described.len(), 1);
        let cookie = described[0];
        assert_eq!(cookie.name.as_str(), "id");
        assert!(cookie.secure);
        assert!(cookie.http_only);
        assert_eq!(cookie.same_site, CookieSameSite::Lax);
        assert!(cookie.path_present);
        assert!(cookie.domain_present);
        assert!(!cookie.expires_present);
        assert!(!cookie.max_age_present);
        // No expiry attribute at all: a session cookie, not a stored one.
        assert_eq!(cookie.persistence, CookiePersistence::Session);
        assert_eq!(usize::from(cookie.value_length), secret.len());

        // The decisive assertion: nothing derived from the value is recoverable from the record,
        // including through its own `Debug`, which the error paths could otherwise print.
        let rendered = format!("{cookie:?}");
        assert!(
            !rendered.contains(secret),
            "described cookie leaked its value: {rendered}"
        );
        for fragment in [&secret[..8], "nas.example.test"] {
            assert!(
                !rendered.contains(fragment),
                "described cookie leaked {fragment:?}: {rendered}"
            );
        }

        // The same value hashes the same way within a process, and a different value does not.
        assert_eq!(cookie_fingerprint(secret), cookie_fingerprint(secret));
        assert_ne!(
            cookie_fingerprint(secret),
            cookie_fingerprint("a-different-value")
        );
    }

    #[test]
    fn expiry_attributes_separate_session_persistent_and_cleared_cookies() {
        let described = |header: &str| {
            let mut headers = HeaderMap::new();
            headers.append(SET_COOKIE, HeaderValue::from_str(header).unwrap());
            cookie_facts(&headers)
                .described()
                .next()
                .expect("one described cookie")
        };

        assert_eq!(
            described("id=abc; Path=/").persistence,
            CookiePersistence::Session
        );
        assert_eq!(
            described("stay_login=1; Max-Age=604800").persistence,
            CookiePersistence::Persistent
        );
        assert_eq!(
            described("stay_login=1; Expires=Wed, 09 Jun 2027 10:18:14 GMT").persistence,
            CookiePersistence::Persistent
        );
        // DSM clears a session on logout with either shape, and both are a deletion rather
        // than a rotation.
        assert_eq!(
            described("id=; Max-Age=0; Path=/").persistence,
            CookiePersistence::Deletion
        );
        assert_eq!(
            described("id=; Expires=Thu, 01 Jan 1970 00:00:00 GMT").persistence,
            CookiePersistence::Deletion
        );
        // Attribute names are case-insensitive, and SameSite is a closed set.
        let mixed = described("id=abc; secure; HTTPONLY; samesite=STRICT");
        assert!(mixed.secure);
        assert!(mixed.http_only);
        assert_eq!(mixed.same_site, CookieSameSite::Strict);
        assert_eq!(
            described("id=abc; SameSite=Weird").same_site,
            CookieSameSite::Unrecognized
        );

        // A header with no `name=` at all is counted rather than described, so the record never
        // claims a response set fewer cookies than it did.
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, HeaderValue::from_str("not-a-cookie").unwrap());
        let facts = cookie_facts(&headers);
        assert_eq!(facts.described().count(), 0);
        assert_eq!(facts.undescribed(), 1);
    }

    #[test]
    fn response_headers_fingerprint_the_machinery_in_front_of_dsm() {
        let mut headers = HeaderMap::new();
        headers.insert("via", HeaderValue::from_static("1.1 relay.example.test"));
        headers.insert("server", HeaderValue::from_static("nginx/1.24.0"));
        headers.insert("x-powered-by", HeaderValue::from_static("PHP/8.2.1"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        headers.insert("x-real-ip", HeaderValue::from_static("203.0.113.7"));
        headers.insert("cf-ray", HeaderValue::from_static("8b0e0000abcd-CDG"));
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("id=session-value; Path=/"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("AWSALB=affinity-value; Path=/"),
        );

        let facts = intermediary_facts(&headers);
        // Sanitized, so the space in the Via chain is substituted rather than dropped.
        assert_eq!(facts.via.as_str(), "1.1_relay.example.test");
        assert_eq!(facts.server.as_str(), "nginx/1.24.0");
        assert_eq!(facts.powered_by.as_str(), "PHP/8.2.1");
        assert!(facts.forwarded_for_reflected);
        assert!(facts.real_ip_reflected);
        assert_eq!(facts.cdn_marker, CdnMarker::Cloudflare);
        // `id` is DSM's own; the load balancer's affinity cookie is not.
        assert_eq!(facts.foreign_cookie_count, 1);

        // Nothing announced means nothing is claimed.
        let bare = intermediary_facts(&HeaderMap::new());
        assert!(bare.is_empty());
        assert_eq!(bare.cdn_marker, CdnMarker::None);

        assert!(is_dsm_cookie_name("id"));
        assert!(is_dsm_cookie_name("STAY_LOGIN"));
        assert!(!is_dsm_cookie_name("AWSALB"));
    }

    /// Only the described capacity is described; the rest is counted, never dropped in silence.
    #[test]
    fn cookies_beyond_the_record_capacity_are_counted_rather_than_lost() {
        let mut headers = HeaderMap::new();
        for index in 0..(MAX_DESCRIBED_COOKIES + 2) {
            headers.append(
                SET_COOKIE,
                HeaderValue::from_str(&format!("cookie{index}=value{index}; Path=/")).unwrap(),
            );
        }
        let facts = cookie_facts(&headers);
        assert_eq!(facts.described().count(), MAX_DESCRIBED_COOKIES);
        assert_eq!(usize::from(facts.undescribed()), 2);
    }

    #[derive(Debug)]
    struct CapturedRequest {
        request_line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    fn scripted_server(responses: Vec<String>) -> (String, JoinHandle<Vec<CapturedRequest>>) {
        scripted_server_with_status(
            responses
                .into_iter()
                .map(|body| (StatusCode::OK, body))
                .collect(),
        )
    }

    fn scripted_server_with_status(
        responses: Vec<(StatusCode, String)>,
    ) -> (String, JoinHandle<Vec<CapturedRequest>>) {
        scripted_server_with_status_hook(responses, |_| {})
    }

    /// Run `before_response(index)` on the server thread immediately *before* response `index` is
    /// written.
    ///
    /// The hook must not run after the write. Tests use it to trip a cancellation token, and the
    /// client checks that token as soon as it has parsed the response -- so a hook that fired
    /// after the write would race the client and only usually win. Writing the response after the
    /// hook gives the side effect a happens-before edge to everything the client does with that
    /// response, which is also the honest scenario: the operator cancelled while the response was
    /// still in flight.
    fn scripted_server_with_status_hook<F>(
        responses: Vec<(StatusCode, String)>,
        mut before_response: F,
    ) -> (String, JoinHandle<Vec<CapturedRequest>>)
    where
        F: FnMut(usize) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (index, (status, response_body)) in responses.into_iter().enumerate() {
                let mut stream = accept_scripted_connection(&listener, index);
                requests.push(read_scripted_request(&mut stream, index));
                before_response(index);
                write_scripted_response(&mut stream, status, &response_body);
            }
            requests
        });
        (format!("http://{address}/prefix/"), handle)
    }

    fn scripted_download_server_with_hook<F>(
        responses: Vec<(StatusCode, &'static str, Vec<u8>)>,
        mut before_response: F,
    ) -> (String, JoinHandle<Vec<CapturedRequest>>)
    where
        F: FnMut(usize) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (index, (status, content_type, body)) in responses.into_iter().enumerate() {
                let mut stream = accept_scripted_connection(&listener, index);
                requests.push(read_scripted_request(&mut stream, index));
                before_response(index);
                write_scripted_bytes_response(&mut stream, status, content_type, &body);
            }
            requests
        });
        (format!("http://{address}/prefix/"), handle)
    }

    fn scripted_slow_download_server(
        body: Vec<u8>,
        initial_delay: Duration,
        inter_byte_delay: Duration,
    ) -> (String, JoinHandle<Vec<CapturedRequest>>) {
        scripted_slow_download_server_with_hook(body, initial_delay, inter_byte_delay, || {})
    }

    fn scripted_slow_download_server_with_hook<F>(
        body: Vec<u8>,
        initial_delay: Duration,
        inter_byte_delay: Duration,
        before_body: F,
    ) -> (String, JoinHandle<Vec<CapturedRequest>>)
    where
        F: FnOnce() + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_nodelay(true).unwrap();
            stream
                .set_write_timeout(Some(SCRIPTED_SERVER_TIMEOUT))
                .unwrap();
            let request = read_scripted_request(&mut stream, 0);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.flush().unwrap();
            before_body();
            thread::sleep(initial_delay);
            for byte in body {
                if stream.write_all(&[byte]).is_err() || stream.flush().is_err() {
                    break;
                }
                thread::sleep(inter_byte_delay);
            }
            vec![request]
        });
        (format!("http://{address}/prefix/"), handle)
    }

    fn scripted_delayed_download_headers_server(
        delay: Duration,
    ) -> (String, JoinHandle<Vec<CapturedRequest>>) {
        scripted_delayed_download_headers_server_with_hook(delay, || {})
    }

    fn scripted_delayed_download_headers_server_with_hook<F>(
        delay: Duration,
        before_headers: F,
    ) -> (String, JoinHandle<Vec<CapturedRequest>>)
    where
        F: FnOnce() + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_write_timeout(Some(SCRIPTED_SERVER_TIMEOUT))
                .unwrap();
            let request = read_scripted_request(&mut stream, 0);
            before_headers();
            thread::sleep(delay);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.flush();
            vec![request]
        });
        (format!("http://{address}/prefix/"), handle)
    }

    fn fingerprint_test_client_with_timeouts(
        base_url: &str,
        retries: u32,
        idle_timeout: Duration,
        operation_timeout: Duration,
    ) -> ApiClient {
        let http = crate::blocking_client_builder()
            .expect("ring provider installs")
            .timeout(idle_timeout.min(operation_timeout))
            .redirect(Policy::none())
            .build()
            .unwrap();
        let download_http = crate::async_client_builder()
            .expect("ring provider installs")
            .connect_timeout(idle_timeout.min(operation_timeout))
            .read_timeout(idle_timeout.min(operation_timeout))
            .redirect(Policy::none())
            .build()
            .unwrap();
        ApiClient {
            http,
            download_http,
            base: normalize_base_url(base_url, true).unwrap(),
            apis: HashMap::from([(
                "SYNO.FileStation.Download".to_owned(),
                ApiSpec {
                    path: "entry.cgi".to_owned(),
                    min_version: 1,
                    max_version: 2,
                    _request_format: None,
                },
            )]),
            session: Some(Session {
                sid: Zeroizing::new("download-test-session".to_owned()),
                syno_token: Some(Zeroizing::new("download-test-token".to_owned())),
            }),
            retries,
            control_timeout: idle_timeout,
            control_deadline: None,
            upload_timeout: operation_timeout,
            operation_timeout,
            upload_rate_limit: None,
            cancellation: CancellationToken::default(),
            observer: None,
        }
    }

    fn fingerprint_test_client(base_url: &str, retries: u32) -> ApiClient {
        fingerprint_test_client_with_timeouts(
            base_url,
            retries,
            Duration::from_secs(1),
            Duration::from_secs(2),
        )
    }

    fn scripted_server_monitoring_extra_requests(
        response: String,
    ) -> (
        String,
        std::sync::mpsc::Sender<()>,
        JoinHandle<Vec<CapturedRequest>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (done_send, done_receive) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let mut stream = accept_scripted_connection(&listener, 0);
            requests.push(read_scripted_request(&mut stream, 0));
            write_scripted_response(&mut stream, StatusCode::OK, &response);

            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let index = requests.len();
                        requests.push(read_scripted_request(&mut stream, index));
                        write_scripted_response(
                            &mut stream,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            r#"{"success":false,"error":{"code":500}}"#,
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        match done_receive.recv_timeout(Duration::from_millis(1)) {
                            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        }
                    }
                    Err(error) => panic!("scripted server failed while monitoring: {error}"),
                }
            }
            requests
        });
        (format!("http://{address}/prefix/"), done_send, handle)
    }

    fn accept_scripted_connection(listener: &TcpListener, index: usize) -> TcpStream {
        let deadline = Instant::now() + SCRIPTED_SERVER_TIMEOUT;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for scripted request {index}"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("failed to accept scripted request {index}: {error}"),
            }
        }
    }

    fn read_scripted_request(stream: &mut TcpStream, index: usize) -> CapturedRequest {
        let deadline = Instant::now() + SCRIPTED_SERVER_TIMEOUT;
        let mut received = Vec::new();
        let header_end = loop {
            let mut buffer = [0_u8; 4096];
            let count = read_scripted_bytes(stream, &mut buffer, deadline, index, "headers");
            assert!(
                count > 0,
                "connection closed before request {index} headers"
            );
            received.extend_from_slice(&buffer[..count]);
            if let Some(position) = find_bytes(&received, b"\r\n\r\n") {
                break position + 4;
            }
        };
        let header_text = String::from_utf8(received[..header_end].to_vec()).unwrap();
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().unwrap().to_owned();
        let headers: Vec<_> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let content_length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        while received.len() - header_end < content_length {
            let mut buffer = [0_u8; 8192];
            let count = read_scripted_bytes(stream, &mut buffer, deadline, index, "body");
            assert!(count > 0, "connection closed before request {index} body");
            received.extend_from_slice(&buffer[..count]);
        }
        CapturedRequest {
            request_line,
            headers,
            body: received[header_end..header_end + content_length].to_vec(),
        }
    }

    fn read_scripted_bytes(
        stream: &mut TcpStream,
        buffer: &mut [u8],
        deadline: Instant,
        index: usize,
        part: &str,
    ) -> usize {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out reading scripted request {index} {part}"
        );
        stream.set_read_timeout(Some(remaining)).unwrap();
        stream.read(buffer).unwrap_or_else(|error| {
            panic!("failed reading scripted request {index} {part}: {error}")
        })
    }

    fn write_scripted_response(stream: &mut TcpStream, status: StatusCode, response_body: &str) {
        if let Some(encoded) = response_body.strip_prefix("sdsync-test-binary:") {
            let mut body = Vec::with_capacity(encoded.len() / 2);
            for pair in encoded.as_bytes().as_chunks::<2>().0 {
                let high = char::from(pair[0])
                    .to_digit(16)
                    .expect("scripted binary high nibble") as u8;
                let low = char::from(pair[1])
                    .to_digit(16)
                    .expect("scripted binary low nibble") as u8;
                body.push((high << 4) | low);
            }
            assert_eq!(body.len() * 2, encoded.len(), "scripted binary hex length");
            write_scripted_bytes_response(stream, status, "application/octet-stream", &body);
            return;
        }
        stream
            .set_write_timeout(Some(SCRIPTED_SERVER_TIMEOUT))
            .unwrap();
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Unknown"),
            response_body.len(),
            response_body
        )
        .unwrap();
        stream.flush().unwrap();
    }

    fn scripted_binary_response(body: &[u8]) -> String {
        let mut encoded = String::from("sdsync-test-binary:");
        for byte in body {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").unwrap();
        }
        encoded
    }

    fn write_scripted_bytes_response(
        stream: &mut TcpStream,
        status: StatusCode,
        content_type: &str,
        body: &[u8],
    ) {
        stream
            .set_write_timeout(Some(SCRIPTED_SERVER_TIMEOUT))
            .unwrap();
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Unknown"),
            body.len(),
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn decode_scripted_error(status: StatusCode, body: String, api: &str, method: &str) -> Error {
        let (base, server) = scripted_server_with_status(vec![(status, body)]);
        let url = Url::parse(&base).unwrap().join("webapi/entry.cgi").unwrap();
        let response = crate::blocking_client_builder()
            .expect("ring provider installs")
            .build()
            .expect("client builds")
            .post(url)
            .send()
            .unwrap();
        let error =
            decode_response_observed::<Value>(response, api, method, &mut ResponseFacts::default())
                .unwrap_err();
        assert_eq!(server.join().unwrap().len(), 1);
        error
    }

    fn rendered_error(error: &Error) -> String {
        format!("{error}\n{error:?}")
    }

    fn required_discovery() -> String {
        serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3}
            }
        })
        .to_string()
    }

    fn browser_discovery() -> String {
        serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {
                    "path": "auth-route.cgi",
                    "minVersion": 3,
                    "maxVersion": 7
                },
                "SYNO.FileStation.List": {
                    "path": "list-route.cgi",
                    "minVersion": 1,
                    "maxVersion": 2
                }
            }
        })
        .to_string()
    }

    fn write_probe_discovery(server_copy: bool) -> String {
        let mut discovery = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Delete": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.MD5": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Download": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3}
            }
        });
        if server_copy {
            discovery["data"]["SYNO.FileStation.CopyMove"] = serde_json::json!({
                "path": "entry.cgi",
                "minVersion": 1,
                "maxVersion": 3
            });
        }
        discovery.to_string()
    }

    fn getinfo_directory(path: &str) -> String {
        let name = path.rsplit('/').next().unwrap();
        serde_json::json!({
            "success": true,
            "data": {"files": [{
                "path": path,
                "name": name,
                "isdir": true,
                "additional": {}
            }]}
        })
        .to_string()
    }

    fn getinfo_file(path: &str, size: u64, mtime_seconds: Option<i64>) -> String {
        let name = path.rsplit('/').next().unwrap();
        let additional = match mtime_seconds {
            Some(mtime) => serde_json::json!({"size": size, "time": {"mtime": mtime}}),
            None => serde_json::json!({"size": size}),
        };
        serde_json::json!({
            "success": true,
            "data": {"files": [{
                "path": path,
                "name": name,
                "isdir": false,
                "additional": additional
            }]}
        })
        .to_string()
    }

    fn connect_test_client(base_url: String) -> ApiClient {
        ApiClient::connect(&ClientOptions {
            base_url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap()
    }

    fn login_response() -> String {
        r#"{"success":true,"data":{"sid":"test-session","synotoken":"test-token"}}"#.to_owned()
    }

    #[test]
    fn bounded_browser_deadline_is_shared_by_discovery_fallback_and_totp_login() {
        let responses = vec![
            (StatusCode::BAD_GATEWAY, "backend unavailable".to_owned()),
            (StatusCode::OK, browser_discovery()),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":403}}"#.to_owned(),
            ),
        ];
        let (url, server) = scripted_server_with_status(responses);
        let mut client = ApiClient::connect_for_browsing_bounded(
            &ClientOptions {
                base_url: url,
                allow_http: true,
                accept_invalid_certs: false,
                ca_certificate: None,
                connect_timeout: Duration::from_secs(2),
                request_timeout: Duration::from_secs(2),
                retries: 0,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        let shared_deadline = client
            .control_deadline
            .expect("bounded browsing client must retain its absolute deadline");
        let first = client.login("alice", "password", None).unwrap_err();
        assert_eq!(first.api_code(), Some(403));
        assert_eq!(client.control_deadline, Some(shared_deadline));

        // Deterministically model discovery and the first login consuming the
        // shared budget. The TOTP attempt must consult that same absolute
        // deadline instead of receiving a fresh per-request timeout.
        let expired_deadline = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("monotonic clock must support a one-second lookback");
        client.control_deadline = Some(expired_deadline);
        let second = client
            .login("alice", "password", Some("123456"))
            .unwrap_err();
        assert!(
            rendered_error(&second).contains("exceeded its total deadline"),
            "unexpected bounded-login error: {second:?}"
        );
        assert_eq!(client.control_deadline, Some(expired_deadline));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].request_line.contains("/webapi/entry.cgi"));
        assert!(requests[1].request_line.contains("/webapi/query.cgi"));
        assert!(!String::from_utf8_lossy(&requests[2].body).contains("otp_code"));
    }

    #[test]
    fn bounded_logout_replaces_an_exhausted_probe_deadline_with_its_cleanup_slice() {
        let responses = vec![
            (StatusCode::OK, browser_discovery()),
            (StatusCode::OK, login_response()),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
        ];
        let (url, server) = scripted_server_with_status(responses);
        let mut client = ApiClient::connect_for_browsing_bounded(
            &ClientOptions {
                base_url: url,
                allow_http: true,
                accept_invalid_certs: false,
                ca_certificate: None,
                connect_timeout: Duration::from_secs(2),
                request_timeout: Duration::from_secs(2),
                retries: 0,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        client.login("alice", "password", None).unwrap();
        let expired_probe_deadline = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("monotonic clock must support a one-second lookback");
        client.control_deadline = Some(expired_probe_deadline);
        let cleanup_timeout = Duration::from_secs(5);
        let earliest_cleanup_deadline = Instant::now()
            .checked_add(cleanup_timeout)
            .expect("cleanup deadline must be representable");
        client
            .logout_bounded(cleanup_timeout)
            .expect("reserved cleanup budget must close a session after probe exhaustion");
        let cleanup_deadline = client
            .control_deadline
            .expect("bounded logout must retain its fresh cleanup deadline");
        assert!(cleanup_deadline >= earliest_cleanup_deadline);
        assert_ne!(cleanup_deadline, expired_probe_deadline);
        assert!(client.session.is_none());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(String::from_utf8_lossy(&requests[2].body).contains("method=logout"));
    }

    #[test]
    fn directory_browser_authenticates_before_listing_discovered_shared_folders_and_logs_out() {
        let shares = serde_json::json!({
            "success": true,
            "data": {
                "total": 5,
                "shares": [
                    {
                        "name": "zeta",
                        "path": "/zeta",
                        "additional": {"perm": {"acl": {"read": true, "exec": true}}}
                    },
                    {
                        "name": "blocked-special-right",
                        "path": "/blocked-special-right",
                        "additional": {"perm": {"adv_right": {"disable_list": true}}}
                    },
                    {
                        "name": "blocked-read",
                        "path": "/blocked-read",
                        "additional": {"perm": {"acl": {"read": false, "exec": true}}}
                    },
                    {
                        "name": "blocked-traverse",
                        "path": "/blocked-traverse",
                        "additional": {"perm": {"acl": {"read": true, "exec": false}}}
                    },
                    {"name": "alpha", "path": "/alpha"}
                ]
            }
        })
        .to_string();
        let (url, server) = scripted_server(vec![
            browser_discovery(),
            login_response(),
            shares,
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = ApiClient::connect_for_browsing(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();

        client
            .login("browser-user", "fixture-password", None)
            .unwrap();
        let page = client.browse_directories("/", 10).unwrap();
        client.logout().unwrap();

        assert_eq!(
            page.directories,
            vec![
                RemoteDirectory {
                    name: "alpha".to_owned(),
                    path: "/alpha".to_owned(),
                },
                RemoteDirectory {
                    name: "zeta".to_owned(),
                    path: "/zeta".to_owned(),
                },
            ]
        );
        assert_eq!(page.parent, "/");
        assert!(!page.truncated);

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[1].request_line,
            "POST /prefix/webapi/auth-route.cgi HTTP/1.1"
        );
        assert_eq!(
            requests[2].request_line,
            "POST /prefix/webapi/list-route.cgi HTTP/1.1"
        );
        assert_eq!(
            requests[3].request_line,
            "POST /prefix/webapi/auth-route.cgi HTTP/1.1"
        );
        let login = String::from_utf8_lossy(&requests[1].body);
        let listing = String::from_utf8_lossy(&requests[2].body);
        let logout = String::from_utf8_lossy(&requests[3].body);
        assert!(login.contains("method=login"));
        assert!(login.contains("session=FileStation"));
        assert!(login.contains("passwd=fixture-password"));
        assert!(listing.contains("method=list_share"));
        assert!(listing.contains("additional=%5B%22perm%22%5D"));
        assert!(logout.contains("method=logout"));
        assert!(!listing.contains("fixture-password"));
        assert!(!logout.contains("fixture-password"));
    }

    #[test]
    fn directory_browser_descends_only_into_returned_acl_visible_direct_children() {
        let children = serde_json::json!({
            "success": true,
            "data": {
                "total": 6,
                "files": [
                    {"name": "zeta", "path": "/share/base/zeta", "isdir": true,
                     "additional": {"perm": {"acl": {"read": true, "exec": true}}}},
                    {"name": "file.txt", "path": "/share/base/file.txt", "isdir": false},
                    {"name": "denied", "path": "/share/base/denied", "isdir": true,
                     "additional": {"perm": {"acl": {"read": false, "exec": true}}}},
                    {"name": "mounted", "path": "/share/base/mounted", "isdir": true,
                     "additional": {"mount_point_type": "remote"}},
                    {"name": "disabled", "path": "/share/base/disabled", "isdir": true,
                     "disable_list": true},
                    {"name": "alpha", "path": "/share/base/alpha", "isdir": true}
                ]
            }
        })
        .to_string();
        let (url, server) = scripted_server(vec![
            browser_discovery(),
            login_response(),
            children,
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = ApiClient::connect_for_browsing(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();

        client
            .login("browser-user", "fixture-password", None)
            .unwrap();
        let page = client.browse_directories("/share/base", 10).unwrap();
        client.logout().unwrap();

        assert_eq!(
            page.directories,
            vec![
                RemoteDirectory {
                    name: "alpha".to_owned(),
                    path: "/share/base/alpha".to_owned(),
                },
                RemoteDirectory {
                    name: "zeta".to_owned(),
                    path: "/share/base/zeta".to_owned(),
                },
            ]
        );
        let requests = server.join().unwrap();
        let listing = String::from_utf8_lossy(&requests[2].body);
        assert!(listing.contains("method=list"));
        assert!(listing.contains("folder_path=%22%2Fshare%2Fbase%22"));
        assert!(listing.contains("filetype=%22dir%22"));
        assert!(listing.contains("perm"));
        assert!(listing.contains("mount_point_type"));
    }

    #[test]
    fn directory_browser_permission_failure_is_redacted_and_still_allows_logout() {
        let reflected = "fixture-password-must-not-echo";
        let denied = serde_json::json!({
            "success": false,
            "error": {"code": 105, "errors": [{"path": reflected}]}
        })
        .to_string();
        let (url, server) = scripted_server(vec![
            browser_discovery(),
            login_response(),
            denied,
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = ApiClient::connect_for_browsing(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client
            .login("browser-user", "fixture-password", None)
            .unwrap();

        let error = client.browse_directories("/share", 10).unwrap_err();
        assert!(rendered_error(&error).contains("code 105"));
        assert!(!rendered_error(&error).contains(reflected));
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(String::from_utf8_lossy(&requests[2].body).contains("method=list"));
        assert!(String::from_utf8_lossy(&requests[3].body).contains("method=logout"));
    }

    fn task_start_response(taskid: &str) -> String {
        serde_json::json!({"success": true, "data": {"taskid": taskid}}).to_string()
    }

    #[test]
    fn remote_download_builds_the_complete_fingerprint_without_secrets_in_the_url() {
        let payload = b"abc".to_vec();
        let (base, server) = scripted_download_server_with_hook(
            vec![(StatusCode::OK, "application/octet-stream", payload.clone())],
            |_| {},
        );
        let client = fingerprint_test_client(&base, 0);

        let fingerprint = client
            .remote_content_fingerprint(
                "/share/payload.bin",
                payload.len() as u64,
                &CancellationToken::default(),
            )
            .unwrap();

        assert_eq!(fingerprint.to_string(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(fingerprint.crc32_hex().as_deref(), Some("352441c2"));
        assert_eq!(
            fingerprint.sha256_hex().as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].request_line,
            "POST /prefix/webapi/entry.cgi HTTP/1.1"
        );
        assert!(!requests[0].request_line.contains("download-test-session"));
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("api=SYNO.FileStation.Download"));
        assert!(body.contains("_sid=download-test-session"));
        assert!(body.contains("SynoToken=download-test-token"));
        let form_url = Url::parse(&format!("http://form.invalid/?{body}")).unwrap();
        let fields = form_url
            .query_pairs()
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(fields.get("mode").map(String::as_str), Some("\"download\""));
        assert_eq!(
            fields.get("path").map(String::as_str),
            Some("[\"/share/payload.bin\"]")
        );
    }

    #[test]
    fn remote_download_keeps_caller_owned_session_fields_in_a_zeroizing_container() {
        let client = fingerprint_test_client("http://files.example.test/prefix/", 0);
        let fields: Zeroizing<Vec<(String, String)>> =
            client.download_form_fields("/share/payload.bin").unwrap();
        let value = |name: &str| {
            fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.as_str())
        };

        assert_eq!(value("_sid"), Some("download-test-session"));
        assert_eq!(value("SynoToken"), Some("download-test-token"));
        assert_eq!(value("path"), Some("[\"/share/payload.bin\"]"));
    }

    #[test]
    fn remote_download_rejects_non_binary_json_and_wrong_size_responses() {
        let (html_base, html_server) = scripted_download_server_with_hook(
            vec![(StatusCode::OK, "text/html", b"<html>proxy</html>".to_vec())],
            |_| {},
        );
        let html_error = fingerprint_test_client(&html_base, 0)
            .remote_content_fingerprint("/share/payload.bin", 18, &CancellationToken::default())
            .unwrap_err();
        assert!(matches!(
            html_error,
            Error::InvalidResponse { operation, message }
                if operation == "SYNO.FileStation.Download.download"
                    && message == "download response was not an octet stream"
        ));
        assert_eq!(html_server.join().unwrap().len(), 1);

        let (json_base, json_server) = scripted_download_server_with_hook(
            vec![(
                StatusCode::OK,
                "application/json; charset=utf-8",
                br#"{"success":false,"error":{"code":408}}"#.to_vec(),
            )],
            |_| {},
        );
        let json_error = fingerprint_test_client(&json_base, 0)
            .remote_content_fingerprint("/share/missing.bin", 1, &CancellationToken::default())
            .unwrap_err();
        assert!(matches!(json_error, Error::Api { code: 408, .. }));
        assert_eq!(json_server.join().unwrap().len(), 1);

        let (size_base, size_server) = scripted_download_server_with_hook(
            vec![(StatusCode::OK, "application/octet-stream", b"abc".to_vec())],
            |_| {},
        );
        let size_error = fingerprint_test_client(&size_base, 0)
            .remote_content_fingerprint("/share/payload.bin", 4, &CancellationToken::default())
            .unwrap_err();
        assert!(matches!(
            size_error,
            Error::RemoteSnapshotChanged(path) if path == "/share/payload.bin"
        ));
        assert_eq!(size_server.join().unwrap().len(), 1);
    }

    #[test]
    fn remote_download_honors_cancellation_before_reading_payload_bytes() {
        let cancellation = CancellationToken::default();
        let server_cancellation = cancellation.clone();
        let (base, server) = scripted_download_server_with_hook(
            vec![(
                StatusCode::OK,
                "application/octet-stream",
                b"payload".to_vec(),
            )],
            move |_| server_cancellation.cancel(),
        );

        assert!(matches!(
            fingerprint_test_client(&base, 0).remote_content_fingerprint(
                "/share/payload.bin",
                7,
                &cancellation,
            ),
            Err(Error::Cancelled)
        ));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn remote_download_retries_a_transient_status_then_hashes_the_single_complete_body() {
        let (base, server) = scripted_download_server_with_hook(
            vec![
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "text/plain",
                    b"temporary".to_vec(),
                ),
                (StatusCode::OK, "application/octet-stream", b"abc".to_vec()),
            ],
            |_| {},
        );

        let fingerprint = fingerprint_test_client(&base, 1)
            .remote_content_fingerprint("/share/payload.bin", 3, &CancellationToken::default())
            .unwrap();
        assert_eq!(fingerprint.crc32_hex().as_deref(), Some("352441c2"));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn remote_download_stall_is_idle_bounded_and_observes_cancellation() {
        let cancellation = CancellationToken::default();
        let server_cancellation = cancellation.clone();
        let (base, server) = scripted_slow_download_server_with_hook(
            b"payload".to_vec(),
            Duration::from_millis(120),
            Duration::ZERO,
            move || server_cancellation.cancel(),
        );
        let client = fingerprint_test_client_with_timeouts(
            &base,
            0,
            Duration::from_millis(60),
            Duration::from_secs(1),
        );
        let started = Instant::now();

        assert!(matches!(
            client.remote_content_fingerprint("/share/payload.bin", 7, &cancellation),
            Err(Error::Cancelled)
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn remote_download_idle_timeout_releases_the_bounded_runtime() {
        let (base, server) = scripted_slow_download_server(
            b"payload".to_vec(),
            Duration::from_millis(150),
            Duration::ZERO,
        );
        let client = fingerprint_test_client_with_timeouts(
            &base,
            0,
            Duration::from_millis(60),
            Duration::from_secs(1),
        );
        let started = Instant::now();

        let error = client
            .remote_content_fingerprint("/share/payload.bin", 7, &CancellationToken::default())
            .unwrap_err();

        assert!(matches!(error, Error::Http { source, .. } if source.is_timeout()));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn synchronous_download_is_panic_free_inside_a_tokio_runtime() {
        let payload = b"nested-runtime".to_vec();
        let (base, server) = scripted_download_server_with_hook(
            vec![(StatusCode::OK, "application/octet-stream", payload.clone())],
            |_| {},
        );
        let client = fingerprint_test_client(&base, 0);
        let caller_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let fingerprint = caller_runtime.block_on(async move {
            client.remote_content_fingerprint(
                "/share/payload.bin",
                payload.len() as u64,
                &CancellationToken::default(),
            )
        });

        assert_eq!(
            fingerprint.unwrap(),
            ContentMd5::from_content(b"nested-runtime")
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn remote_download_header_stall_observes_cancellation_before_transport_error() {
        let cancellation = CancellationToken::default();
        let server_cancellation = cancellation.clone();
        let (base, server) = scripted_delayed_download_headers_server_with_hook(
            Duration::from_millis(150),
            move || server_cancellation.cancel(),
        );
        let client = fingerprint_test_client_with_timeouts(
            &base,
            0,
            Duration::from_millis(60),
            Duration::from_secs(1),
        );

        assert!(matches!(
            client.remote_content_fingerprint("/share/payload.bin", 0, &cancellation),
            Err(Error::Cancelled)
        ));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn remote_download_header_stall_reports_the_total_deadline() {
        let (base, server) = scripted_delayed_download_headers_server(Duration::from_millis(150));
        let client = fingerprint_test_client_with_timeouts(
            &base,
            0,
            Duration::from_millis(200),
            Duration::from_millis(70),
        );

        assert!(matches!(
            client.remote_content_fingerprint(
                "/share/payload.bin",
                0,
                &CancellationToken::default()
            ),
            Err(Error::OperationTimedOut {
                operation: "remote content fingerprint download"
            })
        ));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn remote_download_can_outlive_idle_timeout_when_every_chunk_makes_progress() {
        let body = b"abcdefghijklmnopqrstuvwxyz".to_vec();
        let (base, server) =
            scripted_slow_download_server(body.clone(), Duration::ZERO, Duration::from_millis(50));
        let idle_timeout = Duration::from_secs(1);
        let client =
            fingerprint_test_client_with_timeouts(&base, 0, idle_timeout, Duration::from_secs(4));
        let started = Instant::now();

        let fingerprint = client
            .remote_content_fingerprint(
                "/share/payload.bin",
                body.len() as u64,
                &CancellationToken::default(),
            )
            .unwrap();

        assert_eq!(fingerprint, ContentMd5::from_content(&body));
        assert!(started.elapsed() > idle_timeout);
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn remote_download_drip_feed_cannot_extend_the_total_operation_deadline() {
        let (base, server) = scripted_slow_download_server(
            b"abcdef".to_vec(),
            Duration::ZERO,
            Duration::from_millis(30),
        );
        let client = fingerprint_test_client_with_timeouts(
            &base,
            0,
            Duration::from_millis(100),
            Duration::from_millis(70),
        );
        let started = Instant::now();

        assert!(matches!(
            client.remote_content_fingerprint(
                "/share/payload.bin",
                6,
                &CancellationToken::default()
            ),
            Err(Error::OperationTimedOut {
                operation: "remote content fingerprint download"
            })
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn upload_and_copy_evidence_rejects_incomplete_or_collision_like_values() {
        let expected = ContentMd5::from_digests([7_u8; 16], 42, [1_u8; 32]);
        let legacy = ContentMd5::from_bytes([7_u8; 16]);
        let different_sha = ContentMd5::from_digests([7_u8; 16], 42, [2_u8; 32]);

        assert!(!content_evidence_matches(expected, legacy));
        assert!(!content_evidence_matches(legacy, expected));
        assert!(!content_evidence_matches(expected, different_sha));
        assert!(content_evidence_matches(expected, expected));
    }

    #[test]
    fn remote_md5_stops_started_tasks_on_cancel_timeout_and_missing_status() {
        let cancellation = CancellationToken::default();
        let hook_cancellation = cancellation.clone();
        let responses = vec![
            (StatusCode::OK, write_probe_discovery(false)),
            (StatusCode::OK, login_response()),
            (StatusCode::OK, task_start_response("cancelled-md5")),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
        ];
        let (url, server) = scripted_server_with_status_hook(responses, move |index| {
            if index == 2 {
                hook_cancellation.cancel();
            }
        });
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(matches!(
            client.remote_content_md5("/share/file.bin", &cancellation),
            Err(Error::Cancelled)
        ));
        let requests = server.join().unwrap();
        assert!(String::from_utf8_lossy(&requests[3].body).contains("method=stop"));

        let responses = vec![
            write_probe_discovery(false),
            login_response(),
            task_start_response("timed-out-md5"),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        client.operation_timeout = Duration::ZERO;
        assert!(matches!(
            client.remote_content_md5("/share/file.bin", &CancellationToken::default()),
            Err(Error::OperationTimedOut {
                operation: "remote MD5 calculation"
            })
        ));
        let requests = server.join().unwrap();
        assert!(String::from_utf8_lossy(&requests[3].body).contains("method=stop"));

        let responses = vec![
            write_probe_discovery(false),
            login_response(),
            task_start_response("missing-status-md5"),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client
            .remote_content_md5("/share/file.bin", &CancellationToken::default())
            .unwrap_err();
        assert!(matches!(
            error,
            Error::InvalidResponse { ref operation, .. }
                if operation == "SYNO.FileStation.MD5.status"
        ));
        let requests = server.join().unwrap();
        assert!(String::from_utf8_lossy(&requests[4].body).contains("method=stop"));
    }

    #[test]
    fn remote_md5_rejects_invalid_task_and_finished_digest_data() {
        let responses = vec![
            write_probe_discovery(false),
            login_response(),
            task_start_response(""),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client
            .remote_content_md5("/share/file.bin", &CancellationToken::default())
            .unwrap_err();
        assert!(matches!(error, Error::InvalidResponse { .. }));
        assert_eq!(server.join().unwrap().len(), 3);

        for status in [
            r#"{"success":true,"data":{"finished":true}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":true,"md5":"not-a-digest"}}"#.to_owned(),
        ] {
            let responses = vec![
                write_probe_discovery(false),
                login_response(),
                task_start_response("finished-md5"),
                status,
            ];
            let (url, server) = scripted_server(responses);
            let mut client = connect_test_client(url);
            client.login("alice", "password", None).unwrap();
            assert!(
                client
                    .remote_content_md5("/share/file.bin", &CancellationToken::default())
                    .is_err()
            );
            assert_eq!(server.join().unwrap().len(), 4);
        }
    }

    #[test]
    fn content_selection_and_server_copy_fail_closed_before_network_mutation() {
        let (url, finish_server, server) =
            scripted_server_monitoring_extra_requests(write_probe_discovery(true));
        let client = connect_test_client(url);
        let mut inventory = RemoteInventory {
            root_exists: true,
            entries: BTreeMap::from([(
                "folder".to_owned(),
                RemoteEntry {
                    relative: "folder".to_owned(),
                    remote_path: "/share/root/folder".to_owned(),
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_seconds: 0,
                    mount_point_type: None,
                    content_md5: None,
                },
            )]),
        };
        let cancellation = CancellationToken::default();
        let missing = BTreeSet::from(["missing.bin".to_owned()]);
        assert!(matches!(
            client
                .populate_remote_content_md5(&mut inventory, &missing, &cancellation)
                .unwrap_err(),
            Error::Message(message)
                if message
                    == "remote content selection referenced missing inventory path \"missing.bin\""
        ));
        let directory = BTreeSet::from(["folder".to_owned()]);
        assert!(matches!(
            client
                .populate_remote_content_md5(&mut inventory, &directory, &cancellation)
                .unwrap_err(),
            Error::Message(message)
                if message == "remote content selection referenced non-file path \"folder\""
        ));

        let root = RemoteRoot::parse("/share/root").unwrap();
        let digest = ContentMd5::from_bytes([0_u8; 16]);
        for (source, destination) in [
            ("/share/root/a/file.bin", "/share/root/a/file.bin"),
            ("/share/root/a/file.bin", "/share/root/b/renamed.bin"),
        ] {
            assert!(matches!(
                client.copy_file_verified(
                    &root,
                    source,
                    destination,
                    1,
                    digest,
                    &CancellationToken::default(),
                ),
                Err(Error::Message(message))
                    if message
                        == "safe server-side copy requires different parents and an unchanged basename"
            ));
        }
        assert!(matches!(
            client.copy_file_verified(
                &root,
                "/share/root/a/file.bin",
                "/share/escape/file.bin",
                1,
                digest,
                &CancellationToken::default(),
            ),
            Err(Error::UnsafeRemotePath { path, reason })
                if path == "/share/escape/file.bin"
                    && reason
                        == "delete target must be a normalized strict child of the configured destination"
        ));
        let cancelled = CancellationToken::default();
        cancelled.cancel();
        assert!(matches!(
            client.copy_file_verified(
                &root,
                "/share/root/a/file.bin",
                "/share/root/b/file.bin",
                1,
                digest,
                &cancelled,
            ),
            Err(Error::Cancelled)
        ));
        finish_server.send(()).unwrap();
        assert_eq!(
            server.join().unwrap().len(),
            1,
            "local validation unexpectedly issued a second HTTP request"
        );

        let (url, server) = scripted_server(vec![required_discovery()]);
        let client = connect_test_client(url);
        assert!(matches!(
            client.copy_file_verified(
                &root,
                "/share/root/a/file.bin",
                "/share/root/b/file.bin",
                1,
                digest,
                &CancellationToken::default(),
            ),
            Err(Error::ServerCopyNotStarted)
        ));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn task_ids_and_retry_classification_enforce_bounded_safe_values() {
        assert!(validate_task_id("task-123", "test.task").is_ok());
        for invalid in ["", "line\nbreak"] {
            assert!(matches!(
                validate_task_id(invalid, "test.task"),
                Err(Error::InvalidResponse { .. })
            ));
        }
        let oversized = "x".repeat(1025);
        assert!(validate_task_id(&oversized, "test.task").is_err());

        for status in [408, 425, 429, 502, 503, 504] {
            assert!(retryable(&Error::HttpStatus {
                operation: "test".to_owned(),
                status: StatusCode::from_u16(status).unwrap(),
                message: String::new(),
            }));
        }
        assert!(!retryable(&Error::HttpStatus {
            operation: "test".to_owned(),
            status: StatusCode::UNAUTHORIZED,
            message: String::new(),
        }));
        for code in [102, 103, 104, 105, 407, 409] {
            assert!(matches!(
                copy_start_error(Error::Api {
                    api: "SYNO.FileStation.CopyMove".to_owned(),
                    operation: "start".to_owned(),
                    code,
                    description: String::new(),
                    details: Vec::new(),
                }),
                Error::ServerCopyNotStarted
            ));
        }
        assert!(matches!(
            copy_start_error(Error::Cancelled),
            Error::Cancelled
        ));
    }

    #[test]
    fn remote_inventory_preserves_hierarchy_metadata_and_mount_boundaries() {
        let root_listing = serde_json::json!({"success":true,"data":{"total":3,"files":[
            {"path":"/share/root/file.bin","name":"file.bin","isdir":false,"additional":{"size":7,"time":{"mtime":11}}},
            {"path":"/share/root/sub","name":"sub","isdir":true,"additional":{}},
            {"path":"/share/root/mounted","name":"mounted","isdir":true,"additional":{"mount_point_type":"cifs"}}
        ]}}).to_string();
        let sub_listing = serde_json::json!({"success":true,"data":{"total":1,"files":[
            {"path":"/share/root/sub/nested.txt","name":"nested.txt","isdir":false,"additional":{"size":9,"time":{"mtime":13}}}
        ]}}).to_string();
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            getinfo_directory("/share"),
            getinfo_directory("/share/root"),
            root_listing,
            sub_listing,
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let inventory = client
            .remote_inventory(
                &RemoteRoot::parse("/share/root").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap();
        assert!(inventory.root_exists);
        assert_eq!(inventory.entries.len(), 4);
        let file = &inventory.entries["file.bin"];
        assert_eq!(
            (file.kind, file.size, file.mtime_seconds),
            (EntryKind::File, 7, 11)
        );
        assert_eq!(
            inventory.entries["mounted"].mount_point_type.as_deref(),
            Some("cifs")
        );
        assert_eq!(
            inventory.entries["sub/nested.txt"].remote_path,
            "/share/root/sub/nested.txt"
        );
        assert_eq!(server.join().unwrap().len(), 6);
    }

    #[test]
    fn diagnostic_inventory_is_single_page_non_recursive_and_samples_five_deterministically() {
        let list = serde_json::json!({
            "success": true,
            "data": {
                "total": 9,
                "files": [
                    {"path":"/share/root/zeta.bin","name":"zeta.bin","isdir":false,"additional":{"size":6,"time":{"mtime":106}}},
                    {"path":"/share/root/beta","name":"beta","isdir":true,"additional":{}},
                    {"path":"/share/root/epsilon.bin","name":"epsilon.bin","isdir":false,"additional":{"size":5,"time":{"mtime":105}}},
                    {"path":"/share/root/alpha.bin","name":"alpha.bin","isdir":false,"additional":{"size":1,"time":{"mtime":101}}},
                    {"path":"/share/root/delta.bin","name":"delta.bin","isdir":false,"additional":{"size":4,"time":{"mtime":104}}},
                    {"path":"/share/root/gamma.bin","name":"gamma.bin","isdir":false,"additional":{"size":3,"time":{"mtime":103}}}
                ]
            }
        })
        .to_string();
        let responses = vec![
            write_probe_discovery(false),
            login_response(),
            getinfo_directory("/share/root"),
            list,
        ];
        let (url, server) = scripted_server(responses);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();

        let report = client
            .diagnostic_remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
            .unwrap();
        assert!(report.root_exists);
        assert_eq!(report.total_entries, 9);
        assert_eq!(report.sample.len(), 5);
        assert!(report.truncated);
        assert_eq!(report.truncated_count, 4);
        assert_eq!(report.truncated_reason, Some("sample_limit"));
        assert_eq!(report.pages_requested, 1);
        assert_eq!(report.traversal_depth, 1);
        assert_eq!(report.deadline_ms, 5_000);
        assert_eq!(
            report
                .sample
                .iter()
                .map(|entry| entry.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["alpha.bin", "beta", "delta.bin", "epsilon.bin", "gamma.bin"]
        );
        assert_eq!(report.sample[0].size_bytes, Some(1));
        assert_eq!(report.sample[0].mtime_seconds, Some(101));
        assert_eq!(report.sample[1].kind, EntryKind::Directory);
        assert_eq!(report.sample[1].size_bytes, None);

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        let list_body = String::from_utf8_lossy(&requests[3].body);
        assert!(list_body.contains("limit=6"));
        assert!(list_body.contains("offset=0"));
        assert!(!list_body.contains("offset=6"));
    }

    #[test]
    fn diagnostic_shared_folder_discovery_is_one_bounded_page_and_samples_five() {
        let shares = serde_json::json!({
            "success": true,
            "data": {
                "total": 8,
                "shares": [
                    {"path":"/zeta","name":"zeta"},
                    {"path":"/beta","name":"beta"},
                    {"path":"/epsilon","name":"epsilon"},
                    {"path":"/alpha","name":"alpha"},
                    {"path":"/delta","name":"delta"},
                    {"path":"/gamma","name":"gamma"}
                ]
            }
        })
        .to_string();
        let (url, server) =
            scripted_server(vec![write_probe_discovery(false), login_response(), shares]);
        let mut client = connect_test_client(url);
        client
            .login("diagnostic-user", "diagnostic-password", None)
            .unwrap();

        let report = client.diagnostic_visible_shared_folders().unwrap();

        assert!(report.root_exists);
        assert_eq!(report.total_entries, 8);
        assert_eq!(report.sample.len(), 5);
        assert!(report.truncated);
        assert_eq!(report.truncated_count, 3);
        assert_eq!(report.truncated_reason, Some("sample_limit"));
        assert_eq!(report.pages_requested, 1);
        assert_eq!(report.traversal_depth, 0);
        assert_eq!(report.deadline_ms, 5_000);
        assert_eq!(
            report
                .sample
                .iter()
                .map(|entry| (
                    entry.relative_path.as_str(),
                    entry.name.as_str(),
                    entry.kind
                ))
                .collect::<Vec<_>>(),
            [
                ("/alpha", "alpha", EntryKind::Directory),
                ("/beta", "beta", EntryKind::Directory),
                ("/delta", "delta", EntryKind::Directory),
                ("/epsilon", "epsilon", EntryKind::Directory),
                ("/gamma", "gamma", EntryKind::Directory),
            ]
        );
        assert!(report.sample.iter().all(|entry| {
            entry.size_bytes.is_none() && entry.mtime_seconds.is_none() && !entry.mount_boundary
        }));
        let rendered = format!("{report:?}");
        assert!(!rendered.contains("diagnostic-password"));
        assert!(!rendered.contains("e2e-session-secret"));

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        let listing = String::from_utf8_lossy(&requests[2].body);
        assert!(listing.contains("method=list_share"));
        assert!(listing.contains("limit=6"));
        assert!(listing.contains("offset=0"));
        assert!(!listing.contains("method=getinfo"));
        assert!(!listing.contains("method=list&"));
        assert!(!listing.contains("diagnostic-password"));
    }

    #[test]
    fn diagnostic_shared_folder_discovery_preserves_zero_entry_evidence() {
        let shares = serde_json::json!({
            "success": true,
            "data": {"total": 0, "shares": []}
        })
        .to_string();
        let (url, server) =
            scripted_server(vec![write_probe_discovery(false), login_response(), shares]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();

        let report = client.diagnostic_visible_shared_folders().unwrap();

        assert!(report.root_exists);
        assert_eq!(report.total_entries, 0);
        assert!(report.sample.is_empty());
        assert!(!report.truncated);
        assert_eq!(report.truncated_count, 0);
        assert_eq!(report.truncated_reason, None);
        assert_eq!(report.pages_requested, 1);
        assert_eq!(report.traversal_depth, 0);
        assert_eq!(report.deadline_ms, 5_000);
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn diagnostic_shared_folder_discovery_retains_reported_roots_without_permission_claims() {
        let shares = serde_json::json!({
            "success": true,
            "data": {
                "total": 1,
                "shares": [{
                    "path": "/reported",
                    "name": "reported",
                    "disable_list": true,
                    "additional": {
                        "perm": {
                            "adv_right": {"disable_list": true},
                            "acl": {"read": false, "exec": false}
                        }
                    }
                }]
            }
        })
        .to_string();
        let (url, server) =
            scripted_server(vec![write_probe_discovery(false), login_response(), shares]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();

        let report = client.diagnostic_visible_shared_folders().unwrap();

        assert_eq!(report.total_entries, 1);
        assert_eq!(report.sample.len(), 1);
        assert_eq!(report.sample[0].relative_path, "/reported");
        assert_eq!(report.sample[0].name, "reported");
        assert!(!report.truncated);
        assert_eq!(report.truncated_count, 0);
        assert_eq!(report.truncated_reason, None);
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn diagnostic_inventory_display_fields_are_character_bounded() {
        let value = format!("{}é{}", "a".repeat(511), "z".repeat(32));
        let (bounded, truncated) = bounded_diagnostic_text(&value, 512);
        assert!(truncated);
        assert_eq!(bounded.chars().count(), 512);
        assert!(bounded.ends_with('é'));

        let (complete, truncated) = bounded_diagnostic_text("alpha", 512);
        assert_eq!(complete, "alpha");
        assert!(!truncated);
    }

    /// A snapshot is only worth taking if it can disagree with what is on the NAS later. An
    /// absent `size` or `time` on a file must therefore be rejected outright rather than stored
    /// as `0`, which would compare equal to the same coercion at delete time and wave through a
    /// file whose content was replaced after planning.
    #[test]
    fn remote_inventory_rejects_a_file_missing_the_metadata_a_snapshot_compares() {
        let incomplete = [
            (
                serde_json::json!({}),
                "file information contained no byte size or modified time",
            ),
            (
                serde_json::json!({"time": {"mtime": 13}}),
                "file information contained no byte size",
            ),
            (
                serde_json::json!({"size": 9}),
                "file information contained no modified time",
            ),
        ];
        for (additional, expected_message) in incomplete {
            let listing = serde_json::json!({"success":true,"data":{"total":1,"files":[
                {"path":"/share/root/file.bin","name":"file.bin","isdir":false,"additional":additional}
            ]}})
            .to_string();
            let (url, server) = scripted_server(vec![
                required_discovery(),
                login_response(),
                getinfo_directory("/share"),
                getinfo_directory("/share/root"),
                listing,
            ]);
            let mut client = connect_test_client(url);
            client.login("alice", "password", None).unwrap();
            let error = client
                .remote_inventory(
                    &RemoteRoot::parse("/share/root").unwrap(),
                    &CancellationToken::default(),
                )
                .expect_err("an unusable file snapshot must not be stored as zero");
            let Error::InvalidResponse { operation, message } = &error else {
                panic!("expected a malformed-response error, got {error}");
            };
            assert_eq!(operation, "SYNO.FileStation.List.list");
            assert_eq!(message, expected_message);
            assert_eq!(server.join().unwrap().len(), 5);
        }
    }

    /// Directories are exempt: File Station omits both fields for them, and an empty
    /// `additional` object is the shape it actually sends. Rejecting those would fail every
    /// inventory containing a subdirectory.
    #[test]
    fn remote_inventory_accepts_directories_without_size_or_mtime() {
        let root_listing = serde_json::json!({"success":true,"data":{"total":2,"files":[
            {"path":"/share/root/sub","name":"sub","isdir":true,"additional":{}},
            {"path":"/share/root/plain","name":"plain","isdir":true}
        ]}})
        .to_string();
        let empty = serde_json::json!({"success":true,"data":{"total":0,"files":[]}}).to_string();
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            getinfo_directory("/share"),
            getinfo_directory("/share/root"),
            root_listing,
            empty.clone(),
            empty,
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let inventory = client
            .remote_inventory(
                &RemoteRoot::parse("/share/root").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap();
        for relative in ["sub", "plain"] {
            let entry = &inventory.entries[relative];
            assert_eq!(
                (entry.kind, entry.size, entry.mtime_seconds),
                (EntryKind::Directory, 0, 0)
            );
        }
        assert_eq!(server.join().unwrap().len(), 7);
    }

    /// The pre-delete re-verify must fail closed on the same absence, including against the
    /// all-zero snapshot that the previous coercion would have accepted unconditionally.
    #[test]
    fn live_metadata_snapshot_rejects_absent_file_metadata_against_a_zero_snapshot() {
        let absent = [
            serde_json::json!({}),
            serde_json::json!({"time": {"mtime": 0}}),
            serde_json::json!({"size": 0}),
        ];
        for additional in absent {
            let response = serde_json::json!({"success":true,"data":{"files":[
                {"path":"/share/root/file.bin","name":"file.bin","isdir":false,"additional":additional}
            ]}})
            .to_string();
            let (url, server) =
                scripted_server(vec![required_discovery(), login_response(), response]);
            let mut client = connect_test_client(url);
            client.login("alice", "password", None).unwrap();
            let error = client
                .verify_remote_metadata_snapshot(
                    "/share/root/file.bin",
                    EntryKind::File,
                    0,
                    0,
                    true,
                    &CancellationToken::default(),
                )
                .expect_err("absent metadata must never satisfy a stored snapshot");
            assert!(
                matches!(&error, Error::InvalidResponse { operation, .. }
                    if operation == "SYNO.FileStation.List.getinfo"),
                "expected a malformed-response error, got {error}"
            );
            assert_eq!(server.join().unwrap().len(), 3);
        }

        // The fully populated response the NAS actually sends still satisfies its snapshot.
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            getinfo_file("/share/root/file.bin", 9, Some(123)),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        client
            .verify_remote_metadata_snapshot(
                "/share/root/file.bin",
                EntryKind::File,
                9,
                123,
                true,
                &CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn file_metadata_fails_closed_for_files_and_defaults_only_for_directories() {
        let complete = RemoteAdditionalWire {
            size: Some(9),
            time: Some(RemoteTimeWire { mtime: Some(13) }),
            mount_point_type: None,
            perm: None,
        };
        for kind in [EntryKind::File, EntryKind::Directory] {
            assert_eq!(
                file_metadata("op", kind, &complete).unwrap(),
                (9, 13),
                "a complete response is read the same way for either kind"
            );
        }
        assert_eq!(
            file_metadata("op", EntryKind::Directory, &RemoteAdditionalWire::default()).unwrap(),
            (0, 0)
        );
        assert!(matches!(
            file_metadata("op", EntryKind::File, &RemoteAdditionalWire::default()),
            Err(Error::InvalidResponse { .. })
        ));
    }

    #[test]
    fn remote_inventory_rejects_stalled_or_inconsistent_directory_pages() {
        let bad_pages = [
            serde_json::json!({"success":true,"data":{"total":1,"files":[]}}),
            serde_json::json!({"success":true,"data":{"total":1,"files":[{"path":"/share/root","name":"root","isdir":true}]}}),
            serde_json::json!({"success":true,"data":{"total":1,"files":[{"path":"/share/root/a","name":"wrong","isdir":false,"additional":{"size":1,"time":{"mtime":1}}}]}}),
            serde_json::json!({"success":true,"data":{"total":2,"files":[
                {"path":"/share/root/a","name":"a","isdir":false,"additional":{"size":1,"time":{"mtime":1}}},
                {"path":"/share/root/a","name":"a","isdir":false,"additional":{"size":1,"time":{"mtime":1}}}
            ]}}),
        ];
        for page in bad_pages {
            let (url, server) = scripted_server(vec![
                required_discovery(),
                login_response(),
                getinfo_directory("/share"),
                getinfo_directory("/share/root"),
                page.to_string(),
            ]);
            let mut client = connect_test_client(url);
            client.login("alice", "password", None).unwrap();
            assert!(matches!(
                client.remote_inventory(
                    &RemoteRoot::parse("/share/root").unwrap(),
                    &CancellationToken::default()
                ),
                Err(Error::InvalidResponse { .. })
            ));
            assert_eq!(server.join().unwrap().len(), 5);
        }
    }

    /// A directory large enough to paginate used to be walked to completion no matter what, so a
    /// Ctrl-C during the remote scan was swallowed until the whole listing had been drained.
    #[test]
    fn remote_inventory_stops_between_directory_pages_once_cancellation_arrives() {
        let cancellation = CancellationToken::default();
        let hook_cancellation = cancellation.clone();
        let first_page = serde_json::json!({
            "success": true,
            "data": {"total": 2, "files": [
                {"path":"/share/root/a","name":"a","isdir":false,"additional":{"size":1,"time":{"mtime":1}}}
            ]}
        })
        .to_string();
        let responses = vec![
            (StatusCode::OK, required_discovery()),
            (StatusCode::OK, login_response()),
            (StatusCode::OK, getinfo_directory("/share")),
            (StatusCode::OK, getinfo_directory("/share/root")),
            (StatusCode::OK, first_page),
        ];
        // Cancel while the first page is being served, exactly as the signal handler would.
        let (url, server) = scripted_server_with_status_hook(responses, move |index| {
            if index == 4 {
                hook_cancellation.cancel();
            }
        });
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();

        assert!(matches!(
            client.remote_inventory(&RemoteRoot::parse("/share/root").unwrap(), &cancellation),
            Err(Error::Cancelled)
        ));
        // The server script holds no reply for a second page: the client never asked for one.
        assert_eq!(server.join().unwrap().len(), 5);
    }

    /// Control-request backoff runs deep inside `send_form_with_retry`, where no per-operation
    /// token is in scope. The client carries the process token so a cancelled run abandons the
    /// pause instead of sleeping through it and retrying work nobody is waiting for.
    #[test]
    fn control_request_backoff_is_abandoned_once_the_client_is_cancelled() {
        let (url, server) = scripted_server_with_status(vec![
            (StatusCode::OK, required_discovery()),
            (StatusCode::OK, login_response()),
            (
                StatusCode::BAD_GATEWAY,
                "temporary proxy failure".to_owned(),
            ),
        ]);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 1,
        })
        .unwrap();
        client.login("alice", "password", None).unwrap();

        let cancelled = CancellationToken::default();
        cancelled.cancel();
        let client = client.with_cancellation(&cancelled);

        // The per-operation token stays live, so only the client-held one can end this call.
        assert!(matches!(
            client.remote_inventory(
                &RemoteRoot::parse("/share/root").unwrap(),
                &CancellationToken::default()
            ),
            Err(Error::Cancelled)
        ));
        // A retryable gateway failure would otherwise have been retransmitted after a pause.
        assert_eq!(server.join().unwrap().len(), 3);
    }

    /// Cancellation must stop retransmission without stopping the single request a cleanup path
    /// depends on. A guard at the top of the retry loop would break exactly this: `allow_retry`
    /// is false for the non-recursive delete and the task-stop requests, so their attempt budget
    /// is zero and a loop-top check would abandon remote state instead of tidying it.
    #[test]
    fn a_cancelled_client_still_sends_the_one_request_a_non_retrying_call_owes() {
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();

        let cancelled = CancellationToken::default();
        cancelled.cancel();
        let client = client.with_cancellation(&cancelled);

        client
            .delete_non_recursive(
                &RemoteRoot::parse("/share/root").unwrap(),
                "/share/root/stale.bin",
            )
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(String::from_utf8_lossy(&requests[2].body).contains("method=delete"));
    }

    #[test]
    fn remote_inventory_distinguishes_missing_roots_from_invalid_ancestors() {
        let missing = r#"{"success":false,"error":{"code":408}}"#.to_owned();
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            missing.clone(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let inventory = client
            .remote_inventory(
                &RemoteRoot::parse("/share/root").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap();
        assert!(!inventory.root_exists && inventory.entries.is_empty());
        assert_eq!(server.join().unwrap().len(), 3);

        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            getinfo_file("/share", 1, None),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(
            client
                .remote_inventory(
                    &RemoteRoot::parse("/share/root").unwrap(),
                    &CancellationToken::default()
                )
                .unwrap_err()
                .to_string()
                .contains("not a directory")
        );
        assert_eq!(server.join().unwrap().len(), 3);

        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            getinfo_directory("/share"),
            getinfo_directory("/share/root"),
            missing,
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let inventory = client
            .remote_inventory(
                &RemoteRoot::parse("/share/root").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap();
        assert!(!inventory.root_exists && inventory.entries.is_empty());
        assert_eq!(server.join().unwrap().len(), 5);
    }

    #[test]
    fn discovery_falls_back_to_query_cgi_and_mutations_use_bounded_forms() {
        let (url, server) = scripted_server_with_status(vec![
            (StatusCode::BAD_GATEWAY, "backend unavailable".to_owned()),
            (StatusCode::OK, write_probe_discovery(false)),
            (StatusCode::OK, login_response()),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        client.create_folder("/share/root/new").unwrap();
        client
            .delete_non_recursive(
                &RemoteRoot::parse("/share/root").unwrap(),
                "/share/root/new",
            )
            .unwrap();
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .request_line
                .contains("/prefix/webapi/entry.cgi")
        );
        assert!(
            requests[1]
                .request_line
                .contains("/prefix/webapi/query.cgi")
        );
        assert!(String::from_utf8_lossy(&requests[3].body).contains("method=create"));
        assert!(String::from_utf8_lossy(&requests[4].body).contains("recursive=false"));
    }

    #[test]
    fn copy_tasks_are_stopped_after_timeout_missing_status_and_poll_failure() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let digest = ContentMd5::from_bytes([0_u8; 16]);
        let copy = |client: &ApiClient| {
            client.copy_file_verified(
                &root,
                "/share/root/a/file.bin",
                "/share/root/b/file.bin",
                1,
                digest,
                &CancellationToken::default(),
            )
        };

        let (url, server) = scripted_server(vec![
            write_probe_discovery(true),
            login_response(),
            task_start_response("timeout-copy"),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        client.operation_timeout = Duration::ZERO;
        assert!(matches!(
            copy(&client),
            Err(Error::OperationTimedOut {
                operation: "server-side file copy"
            })
        ));
        assert!(String::from_utf8_lossy(&server.join().unwrap()[3].body).contains("method=stop"));

        for status in [
            r#"{"success":true}"#.to_owned(),
            r#"{"success":false,"error":{"code":402}}"#.to_owned(),
        ] {
            let (url, server) = scripted_server(vec![
                write_probe_discovery(true),
                login_response(),
                task_start_response("failed-copy"),
                status,
                r#"{"success":true}"#.to_owned(),
            ]);
            let mut client = connect_test_client(url);
            client.login("alice", "password", None).unwrap();
            assert!(copy(&client).is_err());
            assert!(
                String::from_utf8_lossy(&server.join().unwrap()[4].body).contains("method=stop")
            );
        }

        let cancellation = CancellationToken::default();
        let hook = cancellation.clone();
        let (url, server) = scripted_server_with_status_hook(
            vec![
                (StatusCode::OK, write_probe_discovery(true)),
                (StatusCode::OK, login_response()),
                (StatusCode::OK, task_start_response("poll-copy")),
                (
                    StatusCode::OK,
                    r#"{"success":true,"data":{"finished":false}}"#.to_owned(),
                ),
                (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            ],
            move |index| {
                if index == 3 {
                    hook.cancel()
                }
            },
        );
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(matches!(
            client.copy_file_verified(
                &root,
                "/share/root/a/file.bin",
                "/share/root/b/file.bin",
                1,
                digest,
                &cancellation,
            ),
            Err(Error::Cancelled)
        ));
        assert!(String::from_utf8_lossy(&server.join().unwrap()[4].body).contains("method=stop"));
    }

    #[test]
    fn remote_content_verification_fails_closed_for_missing_directory_and_size_mismatch() {
        let root_digest = ContentMd5::from_content(b"payload");
        let cases = [
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            getinfo_directory("/share/file.bin"),
            getinfo_file("/share/file.bin", 8, None),
        ];
        for response in cases {
            let (url, server) =
                scripted_server(vec![required_discovery(), login_response(), response]);
            let mut client = connect_test_client(url);
            client.login("alice", "password", None).unwrap();
            assert!(matches!(client.verify_remote_content(
                "/share/file.bin", 7, root_digest, &CancellationToken::default(),
            ), Err(Error::ContentVerificationFailed(path)) if path == "/share/file.bin"));
            assert_eq!(server.join().unwrap().len(), 3);
        }

        let missing_size = serde_json::json!({"success":true,"data":{"files":[{
            "path":"/share/file.bin","name":"file.bin","isdir":false,"additional":{}
        }]}})
        .to_string();
        let (url, server) =
            scripted_server(vec![required_discovery(), login_response(), missing_size]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(matches!(
            client.verify_remote_content(
                "/share/file.bin",
                7,
                root_digest,
                &CancellationToken::default(),
            ),
            Err(Error::InvalidResponse { .. })
        ));
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn metadata_revalidation_rejects_missing_ambiguous_and_misdirected_results() {
        let responses = [
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true,"data":{"files":[]}}"#.to_owned(),
            getinfo_file("/share/other.bin", 7, None),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
        ];
        for response in responses {
            let (url, server) =
                scripted_server(vec![required_discovery(), login_response(), response]);
            let mut client = connect_test_client(url);
            client.login("alice", "password", None).unwrap();
            assert!(
                client
                    .verify_remote_metadata_snapshot(
                        "/share/file.bin",
                        EntryKind::File,
                        7,
                        0,
                        false,
                        &CancellationToken::default(),
                    )
                    .is_err()
            );
            assert_eq!(server.join().unwrap().len(), 3);
        }

        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            getinfo_directory("/share/folder"),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        client
            .verify_remote_metadata_snapshot(
                "/share/folder",
                EntryKind::Directory,
                0,
                0,
                false,
                &CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn failed_relogin_clears_session_and_logout_without_session_is_idempotent() {
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(matches!(
            client.login("alice", "new-password", Some("123456")),
            Err(Error::InvalidResponse { .. })
        ));
        assert!(client.required_session().is_err());
        client.logout().unwrap();
        assert_eq!(server.join().unwrap().len(), 3);

        let (url, server) = scripted_server(vec![
            required_discovery(),
            r#"{"success":true,"data":{"sid":""}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        assert!(matches!(
            client.login("alice", "password", None),
            Err(Error::InvalidResponse { .. })
        ));
        assert_eq!(server.join().unwrap().len(), 2);

        let (url, server) = scripted_server(vec![required_discovery()]);
        let mut client = connect_test_client(url);
        let auth = client.apis.get_mut("SYNO.API.Auth").unwrap();
        auth.min_version = 1;
        auth.max_version = 2;
        assert!(matches!(
            client.login("alice", "password", None),
            Err(Error::UnsupportedApiVersion { .. })
        ));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn tls_configuration_and_probe_failures_keep_diagnostics_bounded() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing = std::env::temp_dir().join(format!("missing-sdsync-ca-{nonce}.pem"));
        let options = |path| ClientOptions {
            base_url: "https://files.example.test".to_owned(),
            allow_http: false,
            accept_invalid_certs: false,
            ca_certificate: Some(path),
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            retries: 0,
        };
        assert!(
            matches!(ApiClient::connect(&options(missing.clone())), Err(Error::FileIo { path, .. }) if path == missing)
        );
        let invalid = std::env::temp_dir().join(format!("invalid-sdsync-ca-{nonce}.pem"));
        fs::write(
            &invalid,
            b"-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        // reqwest defers certificate parsing, so a PEM block with an unusable payload is caught
        // when the TLS client is built rather than when the file is read. Either way the client
        // must not come up with an unverifiable trust anchor.
        let Err(error) = ApiClient::connect(&options(invalid.clone())) else {
            panic!("an unparsable CA certificate must never produce a client");
        };
        let Error::Http { operation, .. } = &error else {
            panic!("expected a TLS setup failure, got {error}");
        };
        assert_eq!(operation, "building HTTP client");
        fs::remove_file(invalid).unwrap();

        // A file that parses cleanly but yields no certificate is the dangerous case: the
        // operator asked for a pinned CA and would otherwise get one that was never added.
        for (label, contents) in [
            ("empty", &b""[..]),
            ("textual", &b"this file is not a certificate\n"[..]),
            (
                "key-only",
                &b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n-----END PRIVATE KEY-----\n"[..],
            ),
        ] {
            let path = std::env::temp_dir().join(format!("{label}-sdsync-ca-{nonce}.pem"));
            fs::write(&path, contents).unwrap();
            let Err(error) = ApiClient::connect(&options(path.clone())) else {
                panic!("a {label} CA file must never produce a client");
            };
            let Error::Message(message) = &error else {
                panic!("expected a rejected CA file, got {error}");
            };
            assert_eq!(
                *message,
                format!(
                    "CA certificate file {path:?} contains no certificate; --ca-certificate must name a PEM file with at least one CERTIFICATE block"
                )
            );
            assert!(
                message.contains(&format!("{path:?}")),
                "the rejected CA path must be named: {message}"
            );
            fs::remove_file(&path).unwrap();
        }

        let root = RemoteRoot::parse("/share/root").unwrap();
        let mut report = initial_write_probe_report(
            &root,
            "/share/root/probe".to_owned(),
            1,
            ContentMd5::from_bytes([0_u8; 16]),
            0,
            false,
        );
        report.leftover_remote_probe_path = Some("/share/root/probe".to_owned());
        let failure = WriteProbeFailure {
            cause: Error::Cancelled,
            cleanup_error: Some(Error::Message("cleanup failed".to_owned())),
            report,
        };
        let rendered = failure.to_string();
        assert!(rendered.contains("operation cancelled"));
        assert!(rendered.contains("cleanup also failed"));
        assert!(rendered.contains("leftover probe path"));
        assert_eq!(
            std::error::Error::source(&failure).unwrap().to_string(),
            "operation cancelled"
        );
    }

    #[test]
    fn operator_diagnostics_distinguish_proxy_auth_and_storage_failures() {
        assert!(http_status_hint(StatusCode::FOUND, b"").contains("redirects are disabled"));
        assert!(http_status_hint(StatusCode::PAYLOAD_TOO_LARGE, b"").contains("body-size limit"));
        assert!(http_status_hint(StatusCode::BAD_GATEWAY, b"").contains("could not reach"));
        assert!(http_status_hint(StatusCode::GATEWAY_TIMEOUT, b"").contains("timed out"));
        assert_eq!(
            http_status_hint(StatusCode::BAD_REQUEST, b""),
            "empty response body"
        );
        assert_eq!(
            http_status_hint(StatusCode::BAD_REQUEST, b"bad\nbody"),
            "bad\\nbody"
        );
        assert!(looks_like_html(b"  <!DOCTYPE HTML><title>proxy</title>"));
        assert!(!looks_like_html(b"{\"success\":false}"));

        for (api, code, expected) in [
            ("SYNO.API.Auth", 400, "password is incorrect"),
            ("SYNO.API.Auth", 403, "OTP is required"),
            ("SYNO.API.Auth", 407, "source IP is blocked"),
            ("SYNO.API.Auth", 410, "must be changed"),
            ("SYNO.FileStation.List", 106, "session timed out"),
            ("SYNO.FileStation.List", 150, "reverse-proxy routing"),
            ("SYNO.FileStation.List", 408, "does not exist"),
            ("SYNO.FileStation.List", 415, "quota"),
            ("SYNO.FileStation.List", 418, "illegal remote name"),
            ("SYNO.FileStation.Delete", 900, "delete failed"),
            (
                "SYNO.FileStation.CreateFolder",
                1100,
                "folder creation failed",
            ),
            ("SYNO.FileStation.Upload", 1800, "Content-Length"),
            ("SYNO.FileStation.Upload", 1805, "overwrite/skip policy"),
        ] {
            assert!(api_error_description(api, code).unwrap().contains(expected));
        }
        assert!(api_error_description("SYNO.API.Auth", 9999).is_none());
        assert!(api_error_description("SYNO.FileStation.List", 9999).is_none());
    }

    #[test]
    fn authenticated_responses_never_echo_raw_server_content() {
        let session_marker = "reflected-secret-sid-and-synotoken";

        let status_error = decode_scripted_error(
            StatusCode::BAD_GATEWAY,
            format!("proxy reflected _sid={session_marker}&SynoToken={session_marker}"),
            "SYNO.FileStation.List",
            "list",
        );
        let rendered = rendered_error(&status_error);
        assert!(rendered.contains("HTTP 502 Bad Gateway"));
        assert!(rendered.contains("authenticated API response body withheld"));
        assert!(!rendered.contains(session_marker));

        let malformed_error = decode_scripted_error(
            StatusCode::OK,
            format!("<html>reflected {session_marker}</html>"),
            "SYNO.FileStation.Upload",
            "upload",
        );
        let rendered = rendered_error(&malformed_error);
        assert!(rendered.contains("authenticated API response body withheld"));
        assert!(rendered.contains("proxy returned HTML"));
        assert!(!rendered.contains(session_marker));

        let api_error = decode_scripted_error(
            StatusCode::OK,
            serde_json::json!({
                "success": false,
                "error": {"code": 900, "errors": {"reflected": session_marker}}
            })
            .to_string(),
            "SYNO.FileStation.Delete",
            "delete",
        );
        let rendered = rendered_error(&api_error);
        assert!(rendered.contains("code 900: delete failed"));
        assert!(!rendered.contains(session_marker));
        assert!(matches!(api_error, Error::Api { details, .. } if details.is_empty()));
    }

    #[test]
    fn authentication_is_redacted_while_discovery_keeps_safe_route_diagnostics() {
        let password_marker = "reflected-password-marker";
        let auth_error = decode_scripted_error(
            StatusCode::OK,
            format!("<html>passwd={password_marker}&otp_code=654321</html>"),
            "SYNO.API.Auth",
            "login",
        );
        let rendered = rendered_error(&auth_error);
        assert!(rendered.contains("authentication response body withheld"));
        assert!(rendered.contains("proxy returned HTML"));
        assert!(!rendered.contains(password_marker));
        assert!(!rendered.contains("654321"));

        let discovery_marker = "safe-unauthenticated-route-diagnostic";
        let discovery_error = decode_scripted_error(
            StatusCode::OK,
            format!("<html>{discovery_marker}</html>"),
            "SYNO.API.Info",
            "query",
        );
        let rendered = rendered_error(&discovery_error);
        assert!(rendered.contains(discovery_marker));
        assert!(rendered.contains("proxy returned HTML"));
    }

    #[test]
    fn observed_reader_reports_bytes_and_can_cancel() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let observer: UploadObserver = Arc::new(move |event| {
            captured.lock().unwrap().push(event);
            true
        });
        let mut reader = ObservedReader {
            inner: std::io::Cursor::new(b"payload"),
            observer: Some(observer),
            cancelled: Arc::new(AtomicBool::new(false)),
            throttle: None,
        };
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"payload");
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &[UploadTransferEvent::Advanced { bytes: 7 }]
        );

        let observer: UploadObserver = Arc::new(|_| false);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut reader = ObservedReader {
            inner: std::io::Cursor::new(b"cancel"),
            observer: Some(observer),
            cancelled: Arc::clone(&cancelled),
            throttle: None,
        };
        let error = reader.read(&mut [0_u8; 8]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert!(cancelled.load(Ordering::Acquire));

        let wrapped = Err(Error::Message(
            "reqwest wrapped the interrupted body read".to_owned(),
        ));
        assert!(matches!(
            prioritize_observer_cancellation(&cancelled, wrapped),
            Err(Error::Cancelled)
        ));
    }

    fn rate(bytes_per_second: u64) -> NonZeroU64 {
        NonZeroU64::new(bytes_per_second).expect("test rates are non-zero")
    }

    /// The bucket is a pure function of the instants it is handed, so its whole behaviour is
    /// pinned here without waiting on a real clock.
    #[test]
    fn a_token_bucket_starts_full_and_refills_at_the_configured_rate() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(rate(1000), start);

        // It opens holding one second of traffic and hands that burst over in one grant.
        assert_eq!(bucket.take(1000, start), RateGrant::Ready(1000));
        // Drained, it quotes the wait for a whole chunk rather than granting nothing.
        assert_eq!(
            bucket.take(1000, start),
            RateGrant::Wait(Duration::from_secs(1))
        );
        // Half a second of refill affords exactly half the chunk.
        assert_eq!(
            bucket.take(1000, start + Duration::from_millis(500)),
            RateGrant::Ready(500)
        );
        // An idle minute does not bank a minute of credit: refill stops at the burst size.
        assert_eq!(
            bucket.take(4000, start + Duration::from_secs(60)),
            RateGrant::Ready(1000)
        );
    }

    /// The wait a starved reader is quoted is what bounds how long it sleeps between
    /// cancellation checks, so it must never scale with the size of the read.
    #[test]
    fn a_token_bucket_never_queues_a_reader_for_more_than_one_burst() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(rate(4), start);

        assert_eq!(bucket.take(u64::MAX, start), RateGrant::Ready(4));
        assert_eq!(
            bucket.take(u64::MAX, start),
            RateGrant::Wait(Duration::from_secs(1))
        );
        // A partial refill is credited to the byte instead of being rounded away.
        let quarter = start + Duration::from_millis(250);
        assert_eq!(bucket.take(2, quarter), RateGrant::Ready(1));
        assert_eq!(
            bucket.take(2, quarter),
            RateGrant::Wait(Duration::from_millis(500))
        );
        // A clock that fails to advance must not mint credit out of nothing.
        assert_eq!(
            bucket.take(2, start),
            RateGrant::Wait(Duration::from_millis(500))
        );
        // An empty read never waits.
        assert_eq!(bucket.take(0, quarter), RateGrant::Ready(0));
    }

    #[test]
    fn an_absent_or_zero_rate_leaves_uploads_unlimited() {
        assert!(upload_rate_bucket(None).is_none());
        assert!(upload_rate_bucket(Some(0)).is_none());
        assert!(upload_rate_bucket(Some(1)).is_some());
        assert!(upload_throttle(None, &CancellationToken::default()).is_none());
    }

    /// The budget has to be shared by the worker clones, not handed out per clone -- otherwise
    /// `--jobs` would quietly multiply the limit instead of dividing it.
    #[test]
    fn worker_clones_report_and_share_one_budget() {
        let client = ApiClient {
            http: crate::blocking_client_builder()
                .expect("ring provider installs")
                .build()
                .expect("client builds"),
            download_http: crate::async_client_builder()
                .expect("ring provider installs")
                .build()
                .expect("client builds"),
            base: Url::parse("https://files.example.test/webapi/").unwrap(),
            apis: HashMap::new(),
            session: None,
            retries: 0,
            control_timeout: Duration::from_secs(1),
            control_deadline: None,
            upload_timeout: Duration::from_secs(1),
            operation_timeout: Duration::from_secs(1),
            upload_rate_limit: None,
            cancellation: CancellationToken::default(),
            observer: None,
        };
        assert_eq!(client.max_upload_rate(), None);

        let limited = client.clone().with_max_upload_rate(Some(4096));
        assert_eq!(limited.max_upload_rate(), Some(4096));
        // A zero rate is not a limit of zero, it is no limit at all.
        assert_eq!(
            limited
                .clone()
                .with_max_upload_rate(Some(0))
                .max_upload_rate(),
            None
        );

        // Spending the budget through one clone must leave nothing for the other.
        let worker = limited.clone();
        let budget = limited.upload_rate_limit.clone().unwrap();
        let worker_budget = worker.upload_rate_limit.clone().unwrap();
        assert!(Arc::ptr_eq(&budget, &worker_budget));
        let now = Instant::now();
        assert_eq!(
            budget.lock().unwrap().take(4096, now),
            RateGrant::Ready(4096)
        );
        assert_eq!(
            worker_budget.lock().unwrap().take(4096, now),
            RateGrant::Wait(Duration::from_secs(1)),
            "a clone must draw on the same drained budget, not a fresh one"
        );
    }

    fn throttled_reader(
        payload: &[u8],
        bytes_per_second: Option<u64>,
        cancellation: &CancellationToken,
        cancelled: &Arc<AtomicBool>,
    ) -> ObservedReader<std::io::Cursor<Vec<u8>>> {
        let bucket = upload_rate_bucket(bytes_per_second);
        ObservedReader {
            inner: std::io::Cursor::new(payload.to_vec()),
            observer: None,
            cancelled: Arc::clone(cancelled),
            throttle: upload_throttle(bucket.as_ref(), cancellation),
        }
    }

    /// The regression that matters most: with no limit configured the reader must behave
    /// exactly as it always has -- full-buffer reads, byte-identical payload.
    #[test]
    fn an_unlimited_observed_reader_is_unchanged_by_the_rate_limiter() {
        let payload: Vec<u8> = (0..64_u32 * 1024).map(|index| index as u8).collect();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut reader =
            throttled_reader(&payload, None, &CancellationToken::default(), &cancelled);

        // The first read fills the whole buffer: nothing shortens it.
        let mut window = [0_u8; 4096];
        assert_eq!(reader.read(&mut window).unwrap(), window.len());
        assert_eq!(window.as_slice(), &payload[..window.len()]);

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest.as_slice(), &payload[window.len()..]);
        assert!(!cancelled.load(Ordering::Acquire));
    }

    /// A limit that the opening burst already covers must change the bytes not at all -- only
    /// the pace at which they are handed over.
    #[test]
    fn a_throttled_reader_delivers_the_same_bytes_as_an_unlimited_one() {
        let payload: Vec<u8> = (0..64_u32 * 1024).map(|index| index as u8).collect();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut reader = throttled_reader(
            &payload,
            Some(1024 * 1024),
            &CancellationToken::default(),
            &cancelled,
        );

        let mut delivered = Vec::new();
        reader.read_to_end(&mut delivered).unwrap();
        assert_eq!(delivered, payload);
        assert!(!cancelled.load(Ordering::Acquire));
    }

    /// A reader that outruns the budget waits for refill and then continues, rather than
    /// reporting a short read as end of file.
    #[test]
    fn a_throttled_reader_waits_for_refill_and_then_continues() {
        let payload = vec![9_u8; 4096];
        let cancelled = Arc::new(AtomicBool::new(false));
        // One kilobyte per second: one millisecond of refill buys one byte.
        let mut reader = throttled_reader(
            &payload,
            Some(1000),
            &CancellationToken::default(),
            &cancelled,
        );

        // The opening burst is exactly one second of traffic, however large the buffer.
        let mut window = [0_u8; 4096];
        assert_eq!(reader.read(&mut window).unwrap(), 1000);

        // The budget is spent, so this read can only be served after a real wait.
        let mut next = [0_u8; 4];
        let count = reader.read(&mut next).unwrap();
        assert!(
            (1..=next.len()).contains(&count),
            "a waiting reader must return bytes, not end of file: {count}"
        );
        assert!(next[..count].iter().all(|byte| *byte == 9));
    }

    /// Throttling must not blunt cancellation: a reader parked on the bucket has to notice the
    /// token and surface the same interruption an unthrottled reader does.
    #[test]
    fn a_throttled_reader_is_interrupted_promptly_by_cancellation() {
        // A waiting reader re-checks the token at least this often, so a limit can never park
        // a cancelled transfer behind one long sleep.
        assert!(RATE_LIMIT_POLL_INTERVAL <= Duration::from_millis(50));

        let cancellation = CancellationToken::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        // One byte per second: after the opening byte every read has to wait.
        let mut reader = throttled_reader(&[7_u8; 64], Some(1), &cancellation, &cancelled);
        assert_eq!(reader.read(&mut [0_u8; 64]).unwrap(), 1);

        let signal = cancellation.clone();
        let canceller = thread::spawn(move || signal.cancel());
        let error = reader
            .read(&mut [0_u8; 64])
            .expect_err("a cancelled transfer must not keep waiting on the bucket");
        canceller.join().unwrap();

        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        // The interruption has to reach the caller as a cancellation, not as a transport error.
        assert!(cancelled.load(Ordering::Acquire));
        assert!(matches!(
            prioritize_observer_cancellation(&cancelled, Ok(())),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn endpoint_preserves_reverse_proxy_prefix() {
        let base = normalize_base_url("https://files.example.test/nas", false).unwrap();
        assert_eq!(
            endpoint_url(&base, "entry.cgi").unwrap().as_str(),
            "https://files.example.test/nas/webapi/entry.cgi"
        );
        let root = normalize_base_url("https://files.example.test", false).unwrap();
        assert_eq!(
            endpoint_url(&root, "FileStation/file_share.cgi")
                .unwrap()
                .as_str(),
            "https://files.example.test/webapi/FileStation/file_share.cgi"
        );
        let encoded = normalize_base_url("https://files.example.test/nas%20one", false).unwrap();
        assert_eq!(
            endpoint_url(&encoded, "entry.cgi").unwrap().as_str(),
            "https://files.example.test/nas%20one/webapi/entry.cgi"
        );
    }

    #[test]
    fn endpoint_rejects_discovery_escape() {
        let base = normalize_base_url("https://files.example.test/nas/", false).unwrap();
        for path in [
            "/entry.cgi",
            "../entry.cgi",
            "https://evil.test/x",
            "x?secret=1",
        ] {
            assert!(endpoint_url(&base, path).is_err(), "{path}");
        }
    }

    #[test]
    fn url_security_defaults_are_strict() {
        assert!(normalize_base_url("http://nas.test", false).is_err());
        assert!(normalize_base_url("http://nas.test", true).is_ok());
        assert!(normalize_base_url("https://user:pass@nas.test", false).is_err());
        assert!(normalize_base_url("https://nas.test/?x=1", false).is_err());
    }

    #[test]
    fn session_headers_are_exact_sensitive_and_injection_safe() {
        let session = Session {
            sid: Zeroizing::new("sid-with_-safe.characters".to_owned()),
            syno_token: Some(Zeroizing::new("token-with.+/=".to_owned())),
        };
        let headers = session.request_headers().unwrap();
        assert_eq!(
            headers.cookie.to_str().unwrap(),
            "id=sid-with_-safe.characters"
        );
        assert_eq!(
            headers.syno_token.as_ref().unwrap().to_str().unwrap(),
            "token-with.+/="
        );
        assert!(headers.cookie.is_sensitive());
        assert!(headers.syno_token.as_ref().unwrap().is_sensitive());
        let rendered = format!(
            "{:?}{:?}",
            headers.cookie,
            headers.syno_token.as_ref().unwrap()
        );
        assert!(!rendered.contains("sid-with"));
        assert!(!rendered.contains("token-with"));

        for sid in [
            "sid; forged=value",
            "sid,forged",
            "sid\\forged",
            "sid\r\nX-Forged: value",
        ] {
            let invalid = Session {
                sid: Zeroizing::new(sid.to_owned()),
                syno_token: None,
            };
            let error = invalid
                .request_headers()
                .err()
                .expect("unsafe SID must be rejected");
            let rendered = format!("{error:?}");
            assert!(rendered.contains("cannot be used safely"));
            assert!(!rendered.contains(sid));
        }

        let invalid_token = Session {
            sid: Zeroizing::new("safe-sid".to_owned()),
            syno_token: Some(Zeroizing::new("token\r\nX-Forged: value".to_owned())),
        };
        let error = invalid_token
            .request_headers()
            .err()
            .expect("unsafe SynoToken must be rejected");
        let rendered = format!("{error:?}");
        assert!(rendered.contains("cannot be used safely"));
        assert!(!rendered.contains("X-Forged"));
    }

    #[test]
    fn control_requests_are_capped_below_long_upload_timeouts() {
        assert_eq!(
            control_request_timeout(Duration::from_secs(7_200)),
            Duration::from_secs(10)
        );
        assert_eq!(
            control_request_timeout(Duration::from_secs(2)),
            Duration::from_secs(2)
        );
        assert_eq!(STOP_REQUEST_TIMEOUT, Duration::from_secs(3));
    }

    #[test]
    fn live_metadata_snapshot_rejects_a_replaced_file_without_retry() {
        let responses = vec![
            required_discovery(),
            r#"{"success":true,"data":{"sid":"secret-sid"}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/root/file.bin","name":"file.bin","isdir":false,"additional":{"size":9,"time":{"mtime":123}}}]}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 3,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();

        let error = client
            .verify_remote_metadata_snapshot(
                "/share/root/file.bin",
                EntryKind::File,
                8,
                123,
                true,
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert!(
            matches!(error, Error::RemoteSnapshotChanged(path) if path == "/share/root/file.bin")
        );
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        let metadata_probe = String::from_utf8_lossy(&requests[2].body);
        assert!(metadata_probe.contains("method=getinfo"));
        assert!(metadata_probe.contains("%2Fshare%2Froot%2Ffile.bin"));
    }

    #[test]
    fn destination_permission_checks_the_exact_existing_root_without_mutation() {
        let responses = vec![
            required_discovery(),
            r#"{"success":true,"data":{"sid":"secret-sid","synotoken":"csrf-secret"}}"#
                .to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share","name":"share","isdir":true,"additional":{}}]}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/restricted","name":"restricted","isdir":true,"additional":{}}]}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();

        let check = client
            .verify_destination_writable(&RemoteRoot::parse("/share/restricted").unwrap())
            .unwrap();
        assert_eq!(
            check,
            DestinationWriteCheck {
                checked_directory: "/share/restricted".to_owned(),
                destination_exists: true,
            }
        );
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(
            requests.iter().all(|request| {
                request.request_line == "POST /prefix/webapi/entry.cgi HTTP/1.1"
            })
        );
        let permission = String::from_utf8_lossy(&requests[4].body);
        assert!(permission.contains("api=SYNO.FileStation.CheckPermission"));
        assert!(permission.contains("version=3"));
        assert!(permission.contains("method=write"));
        assert!(permission.contains("path=%22%2Fshare%2Frestricted%22"));
        assert!(permission.contains("filename=%22.synology-drive-sync-write-check-"));
        assert!(permission.contains("create_only=true"));
        assert!(!permission.contains("list_share"));
    }

    #[test]
    fn destination_permission_rejects_a_mounted_ancestor_before_the_write_check() {
        let responses = vec![
            required_discovery(),
            r#"{"success":true,"data":{"sid":"secret-sid"}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share","name":"share","isdir":true,"additional":{"mount_point_type":"cifs"}}]}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = connect_test_client(url);
        client.login("mirror-user", "password", None).unwrap();
        assert!(matches!(
            client.verify_destination_writable(
                &RemoteRoot::parse("/share/nested/destination").unwrap()
            ),
            Err(Error::RemoteMountRoot { path, mount_type })
                if path == "/share" && mount_type == "cifs"
        ));
        client.logout().unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(
            requests.iter().all(|request| {
                !String::from_utf8_lossy(&request.body).contains("method=write")
            })
        );
    }

    #[test]
    fn missing_destination_checks_first_missing_child_at_nearest_existing_ancestor() {
        let responses = vec![
            required_discovery(),
            r#"{"success":true,"data":{"sid":"secret-sid"}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share","name":"share","isdir":true,"additional":{}}]}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/team","name":"team","isdir":true,"additional":{}}]}}"#.to_owned(),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();

        let check = client
            .verify_destination_writable(&RemoteRoot::parse("/share/team/new/deeper").unwrap())
            .unwrap();
        assert_eq!(
            check,
            DestinationWriteCheck {
                checked_directory: "/share/team".to_owned(),
                destination_exists: false,
            }
        );
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
        let missing_probe = String::from_utf8_lossy(&requests[4].body);
        assert!(missing_probe.contains("method=getinfo"));
        assert!(missing_probe.contains("%2Fshare%2Fteam%2Fnew"));
        let permission = String::from_utf8_lossy(&requests[5].body);
        assert!(permission.contains("api=SYNO.FileStation.CheckPermission"));
        assert!(permission.contains("path=%22%2Fshare%2Fteam%22"));
        assert!(permission.contains("filename=%22new%22"));
        assert!(permission.contains("create_only=true"));
        assert!(
            requests
                .iter()
                .all(|request| !String::from_utf8_lossy(&request.body).contains("new%2Fdeeper"))
        );
    }

    #[test]
    fn destination_permission_denial_is_redacted_and_never_falls_back_to_share_access() {
        let reflected_marker = "reflected-session-or-proxy-secret";
        let responses = vec![
            required_discovery(),
            r#"{"success":true,"data":{"sid":"secret-sid"}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share","name":"share","isdir":true,"additional":{}}]}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/restricted","name":"restricted","isdir":true,"additional":{}}]}}"#.to_owned(),
            serde_json::json!({
                "success": false,
                "error": {"code": 105, "errors": {"reflected": reflected_marker}}
            })
            .to_string(),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();

        let error = client
            .verify_destination_writable(&RemoteRoot::parse("/share/restricted").unwrap())
            .unwrap_err();
        let rendered = rendered_error(&error);
        assert!(rendered.contains("SYNO.FileStation.CheckPermission.write"));
        assert!(rendered.contains("code 105"));
        assert!(!rendered.contains(reflected_marker));
        assert!(matches!(error, Error::Api { details, .. } if details.is_empty()));
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert!(requests.iter().all(|request| {
            !String::from_utf8_lossy(&request.body).contains("method=list_share")
        }));
    }

    #[test]
    fn connection_requires_check_permission_v3_capability() {
        let discovery = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
            }
        })
        .to_string();
        let (url, server) = scripted_server(vec![discovery]);
        let result = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        });
        assert!(
            matches!(result, Err(Error::MissingApi(api)) if api == "SYNO.FileStation.CheckPermission")
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn disposable_write_probe_verifies_upload_copy_and_non_recursive_cleanup() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-success";
        let upload_path = format!("{probe_path}/{WRITE_PROBE_FILE_NAME}");
        let copy_directory = format!("{probe_path}/{WRITE_PROBE_COPY_DIRECTORY}");
        let copy_path = format!("{copy_directory}/{WRITE_PROBE_FILE_NAME}");
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let local_path = local.entry.full_path.clone();
        let size = local.entry.size;
        let mtime_seconds = local.entry.mtime_ms.div_euclid(1000);
        let digest = local.entry.content_md5.unwrap().to_string();
        let responses = vec![
            write_probe_discovery(true),
            r#"{"success":true,"data":{"sid":"secret-sid","synotoken":"csrf-secret"}}"#.to_owned(),
            getinfo_directory("/share"),
            getinfo_directory("/share/root"),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            getinfo_directory(probe_path),
            r#"{"success":true,"data":{"total":0,"offset":0,"files":[]}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            getinfo_file(&upload_path, size, None),
            scripted_binary_response(WRITE_PROBE_PAYLOAD),
            getinfo_file(&upload_path, size, Some(mtime_seconds)),
            r#"{"success":true}"#.to_owned(),
            getinfo_directory(&copy_directory),
            r#"{"success":true,"data":{"total":0,"offset":0,"files":[]}}"#.to_owned(),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true,"data":{"taskid":"copy-task"}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":true}}"#.to_owned(),
            getinfo_file(&copy_path, size, None),
            scripted_binary_response(WRITE_PROBE_PAYLOAD),
            getinfo_file(&copy_path, size, Some(mtime_seconds)),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();

        let report = client
            .run_write_probe_with_local(
                &root,
                probe_path,
                &local.entry,
                &CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(report.target_path, "/share/root");
        assert_eq!(report.probe_path, probe_path);
        assert!(report.target_verified);
        assert!(report.directory_created);
        assert!(report.upload_attempted);
        assert!(report.upload_verified);
        assert_eq!(report.uploaded_size, size);
        assert_eq!(report.uploaded_md5.to_string(), digest);
        assert_eq!(report.uploaded_mtime_seconds, mtime_seconds);
        assert!(report.server_copy_supported);
        assert!(report.server_copy_attempted);
        assert!(report.server_copy_verified);
        assert!(report.cleanup_completed);
        assert_eq!(report.leftover_remote_probe_path, None);
        client.logout().unwrap();
        drop(local);
        assert!(!local_path.exists());

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 27);
        for index in [5, 12] {
            let create = String::from_utf8_lossy(&requests[index].body);
            assert!(create.contains("api=SYNO.FileStation.CreateFolder"));
            assert!(create.contains("force_parent=false"));
        }
        let upload = &requests[8];
        assert!(find_bytes(&upload.body, b"name=\"overwrite\"").is_some());
        assert!(find_bytes(&upload.body, b"\r\n\r\nfalse\r\n").is_some());
        assert!(find_bytes(&upload.body, WRITE_PROBE_PAYLOAD).is_some());
        let copy = String::from_utf8_lossy(&requests[16].body);
        assert!(copy.contains("api=SYNO.FileStation.CopyMove"));
        assert!(copy.contains("remove_src=false"));
        assert!(!copy.contains("overwrite"));
        for delete in &requests[21..=24] {
            let body = String::from_utf8_lossy(&delete.body);
            assert!(body.contains("api=SYNO.FileStation.Delete"));
            assert!(body.contains("recursive=false"));
        }
    }

    #[test]
    fn cancelled_write_probe_attempts_cleanup_and_surfaces_a_leftover_path() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-cancel";
        let copy_directory = format!("{probe_path}/{WRITE_PROBE_COPY_DIRECTORY}");
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let cancellation = CancellationToken::default();
        let cancel_before_create_response = cancellation.clone();
        let responses = vec![
            (StatusCode::OK, write_probe_discovery(false)),
            (
                StatusCode::OK,
                r#"{"success":true,"data":{"sid":"secret-sid"}}"#.to_owned(),
            ),
            (StatusCode::OK, getinfo_directory("/share")),
            (StatusCode::OK, getinfo_directory("/share/root")),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            ),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            ),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            ),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            ),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":421}}"#.to_owned(),
            ),
            (StatusCode::OK, getinfo_directory(probe_path)),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
        ];
        let (url, server) = scripted_server_with_status_hook(responses, move |index| {
            if index == 5 {
                cancel_before_create_response.cancel();
            }
        });
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();

        let failure = client
            .run_write_probe_with_local(&root, probe_path, &local.entry, &cancellation)
            .unwrap_err();
        assert!(matches!(failure.cause, Error::Cancelled));
        assert!(matches!(
            failure.cleanup_error,
            Some(Error::Api { code: 421, .. })
        ));
        assert!(failure.report.target_verified);
        assert!(failure.report.directory_created);
        assert!(!failure.report.upload_attempted);
        assert!(!failure.report.cleanup_completed);
        assert_eq!(
            failure.report.leftover_remote_probe_path.as_deref(),
            Some(probe_path)
        );
        assert!(failure.to_string().contains(probe_path));
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 12);
        for (request, expected_path) in requests[6..=9].iter().zip([
            format!("{copy_directory}/{WRITE_PROBE_FILE_NAME}"),
            copy_directory,
            format!("{probe_path}/{WRITE_PROBE_FILE_NAME}"),
            probe_path.to_owned(),
        ]) {
            let body = String::from_utf8_lossy(&request.body);
            assert!(body.contains("api=SYNO.FileStation.Delete"));
            assert!(body.contains("recursive=false"));
            assert!(body.contains(&expected_path.replace('/', "%2F")));
        }
    }

    #[test]
    fn write_probe_refuses_an_absent_target_before_any_remote_mutation() {
        let root = RemoteRoot::parse("/share/missing").unwrap();
        let probe_path = "/share/missing/.synology-drive-sync-probe-test-missing";
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let responses = vec![
            write_probe_discovery(false),
            r#"{"success":true,"data":{"sid":"secret-sid"}}"#.to_owned(),
            getinfo_directory("/share"),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();

        let failure = client
            .run_write_probe_with_local(
                &root,
                probe_path,
                &local.entry,
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert!(failure.cause.to_string().contains("must already exist"));
        assert!(!failure.report.target_verified);
        assert!(!failure.report.directory_created);
        assert!(failure.report.cleanup_completed);
        assert_eq!(failure.report.leftover_remote_probe_path, None);
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests.iter().skip(1).all(|request| {
            let body = String::from_utf8_lossy(&request.body);
            !body.contains("SYNO.FileStation.CreateFolder")
                && !body.contains("SYNO.FileStation.Upload")
                && !body.contains("SYNO.FileStation.Delete")
        }));
    }

    #[test]
    fn full_flow_keeps_secrets_out_of_urls_and_streams_known_length_upload() {
        let discovery = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2, "requestFormat": "JSON"},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2, "requestFormat": "JSON"},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3},
                "SYNO.FileStation.MD5": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Download": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
            }
        })
        .to_string();
        let responses = vec![
            discovery,
            r#"{"success":false,"error":{"code":403,"errors":{"token":"challenge-secret","types":[{"type":"otp"}]}}}"#.to_owned(),
            r#"{"success":true,"data":{"sid":"secret-sid","synotoken":"csrf-secret"}}"#.to_owned(),
            r#"{"success":true,"data":{"shares":[{"path":"/share"}]}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share","name":"share","isdir":true,"additional":{}}]}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/root","name":"root","isdir":true,"additional":{"mount_point_type":""}}]}}"#.to_owned(),
            r#"{"success":true,"data":{"total":2,"offset":0,"files":[{"path":"/share/root/a.txt","name":"a.txt","isdir":false,"additional":{"size":1,"time":{"mtime":1}}}]}}"#.to_owned(),
            r#"{"success":true,"data":{"total":2,"offset":1,"files":[{"path":"/share/root/b.txt","name":"b.txt","isdir":false,"additional":{"size":1,"time":{"mtime":1}}}]}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/root/folder/upload.bin","name":"upload.bin","isdir":false,"additional":{"size":31}}]}}"#.to_owned(),
            scripted_binary_response(b"\0multipart payload\r\nwith binary"),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        let error = client.login("mirror-user", "p@ss word", None).unwrap_err();
        assert_eq!(error.api_code(), Some(403));
        assert!(!format!("{error:?}").contains("challenge-secret"));
        client
            .login("mirror-user", "p@ss word", Some("123456"))
            .unwrap();

        let root = RemoteRoot::parse("/share/root").unwrap();
        client.verify_share_writable(&root).unwrap();
        let inventory = client
            .remote_inventory(&root, &CancellationToken::default())
            .unwrap();
        assert!(inventory.root_exists);
        assert_eq!(inventory.entries.len(), 2);

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sdsync-upload-{nonce}.bin"));
        let payload = b"\0multipart payload\r\nwith binary";
        fs::write(&path, payload).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let mtime_ms = i64::try_from(
            metadata
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let local = LocalEntry {
            relative: "folder/upload.bin".to_owned(),
            full_path: path.clone(),
            kind: EntryKind::File,
            size: metadata.len(),
            mtime_ms,
            content_md5: Some(ContentMd5::from_content(payload)),
        };
        client
            .upload(&local, "/share/root/folder/upload.bin")
            .unwrap();
        client.logout().unwrap();
        fs::remove_file(path).unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 12);
        assert!(
            requests
                .iter()
                .all(|request| request.request_line == "POST /prefix/webapi/entry.cgi HTTP/1.1")
        );
        for request in &requests {
            assert!(!request.request_line.contains("p@ss"));
            assert!(!request.request_line.contains("123456"));
            assert!(!request.request_line.contains("secret-sid"));
            assert!(!request.request_line.contains("csrf-secret"));
        }

        let first_login = String::from_utf8_lossy(&requests[1].body);
        assert!(first_login.contains("passwd=p%40ss+word"));
        assert!(!first_login.contains("otp_code"));
        let otp_login = String::from_utf8_lossy(&requests[2].body);
        assert!(otp_login.contains("otp_code=123456"));
        let get_info = String::from_utf8_lossy(&requests[4].body);
        assert!(get_info.contains("method=getinfo"));
        assert!(get_info.contains("mount_point_type"));
        let root_info = String::from_utf8_lossy(&requests[5].body);
        assert!(root_info.contains("%2Fshare%2Froot"));
        let list = String::from_utf8_lossy(&requests[6].body);
        assert!(list.contains("_sid=secret-sid"));
        assert!(list.contains("SynoToken=csrf-secret"));
        let second_page = String::from_utf8_lossy(&requests[7].body);
        assert!(second_page.contains("offset=1"));

        let upload = &requests[8];
        assert!(
            upload
                .headers
                .iter()
                .any(|(name, value)| name == "content-length"
                    && value.parse::<usize>().unwrap() == upload.body.len())
        );
        assert!(!upload.headers.iter().any(|(name, value)| {
            name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked")
        }));
        let token_position = find_bytes(&upload.body, b"name=\"SynoToken\"").unwrap();
        let file_position = find_bytes(&upload.body, b"name=\"file\"").unwrap();
        let payload_position = find_bytes(&upload.body, payload).unwrap();
        assert!(token_position < file_position && file_position < payload_position);
        let verification = String::from_utf8_lossy(&requests[10].body);
        assert!(verification.contains("SYNO.FileStation.Download"));
        assert!(verification.contains("%5B%22%2Fshare%2Froot%2Ffolder%2Fupload.bin%22%5D"));
    }

    #[test]
    fn retryable_upload_response_is_reconciled_before_retransmission() {
        let discovery = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3},
                "SYNO.FileStation.MD5": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Download": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
            }
        })
        .to_string();
        let responses = vec![
            (StatusCode::OK, discovery),
            (
                StatusCode::OK,
                r#"{"success":true,"data":{"sid":"secret-sid","synotoken":"csrf-secret"}}"#
                    .to_owned(),
            ),
            (StatusCode::BAD_GATEWAY, "temporary proxy failure".to_owned()),
            (
                StatusCode::OK,
                r#"{"success":true,"data":{"files":[{"path":"/share/root/abc.bin","name":"abc.bin","isdir":false,"additional":{"size":3}}]}}"#.to_owned(),
            ),
            (StatusCode::OK, scripted_binary_response(b"abc")),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
        ];
        let (url, server) = scripted_server_with_status(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 1,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sdsync-reconcile-{nonce}.bin"));
        fs::write(&path, b"abc").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let local = LocalEntry {
            relative: "abc.bin".to_owned(),
            full_path: path.clone(),
            kind: EntryKind::File,
            size: 3,
            mtime_ms: i64::try_from(
                metadata
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap(),
            content_md5: Some(ContentMd5::from_content(b"abc")),
        };
        client.upload(&local, "/share/root/abc.bin").unwrap();
        client.logout().unwrap();
        fs::remove_file(path).unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 6);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.headers.iter().any(|(name, value)| {
                    name == "content-type" && value.starts_with("multipart/form-data")
                }))
                .count(),
            1
        );
        let probe = String::from_utf8_lossy(&requests[3].body);
        assert!(probe.contains("method=getinfo"));
    }

    #[test]
    fn server_copy_is_non_overwriting_and_destination_content_is_verified() {
        let discovery = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3},
                "SYNO.FileStation.MD5": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Download": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CopyMove": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 3}
            }
        })
        .to_string();
        let responses = vec![
            discovery,
            r#"{"success":true,"data":{"sid":"secret-sid","synotoken":"csrf-secret"}}"#.to_owned(),
            r#"{"success":true,"data":{"taskid":"copy-task"}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":true}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/root/new/report.bin","name":"report.bin","isdir":false,"additional":{"size":3}}]}}"#.to_owned(),
            scripted_binary_response(b"abc"),
            r#"{"success":true}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();
        client
            .copy_file_verified(
                &RemoteRoot::parse("/share/root").unwrap(),
                "/share/root/old/report.bin",
                "/share/root/new/report.bin",
                3,
                ContentMd5::from_content(b"abc"),
                &CancellationToken::default(),
            )
            .unwrap();
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
        let copy_start = String::from_utf8_lossy(&requests[2].body);
        assert!(copy_start.contains("SYNO.FileStation.CopyMove"));
        assert!(copy_start.contains("remove_src=false"));
        assert!(!copy_start.contains("overwrite"));
        let size_check = String::from_utf8_lossy(&requests[4].body);
        assert!(size_check.contains("method=getinfo"));
        let download = String::from_utf8_lossy(&requests[5].body);
        assert!(download.contains("SYNO.FileStation.Download"));
        assert!(download.contains("%5B%22%2Fshare%2Froot%2Fnew%2Freport.bin%22%5D"));
    }

    #[test]
    fn delete_guard_rejects_root_escape_and_traversal() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        for path in [
            "/share/root",
            "/share/root/a/../outside",
            "/share/rooted/file",
            "/share/other/file",
        ] {
            assert!(validate_delete_target(&root, path).is_err(), "{path}");
        }
        assert!(validate_delete_target(&root, "/share/root/folder/file").is_ok());
    }

    #[test]
    fn refuses_a_mounted_remote_root() {
        let discovery = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3}
            }
        })
        .to_string();
        let responses = vec![
            discovery,
            r#"{"success":true,"data":{"sid":"sid"}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share","name":"share","isdir":true,"additional":{}}]}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/mounted","name":"mounted","isdir":true,"additional":{"mount_point_type":"cifs"}}]}}"#.to_owned(),
        ];
        let (url, server) = scripted_server(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap();
        client.login("user", "password", None).unwrap();
        let error = client
            .remote_inventory(
                &RemoteRoot::parse("/share/mounted/child").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert!(matches!(error, Error::RemoteMountRoot { .. }));
        assert_eq!(server.join().unwrap().len(), 4);
    }

    /// Every mapped DSM code must render the exact operator-facing sentence, because that string
    /// is the whole contract a user has when a sync fails against a NAS they cannot inspect.
    #[test]
    fn dsm_error_codes_map_to_their_exact_operator_facing_descriptions() {
        for (code, expected) in [
            (400, "account does not exist or password is incorrect"),
            (401, "account is disabled"),
            (402, "account is not permitted to sign in"),
            (403, "two-factor OTP is required"),
            (404, "two-factor OTP is invalid or expired"),
            (406, "two-factor OTP is enforced"),
            (407, "source IP is blocked"),
            (408, "password has expired"),
            (409, "password has expired"),
            (410, "password must be changed"),
        ] {
            assert_eq!(
                api_error_description("SYNO.API.Auth", code),
                Some(expected),
                "SYNO.API.Auth code {code}"
            );
        }

        for (code, expected) in [
            (100, "unknown error"),
            (101, "missing API, method, or version parameter"),
            (102, "requested API does not exist"),
            (103, "requested method does not exist"),
            (104, "requested API version is unsupported"),
            (105, "session does not have permission"),
            (106, "session timed out; rerun to authenticate again"),
            (107, "session was interrupted by a duplicate login"),
            (119, "session is invalid; rerun to authenticate again"),
            (
                150,
                "request source IP differs from login IP; fix reverse-proxy routing",
            ),
            (400, "invalid file-operation parameter"),
            (402, "file subsystem is busy"),
            (407, "operation is not permitted"),
            (408, "remote file or directory does not exist"),
            (411, "remote filesystem is read-only"),
            (414, "remote item already exists"),
            (415, "disk quota exceeded"),
            (416, "no space left on the device"),
            (417, "remote input/output error"),
            (418, "illegal remote name or path"),
            (421, "remote resource is busy"),
            (900, "delete failed"),
            (1100, "folder creation failed"),
            (1101, "parent folder item-count limit exceeded"),
            (1800, "upload Content-Length is missing or mismatched"),
            (1801, "upload receive timeout"),
            (1802, "upload file part has no filename"),
            (1803, "upload was cancelled"),
            (1804, "file is too large for the destination filesystem"),
            (1805, "upload overwrite/skip policy is missing"),
        ] {
            assert_eq!(
                api_error_description("SYNO.FileStation.List", code),
                Some(expected),
                "File Station code {code}"
            );
        }

        // The two tables overlap numerically and must never be confused: 408 means an expired
        // password during authentication and a missing path everywhere else.
        assert_eq!(
            api_error_description("SYNO.API.Auth", 408),
            Some("password has expired")
        );
        assert_eq!(
            api_error_description("SYNO.FileStation.Delete", 408),
            Some("remote file or directory does not exist")
        );
        // Auth never falls through to the File Station table for codes it does not define.
        for code in [106, 119, 150, 414, 1100] {
            assert_eq!(
                api_error_description("SYNO.API.Auth", code),
                None,
                "SYNO.API.Auth must not borrow File Station code {code}"
            );
        }
        for api in ["SYNO.API.Auth", "SYNO.FileStation.Upload"] {
            assert_eq!(api_error_description(api, -1), None);
            assert_eq!(api_error_description(api, 0), None);
        }

        // Redirect refusal is a credential-safety guarantee: every redirect status must produce
        // the refusal hint rather than a reflected body.
        for status in [
            StatusCode::MOVED_PERMANENTLY,
            StatusCode::FOUND,
            StatusCode::SEE_OTHER,
            StatusCode::TEMPORARY_REDIRECT,
            StatusCode::PERMANENT_REDIRECT,
        ] {
            assert_eq!(
                http_status_hint(status, b"Location: https://attacker.example/"),
                "redirects are disabled to prevent credentials crossing origins; expose /webapi/* directly at the configured HTTPS URL",
                "status {status}"
            );
        }
        assert_eq!(
            http_status_hint(StatusCode::PAYLOAD_TOO_LARGE, b"ignored"),
            "request body is larger than the reverse proxy permits; raise its upload/body-size limit"
        );
        assert_eq!(
            http_status_hint(StatusCode::BAD_GATEWAY, b"ignored"),
            "reverse proxy could not reach the File Station backend"
        );
        assert_eq!(
            http_status_hint(StatusCode::GATEWAY_TIMEOUT, b"ignored"),
            "reverse proxy timed out; raise its send/read timeout for large uploads"
        );
        // Unmapped statuses fall back to a bounded, escaped snippet of the body.
        assert_eq!(
            http_status_hint(StatusCode::SERVICE_UNAVAILABLE, b"  maintenance\tmode  "),
            "maintenance\\tmode"
        );
        let long = vec![b'x'; 4096];
        assert_eq!(http_status_hint(StatusCode::IM_A_TEAPOT, &long).len(), 512);
    }

    #[test]
    fn connect_rejects_an_api_whose_advertised_range_excludes_the_required_version() {
        let discovery = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
            }
        })
        .to_string();
        let (url, server) = scripted_server(vec![discovery]);
        let Err(error) = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        }) else {
            panic!("connect must reject an unsupported CheckPermission version range");
        };
        assert!(
            matches!(
                &error,
                Error::UnsupportedApiVersion { api, version: 3, min: 1, max: 2 }
                    if api == "SYNO.FileStation.CheckPermission"
            ),
            "unexpected error: {error}"
        );
        assert_eq!(
            error.to_string(),
            "required Synology API SYNO.FileStation.CheckPermission version 3 is not available (server offers 1..=2)"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn transport_failures_name_the_operation_and_stay_retryable() {
        let responses = vec![required_discovery(), login_response()];
        let (url, server) = scripted_server(responses);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        // The scripted server stops listening after its final response, so the next control
        // request cannot connect at all.
        server.join().unwrap();

        let error = client
            .verify_destination_writable(&RemoteRoot::parse("/share/root").unwrap())
            .unwrap_err();
        let Error::Http { operation, source } = &error else {
            panic!("expected a transport error, got {error}");
        };
        assert_eq!(operation, "SYNO.FileStation.List.getinfo");
        assert!(
            error.to_string().contains("SYNO.FileStation.List.getinfo"),
            "the reported failure must name the operation: {error}"
        );
        assert!(
            retryable(&error),
            "a failure to reach the NAS must remain retryable so a flapping proxy is retried: \
             {source:?}"
        );
    }

    /// A started MD5 task is server-side work. Every abandonment path must stop it, and a start
    /// response without a task ID must fail closed rather than poll a task that may not exist.
    #[test]
    fn md5_task_abandonment_always_stops_the_task_and_missing_ids_fail_closed() {
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client
            .remote_content_md5("/share/file.bin", &CancellationToken::default())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unexpected response during SYNO.FileStation.MD5.start: successful response contained no task ID"
        );
        let requests = server.join().unwrap();
        assert_eq!(
            requests.len(),
            3,
            "no task exists, so nothing may be stopped"
        );

        // A failing status poll abandons the task, so it must be stopped before returning.
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            task_start_response("failing-status-md5"),
            r#"{"success":false,"error":{"code":417}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client
            .remote_content_md5("/share/file.bin", &CancellationToken::default())
            .unwrap_err();
        assert!(
            matches!(error, Error::Api { code: 417, .. }),
            "unexpected error: {error}"
        );
        assert_eq!(
            error.to_string(),
            "Synology API SYNO.FileStation.MD5.status failed with code 417: remote input/output error"
        );
        let requests = server.join().unwrap();
        let stop = String::from_utf8_lossy(&requests[4].body);
        assert!(stop.contains("method=stop"));
        assert!(stop.contains("failing-status-md5"));

        // Cancelling while the poll loop is sleeping must also stop the task.
        let cancellation = CancellationToken::default();
        let cancel_before_status_response = cancellation.clone();
        let (url, server) = scripted_server_with_status_hook(
            vec![
                (StatusCode::OK, write_probe_discovery(false)),
                (StatusCode::OK, login_response()),
                (StatusCode::OK, task_start_response("sleeping-md5")),
                (
                    StatusCode::OK,
                    r#"{"success":true,"data":{"finished":false}}"#.to_owned(),
                ),
                (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            ],
            move |index| {
                if index == 3 {
                    cancel_before_status_response.cancel();
                }
            },
        );
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(matches!(
            client.remote_content_md5("/share/file.bin", &cancellation),
            Err(Error::Cancelled)
        ));
        let requests = server.join().unwrap();
        let stop = String::from_utf8_lossy(&requests[4].body);
        assert!(stop.contains("method=stop"));
        assert!(stop.contains("sleeping-md5"));
    }

    #[test]
    fn copy_start_without_a_task_id_fails_closed_and_cancellation_stops_the_task() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let (url, server) = scripted_server(vec![
            write_probe_discovery(true),
            login_response(),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client
            .copy_file_verified(
                &root,
                "/share/root/a/report.bin",
                "/share/root/b/report.bin",
                7,
                ContentMd5::from_bytes([1_u8; 16]),
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unexpected response during SYNO.FileStation.CopyMove.start: successful response contained no task ID"
        );
        assert_eq!(server.join().unwrap().len(), 3);

        let cancellation = CancellationToken::default();
        let cancel_before_start_response = cancellation.clone();
        let (url, server) = scripted_server_with_status_hook(
            vec![
                (StatusCode::OK, write_probe_discovery(true)),
                (StatusCode::OK, login_response()),
                (StatusCode::OK, task_start_response("cancelled-copy")),
                (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            ],
            move |index| {
                if index == 2 {
                    cancel_before_start_response.cancel();
                }
            },
        );
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(matches!(
            client.copy_file_verified(
                &root,
                "/share/root/a/report.bin",
                "/share/root/b/report.bin",
                7,
                ContentMd5::from_bytes([1_u8; 16]),
                &cancellation,
            ),
            Err(Error::Cancelled)
        ));
        let requests = server.join().unwrap();
        let stop = String::from_utf8_lossy(&requests[3].body);
        assert!(stop.contains("method=stop"));
        assert!(stop.contains("cancelled-copy"));
    }

    #[test]
    fn share_and_destination_write_checks_fail_closed_without_mutating_the_nas() {
        let root = RemoteRoot::parse("/share/root").unwrap();

        // A successful envelope with no share list is not evidence of a writable share.
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert_eq!(
            client.verify_share_writable(&root).unwrap_err().to_string(),
            "unexpected response during SYNO.FileStation.List.list_share: successful response contained no share list"
        );
        server.join().unwrap();

        // A share list that omits the configured share must name that share, not another one.
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            r#"{"success":true,"data":{"shares":[{"path":"/other"},{"path":"/shared"}]}}"#
                .to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client.verify_share_writable(&root).unwrap_err();
        assert!(matches!(&error, Error::ShareNotWritable(share) if share == "share"));
        assert_eq!(
            error.to_string(),
            "DSM shared folder /share is unavailable or not writable by this account"
        );
        server.join().unwrap();

        // An ancestor that exists as a file is a configuration error, not a permission error.
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            getinfo_file("/share", 4, None),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert_eq!(
            client
                .verify_destination_writable(&root)
                .unwrap_err()
                .to_string(),
            "remote destination ancestor /share exists but is not a directory"
        );
        server.join().unwrap();

        // Nothing exists at all: report the unavailable share rather than probing a missing tree.
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(matches!(
            client.verify_destination_writable(&root),
            Err(Error::ShareNotWritable(ref share)) if share == "share"
        ));
        server.join().unwrap();

        // A non-"missing path" DSM failure is surfaced verbatim instead of being reinterpreted.
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            r#"{"success":false,"error":{"code":105}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client.verify_destination_writable(&root).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Synology API SYNO.FileStation.List.getinfo failed with code 105: session does not have permission"
        );
        server.join().unwrap();
    }

    #[test]
    fn remote_inventory_surfaces_non_missing_ancestor_failures() {
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            r#"{"success":false,"error":{"code":105}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client
            .remote_inventory(
                &RemoteRoot::parse("/share/root").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Synology API SYNO.FileStation.List.getinfo failed with code 105: session does not have permission"
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn upload_preflight_rejects_missing_resized_and_rewritten_sources() {
        let (url, server) = scripted_server(vec![required_discovery(), login_response()]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        server.join().unwrap();

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing = std::env::temp_dir().join(format!("sdsync-preflight-missing-{nonce}.bin"));
        let absent = LocalEntry {
            relative: "missing.bin".to_owned(),
            full_path: missing.clone(),
            kind: EntryKind::File,
            size: 3,
            mtime_ms: 0,
            content_md5: None,
        };
        let error = client
            .preflight_upload_source(&absent, &CancellationToken::default())
            .unwrap_err();
        assert!(
            matches!(&error, Error::FileIo { path, .. } if *path == missing),
            "unexpected error: {error}"
        );

        let path = std::env::temp_dir().join(format!("sdsync-preflight-{nonce}.bin"));
        fs::write(&path, b"payload").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let mtime_ms = i64::try_from(
            metadata
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();

        // A stale size in the planning snapshot means the file changed under us.
        let resized = LocalEntry {
            relative: "payload.bin".to_owned(),
            full_path: path.clone(),
            kind: EntryKind::File,
            size: metadata.len() + 1,
            mtime_ms,
            content_md5: None,
        };
        assert!(matches!(
            client.preflight_upload_source(&resized, &CancellationToken::default()),
            Err(Error::SourceChanged(ref changed)) if *changed == path
        ));

        // Same size and mtime, different bytes: only the digest can catch this.
        let rewritten = LocalEntry {
            relative: "payload.bin".to_owned(),
            full_path: path.clone(),
            kind: EntryKind::File,
            size: metadata.len(),
            mtime_ms,
            content_md5: Some(ContentMd5::from_bytes([0_u8; 16])),
        };
        assert!(matches!(
            client.preflight_upload_source(&rewritten, &CancellationToken::default()),
            Err(Error::SourceChanged(ref changed)) if *changed == path
        ));

        let unchanged = LocalEntry {
            relative: "payload.bin".to_owned(),
            full_path: path.clone(),
            kind: EntryKind::File,
            size: metadata.len(),
            mtime_ms,
            content_md5: Some(ContentMd5::from_content(b"payload")),
        };
        client
            .preflight_upload_source(&unchanged, &CancellationToken::default())
            .unwrap();
        fs::remove_file(&path).unwrap();
    }

    fn write_probe_client(responses: Vec<String>) -> (ApiClient, JoinHandle<Vec<CapturedRequest>>) {
        let (url, server) = scripted_server(responses);
        let mut client = connect_test_client(url);
        client.login("probe-user", "password", None).unwrap();
        (client, server)
    }

    /// One refusal case: the responses that follow login, the total request count they should
    /// produce, and an assertion over the resulting probe failure cause.
    type ProbeRefusalCase = (Vec<String>, usize, Box<dyn Fn(&Error)>);

    /// The probe must prove its target is a real, unmounted directory and that its unique child
    /// name is free before it creates anything. Each refusal happens before the first mutation.
    #[test]
    fn write_probe_verifies_its_target_before_creating_anything() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-target";
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let cancellation = CancellationToken::default();

        let cases: Vec<ProbeRefusalCase> = vec![
            (
                vec![getinfo_file("/share", 4, None)],
                3,
                Box::new(|error: &Error| {
                    assert_eq!(
                        error.to_string(),
                        "write-probe target ancestor \"/share\" is not a directory"
                    );
                }),
            ),
            (
                vec![r#"{"success":false,"error":{"code":105}}"#.to_owned()],
                3,
                Box::new(|error: &Error| {
                    assert_eq!(
                        error.to_string(),
                        "Synology API SYNO.FileStation.List.getinfo failed with code 105: session does not have permission"
                    );
                }),
            ),
            (
                vec![
                    getinfo_directory("/share"),
                    r#"{"success":true,"data":{"files":[{"path":"/share/root","name":"root","isdir":true,"additional":{"mount_point_type":"nfs"}}]}}"#.to_owned(),
                ],
                4,
                Box::new(|error: &Error| {
                    assert!(
                        matches!(error, Error::RemoteMountRoot { path, mount_type }
                            if path == "/share/root" && mount_type == "nfs"),
                        "unexpected error: {error}"
                    );
                }),
            ),
            (
                vec![
                    getinfo_directory("/share"),
                    getinfo_directory("/share/root"),
                    getinfo_directory(probe_path),
                ],
                5,
                Box::new(move |error: &Error| {
                    assert_eq!(
                        error.to_string(),
                        format!(
                            "refusing write probe because unique path {probe_path:?} already exists"
                        )
                    );
                }),
            ),
            (
                vec![
                    getinfo_directory("/share"),
                    getinfo_directory("/share/root"),
                    r#"{"success":false,"error":{"code":407}}"#.to_owned(),
                ],
                5,
                Box::new(|error: &Error| {
                    assert_eq!(
                        error.to_string(),
                        "Synology API SYNO.FileStation.List.getinfo failed with code 407: operation is not permitted"
                    );
                }),
            ),
        ];

        for (index, (tail, expected_requests, check)) in cases.into_iter().enumerate() {
            let mut responses = vec![write_probe_discovery(false), login_response()];
            responses.extend(tail);
            let (client, server) = write_probe_client(responses);
            let failure = client
                .run_write_probe_with_local(&root, probe_path, &local.entry, &cancellation)
                .unwrap_err();
            check(&failure.cause);
            assert!(!failure.report.directory_created, "case {index}");
            assert!(failure.report.cleanup_completed, "case {index}");
            assert_eq!(
                failure.report.leftover_remote_probe_path, None,
                "case {index}"
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), expected_requests, "case {index}");
            // Match the `api=` form field rather than a bare API name: the discovery request
            // legitimately lists every API in its `query` parameter, and a substring test would
            // mistake that for a mutation.
            assert!(
                requests.iter().all(|request| {
                    let body = String::from_utf8_lossy(&request.body);
                    !body.contains("api=SYNO.FileStation.CreateFolder")
                        && !body.contains("api=SYNO.FileStation.Delete")
                        && !body.contains("api=SYNO.FileStation.Upload")
                }),
                "case {index} must not mutate the NAS"
            );
        }
    }

    /// A decode failure has to explain itself, in schema terms and without republishing the body.
    ///
    /// The four bodies are the shapes that actually cost round trips against a live NAS. What is
    /// pinned is the *member path*: `decode` on its own tells an operator only that something did
    /// not match, and the path is what turns the next occurrence into a one-line diagnosis.
    #[test]
    fn a_decode_failure_names_the_member_its_type_and_never_the_value() {
        #[derive(Debug, Deserialize)]
        struct Info {
            #[allow(dead_code)]
            #[serde(default, alias = "support_virtual")]
            support_virtual_protocol: Option<String>,
        }
        #[derive(Debug, Deserialize)]
        struct Files {
            #[allow(dead_code)]
            files: Vec<Item>,
        }
        #[derive(Debug, Deserialize)]
        struct Item {
            #[allow(dead_code)]
            path: String,
            #[allow(dead_code)]
            name: String,
        }

        // The DSM 7 `Info.get` body, against the schema this tool used to carry.
        let dsm_seven_info = br#"{"data":{"hostname":"NAS","support_virtual":{"enable_iso_mount":true},"support_virtual_protocol":["cifs","nfs"]},"success":true}"#;
        let error = serde_json::from_slice::<Envelope<Info>>(dsm_seven_info).unwrap_err();
        let fault = decode_fault(dsm_seven_info, &error);
        assert_eq!(fault.kind, DecodeFaultKind::TypeMismatch);
        assert_eq!(fault.path.as_str(), "data.support_virtual");
        assert_eq!(fault.found, JsonKind::Object);
        assert_eq!(
            fault.describe(),
            "type-mismatch at data.support_virtual; expected a_string, found object"
        );

        // The same member when only the documented spelling is present, which is an array.
        let array_form =
            br#"{"data":{"support_virtual_protocol":["cifs","nfs"]},"success":true}"#.as_slice();
        let error = serde_json::from_slice::<Envelope<Info>>(array_form).unwrap_err();
        let fault = decode_fault(array_form, &error);
        assert_eq!(fault.path.as_str(), "data.support_virtual_protocol");
        assert_eq!(fault.found, JsonKind::Array);

        // The `getinfo` per-entry status shape: the index is part of the path, so a body with
        // several requested paths still names which one disagreed.
        let per_entry = br#"{"data":{"files":[{"path":"/a","name":"a"},{"code":408,"isdir":false,"path":"/home/x"}]},"success":true}"#;
        let error = serde_json::from_slice::<Envelope<Files>>(per_entry).unwrap_err();
        let fault = decode_fault(per_entry, &error);
        assert_eq!(fault.kind, DecodeFaultKind::MissingField);
        assert_eq!(fault.path.as_str(), "data.files.1.name");
        assert_eq!(fault.field.as_str(), "name");
        assert_eq!(fault.found, JsonKind::Absent);

        // A value of the wrong type is described by its type alone. The value here is the shape a
        // session identifier would arrive in, and it must not appear anywhere in the diagnosis.
        let secret_shaped =
            br#"{"data":{"files":[{"path":"/a","name":"WQwvhBqfrOM4gPcQ"}]},"success":true}"#;
        let error = serde_json::from_slice::<Envelope<Files>>(secret_shaped).unwrap();
        assert!(error.success, "this body decodes; it is the control");
        let mistyped =
            br#"{"data":{"files":[{"path":"/a","name":9223372036854775807}]},"success":true}"#;
        let error = serde_json::from_slice::<Envelope<Files>>(mistyped).unwrap_err();
        let fault = decode_fault(mistyped, &error);
        assert_eq!(fault.path.as_str(), "data.files.0.name");
        assert_eq!(fault.found, JsonKind::Number);
        let rendered = format!("{} {:?}", fault.describe(), fault);
        for forbidden in ["9223372036854775807", "WQwvhBqfrOM4gPcQ", "/a"] {
            assert!(
                !rendered.contains(forbidden),
                "a decode diagnosis rendered response content: {rendered}"
            );
        }
    }

    /// The diagnosis has to reach the call record, or no operator will ever see it.
    #[test]
    fn a_decode_failure_reaches_the_recorded_call() {
        let completed: Arc<Mutex<Vec<ApiCallDetail>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&completed);
        let observer: RequestObserver = Arc::new(move |observation| {
            if let ApiObservation::CallCompleted(call) = observation
                && let Ok(mut sink) = sink.lock()
            {
                sink.push(call);
            }
        });
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            // `files` is an object where the client requires an array.
            r#"{"success":true,"data":{"files":{"path":"/share/root"}}}"#.to_owned(),
        ]);
        let mut client = ApiClient::connect_observed(
            &ClientOptions {
                base_url: url,
                allow_http: true,
                accept_invalid_certs: false,
                ca_certificate: None,
                connect_timeout: Duration::from_secs(2),
                request_timeout: Duration::from_secs(5),
                retries: 0,
            },
            Some(observer),
        )
        .unwrap();
        client.login("alice", "password", None).unwrap();
        let error = client.get_info("/share/root").unwrap_err();
        assert!(matches!(error, Error::InvalidResponse { .. }));
        // The operator-facing message carries the schema diagnosis and withholds the body.
        assert!(
            error.to_string().contains("type-mismatch at data.files"),
            "unexpected message: {error}"
        );
        assert!(error.to_string().contains("response body withheld"));

        let calls = completed.lock().unwrap();
        let record = calls
            .iter()
            .find(|call| call.method == "getinfo")
            .expect("the failing call is recorded");
        assert_eq!(record.outcome, RequestOutcome::Decode);
        let fault = record.decode.expect("a decode outcome carries its reason");
        assert_eq!(fault.path.as_str(), "data.files");
        assert_eq!(fault.found, JsonKind::Object);
        drop(calls);
        server.join().unwrap();
    }

    /// The absence check must read File Station's *per-entry* status, not only its envelope one.
    ///
    /// This is the response a live DSM 7.2 sends for `getinfo` on a path that is not there: the
    /// envelope succeeds, and the single entry carries `code: 408` with neither a `name` nor an
    /// `isdir`. Requiring those two members made the third `getinfo` of a `--write-test` run --
    /// the one that asks whether the probe's own unique directory already exists -- fail to
    /// deserialize, which reported the destination as returning an invalid response when what it
    /// had actually returned was "no, that path is free".
    #[test]
    fn a_per_entry_408_reads_as_an_absent_path_rather_than_as_a_decode_failure() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-absent";
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let cancellation = CancellationToken::default();
        let per_entry_absent = serde_json::json!({
            "success": true,
            "data": {"files": [{"code": 408, "path": probe_path}]}
        })
        .to_string();

        let responses = vec![
            write_probe_discovery(false),
            login_response(),
            getinfo_directory("/share"),
            getinfo_directory("/share/root"),
            per_entry_absent,
            // Reached only because the absence check answered "free"; the collision code ends the
            // probe here without mutating anything, which keeps this test's blast radius at zero.
            r#"{"success":false,"error":{"code":414}}"#.to_owned(),
        ];
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(&root, probe_path, &local.entry, &cancellation)
            .unwrap_err();
        assert!(
            matches!(failure.cause, Error::Api { code: 414, .. }),
            "the run must reach folder creation, not stop at the absence check: {}",
            failure.cause
        );
        assert!(failure.report.target_verified);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(
            String::from_utf8_lossy(&requests[5].body)
                .contains("api=SYNO.FileStation.CreateFolder")
        );
    }

    /// A per-entry status is a DSM verdict and stays one; a genuinely malformed entry stays an
    /// error, with a message that names what was missing.
    #[test]
    fn per_entry_statuses_and_malformed_entries_are_told_apart() {
        let permission_denied = RemoteItemWire {
            path: "/share/root/secret".to_owned(),
            name: None,
            isdir: None,
            code: Some(407),
            disable_list: false,
            additional: None,
        };
        let error = permission_denied
            .into_item("SYNO.FileStation.List", "getinfo")
            .unwrap_err();
        assert!(
            matches!(&error, Error::Api { code: 407, api, operation, .. }
                if api == "SYNO.FileStation.List" && operation == "getinfo"),
            "unexpected error: {error}"
        );
        assert_eq!(error.api_code(), Some(407));

        // `code: 0` is File Station saying the entry is fine, so the entry still has to describe
        // itself. Nothing here may be defaulted into existence.
        let nameless = RemoteItemWire {
            path: "/share/root/child".to_owned(),
            name: None,
            isdir: Some(false),
            code: Some(0),
            disable_list: false,
            additional: None,
        };
        let error = nameless
            .into_item("SYNO.FileStation.List", "list")
            .unwrap_err();
        assert!(
            matches!(&error, Error::InvalidResponse { operation, message }
                if operation == "SYNO.FileStation.List.list" && message == "entry described no name"),
            "unexpected error: {error}"
        );

        let complete = RemoteItemWire {
            path: "/share/root/child".to_owned(),
            name: Some("child".to_owned()),
            isdir: Some(true),
            code: None,
            disable_list: true,
            additional: None,
        };
        let item = complete.into_item("SYNO.FileStation.List", "list").unwrap();
        assert_eq!(item.name, "child");
        assert!(item.isdir);
        assert!(item.disable_list);
    }

    /// A deterministic name collision (414) is somebody else's directory and must never be
    /// cleaned up. Any other creation failure may have partially landed, so cleanup must run.
    #[test]
    fn write_probe_cleans_up_after_ambiguous_creation_but_never_after_a_collision() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-create";
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let cancellation = CancellationToken::default();
        let preamble = || {
            vec![
                write_probe_discovery(false),
                login_response(),
                getinfo_directory("/share"),
                getinfo_directory("/share/root"),
                r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            ]
        };

        let mut responses = preamble();
        responses.push(r#"{"success":false,"error":{"code":414}}"#.to_owned());
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(&root, probe_path, &local.entry, &cancellation)
            .unwrap_err();
        assert_eq!(
            failure.cause.to_string(),
            "Synology API SYNO.FileStation.CreateFolder.create failed with code 414: remote item already exists"
        );
        assert!(!failure.report.directory_created);
        assert!(failure.report.cleanup_completed);
        assert_eq!(failure.report.leftover_remote_probe_path, None);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(
            !String::from_utf8_lossy(&requests[5].body).contains("Delete"),
            "a colliding directory belongs to someone else and must never be deleted"
        );

        // A non-collision failure may have landed, so cleanup runs; a failing final absence check
        // is reported as an independent cleanup error alongside the original cause.
        let mut responses = preamble();
        responses.push(r#"{"success":false,"error":{"code":407}}"#.to_owned());
        responses.extend(std::iter::repeat_n(
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            4,
        ));
        responses.push(r#"{"success":false,"error":{"code":105}}"#.to_owned());
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(&root, probe_path, &local.entry, &cancellation)
            .unwrap_err();
        assert!(
            matches!(failure.cause, Error::Api { code: 407, .. }),
            "unexpected cause: {}",
            failure.cause
        );
        assert!(
            matches!(failure.cleanup_error, Some(Error::Api { code: 105, .. })),
            "an unverifiable cleanup must be reported separately from the original cause"
        );
        assert!(!failure.report.cleanup_completed);
        assert_eq!(
            failure.report.leftover_remote_probe_path.as_deref(),
            Some(probe_path)
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 11);
        for request in &requests[6..=9] {
            let body = String::from_utf8_lossy(&request.body);
            assert!(body.contains("api=SYNO.FileStation.Delete"));
            assert!(body.contains("recursive=false"));
        }
    }

    #[test]
    fn write_probe_refuses_a_directory_it_did_not_get_exclusively() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-exclusive";
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let cancellation = CancellationToken::default();
        let preamble = || {
            vec![
                write_probe_discovery(false),
                login_response(),
                getinfo_directory("/share"),
                getinfo_directory("/share/root"),
                r#"{"success":false,"error":{"code":408}}"#.to_owned(),
                r#"{"success":true}"#.to_owned(),
            ]
        };
        let cleanup = || {
            let mut responses =
                std::iter::repeat_n(r#"{"success":false,"error":{"code":408}}"#.to_owned(), 4)
                    .collect::<Vec<_>>();
            responses.push(r#"{"success":false,"error":{"code":408}}"#.to_owned());
            responses
        };

        // "Created" but readable as a file: refuse rather than upload into an unknown object.
        let mut responses = preamble();
        responses.push(getinfo_file(probe_path, 9, None));
        responses.extend(cleanup());
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(&root, probe_path, &local.entry, &cancellation)
            .unwrap_err();
        assert_eq!(
            failure.cause.to_string(),
            format!("write-probe path {probe_path:?} was not created as a directory")
        );
        assert!(failure.report.directory_created);
        assert!(!failure.report.upload_attempted);
        assert!(failure.report.cleanup_completed);
        assert_eq!(server.join().unwrap().len(), 12);

        // A non-empty "fresh" directory means the name was not exclusively ours.
        let mut responses = preamble();
        responses.push(getinfo_directory(probe_path));
        responses.push(
            serde_json::json!({
                "success": true,
                "data": {"total": 1, "files": [{
                    "path": format!("{probe_path}/stranger.txt"),
                    "name": "stranger.txt",
                    "isdir": false,
                    "additional": {}
                }]}
            })
            .to_string(),
        );
        responses.extend(cleanup());
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(&root, probe_path, &local.entry, &cancellation)
            .unwrap_err();
        assert_eq!(
            failure.cause.to_string(),
            format!("write-probe directory {probe_path:?} was not empty after creation")
        );
        assert!(!failure.report.upload_attempted);
        assert!(failure.report.cleanup_completed);
        assert_eq!(server.join().unwrap().len(), 13);
    }

    /// When the probe itself succeeds but cleanup cannot prove the directory is gone, the caller
    /// must still get a failure naming the leftover path -- a silent success would leave litter.
    #[test]
    fn successful_write_probe_still_fails_when_cleanup_leaves_the_directory_behind() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-leftover";
        let upload_path = format!("{probe_path}/{WRITE_PROBE_FILE_NAME}");
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let size = local.entry.size;
        let mtime_seconds = local.entry.mtime_ms.div_euclid(1000);
        let responses = vec![
            write_probe_discovery(false),
            login_response(),
            getinfo_directory("/share"),
            getinfo_directory("/share/root"),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            getinfo_directory(probe_path),
            r#"{"success":true,"data":{"total":0,"files":[]}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            getinfo_file(&upload_path, size, None),
            scripted_binary_response(WRITE_PROBE_PAYLOAD),
            getinfo_file(&upload_path, size, Some(mtime_seconds)),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
            getinfo_directory(probe_path),
        ];
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(
                &root,
                probe_path,
                &local.entry,
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert_eq!(
            failure.cause.to_string(),
            format!("write-probe cleanup left remote path {probe_path:?}")
        );
        assert!(
            failure.cleanup_error.is_none(),
            "the cleanup failure is already the cause and must not be duplicated"
        );
        assert!(failure.report.upload_verified);
        assert!(!failure.report.server_copy_supported);
        assert!(!failure.report.server_copy_attempted);
        assert!(!failure.report.cleanup_completed);
        assert_eq!(
            failure.report.leftover_remote_probe_path.as_deref(),
            Some(probe_path)
        );
        assert!(failure.to_string().contains("inspect and remove leftover"));
        assert_eq!(server.join().unwrap().len(), 17);
    }

    fn temp_upload_source(tag: &str, contents: &[u8]) -> (PathBuf, LocalEntry) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sdsync-upload-{tag}-{nonce}.bin"));
        fs::write(&path, contents).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let entry = LocalEntry {
            relative: "abc.bin".to_owned(),
            full_path: path.clone(),
            kind: EntryKind::File,
            size: metadata.len(),
            mtime_ms: i64::try_from(
                metadata
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap(),
            content_md5: Some(ContentMd5::from_content(contents)),
        };
        (path, entry)
    }

    fn upload_client(
        responses: Vec<(StatusCode, String)>,
        retries: u32,
    ) -> (ApiClient, JoinHandle<Vec<CapturedRequest>>) {
        let (url, server) = scripted_server_with_status(responses);
        let mut client = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries,
        })
        .unwrap();
        client.login("mirror-user", "password", None).unwrap();
        (client, server)
    }

    fn upload_preamble() -> Vec<(StatusCode, String)> {
        vec![
            (StatusCode::OK, write_probe_discovery(false)),
            (StatusCode::OK, login_response()),
        ]
    }

    /// An observer that declines the attempt must stop the upload before a single byte reaches
    /// the network, and must still be told the transfer failed.
    #[test]
    fn an_observer_can_refuse_an_attempt_before_any_bytes_are_sent() {
        let (path, local) = temp_upload_source("observer", b"abc");
        let (client, server) = upload_client(upload_preamble(), 0);
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let observer: UploadObserver = Arc::new(move |event| {
            recorded.lock().unwrap().push(event);
            !matches!(event, UploadTransferEvent::AttemptStarted { .. })
        });
        assert!(matches!(
            client.upload_observed(
                &local,
                "/share/root/abc.bin",
                Some(observer),
                &CancellationToken::default(),
            ),
            Err(Error::Cancelled)
        ));
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                UploadTransferEvent::AttemptStarted { attempt: 1 },
                UploadTransferEvent::Failed
            ]
        );
        assert_eq!(
            server.join().unwrap().len(),
            2,
            "discovery and login only: the upload must never be sent"
        );
        fs::remove_file(path).unwrap();
    }

    /// A configured limit must still deliver the whole file. The throttle works by shortening
    /// reads, and a shortened read must never be mistaken for the end of the body.
    #[test]
    fn a_rate_limited_client_still_uploads_the_complete_file() {
        let (path, local) = temp_upload_source("throttled", b"abc");
        let mut responses = upload_preamble();
        responses.extend([
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            (
                StatusCode::OK,
                getinfo_file("/share/root/abc.bin", local.size, None),
            ),
            (StatusCode::OK, scripted_binary_response(b"abc")),
        ]);
        let (client, server) = upload_client(responses, 0);
        // A megabyte per second: the opening burst covers this payload outright, so the limited
        // path is exercised without the test depending on any wall-clock delay.
        let client = client.with_max_upload_rate(Some(1024 * 1024));
        client.upload(&local, "/share/root/abc.bin").unwrap();
        assert_eq!(server.join().unwrap().len(), 5);
        fs::remove_file(&path).unwrap();
    }

    /// A transport failure must fail the upload, name the file it was carrying, and consume
    /// exactly the configured attempt budget -- no silent extra retransmission of a file the NAS
    /// may already have. Which reqwest error kind a dead peer produces is an implementation
    /// detail of the transport and varies with timing, so it is deliberately not asserted here;
    /// the retry classification itself is pinned by
    /// `transport_failures_name_the_operation_and_stay_retryable`.
    #[test]
    fn upload_transport_failures_name_the_file_and_are_not_retried_past_the_budget() {
        let (path, local) = temp_upload_source("transport", b"abc");
        // A budget of zero retries: the single permitted attempt must also be the last one.
        let (client, server) = upload_client(upload_preamble(), 0);
        // The scripted server stops listening once the preamble is served, so the upload has no
        // peer left to talk to.
        assert_eq!(server.join().unwrap().len(), 2, "discovery and login only");
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&attempts);
        let observer: UploadObserver = Arc::new(move |event| {
            recorded.lock().unwrap().push(event);
            true
        });
        let error = client
            .upload_observed(
                &local,
                "/share/root/abc.bin",
                Some(observer),
                &CancellationToken::default(),
            )
            .expect_err("a dead peer must not look like a completed upload");
        let Error::Http { operation, .. } = &error else {
            panic!("expected a transport error, got {error}");
        };
        assert_eq!(operation, "uploading abc.bin");
        assert!(
            error.to_string().contains("abc.bin"),
            "the reported failure must name the file: {error}"
        );
        // How far the multipart body is read before a dead peer surfaces the failure is timing
        // dependent, so only the attempt and outcome events are pinned.
        let events = attempts.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    UploadTransferEvent::AttemptStarted { attempt } => Some(*attempt),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![1],
            "a zero-retry budget allows exactly one attempt: {events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&UploadTransferEvent::Failed),
            "the transfer must be reported failed: {events:?}"
        );
        assert!(
            !events.contains(&UploadTransferEvent::Completed),
            "a failed transfer must never be announced as completed: {events:?}"
        );
        fs::remove_file(path).unwrap();
    }

    /// After a retryable failure the client must never blindly retransmit: it re-reads the local
    /// file and asks the NAS what actually landed. Each answer drives a different decision.
    #[test]
    fn a_retryable_upload_failure_reconciles_remote_state_before_deciding() {
        // The remote object is absent, so the upload genuinely has to be retransmitted.
        let (path, local) = temp_upload_source("absent", b"abc");
        let mut responses = upload_preamble();
        responses.extend([
            (
                StatusCode::BAD_GATEWAY,
                "temporary proxy failure".to_owned(),
            ),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            ),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            (
                StatusCode::OK,
                getinfo_file("/share/root/abc.bin", local.size, None),
            ),
            (StatusCode::OK, scripted_binary_response(b"abc")),
        ]);
        let (client, server) = upload_client(responses, 1);
        client.upload(&local, "/share/root/abc.bin").unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.headers.iter().any(|(name, value)| {
                    name == "content-type" && value.starts_with("multipart/form-data")
                }))
                .count(),
            2,
            "an absent remote object must be retransmitted exactly once more"
        );
        fs::remove_file(path).unwrap();

        // The reconciliation probe itself fails transiently: retry rather than give up.
        let (path, local) = temp_upload_source("flaky-probe", b"abc");
        let mut responses = upload_preamble();
        responses.extend([
            (
                StatusCode::BAD_GATEWAY,
                "temporary proxy failure".to_owned(),
            ),
            (StatusCode::GATEWAY_TIMEOUT, "probe also failed".to_owned()),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            (
                StatusCode::OK,
                getinfo_file("/share/root/abc.bin", local.size, None),
            ),
            (StatusCode::OK, scripted_binary_response(b"abc")),
        ]);
        let (client, server) = upload_client(responses, 1);
        client.upload(&local, "/share/root/abc.bin").unwrap();
        assert_eq!(server.join().unwrap().len(), 7);
        fs::remove_file(path).unwrap();

        // A permission failure during reconciliation is decisive and must surface immediately
        // instead of being masked by another upload attempt.
        let (path, local) = temp_upload_source("denied-probe", b"abc");
        let mut responses = upload_preamble();
        responses.extend([
            (
                StatusCode::BAD_GATEWAY,
                "temporary proxy failure".to_owned(),
            ),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":105}}"#.to_owned(),
            ),
        ]);
        let (client, server) = upload_client(responses, 1);
        let error = client.upload(&local, "/share/root/abc.bin").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Synology API SYNO.FileStation.List.getinfo failed with code 105: session does not have permission"
        );
        assert_eq!(server.join().unwrap().len(), 4);
        fs::remove_file(path).unwrap();
    }

    /// The planning digest is authoritative. If the bytes on disk no longer match it, the upload
    /// must abort rather than publish content nobody planned.
    #[test]
    fn upload_aborts_when_the_source_no_longer_matches_its_planned_digest() {
        // Detected after a successful transfer, before the remote object is trusted.
        let (path, mut local) = temp_upload_source("rewritten", b"abc");
        local.content_md5 = Some(ContentMd5::from_bytes([0_u8; 16]));
        let mut responses = upload_preamble();
        responses.push((StatusCode::OK, r#"{"success":true}"#.to_owned()));
        let (client, server) = upload_client(responses, 0);
        assert!(matches!(
            client.upload(&local, "/share/root/abc.bin"),
            Err(Error::SourceChanged(ref changed)) if *changed == path
        ));
        assert_eq!(server.join().unwrap().len(), 3);
        fs::remove_file(&path).unwrap();

        // Detected while deciding whether a retryable failure is worth retrying.
        let (path, mut local) = temp_upload_source("rewritten-retry", b"abc");
        local.content_md5 = Some(ContentMd5::from_bytes([0_u8; 16]));
        let mut responses = upload_preamble();
        responses.push((
            StatusCode::BAD_GATEWAY,
            "temporary proxy failure".to_owned(),
        ));
        let (client, server) = upload_client(responses, 1);
        assert!(matches!(
            client.upload(&local, "/share/root/abc.bin"),
            Err(Error::SourceChanged(ref changed)) if *changed == path
        ));
        assert_eq!(
            server.join().unwrap().len(),
            3,
            "a changed source must not be retransmitted"
        );
        fs::remove_file(&path).unwrap();

        // The upload reported success but the NAS holds something else: fail closed.
        let (path, local) = temp_upload_source("mismatched", b"abc");
        let mut responses = upload_preamble();
        responses.extend([
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            (
                StatusCode::OK,
                getinfo_file("/share/root/abc.bin", local.size + 5, None),
            ),
        ]);
        let (client, server) = upload_client(responses, 0);
        let error = client.upload(&local, "/share/root/abc.bin").unwrap_err();
        assert!(
            matches!(&error, Error::ContentVerificationFailed(remote)
                if remote == "/share/root/abc.bin"),
            "unexpected error: {error}"
        );
        assert_eq!(server.join().unwrap().len(), 4);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn discovery_failures_report_both_routes_and_reject_unusable_payloads() {
        // Neither CGI endpoint answers usefully; the operator needs to see both attempts.
        let (url, server) = scripted_server_with_status(vec![
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
        ]);
        let Err(error) = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        }) else {
            panic!("a discovery response with no API map must not produce a client");
        };
        assert!(is_discovery_response_failure(&error));
        let rendered = error.to_string();
        assert!(rendered.starts_with(
            "unexpected response during File Station API discovery: File Station API discovery failed through the reverse proxy"
        ));
        assert!(rendered.contains("entry.cgi: unexpected response during SYNO.API.Info.query"));
        assert!(rendered.contains("query.cgi fallback:"));
        assert_eq!(
            rendered
                .matches("successful response contained no API map")
                .count(),
            2
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .request_line
                .contains("/prefix/webapi/entry.cgi")
        );
        assert!(
            requests[1]
                .request_line
                .contains("/prefix/webapi/query.cgi")
        );

        // Discovery is unauthenticated, so a malformed body may be quoted back verbatim -- but
        // only a bounded snippet, and the HTML hint must not fire for non-HTML noise.
        let (url, server) = scripted_server_with_status(vec![
            (StatusCode::OK, "not json at all".to_owned()),
            (StatusCode::OK, "not json at all".to_owned()),
        ]);
        let Err(error) = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        }) else {
            panic!("a non-JSON discovery response must not produce a client");
        };
        assert!(is_discovery_response_failure(&error));
        let rendered = error.to_string();
        assert!(rendered.contains("expected a DSM JSON envelope"));
        assert!(rendered.contains("response: not json at all"));
        assert!(
            !rendered.contains("proxy returned HTML"),
            "the HTML routing hint must only appear for actual HTML"
        );
        server.join().unwrap();

        // A discovery-time DSM error keeps its unauthenticated detail, which is safe to show.
        let (url, server) = scripted_server_with_status(vec![
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":102,"errors":[{"api":"SYNO.FileStation.List"}]}}"#
                    .to_owned(),
            ),
            (
                StatusCode::OK,
                r#"{"success":false,"error":{"code":102}}"#.to_owned(),
            ),
        ]);
        let Err(error) = ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        }) else {
            panic!("a discovery DSM error must not produce a client");
        };
        assert!(
            error
                .to_string()
                .contains("code 102: requested API does not exist")
        );
        assert!(is_discovery_response_failure(&error));
        server.join().unwrap();

        assert!(!is_discovery_response_failure(&Error::Message(
            "both discovery routes failed before receiving a response".to_owned()
        )));
    }

    #[test]
    fn error_detail_shapes_are_normalized_without_dropping_information() {
        assert!(error_details(Value::Null).is_empty());
        assert_eq!(
            error_details(serde_json::json!([{"code": 1}, {"code": 2}])),
            vec![
                serde_json::json!({"code": 1}),
                serde_json::json!({"code": 2})
            ]
        );
        assert_eq!(
            error_details(serde_json::json!({"path": "/share"})),
            vec![serde_json::json!({"path": "/share"})]
        );
        assert_eq!(
            error_details(Value::String("scalar".to_owned())),
            vec![Value::String("scalar".to_owned())]
        );
    }

    #[test]
    fn retry_classification_covers_body_and_non_transport_failures() {
        assert!(retryable(&Error::HttpBody {
            operation: "SYNO.FileStation.List.list".to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "truncated"),
        }));
        for error in [
            Error::Cancelled,
            Error::HttpsRequired,
            Error::MissingApi("SYNO.FileStation.MD5".to_owned()),
            Error::SourceChanged(PathBuf::from("/tmp/a")),
            Error::InvalidResponse {
                operation: "SYNO.FileStation.List.list".to_owned(),
                message: "malformed".to_owned(),
            },
        ] {
            assert!(!retryable(&error), "must not retry: {error}");
        }
    }

    #[test]
    fn legacy_md5_and_complete_fingerprint_capability_gates_remain_distinct() {
        let client_for = |api: &str| ApiClient {
            http: crate::blocking_client_builder()
                .expect("ring provider installs")
                .build()
                .expect("client builds"),
            download_http: crate::async_client_builder()
                .expect("ring provider installs")
                .build()
                .expect("client builds"),
            base: Url::parse("https://files.example.test/webapi/").unwrap(),
            apis: HashMap::from([(
                api.to_owned(),
                ApiSpec {
                    path: "entry.cgi".to_owned(),
                    min_version: 1,
                    max_version: 2,
                    _request_format: None,
                },
            )]),
            session: None,
            retries: 0,
            control_timeout: Duration::from_secs(1),
            control_deadline: None,
            upload_timeout: Duration::from_secs(1),
            operation_timeout: Duration::from_secs(1),
            upload_rate_limit: None,
            cancellation: CancellationToken::default(),
            observer: None,
        };

        let md5_only = client_for("SYNO.FileStation.MD5");
        assert!(md5_only.require_content_api().is_ok());
        assert!(matches!(
            md5_only.require_content_fingerprint_api(),
            Err(Error::MissingApi(api)) if api == "SYNO.FileStation.Download"
        ));

        let download_only = client_for("SYNO.FileStation.Download");
        assert!(download_only.require_content_fingerprint_api().is_ok());
        assert!(matches!(
            download_only.require_content_api(),
            Err(Error::MissingApi(api)) if api == "SYNO.FileStation.MD5"
        ));
    }

    #[test]
    fn discovered_cgi_paths_that_change_origin_are_refused() {
        let base = Url::parse("http://files.example.test/prefix/").unwrap();
        let error = endpoint_url(&base, "a:b").unwrap_err();
        assert_eq!(
            error.to_string(),
            "unexpected response during API endpoint discovery: server returned escaping CGI path \"a:b\""
        );
    }

    /// Both TLS relaxations are opt-in and must be honoured exactly as configured: a supplied CA
    /// is loaded, and `--insecure` never becomes the default.
    #[test]
    fn tls_options_load_a_supplied_ca_and_stay_opt_in() {
        const TEST_CA_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----
MIIDLzCCAhegAwIBAgIUcLvianya9E7OvdW+lA817dSrDWcwDQYJKoZIhvcNAQEL
BQAwJjEkMCIGA1UEAwwbc3lub2xvZ3ktZHJpdmUtc3luYyB0ZXN0IENBMCAXDTI2
MDgxMDAwMjIzNVoYDzIxMjYwNzE3MDAyMjM1WjAmMSQwIgYDVQQDDBtzeW5vbG9n
eS1kcml2ZS1zeW5jIHRlc3QgQ0EwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEK
AoIBAQCrmm7c6Tv4rJvcbgQ7GaKAcab3gRjHI/dkh3XxNl/Qtbvm8wq1Ap7mxYIM
tKXGShFpmYgu67aqtLc1CEYrpc7vSqcCyHDzGBEzc7MCtKz4wuVT+pzqD2YuFOqB
Oi9lrmTIk+Odl8CaBb1/okMOSKjQvh7YTW7TMPXW8+cP+1yts+jQwfYgco3Awgfl
Ptfkh+mXioRzcEkqE7yNL/VFRjAFxDzb3Ld4UHyQzMnGdUm7eelWpO7vn5oE3VFp
x4eJZG6lG26TdnJC/TJArMimQmJds+gV39JS4Lop5z0Ys6kgFba4S5N7dF4Ugsum
KxuFSPq9WqLK8xdpo4/MylNhrOy5AgMBAAGjUzBRMB0GA1UdDgQWBBTsj7InEyT+
U/HEZYBh/HRx6zejbjAfBgNVHSMEGDAWgBTsj7InEyT+U/HEZYBh/HRx6zejbjAP
BgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQAC/dCDOIZwNInXqSYL
8b+a2VD9eq7VlI9l5IZIsrs5ps9xJ90NrHCyetFVP2Uue3e9vz1njlMeQ7ktPtlc
fSMaMJxq1zQEAvj7aQU6xllOI8JapViZeyBkC2+RU+gKnHPrtA4KhFv8TgdLgBE+
N48JfJ7rV01YAIfcMhoyeQ3tGz7PMJkGKR9hxcAN/mfxt8cgySZ5mjqUuoaGaGih
Y8afjx8rE5f79lV35/dT77PX2v5VjT6ONqbnIoATrI6spez5vvTL2MsFLk9Tmrvz
OkbEiszT+gQ1PhePf0E73iXu+Zlfch80DMdAOdgzxZ1UVvkZAjsaisQ4po1WxSYn
FplE
-----END CERTIFICATE-----
";
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let ca_path = std::env::temp_dir().join(format!("sdsync-test-ca-{nonce}.pem"));
        fs::write(&ca_path, TEST_CA_PEM).unwrap();

        // A well-formed CA is accepted and the client is usable afterwards.
        let (url, server) = scripted_server(vec![required_discovery()]);
        ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: Some(ca_path.clone()),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .expect("a valid PEM certificate must be accepted");
        assert_eq!(server.join().unwrap().len(), 1);
        fs::remove_file(&ca_path).unwrap();

        // Surrounding commentary is ordinary in distributed CA bundles; the certificate-present
        // check must not turn that into a spurious rejection.
        let annotated_path = std::env::temp_dir().join(format!("sdsync-test-ca-notes-{nonce}.pem"));
        let mut annotated = b"issued by the lab CA, rotate before 2126\n".to_vec();
        annotated.extend_from_slice(TEST_CA_PEM);
        annotated.extend_from_slice(b"trailing operator notes, not a PEM section\n");
        fs::write(&annotated_path, &annotated).unwrap();
        let (url, server) = scripted_server(vec![required_discovery()]);
        ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: Some(annotated_path.clone()),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .expect("a certificate surrounded by comments must still be accepted");
        assert_eq!(server.join().unwrap().len(), 1);
        fs::remove_file(&annotated_path).unwrap();

        // Certificate validation may be disabled only when explicitly requested.
        let (url, server) = scripted_server(vec![required_discovery()]);
        ApiClient::connect(&ClientOptions {
            base_url: url,
            allow_http: true,
            accept_invalid_certs: true,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .expect("explicitly disabled certificate validation must build a client");
        assert_eq!(server.join().unwrap().len(), 1);

        // HTTPS remains mandatory regardless of either TLS relaxation.
        for accept_invalid_certs in [false, true] {
            let Err(error) = ApiClient::connect(&ClientOptions {
                base_url: "http://files.example.test".to_owned(),
                allow_http: false,
                accept_invalid_certs,
                ca_certificate: None,
                connect_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_secs(1),
                retries: 0,
            }) else {
                panic!("plaintext HTTP must be refused without --allow-http");
            };
            assert!(matches!(error, Error::HttpsRequired));
        }
    }

    #[test]
    fn a_rejected_md5_start_never_polls_and_a_failing_stop_never_masks_the_cause() {
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            r#"{"success":false,"error":{"code":402}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client
            .remote_content_md5("/share/file.bin", &CancellationToken::default())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Synology API SYNO.FileStation.MD5.start failed with code 402: file subsystem is busy"
        );
        assert_eq!(
            server.join().unwrap().len(),
            3,
            "a task that never started must not be polled or stopped"
        );

        // Best-effort task cleanup must not replace the cancellation the caller asked for.
        let cancellation = CancellationToken::default();
        let cancel_before_status_response = cancellation.clone();
        let (url, server) = scripted_server_with_status_hook(
            vec![
                (StatusCode::OK, write_probe_discovery(false)),
                (StatusCode::OK, login_response()),
                (StatusCode::OK, task_start_response("unstoppable-md5")),
                (
                    StatusCode::OK,
                    r#"{"success":true,"data":{"finished":false}}"#.to_owned(),
                ),
                (
                    StatusCode::OK,
                    r#"{"success":false,"error":{"code":407}}"#.to_owned(),
                ),
            ],
            move |index| {
                if index == 3 {
                    cancel_before_status_response.cancel();
                }
            },
        );
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        assert!(matches!(
            client.remote_content_md5("/share/file.bin", &cancellation),
            Err(Error::Cancelled)
        ));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(String::from_utf8_lossy(&requests[4].body).contains("method=stop"));
    }

    #[test]
    fn a_directory_listing_without_data_is_not_treated_as_empty() {
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            getinfo_directory("/share"),
            getinfo_directory("/share/root"),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client
            .remote_inventory(
                &RemoteRoot::parse("/share/root").unwrap(),
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unexpected response during SYNO.FileStation.List.list: successful response contained no directory data"
        );
        assert_eq!(server.join().unwrap().len(), 5);
    }

    #[test]
    fn folder_creation_failures_are_reported_to_the_caller() {
        let (url, server) = scripted_server(vec![
            required_discovery(),
            login_response(),
            r#"{"success":false,"error":{"code":1101}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let error = client.create_folder("/share/root/new").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Synology API SYNO.FileStation.CreateFolder.create failed with code 1101: parent folder item-count limit exceeded"
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        let create = String::from_utf8_lossy(&requests[2].body);
        assert!(create.contains("api=SYNO.FileStation.CreateFolder"));
        assert!(create.contains("force_parent=true"));
    }

    #[test]
    fn a_server_copy_is_polled_until_it_finishes_before_content_is_verified() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let destination = "/share/root/new/report.bin";
        let digest = ContentMd5::from_content(b"report");
        let (url, server) = scripted_server(vec![
            write_probe_discovery(true),
            login_response(),
            task_start_response("slow-copy"),
            r#"{"success":true,"data":{"finished":false}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":true}}"#.to_owned(),
            getinfo_file(destination, 6, None),
            scripted_binary_response(b"report"),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        client
            .copy_file_verified(
                &root,
                "/share/root/old/report.bin",
                destination,
                6,
                digest,
                &CancellationToken::default(),
            )
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
        for index in [3, 4] {
            assert!(String::from_utf8_lossy(&requests[index].body).contains("method=status"));
        }
    }

    /// Every verification step after the upload lands is a fail-closed gate: if any of them cannot
    /// be proven, the probe reports failure and still cleans up after itself.
    #[test]
    fn write_probe_copy_phase_failures_all_fail_closed_and_clean_up() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-copy";
        let upload_path = format!("{probe_path}/{WRITE_PROBE_FILE_NAME}");
        let copy_directory = format!("{probe_path}/{WRITE_PROBE_COPY_DIRECTORY}");
        let copy_path = format!("{copy_directory}/{WRITE_PROBE_FILE_NAME}");
        let local = ProbeLocalFile::create(write_probe_fingerprint()).unwrap();
        let size = local.entry.size;
        let mtime_seconds = local.entry.mtime_ms.div_euclid(1000);
        let missing = r#"{"success":false,"error":{"code":408}}"#.to_owned();
        let succeeded = r#"{"success":true}"#.to_owned();

        // Responses 0..=10: everything up to and including the uploaded file's full fingerprint.
        let uploaded = || {
            vec![
                write_probe_discovery(true),
                login_response(),
                getinfo_directory("/share"),
                getinfo_directory("/share/root"),
                missing.clone(),
                succeeded.clone(),
                getinfo_directory(probe_path),
                r#"{"success":true,"data":{"total":0,"files":[]}}"#.to_owned(),
                succeeded.clone(),
                getinfo_file(&upload_path, size, None),
                scripted_binary_response(WRITE_PROBE_PAYLOAD),
            ]
        };
        // Cleanup: four non-recursive deletes then a final absence check that succeeds.
        let cleanup = || {
            vec![
                missing.clone(),
                missing.clone(),
                succeeded.clone(),
                succeeded.clone(),
                missing.clone(),
            ]
        };

        // The uploaded file's own metadata does not match what was sent.
        let mut responses = uploaded();
        responses.push(getinfo_file(&upload_path, size, Some(mtime_seconds + 60)));
        responses.extend(cleanup());
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(
                &root,
                probe_path,
                &local.entry,
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert!(
            matches!(&failure.cause, Error::RemoteSnapshotChanged(path) if *path == upload_path),
            "unexpected cause: {}",
            failure.cause
        );
        assert!(failure.report.upload_attempted);
        assert!(!failure.report.upload_verified);
        assert!(!failure.report.server_copy_attempted);
        assert!(failure.report.cleanup_completed);
        assert_eq!(server.join().unwrap().len(), 17);

        // The copy directory cannot be created.
        let mut responses = uploaded();
        responses.push(getinfo_file(&upload_path, size, Some(mtime_seconds)));
        responses.push(r#"{"success":false,"error":{"code":411}}"#.to_owned());
        responses.extend(cleanup());
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(
                &root,
                probe_path,
                &local.entry,
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert_eq!(
            failure.cause.to_string(),
            "Synology API SYNO.FileStation.CreateFolder.create failed with code 411: remote filesystem is read-only"
        );
        assert!(failure.report.upload_verified);
        assert!(!failure.report.server_copy_attempted);
        assert!(failure.report.cleanup_completed);
        assert_eq!(server.join().unwrap().len(), 18);

        // The copy task itself fails after the destination was prepared.
        let mut responses = uploaded();
        responses.extend([
            getinfo_file(&upload_path, size, Some(mtime_seconds)),
            succeeded.clone(),
            getinfo_directory(&copy_directory),
            r#"{"success":true,"data":{"total":0,"files":[]}}"#.to_owned(),
            missing.clone(),
            r#"{"success":false,"error":{"code":417}}"#.to_owned(),
        ]);
        responses.extend(cleanup());
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(
                &root,
                probe_path,
                &local.entry,
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert_eq!(
            failure.cause.to_string(),
            "Synology API SYNO.FileStation.CopyMove.start failed with code 417: remote input/output error"
        );
        assert!(failure.report.server_copy_attempted);
        assert!(!failure.report.server_copy_verified);
        assert!(failure.report.cleanup_completed);
        assert_eq!(server.join().unwrap().len(), 22);

        // The copy completes and its content matches, but its metadata does not.
        let mut responses = uploaded();
        responses.extend([
            getinfo_file(&upload_path, size, Some(mtime_seconds)),
            succeeded.clone(),
            getinfo_directory(&copy_directory),
            r#"{"success":true,"data":{"total":0,"files":[]}}"#.to_owned(),
            missing.clone(),
            task_start_response("probe-copy-task"),
            r#"{"success":true,"data":{"finished":true}}"#.to_owned(),
            getinfo_file(&copy_path, size, None),
            scripted_binary_response(WRITE_PROBE_PAYLOAD),
            getinfo_file(&copy_path, size, Some(mtime_seconds + 60)),
        ]);
        responses.extend(cleanup());
        let (client, server) = write_probe_client(responses);
        let failure = client
            .run_write_probe_with_local(
                &root,
                probe_path,
                &local.entry,
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert!(
            matches!(&failure.cause, Error::RemoteSnapshotChanged(path) if *path == copy_path),
            "unexpected cause: {}",
            failure.cause
        );
        assert!(failure.report.server_copy_attempted);
        assert!(!failure.report.server_copy_verified);
        assert!(failure.report.cleanup_completed);
        assert_eq!(server.join().unwrap().len(), 26);
    }

    /// `query=all` returns entries authored by whoever wrote each installed package, so one bad
    /// entry must cost a line of output rather than the whole enumeration.
    #[test]
    fn full_api_enumeration_skips_entries_it_cannot_read_instead_of_failing() {
        let catalogue_body = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                // A third-party package advertising a shape the documented map does not use.
                "SYNO.Third.Party": 42,
                // Present, but with no version range: reportable, and honestly unanswerable.
                "SYNO.Partial.Entry": {"path": "entry.cgi"},
            }
        })
        .to_string();
        let (url, server) = scripted_server(vec![browser_discovery(), catalogue_body]);
        let client = browsing_test_client(url);

        let catalogue = client.enumerate_all_apis().unwrap();
        assert_eq!(catalogue.apis.len(), 3);
        assert_eq!(catalogue.unusable_entries, 1);
        assert_eq!(
            catalogue.apis["SYNO.FileStation.List"].offers_version(2),
            Some(true)
        );
        assert_eq!(
            catalogue.apis["SYNO.FileStation.List"].offers_version(3),
            Some(false)
        );
        // No range advertised is not the same answer as "no".
        assert_eq!(catalogue.apis["SYNO.Partial.Entry"].offers_version(1), None);

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(String::from_utf8_lossy(&requests[1].body).contains("query=all"));
    }

    /// The enumeration reuses discovery's `entry.cgi` to `query.cgi` fallback: this request is
    /// subject to exactly the same reverse-proxy misrouting the fallback exists to survive.
    #[test]
    fn full_api_enumeration_falls_back_to_query_cgi_and_reports_both_routes() {
        let catalogue_body = serde_json::json!({
            "success": true,
            "data": {"SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7}}
        })
        .to_string();
        let (url, server) = scripted_server_with_status(vec![
            (StatusCode::OK, browser_discovery()),
            (StatusCode::BAD_GATEWAY, "no route".to_owned()),
            (StatusCode::OK, catalogue_body),
        ]);
        let client = browsing_test_client(url);
        let catalogue = client.enumerate_all_apis().unwrap();
        assert_eq!(catalogue.apis.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[1].request_line.contains("/webapi/entry.cgi"));
        assert!(requests[2].request_line.contains("/webapi/query.cgi"));

        // Both routes failing is reported as one message naming both, not as a bare timeout.
        let (url, server) = scripted_server_with_status(vec![
            (StatusCode::OK, browser_discovery()),
            (StatusCode::BAD_GATEWAY, "no route".to_owned()),
            (StatusCode::BAD_GATEWAY, "no route".to_owned()),
        ]);
        let client = browsing_test_client(url);
        let error = client.enumerate_all_apis().unwrap_err();
        let rendered = rendered_error(&error);
        assert!(
            rendered.contains("entry.cgi"),
            "unexpected error: {rendered}"
        );
        assert!(
            rendered.contains("query.cgi fallback"),
            "unexpected error: {rendered}"
        );
        server.join().unwrap();
    }

    /// The ablation must vary only the channel, and it must vary it on the wire.
    #[test]
    fn the_session_channel_probe_presents_exactly_the_channels_it_names() {
        let ok = r#"{"success":true,"data":{"total":0,"shares":[]}}"#.to_owned();
        let rejected = r#"{"success":false,"error":{"code":119}}"#.to_owned();
        let (url, server) = scripted_server(vec![
            browser_discovery(),
            login_response(),
            ok.clone(),
            ok,
            rejected.clone(),
            rejected,
        ]);
        let mut client = browsing_test_client(url);
        client.login("alice", "password", None).unwrap();

        let probes = client
            .probe_session_channels("no second login was attempted at this level")
            .unwrap();
        assert_eq!(
            probes.map(|probe| probe.channels),
            SessionChannels::ABLATION_ORDER
        );
        assert!(probes[0].accepted() && probes[1].accepted());
        assert!(!probes[2].accepted() && probes[2].session_rejected());
        assert_eq!(probes[2].dsm_code, Some(119));
        assert_eq!(probes[3].http_status, Some(200));
        // The tokenless-login variant needs a session this client cannot create, so it reports
        // that it did not run rather than contributing a rejection nothing observed.
        assert!(!probes[4].ran());
        assert_eq!(
            probes[4].skipped,
            Some("no second login was attempted at this level")
        );

        let requests = server.join().unwrap();
        let shape = |request: &CapturedRequest| {
            let body = String::from_utf8_lossy(&request.body).into_owned();
            let header = |name: &str| {
                request
                    .headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case(name))
            };
            (
                body.contains("_sid="),
                body.contains("SynoToken="),
                header("cookie"),
                header(X_SYNO_TOKEN_HEADER),
            )
        };
        // All, then the SID field alone, then the cookie alone, then the token header alone.
        assert_eq!(
            requests[2..].iter().map(shape).collect::<Vec<_>>(),
            [
                (true, true, true, true),
                (true, true, false, false),
                (false, false, true, false),
                (false, false, false, true),
            ]
        );
        // No probe ever presents a session identifier the login did not produce.
        for request in &requests[2..] {
            let body = String::from_utf8_lossy(&request.body);
            assert!(
                body.contains("method=list_share"),
                "unexpected body: {body}"
            );
            assert!(body.contains("limit=1"), "unexpected body: {body}");
        }
    }

    /// The tokenless ablation variant is only worth anything if the login really omits the flag.
    ///
    /// `enable_syno_token=yes` is what makes DSM treat a session as the browser-style,
    /// cookie-and-header kind. The variant exists to find out whether that is why a DSM refuses
    /// the documented `_sid` parameter path, so a login that quietly kept sending the flag would
    /// answer a different question while looking like it answered this one.
    #[test]
    fn a_tokenless_login_omits_the_flag_and_presents_only_the_documented_field() {
        let (url, server) = scripted_server(vec![
            required_discovery(),
            // DSM issues no SynoToken to a login that did not ask for one.
            r#"{"success":true,"data":{"sid":"tokenless-session"}}"#.to_owned(),
            r#"{"success":true,"data":{"total":0,"shares":[]}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client
            .login_without_syno_token("alice", "password", None)
            .unwrap();
        let probe = client.probe_tokenless_sid_field().unwrap();
        assert!(probe.ran());
        assert!(probe.accepted());
        assert_eq!(
            probe.channels,
            SessionChannels::SidFieldOnlyTokenlessLogin,
            "the probe must report the variant it actually ran"
        );

        let requests = server.join().unwrap();
        let login = String::from_utf8_lossy(&requests[1].body).into_owned();
        assert!(
            !login.contains("enable_syno_token"),
            "the tokenless login must not ask for a token: {login}"
        );
        assert!(login.contains("format=sid"));
        let probed = String::from_utf8_lossy(&requests[2].body).into_owned();
        assert!(probed.contains("_sid="), "unexpected probe body: {probed}");
        assert!(
            !probed.contains("SynoToken="),
            "the variant presents the documented field alone: {probed}"
        );
        for header in ["cookie", X_SYNO_TOKEN_HEADER] {
            assert!(
                !requests[2]
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(header)),
                "the variant must attach no {header} header"
            );
        }
    }

    /// The ordinary login is unchanged by the tokenless one existing.
    #[test]
    fn the_ordinary_login_still_negotiates_a_syno_token() {
        let (url, server) = scripted_server(vec![required_discovery(), login_response()]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let requests = server.join().unwrap();
        let login = String::from_utf8_lossy(&requests[1].body).into_owned();
        assert!(
            login.contains("enable_syno_token=yes"),
            "unexpected login body: {login}"
        );
    }

    #[test]
    fn session_channel_selectors_and_probe_placeholders_are_self_consistent() {
        for channels in SessionChannels::ABLATION_ORDER {
            let probe = ChannelProbe::unrun(channels);
            assert!(!probe.accepted());
            assert!(!probe.session_rejected());
            assert_eq!(probe.channels, channels);
            assert!(!channels.as_str().is_empty());
            assert!(!channels.describe().is_empty());
        }
        assert!(SessionChannels::All.sends_cookie_header());
        assert!(SessionChannels::All.sends_token_header());
        assert!(SessionChannels::All.sends_sid_field());
        assert!(SessionChannels::All.sends_token_field());
        assert!(SessionChannels::SidFieldOnly.sends_sid_field());
        assert!(SessionChannels::SidFieldOnly.sends_token_field());
        assert!(!SessionChannels::SidFieldOnly.sends_cookie_header());
        assert!(!SessionChannels::SidFieldOnly.sends_token_header());
        assert!(SessionChannels::CookieOnly.sends_cookie_header());
        assert!(!SessionChannels::CookieOnly.sends_sid_field());
        assert!(SessionChannels::TokenHeaderOnly.sends_token_header());
        assert!(!SessionChannels::TokenHeaderOnly.sends_cookie_header());
        // Every ablation variant has a distinct name, or the report could not tell them apart.
        let names = SessionChannels::ABLATION_ORDER
            .map(SessionChannels::as_str)
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), SESSION_CHANNEL_VARIANTS);
        // An unauthenticated client has no session to ablate and says so rather than sending one.
        let (url, server) = scripted_server(vec![browser_discovery()]);
        let client = browsing_test_client(url);
        assert!(client.probe_session_channels("unavailable").is_err());
        server.join().unwrap();
    }

    /// `SYNO.FileStation.Info.get` is the only documented non-admin call that names the host.
    ///
    /// The third response is the one a real DSM 7.2 sends, and the reason this probe reported
    /// `decode` twice per run against a healthy NAS: `support_virtual_protocol` arrives as a JSON
    /// array rather than the comma-separated string the guide documents, and `support_virtual` --
    /// which the guide's worked example uses for the same list -- is an unrelated object of mount
    /// toggles. Reading both through one serde alias made every DSM 7 answer undecodable.
    #[test]
    fn file_station_info_reads_every_shape_dsm_answers_with() {
        let documented = serde_json::json!({
            "success": true,
            "data": {
                "hostname": "DiskStation",
                "is_manager": true,
                "support_sharing": true,
                "support_virtual_protocol": "cifs,nfs,iso"
            }
        })
        .to_string();
        // Synology's own guide uses the other spelling in its worked example.
        let worked_example = serde_json::json!({
            "success": true,
            "data": {"hostname": "Other Station", "support_virtual": "cifs"}
        })
        .to_string();
        let dsm_seven = serde_json::json!({
            "success": true,
            "data": {
                "enable_list_usergrp": false,
                "hostname": "nascheckoffice",
                "is_manager": true,
                "items": [{"gid": 100}],
                "support_file_request": true,
                "support_sharing": true,
                "support_vfs": true,
                "support_virtual": {"enable_iso_mount": true, "enable_remote_mount": true},
                "support_virtual_protocol": ["cifs", "nfs", "iso"],
                "system_codepage": "enu",
                "uid": 1026
            }
        })
        .to_string();
        // Nothing but the envelope: every member is optional, so a probe degrades to "not
        // reported" instead of failing the section it is diagnosing.
        let bare = serde_json::json!({"success": true, "data": {}}).to_string();
        let (url, server) = scripted_server(vec![
            info_discovery(),
            login_response(),
            documented,
            worked_example,
            dsm_seven,
            bare,
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();

        let first = client.file_station_info().unwrap();
        assert_eq!(
            first.hostname.map(|host| host.as_str().to_owned()),
            Some("DiskStation".to_owned())
        );
        assert_eq!(first.is_manager, Some(true));
        assert_eq!(first.support_sharing, Some(true));
        assert_eq!(
            first
                .support_virtual_protocol
                .map(|protocols| protocols.as_str().to_owned()),
            Some("cifs,nfs,iso".to_owned())
        );

        let second = client.file_station_info().unwrap();
        // Sanitized on the way in: a host name is server-supplied text, not terminal formatting.
        assert_eq!(
            second.hostname.map(|host| host.as_str().to_owned()),
            Some("Other_Station".to_owned())
        );
        assert_eq!(second.is_manager, None, "DSM did not say, so neither do we");
        assert_eq!(second.support_sharing, None);
        assert_eq!(
            second
                .support_virtual_protocol
                .map(|protocols| protocols.as_str().to_owned()),
            Some("cifs".to_owned()),
            "the guide's worked-example spelling still reads when it carries protocol names"
        );

        let third = client.file_station_info().unwrap();
        assert_eq!(
            third.hostname.map(|host| host.as_str().to_owned()),
            Some("nascheckoffice".to_owned())
        );
        assert_eq!(third.is_manager, Some(true));
        assert_eq!(
            third
                .support_virtual_protocol
                .map(|protocols| protocols.as_str().to_owned()),
            Some("cifs,nfs,iso".to_owned()),
            "the array form is joined the way the guide's prose describes the value"
        );

        let fourth = client.file_station_info().unwrap();
        assert_eq!(fourth, FileStationInfo::default());
        server.join().unwrap();
    }

    /// A capability probe names its own API and reports how DSM answered, nothing more.
    #[test]
    fn a_capability_probe_reports_the_dsm_answer_without_interpreting_it() {
        let (url, server) = scripted_server(vec![
            info_discovery(),
            login_response(),
            r#"{"success":true,"data":{"total":0,"folders":[]}}"#.to_owned(),
            r#"{"success":false,"error":{"code":105}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();

        let spec = || CapabilityProbeSpec {
            api: "SYNO.FileStation.VirtualFolder",
            method: "list",
            version: 2,
            cgi_path: "entry.cgi",
            parameters: vec![pair("limit", "1")],
        };
        let works = client.probe_capability(spec()).unwrap();
        assert_eq!(works.api, "SYNO.FileStation.VirtualFolder");
        assert_eq!(works.method, "list");
        assert_eq!(works.version, 2);
        assert_eq!(works.outcome, RequestOutcome::Ok);
        assert_eq!(works.dsm_code, None);

        let refused = client.probe_capability(spec()).unwrap();
        assert_eq!(refused.outcome, RequestOutcome::DsmError);
        assert_eq!(refused.dsm_code, Some(105));

        // A CGI path that would leave the configured origin is refused before any request.
        let escaping = client.probe_capability(CapabilityProbeSpec {
            api: "SYNO.FileStation.VirtualFolder",
            method: "list",
            version: 2,
            cgi_path: "../../elsewhere.cgi",
            parameters: Vec::new(),
        });
        assert!(escaping.is_err());
        server.join().unwrap();
    }

    /// The permission check's walk is the resolution: reporting it costs no extra request.
    #[test]
    fn the_destination_walk_is_reported_component_by_component() {
        let root = RemoteRoot::parse("/team/year/quarter").unwrap();

        // Every component exists.
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            getinfo_directory("/team"),
            getinfo_directory("/team/year"),
            getinfo_directory("/team/year/quarter"),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let (resolution, check) = client.verify_destination_writable_with_resolution(&root);
        assert!(check.unwrap().destination_exists);
        assert!(resolution.fully_resolved());
        assert!(!resolution.share_root_missing());
        assert_eq!(resolution.total_components, 3);
        assert_eq!(
            resolution
                .segments
                .iter()
                .map(|segment| (segment.path.as_str(), segment.depth, segment.exists))
                .collect::<Vec<_>>(),
            [
                ("/team", 1, true),
                ("/team/year", 2, true),
                ("/team/year/quarter", 3, true)
            ]
        );
        assert_eq!(
            resolution
                .nearest_existing()
                .map(|segment| segment.path.as_str()),
            Some("/team/year/quarter")
        );
        server.join().unwrap();

        // The last component is missing: the walk stops there and names it.
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            getinfo_directory("/team"),
            getinfo_directory("/team/year"),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let (resolution, check) = client.verify_destination_writable_with_resolution(&root);
        assert!(!check.unwrap().destination_exists);
        assert!(!resolution.fully_resolved());
        assert_eq!(resolution.first_missing, Some(3));
        assert!(!resolution.share_root_missing());
        assert_eq!(resolution.segments.last().unwrap().dsm_code, Some(408));
        assert_eq!(
            resolution
                .nearest_existing()
                .map(|segment| segment.path.as_str()),
            Some("/team/year")
        );
        server.join().unwrap();

        // The share itself is missing: a different fault, and the walk says so.
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let (resolution, check) = client.verify_destination_writable_with_resolution(&root);
        assert!(matches!(check.unwrap_err(), Error::ShareNotWritable(share) if share == "team"));
        assert!(resolution.share_root_missing());
        assert!(resolution.nearest_existing().is_none());
        server.join().unwrap();
    }

    /// A mount boundary and a non-directory ancestor both stop the walk, and both are recorded.
    #[test]
    fn the_destination_walk_records_the_component_that_stopped_it() {
        let root = RemoteRoot::parse("/team/mounted/child").unwrap();
        let mounted = serde_json::json!({
            "success": true,
            "data": {"files": [{
                "path": "/team/mounted",
                "name": "mounted",
                "isdir": true,
                "additional": {"mount_point_type": "cifs"}
            }]}
        })
        .to_string();
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            getinfo_directory("/team"),
            mounted,
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let (resolution, check) = client.verify_destination_writable_with_resolution(&root);
        assert!(matches!(check.unwrap_err(), Error::RemoteMountRoot { .. }));
        assert_eq!(resolution.segments.len(), 2);
        assert!(resolution.segments[1].mount_boundary);
        assert!(resolution.segments[1].exists);
        server.join().unwrap();

        // An ancestor that exists but is a file stops the walk too, and is recorded as existing
        // and not a directory rather than as absent.
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            getinfo_file("/team", 12, None),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let (resolution, check) = client.verify_destination_writable_with_resolution(&root);
        assert!(check.is_err());
        assert!(resolution.segments[0].exists);
        assert!(!resolution.segments[0].is_directory);
        assert_eq!(resolution.first_missing, None);
        server.join().unwrap();

        // A transport-shaped failure mid-walk records the component it stopped on.
        let (url, server) = scripted_server(vec![
            write_probe_discovery(false),
            login_response(),
            getinfo_directory("/team"),
            r#"{"success":false,"error":{"code":105}}"#.to_owned(),
        ]);
        let mut client = connect_test_client(url);
        client.login("alice", "password", None).unwrap();
        let (resolution, check) = client.verify_destination_writable_with_resolution(&root);
        assert_eq!(check.unwrap_err().api_code(), Some(105));
        assert_eq!(resolution.segments.len(), 2);
        assert_eq!(resolution.segments[1].dsm_code, Some(105));
        assert!(!resolution.segments[1].exists);
        server.join().unwrap();
    }

    /// Every requirement this tool declares is one it can actually name a version for.
    #[test]
    fn the_declared_api_requirements_are_coherent() {
        let mut seen = BTreeSet::new();
        for requirement in API_REQUIREMENTS {
            assert!(
                seen.insert(requirement.api),
                "{} is declared twice",
                requirement.api
            );
            assert!(requirement.version >= 1);
            assert!(!requirement.purpose.is_empty());
            assert!(
                DISCOVERY_APIS.contains(&requirement.api),
                "{} is required but never discovered",
                requirement.api
            );
        }
        // The allowlist and the requirement table describe the same set, so neither can drift.
        assert_eq!(seen.len(), DISCOVERY_APIS.len());
    }

    /// A least-privilege client, for the diagnostics that need only Auth and List.
    fn browsing_test_client(base_url: String) -> ApiClient {
        ApiClient::connect_for_browsing(&ClientOptions {
            base_url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        })
        .unwrap()
    }

    /// Discovery covering only the APIs the info and capability probes need.
    fn info_discovery() -> String {
        serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.Info": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
                "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3}
            }
        })
        .to_string()
    }
}
