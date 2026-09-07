//! Focused, secret-free runtime observability.
//!
//! Log records are closed enums plus numeric counters. There is intentionally no public
//! free-form message, header, token, URL, or key/value field. Bearer credentials can only be
//! loaded from an environment variable or file and are held in zeroizing memory.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use reqwest::redirect::Policy;
use zeroize::{Zeroize, Zeroizing};

const MAX_BEARER_TOKEN_BYTES: u64 = 16 * 1024;
const MAX_REMOTE_QUEUE: usize = 65_536;
const MAX_FILE_BACKUPS: usize = 32;

/// Severity threshold. A configured level includes that level and every more-severe level.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    #[default]
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl LogLevel {
    pub fn parse(value: &str) -> ObservabilityResult<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "warn" | "warning" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(ObservabilityError::InvalidLogLevel),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    fn permits(self, event: Self) -> bool {
        event <= self
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// User-facing verbosity when no explicit log level is selected.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
    VeryVerbose,
}

impl Verbosity {
    fn log_level(self) -> LogLevel {
        match self {
            Self::Quiet => LogLevel::Warn,
            Self::Normal => LogLevel::Info,
            Self::Verbose => LogLevel::Debug,
            Self::VeryVerbose => LogLevel::Trace,
        }
    }
}

/// Resolve log level with unambiguous precedence: explicit CLI value, then environment,
/// then the verbosity-derived default. Lower-precedence invalid values are not inspected.
pub fn resolve_log_level(
    explicit: Option<&str>,
    environment: Option<&str>,
    verbosity: Verbosity,
) -> ObservabilityResult<LogLevel> {
    if let Some(level) = explicit {
        LogLevel::parse(level)
    } else if let Some(level) = environment {
        LogLevel::parse(level)
    } else {
        Ok(verbosity.log_level())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Human,
    /// One complete JSON object per line.
    Json,
}

/// Stable machine-readable event codes. Human messages are derived from these codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCode {
    RunBuild,
    RunStarted,
    RunCompleted,
    RunFailed,
    LocalScanStarted,
    LocalScanCompleted,
    ConnectionEstablished,
    ApiDiscoveryStarted,
    ApiDiscoveryCompleted,
    AuthenticationStarted,
    SessionEstablished,
    AuthenticationCompleted,
    RemoteScanStarted,
    RemoteScanCompleted,
    PlanReady,
    ApiCallStarted,
    ApiCallCompleted,
    ApiCallRedirected,
    UploadStarted,
    UploadAttemptStarted,
    UploadProgress,
    UploadCompleted,
    UploadFailed,
    DirectoryCreated,
    EntryDeleted,
    RetryScheduled,
    CancellationRequested,
}

impl EventCode {
    /// Every event code, in emission order.
    ///
    /// Exhaustive by construction: a new variant that is not listed here fails
    /// `every_event_code_has_a_stable_machine_and_human_name`, so no code can ship without a
    /// stable machine name and human message.
    pub const ALL: [Self; 27] = [
        Self::RunBuild,
        Self::RunStarted,
        Self::RunCompleted,
        Self::RunFailed,
        Self::LocalScanStarted,
        Self::LocalScanCompleted,
        Self::ConnectionEstablished,
        Self::ApiDiscoveryStarted,
        Self::ApiDiscoveryCompleted,
        Self::AuthenticationStarted,
        Self::SessionEstablished,
        Self::AuthenticationCompleted,
        Self::RemoteScanStarted,
        Self::RemoteScanCompleted,
        Self::PlanReady,
        Self::ApiCallStarted,
        Self::ApiCallCompleted,
        Self::ApiCallRedirected,
        Self::UploadStarted,
        Self::UploadAttemptStarted,
        Self::UploadProgress,
        Self::UploadCompleted,
        Self::UploadFailed,
        Self::DirectoryCreated,
        Self::EntryDeleted,
        Self::RetryScheduled,
        Self::CancellationRequested,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::RunBuild => "run.build",
            Self::RunStarted => "run.started",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
            Self::LocalScanStarted => "local_scan.started",
            Self::LocalScanCompleted => "local_scan.completed",
            Self::ConnectionEstablished => "connection.established",
            Self::ApiDiscoveryStarted => "api_discovery.started",
            Self::ApiDiscoveryCompleted => "api_discovery.completed",
            Self::AuthenticationStarted => "authentication.started",
            Self::SessionEstablished => "session.established",
            Self::AuthenticationCompleted => "authentication.completed",
            Self::RemoteScanStarted => "remote_scan.started",
            Self::RemoteScanCompleted => "remote_scan.completed",
            Self::PlanReady => "plan.ready",
            Self::ApiCallStarted => "api_call.started",
            Self::ApiCallCompleted => "api_call.completed",
            Self::ApiCallRedirected => "api_call.redirected",
            Self::UploadStarted => "upload.started",
            Self::UploadAttemptStarted => "upload.attempt_started",
            Self::UploadProgress => "upload.progress",
            Self::UploadCompleted => "upload.completed",
            Self::UploadFailed => "upload.failed",
            Self::DirectoryCreated => "directory.created",
            Self::EntryDeleted => "entry.deleted",
            Self::RetryScheduled => "retry.scheduled",
            Self::CancellationRequested => "cancellation.requested",
        }
    }

    fn human(self) -> &'static str {
        match self {
            Self::RunBuild => "build",
            Self::RunStarted => "sync run started",
            Self::RunCompleted => "sync run completed",
            Self::RunFailed => "sync run failed",
            Self::LocalScanStarted => "local scan started",
            Self::LocalScanCompleted => "local scan completed",
            Self::ConnectionEstablished => "connection established",
            Self::ApiDiscoveryStarted => "API discovery started",
            Self::ApiDiscoveryCompleted => "API discovery completed",
            Self::AuthenticationStarted => "authentication started",
            Self::SessionEstablished => "session established",
            Self::AuthenticationCompleted => "authentication completed",
            Self::RemoteScanStarted => "remote scan started",
            Self::RemoteScanCompleted => "remote scan completed",
            Self::PlanReady => "sync plan ready",
            Self::ApiCallStarted => "API call started",
            Self::ApiCallCompleted => "API call completed",
            Self::ApiCallRedirected => "API call redirected",
            Self::UploadStarted => "upload started",
            Self::UploadAttemptStarted => "upload attempt started",
            Self::UploadProgress => "upload progress",
            Self::UploadCompleted => "upload completed",
            Self::UploadFailed => "upload failed",
            Self::DirectoryCreated => "directory created",
            Self::EntryDeleted => "entry deleted",
            Self::RetryScheduled => "retry scheduled",
            Self::CancellationRequested => "cancellation requested",
        }
    }
}

/// Compile-time build identity.
///
/// Every member is a `&'static str` stamped by `build.rs`, so this type carries no runtime-derived
/// text and cannot hold a secret by construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildIdentity {
    pub name: &'static str,
    pub version: &'static str,
    pub target: &'static str,
    pub profile: &'static str,
    pub commit: &'static str,
}

/// The identity of this binary, for the startup banner and the diagnostic report.
pub const BUILD: BuildIdentity = BuildIdentity {
    name: env!("CARGO_PKG_NAME"),
    version: env!("SDSYNC_VERSION"),
    target: env!("SDSYNC_BUILD_TARGET"),
    profile: env!("SDSYNC_BUILD_PROFILE"),
    commit: env!("SDSYNC_BUILD_COMMIT"),
};

impl BuildIdentity {
    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "version": self.version,
            "target": self.target,
            "profile": self.profile,
            "commit": self.commit,
        })
    }

    fn human(self) -> String {
        format!(
            "{} {} ({}) {} {}",
            self.name, self.version, self.commit, self.target, self.profile
        )
    }
}

/// Bounded, sanitized, inline ASCII text.
///
/// This is the only member of any log record that can hold runtime-derived text, and
/// [`InlineAscii::sanitized`] is its only constructor. There is deliberately no `From<&str>`, no
/// `new`, and no `Deref<Target = str>`, so a raw value cannot reach a record by accident. Being
/// `Copy` and heap-free also keeps [`LogEvent`] `Copy`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InlineAscii<const N: usize> {
    bytes: [u8; N],
    length: u8,
}

impl<const N: usize> InlineAscii<N> {
    /// Retain `[A-Za-z0-9]`, `.`, `-`, `_`, `/`, `:`, and `,`; replace every other byte with `_`.
    ///
    /// The comma is retained solely as a list separator for fields that report several names.
    ///
    /// Replacement rather than removal keeps the result the same shape as the input, so a
    /// substituted byte is visible instead of silently closing a gap. Over-long input is truncated
    /// and marked with a trailing `~`.
    pub fn sanitized(input: &str) -> Self {
        debug_assert!(
            N <= u8::MAX as usize,
            "InlineAscii capacity must fit its u8 length"
        );
        let mut bytes = [0_u8; N];
        let mut length = 0_usize;
        let mut truncated = false;
        for byte in input.bytes() {
            if length == N {
                truncated = true;
                break;
            }
            bytes[length] = if byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b'_' | b'/' | b':' | b',')
            {
                byte
            } else {
                b'_'
            };
            length += 1;
        }
        if truncated {
            bytes[N - 1] = b'~';
            length = N;
        }
        Self {
            bytes,
            length: u8::try_from(length).unwrap_or(u8::MAX),
        }
    }

    pub fn as_str(&self) -> &str {
        let end = usize::from(self.length).min(N);
        // Every stored byte came from `sanitized`, which only ever writes ASCII.
        std::str::from_utf8(&self.bytes[..end]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
}

impl<const N: usize> Default for InlineAscii<N> {
    /// The empty token, which is what "no value was observed" renders as.
    fn default() -> Self {
        Self::sanitized("")
    }
}

impl<const N: usize> fmt::Display for InlineAscii<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A URL path or host, bounded to a length that cannot fill a log line.
pub type BoundedText = InlineAscii<64>;

/// Which session channels one request carried.
///
/// Presence only: no value of any channel is representable in this type. The four channels are
/// tracked separately because DSM accepts several at once, and knowing which combination was on
/// the wire is what distinguishes a rejected credential from a mis-carried session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionTransport {
    /// A `Cookie: id=<sid>` header this client synthesized.
    pub cookie_header: bool,
    /// The `X-SYNO-TOKEN` header.
    pub syno_token_header: bool,
    /// The `_sid` form field.
    pub sid_field: bool,
    /// The `SynoToken` form field.
    pub syno_token_field: bool,
}

impl SessionTransport {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "cookie_header": self.cookie_header,
            "syno_token_header": self.syno_token_header,
            "sid_field": self.sid_field,
            "syno_token_field": self.syno_token_field,
        })
    }

    /// Name the attached channels, for a log line or a diagnostic report.
    pub fn describe(self) -> String {
        if self.is_empty() {
            return "none".to_owned();
        }
        let mut parts = Vec::with_capacity(4);
        if self.cookie_header {
            parts.push("cookie");
        }
        if self.syno_token_header {
            parts.push("token-header");
        }
        if self.sid_field {
            parts.push("sid-field");
        }
        if self.syno_token_field {
            parts.push("token-field");
        }
        parts.join("+")
    }
}

/// How a request body was encoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestTransport {
    Form,
    Multipart,
    Download,
}

impl RequestTransport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Multipart => "multipart",
            Self::Download => "download",
        }
    }
}

/// How one request finished.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestOutcome {
    Ok,
    DsmError,
    HttpStatus,
    Redirect,
    Transport,
    Decode,
}

impl RequestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::DsmError => "dsm-error",
            Self::HttpStatus => "http-status",
            Self::Redirect => "redirect",
            Self::Transport => "transport",
            Self::Decode => "decode",
        }
    }

    pub fn is_failure(self) -> bool {
        self != Self::Ok
    }
}

/// What kind of mismatch stopped a response body from deserializing.
///
/// A [`RequestOutcome::Decode`] on its own tells an operator only that DSM answered with
/// something the client could not read, which is the least actionable failure this tool can
/// report. This names the shape of the disagreement instead.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DecodeFaultKind {
    /// The response was read but the reason could not be classified any further.
    #[default]
    Unspecified,
    /// A member the client requires was not in the object.
    MissingField,
    /// The object carried a member the client's schema rejects.
    UnknownField,
    /// One member appeared twice, which for us means two spellings mapped to one field.
    DuplicateField,
    /// The member was there under a JSON type the client does not accept.
    TypeMismatch,
    /// The bytes were not JSON at all.
    Syntax,
    /// The body ended in the middle of a value.
    UnexpectedEnd,
}

impl DecodeFaultKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::MissingField => "missing-field",
            Self::UnknownField => "unknown-field",
            Self::DuplicateField => "duplicate-field",
            Self::TypeMismatch => "type-mismatch",
            Self::Syntax => "syntax",
            Self::UnexpectedEnd => "unexpected-end",
        }
    }
}

/// A JSON value's type.
///
/// The type of a value is schema, not content: it says `array` where the value itself might have
/// said `["cifs","nfs"]`, and it cannot carry a session identifier however DSM shapes its reply.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JsonKind {
    /// The response did not say, or the message did not name a type.
    #[default]
    Unknown,
    /// The member was not present at all.
    Absent,
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl JsonKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Absent => "absent",
            Self::Null => "null",
            Self::Bool => "boolean",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

/// Why one response body did not deserialize, in terms of schema rather than of content.
///
/// Every member is a fixed enum, an integer, or bounded sanitized text drawn from a *name*: the
/// dotted path is built from the object keys and array indices walked to reach the disagreement,
/// the field is the member the deserializer named, and `expected` is the deserializer's own
/// description of what it wanted. No value from the response is representable here, which is the
/// property that lets this be printed next to a failing call on an operator's terminal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecodeFault {
    pub kind: DecodeFaultKind,
    /// Dotted path to the member that did not match, such as `data.files.0.name`. Array elements
    /// appear as their index. Empty when the position could not be resolved to a member.
    pub path: BoundedText,
    /// The member the deserializer named, when it named one.
    pub field: ShortToken,
    /// What the deserializer wanted, in its own words (`a string`, `u64`, `struct Envelope`).
    pub expected: ShortToken,
    /// The JSON type actually found at [`Self::path`].
    pub found: JsonKind,
    /// 1-based position the deserializer reported, retained so two runs can be compared.
    pub line: u32,
    pub column: u32,
}

impl DecodeFault {
    /// One line an operator can act on, in the same `key=value` vocabulary the call lines use.
    pub fn describe(self) -> String {
        use std::fmt::Write as _;
        let mut text = self.kind.as_str().to_owned();
        if !self.path.is_empty() {
            let _ = write!(text, " at {}", self.path);
        }
        if !self.expected.is_empty() {
            let _ = write!(text, "; expected {}", self.expected);
        }
        // `absent` is not reported: the only kind that produces it already says the member was
        // not there, and repeating it turns a one-line finding into a riddle.
        if !matches!(self.found, JsonKind::Unknown | JsonKind::Absent) {
            let _ = write!(text, ", found {}", self.found.as_str());
        }
        text
    }

    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind.as_str(),
            "path": self.path.as_str(),
            "field": self.field.as_str(),
            "expected": self.expected.as_str(),
            "found": self.found.as_str(),
            "line": self.line,
            "column": self.column,
        })
    }
}

/// A cookie name or an intermediary banner, bounded to half a [`BoundedText`].
///
/// Shorter than [`BoundedText`] on purpose: several of these are carried per request record, and
/// no cookie name or `Server`/`Via` banner worth reporting needs more room. Truncation is still
/// marked, so a long value is visibly clipped rather than silently shortened.
pub type ShortToken = InlineAscii<32>;

/// How many cookies of one response are described in full.
///
/// DSM sets at most three (`id`, `smid`, `stay_login`); the fourth slot exists so a load
/// balancer's own affinity cookie -- the single most useful intermediary fingerprint there is --
/// still lands in the record rather than in the overflow count.
pub const MAX_DESCRIBED_COOKIES: usize = 4;

/// Whether a `Set-Cookie` creates a session cookie, a stored one, or clears one.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum CookiePersistence {
    /// Neither `Expires` nor `Max-Age`: the cookie lives only as long as the user agent does.
    #[default]
    Session,
    /// `Expires` or a positive `Max-Age`: the user agent is asked to store it on disk.
    Persistent,
    /// An empty value with `Max-Age=0` or a negative `Max-Age`: the server is clearing it.
    ///
    /// Detected from `Max-Age` alone. An `Expires` date in the past also clears a cookie, but
    /// dating it would mean parsing a server-supplied timestamp, so such a header is reported as
    /// `Persistent` rather than guessed at.
    Deletion,
}

impl CookiePersistence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Persistent => "persistent",
            Self::Deletion => "deletion",
        }
    }
}

/// The `SameSite` attribute, which is a fixed enum rather than server-chosen text.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum CookieSameSite {
    #[default]
    Absent,
    Strict,
    Lax,
    None,
    /// Present but not one of the three defined values.
    Unrecognized,
}

impl CookieSameSite {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Strict => "strict",
            Self::Lax => "lax",
            Self::None => "none",
            Self::Unrecognized => "unrecognized",
        }
    }
}

/// One `Set-Cookie` header, described without its value.
///
/// `fingerprint` is a salted 32-bit digest of the value and exists solely so two observations of
/// the same cookie name can be compared for equality within one run. The salt is drawn once per
/// process, so a digest is not even comparable between runs, and 32 bits of a keyed digest of a
/// 40-plus character opaque identifier carries no recoverable information about it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CookieFact {
    pub name: ShortToken,
    /// Salted digest of the value. Never the value, and never comparable across processes.
    pub fingerprint: u32,
    /// How many bytes the value occupied. A length is not an identifier.
    pub value_length: u16,
    pub persistence: CookiePersistence,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: CookieSameSite,
    /// Attribute *presence*. `Path` and `Domain` values are withheld: a `Domain` names the scope
    /// the intermediary claims, and presence answers the diagnostic question without publishing
    /// the operator's internal naming.
    pub path_present: bool,
    pub domain_present: bool,
    pub expires_present: bool,
    pub max_age_present: bool,
}

impl CookieFact {
    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name.as_str(),
            "fingerprint": format!("{:08x}", self.fingerprint),
            "value_length": self.value_length,
            "persistence": self.persistence.as_str(),
            "secure": self.secure,
            "http_only": self.http_only,
            "same_site": self.same_site.as_str(),
            "path_present": self.path_present,
            "domain_present": self.domain_present,
            "expires_present": self.expires_present,
            "max_age_present": self.max_age_present,
        })
    }
}

/// Every cookie one response set, described but never quoted.
///
/// Fixed capacity keeps [`LogEvent`] `Copy` and heap-free. A response setting more cookies than
/// there are slots reports the excess as a count, so the record never claims to be exhaustive
/// when it is not.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CookieFacts {
    described: [Option<CookieFact>; MAX_DESCRIBED_COOKIES],
    /// `Set-Cookie` headers beyond the described capacity, or ones with no parseable name.
    undescribed: u16,
}

impl CookieFacts {
    /// Add one described cookie, or count it as undescribed when there is no room left.
    pub fn push(&mut self, fact: CookieFact) {
        if let Some(slot) = self.described.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(fact);
        } else {
            self.undescribed = self.undescribed.saturating_add(1);
        }
    }

    /// Count a `Set-Cookie` header that could not be described at all.
    pub fn push_undescribed(&mut self) {
        self.undescribed = self.undescribed.saturating_add(1);
    }

    pub fn described(&self) -> impl Iterator<Item = CookieFact> + '_ {
        self.described.iter().filter_map(|slot| *slot)
    }

    pub fn undescribed(self) -> u16 {
        self.undescribed
    }

    pub fn is_empty(&self) -> bool {
        self.undescribed == 0 && self.described.iter().all(Option::is_none)
    }

    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "described": self
                .described()
                .map(CookieFact::json_value)
                .collect::<Vec<_>>(),
            "undescribed": self.undescribed,
        })
    }
}

/// A content-delivery or caching intermediary recognised from response headers.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum CdnMarker {
    #[default]
    None,
    Cloudflare,
    Fastly,
    Akamai,
    CloudFront,
    Varnish,
    /// A caching or proxy marker that is recognisably one, but not one of the named vendors.
    Other,
}

impl CdnMarker {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cloudflare => "cloudflare",
            Self::Fastly => "fastly",
            Self::Akamai => "akamai",
            Self::CloudFront => "cloudfront",
            Self::Varnish => "varnish",
            Self::Other => "other",
        }
    }
}

/// What one response says about the machinery between this client and DSM.
///
/// These are recorded per response rather than once per run on purpose. A relay that fans
/// consecutive requests out to different DSM backends will change its `Server` banner, its `Via`
/// chain, or its affinity cookie from one request to the next, and only a per-response record can
/// show that.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntermediaryFacts {
    /// The `Via` chain, sanitized. Empty when the header was absent.
    pub via: ShortToken,
    /// The `Server` banner, sanitized. Empty when the header was absent.
    pub server: ShortToken,
    /// The `X-Powered-By` banner, sanitized. Empty when the header was absent.
    pub powered_by: ShortToken,
    /// A response carrying `X-Forwarded-For` or `Forwarded`, which is a request header reflected
    /// back and therefore proof of a proxy that rewrites them.
    pub forwarded_for_reflected: bool,
    /// A response carrying `X-Real-IP`, reflected the same way.
    pub real_ip_reflected: bool,
    pub cdn_marker: CdnMarker,
    /// `Set-Cookie` names this response carried that are not DSM's own.
    pub foreign_cookie_count: u16,
}

impl IntermediaryFacts {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "via": self.via.as_str(),
            "server": self.server.as_str(),
            "powered_by": self.powered_by.as_str(),
            "forwarded_for_reflected": self.forwarded_for_reflected,
            "real_ip_reflected": self.real_ip_reflected,
            "cdn_marker": self.cdn_marker.as_str(),
            "foreign_cookie_count": self.foreign_cookie_count,
        })
    }
}

/// One HTTP round trip against the DSM WebAPI.
///
/// Every member is an enum, an integer, a boolean, a compile-time `&'static str`, or a sanitized
/// [`BoundedText`]. A response body, header value, form-field value, credential, or session
/// identifier is not representable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiCallDetail {
    /// DSM API name. Always a string literal at the call site.
    pub api: &'static str,
    /// DSM method name. Always a string literal at the call site.
    pub method: &'static str,
    pub version: u32,
    /// The URL *path* only. Never a scheme, host, port, query, or fragment.
    pub route: BoundedText,
    pub transport: RequestTransport,
    pub attempt: u32,
    pub max_attempts: u32,
    pub session: SessionTransport,
    /// How many form fields were sent. Never their names or values.
    pub request_fields: u16,
    pub request_bytes: u64,
    pub timeout_ms: u64,
    pub outcome: RequestOutcome,
    pub http_status: Option<u16>,
    pub dsm_code: Option<i64>,
    /// The operator-facing description for `dsm_code`. A compile-time string, never server text.
    pub dsm_description: Option<&'static str>,
    /// How many `Set-Cookie` headers the response carried.
    pub set_cookie_count: u16,
    /// The cookie *names* the response set, comma separated.
    ///
    /// Only the text before each header's first `=` is retained, so the live session value a
    /// `Set-Cookie` carries is not representable here. Empty when the response set none.
    pub set_cookie_names: BoundedText,
    /// The same cookies as [`Self::set_cookie_names`], with their attributes and a value digest.
    ///
    /// This is what makes cookie *permanence* answerable: names alone cannot show that the server
    /// re-set one under a changed value, which is the observation that separates a rotated
    /// session from a rejected one.
    pub cookies: CookieFacts,
    /// What this response revealed about proxies, relays, and caches on the path.
    pub intermediary: IntermediaryFacts,
    pub response_bytes: u64,
    pub elapsed_ms: u64,
    /// The backoff about to be slept, when this attempt scheduled a retry.
    pub retry_backoff_ms: Option<u64>,
    /// The host of a refused redirect's `Location`. Host only; `None` for a relative target.
    pub redirect_host: Option<BoundedText>,
    /// Why the body would not deserialize, when [`Self::outcome`] is [`RequestOutcome::Decode`].
    ///
    /// Present only for that outcome, and holding schema rather than content: without it a decode
    /// failure costs a round trip with the operator to learn which member DSM shaped differently.
    pub decode: Option<DecodeFault>,
}

impl ApiCallDetail {
    /// A record for a request that has not been sent yet.
    pub fn started(
        api: &'static str,
        method: &'static str,
        version: u32,
        route: BoundedText,
        transport: RequestTransport,
    ) -> Self {
        Self {
            api,
            method,
            version,
            route,
            transport,
            attempt: 1,
            max_attempts: 1,
            session: SessionTransport::default(),
            request_fields: 0,
            request_bytes: 0,
            timeout_ms: 0,
            outcome: RequestOutcome::Ok,
            http_status: None,
            dsm_code: None,
            dsm_description: None,
            set_cookie_count: 0,
            set_cookie_names: BoundedText::sanitized(""),
            cookies: CookieFacts::default(),
            intermediary: IntermediaryFacts::default(),
            response_bytes: 0,
            elapsed_ms: 0,
            retry_backoff_ms: None,
            redirect_host: None,
            decode: None,
        }
    }

    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "api": self.api,
            "method": self.method,
            "version": self.version,
            "route": self.route.as_str(),
            "transport": self.transport.as_str(),
            "attempt": self.attempt,
            "max_attempts": self.max_attempts,
            "session": self.session.json_value(),
            "request_fields": self.request_fields,
            "request_bytes": self.request_bytes,
            "timeout_ms": self.timeout_ms,
            "outcome": self.outcome.as_str(),
            "http_status": self.http_status,
            "dsm_code": self.dsm_code,
            "dsm_description": self.dsm_description,
            "set_cookie_count": self.set_cookie_count,
            "set_cookie_names": self.set_cookie_names.as_str(),
            "cookies": self.cookies.json_value(),
            "intermediary": self.intermediary.json_value(),
            "response_bytes": self.response_bytes,
            "elapsed_ms": self.elapsed_ms,
            "retry_backoff_ms": self.retry_backoff_ms,
            "redirect_host": self.redirect_host.map(|host| host.as_str().to_owned()),
            "decode": self.decode.map(DecodeFault::json_value),
        })
    }

    fn human(self) -> String {
        use std::fmt::Write as _;
        let mut line = format!("{}.{} v{}", self.api, self.method, self.version);
        if !self.route.is_empty() {
            let _ = write!(line, " route={}", self.route);
        }
        if self.transport != RequestTransport::Form {
            let _ = write!(line, " transport={}", self.transport.as_str());
        }
        let _ = write!(line, " attempt={}/{}", self.attempt, self.max_attempts);
        let _ = write!(line, " session={}", self.session.describe());
        if let Some(status) = self.http_status {
            let _ = write!(line, " status={status}");
        }
        match self.dsm_code {
            Some(code) => {
                let _ = write!(line, " dsm={code}");
            }
            // Only once a response has actually come back. A request that has not been sent yet
            // has no DSM verdict, and claiming one would be a lie in the common trace line.
            None if self.outcome == RequestOutcome::Ok && self.http_status.is_some() => {
                let _ = write!(line, " dsm=ok");
            }
            None => {}
        }
        if self.outcome.is_failure() {
            let _ = write!(line, " outcome={}", self.outcome.as_str());
        }
        // Whether the server rotated the session on this response, and under which names. This
        // is the discriminator between a session the client may keep reusing and one it has
        // already invalidated by continuing to send the previous identifier.
        if self.set_cookie_count > 0 {
            let _ = write!(line, " set_cookie={}", self.set_cookie_names);
        }
        for cookie in self.cookies.described() {
            // Name, digest, and attribute names only; the value never reaches this formatter.
            let _ = write!(
                line,
                " cookie[{}]={:08x}/{}/{}{}{}",
                cookie.name,
                cookie.fingerprint,
                cookie.persistence.as_str(),
                cookie.same_site.as_str(),
                if cookie.secure { "/secure" } else { "" },
                if cookie.http_only { "/httponly" } else { "" },
            );
        }
        if !self.intermediary.is_empty() {
            let intermediary = self.intermediary;
            let _ = write!(line, " intermediary=");
            let mut parts: Vec<String> = Vec::new();
            if !intermediary.via.is_empty() {
                parts.push(format!("via:{}", intermediary.via));
            }
            if !intermediary.server.is_empty() {
                parts.push(format!("server:{}", intermediary.server));
            }
            if !intermediary.powered_by.is_empty() {
                parts.push(format!("powered_by:{}", intermediary.powered_by));
            }
            if intermediary.forwarded_for_reflected {
                parts.push("forwarded-for-reflected".to_owned());
            }
            if intermediary.real_ip_reflected {
                parts.push("real-ip-reflected".to_owned());
            }
            if intermediary.cdn_marker != CdnMarker::None {
                parts.push(format!("cdn:{}", intermediary.cdn_marker.as_str()));
            }
            if intermediary.foreign_cookie_count > 0 {
                parts.push(format!(
                    "non-dsm-cookies:{}",
                    intermediary.foreign_cookie_count
                ));
            }
            let _ = write!(line, "{}", parts.join("+"));
        }
        if self.response_bytes > 0 {
            let _ = write!(line, " bytes={}", self.response_bytes);
        }
        if self.elapsed_ms > 0 {
            let _ = write!(line, " elapsed_ms={}", self.elapsed_ms);
        }
        if let Some(backoff) = self.retry_backoff_ms {
            let _ = write!(line, " retry_backoff_ms={backoff}");
        }
        if let Some(host) = self.redirect_host {
            let _ = write!(line, " redirect_host={host}");
        }
        if let Some(decode) = self.decode {
            let _ = write!(line, " decode={}", decode.describe());
        }
        if let Some(description) = self.dsm_description {
            let _ = write!(line, " detail=\"{description}\"");
        }
        line
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UrlScheme {
    Https,
    Http,
}

impl UrlScheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificateVerification {
    Enabled,
    CustomCa,
    Disabled,
}

impl CertificateVerification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::CustomCa => "custom-ca",
            Self::Disabled => "disabled",
        }
    }
}

/// The transport identity of a connected client.
///
/// This is the only record that names the endpoint, and it is deliberately emitted at debug level:
/// a default-level run that ships events to a remote collector must not start disclosing a
/// hostname it did not disclose before.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionDetail {
    pub scheme: UrlScheme,
    /// Host only, never userinfo, path, query, or fragment.
    pub host: BoundedText,
    pub port: Option<u16>,
    pub base_path: BoundedText,
    pub certificate_verification: CertificateVerification,
}

impl ConnectionDetail {
    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "scheme": self.scheme.as_str(),
            "host": self.host.as_str(),
            "port": self.port,
            "base_path": self.base_path.as_str(),
            // Pinned into the record so relaxing the client's redirect policy fails the
            // shipped-schema conformance test rather than passing unnoticed.
            "redirects": "refused",
            "certificate_verification": self.certificate_verification.as_str(),
        })
    }

    fn human(self) -> String {
        use std::fmt::Write as _;
        let mut line = format!("scheme={} host={}", self.scheme.as_str(), self.host);
        if let Some(port) = self.port {
            let _ = write!(line, " port={port}");
        }
        let _ = write!(
            line,
            " base_path={} redirects=refused certificate_verification={}",
            self.base_path,
            self.certificate_verification.as_str()
        );
        line
    }
}

/// The `format=` value requested from `SYNO.API.Auth.login`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginFormat {
    Sid,
    Cookie,
}

impl LoginFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sid => "sid",
            Self::Cookie => "cookie",
        }
    }
}

/// The shape of an established DSM session.
///
/// Lengths and presence only. A session identifier, a token, and any digest or fingerprint of
/// either are all deliberately absent: a truncated hash of a live SID would be a credential
/// correlation primitive shipped to a remote collector, and it answers nothing that `sid_length`
/// and [`SessionTransport`] do not already answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionShape {
    pub sid_length: u16,
    pub token_present: bool,
    pub login_format: LoginFormat,
    /// Whether the login response carried `Set-Cookie`.
    pub server_set_cookie: bool,
}

impl SessionShape {
    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "sid_length": self.sid_length,
            "token_present": self.token_present,
            "login_format": self.login_format.as_str(),
            "server_set_cookie": self.server_set_cookie,
        })
    }

    fn human(self) -> String {
        format!(
            "sid_length={} token={} login_format={} server_set_cookie={}",
            self.sid_length,
            if self.token_present {
                "present"
            } else {
                "absent"
            },
            self.login_format.as_str(),
            if self.server_set_cookie {
                "present"
            } else {
                "absent"
            },
        )
    }
}

/// Optional aggregate counters attached to a log record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventMetrics {
    pub operations: u64,
    pub files: u64,
    pub bytes: u64,
    pub elapsed_ms: u64,
    pub throughput_bytes_per_second: u64,
    pub eta_ms: Option<u64>,
}

/// A log event with no free-form or secret-bearing fields.
///
/// The optional detail members are absent on every event that does not carry them, and are then
/// omitted from the JSON object entirely, so records for the original event codes render exactly
/// as they always have and existing `sdsync.log.v1` consumers keep working.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogEvent {
    pub timestamp_ms: u64,
    pub level: LogLevel,
    pub code: EventCode,
    pub operation_id: Option<u64>,
    pub attempt: Option<u32>,
    pub metrics: EventMetrics,
    pub build: Option<BuildIdentity>,
    pub call: Option<ApiCallDetail>,
    pub connection: Option<ConnectionDetail>,
    pub session: Option<SessionShape>,
}

impl LogEvent {
    pub fn new(level: LogLevel, code: EventCode) -> Self {
        Self {
            timestamp_ms: unix_timestamp_ms(),
            level,
            code,
            operation_id: None,
            attempt: None,
            metrics: EventMetrics::default(),
            build: None,
            call: None,
            connection: None,
            session: None,
        }
    }

    pub fn operation(mut self, operation_id: u64) -> Self {
        self.operation_id = Some(operation_id);
        self
    }

    pub fn attempt(mut self, attempt: u32) -> Self {
        self.attempt = Some(attempt);
        self
    }

    pub fn metrics(mut self, metrics: EventMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    pub fn build(mut self, build: BuildIdentity) -> Self {
        self.build = Some(build);
        self
    }

    pub fn call(mut self, call: ApiCallDetail) -> Self {
        self.call = Some(call);
        self
    }

    pub fn connection(mut self, connection: ConnectionDetail) -> Self {
        self.connection = Some(connection);
        self
    }

    pub fn session(mut self, session: SessionShape) -> Self {
        self.session = Some(session);
        self
    }

    fn json_value(self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "schema": "sdsync.log.v1",
            "timestamp_ms": self.timestamp_ms,
            "level": self.level.as_str(),
            "event": self.code.as_str(),
            "operation_id": self.operation_id,
            "attempt": self.attempt,
            "metrics": {
                "operations": self.metrics.operations,
                "files": self.metrics.files,
                "bytes": self.metrics.bytes,
                "elapsed_ms": self.metrics.elapsed_ms,
                "throughput_bytes_per_second": self.metrics.throughput_bytes_per_second,
                "eta_ms": self.metrics.eta_ms,
            }
        });
        let object = value
            .as_object_mut()
            .expect("the record is constructed as a JSON object");
        if let Some(build) = self.build {
            object.insert("build".to_owned(), build.json_value());
        }
        if let Some(call) = self.call {
            object.insert("call".to_owned(), call.json_value());
        }
        if let Some(connection) = self.connection {
            object.insert("connection".to_owned(), connection.json_value());
        }
        if let Some(session) = self.session {
            object.insert("session".to_owned(), session.json_value());
        }
        value
    }

    fn human_line(self) -> String {
        let mut line = format!(
            "{} {:<5} {}",
            self.timestamp_ms,
            self.level.as_str().to_ascii_uppercase(),
            self.code.human()
        );
        if let Some(operation_id) = self.operation_id {
            use std::fmt::Write as _;
            let _ = write!(line, " operation_id={operation_id}");
        }
        if let Some(attempt) = self.attempt {
            use std::fmt::Write as _;
            let _ = write!(line, " attempt={attempt}");
        }
        if self.metrics.operations > 0 {
            use std::fmt::Write as _;
            let _ = write!(line, " operations={}", self.metrics.operations);
        }
        if self.metrics.files > 0 {
            use std::fmt::Write as _;
            let _ = write!(line, " files={}", self.metrics.files);
        }
        if self.metrics.bytes > 0 {
            use std::fmt::Write as _;
            let _ = write!(line, " bytes={}", self.metrics.bytes);
        }
        if self.metrics.elapsed_ms > 0 {
            use std::fmt::Write as _;
            let _ = write!(line, " elapsed_ms={}", self.metrics.elapsed_ms);
        }
        if let Some(build) = self.build {
            use std::fmt::Write as _;
            let _ = write!(line, " {}", build.human());
        }
        if let Some(connection) = self.connection {
            use std::fmt::Write as _;
            let _ = write!(line, " {}", connection.human());
        }
        if let Some(session) = self.session {
            use std::fmt::Write as _;
            let _ = write!(line, " {}", session.human());
        }
        if let Some(call) = self.call {
            use std::fmt::Write as _;
            let _ = write!(line, " {}", call.human());
        }
        line
    }
}

#[derive(Clone, Debug)]
pub struct FileLogConfig {
    pub path: PathBuf,
    pub format: LogFormat,
    pub max_bytes: u64,
    pub backups: usize,
}

/// No raw-token variant is provided intentionally.
#[derive(Clone, Debug)]
pub enum BearerTokenSource {
    Environment(String),
    File(PathBuf),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RemoteDelivery {
    #[default]
    BestEffort,
    Required,
}

#[derive(Clone)]
pub struct RemoteLogConfig {
    pub endpoint: String,
    pub bearer_token: Option<BearerTokenSource>,
    pub queue_capacity: usize,
    pub timeout: Duration,
    pub delivery: RemoteDelivery,
}

pub struct LoggerConfig {
    pub level: LogLevel,
    /// `None` disables the stderr log sink.
    pub stderr: Option<LogFormat>,
    pub file: Option<FileLogConfig>,
    pub remote: Option<RemoteLogConfig>,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            stderr: Some(LogFormat::Human),
            file: None,
            remote: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub remote_events_dropped: u64,
    pub remote_delivery_failures: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteFailure {
    Transport,
    Rejected,
    WorkerStopped,
}

impl fmt::Display for RemoteFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Transport => "remote log transport failed",
            Self::Rejected => "remote log endpoint rejected an event",
            Self::WorkerStopped => "remote log worker stopped",
        })
    }
}

#[derive(Debug)]
pub enum ObservabilityError {
    InvalidLogLevel,
    InvalidFileConfiguration,
    InvalidRemoteEndpoint,
    InvalidRemoteConfiguration,
    InvalidBearerToken,
    RemoteQueueFull,
    RemoteFailure(RemoteFailure),
    FlushTimeout,
    AlreadyShutdown,
    Io {
        operation: &'static str,
        source: io::Error,
    },
    HttpClient(reqwest::Error),
}

impl fmt::Display for ObservabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLogLevel => formatter.write_str("invalid log level"),
            Self::InvalidFileConfiguration => formatter.write_str("invalid file log configuration"),
            Self::InvalidRemoteEndpoint => formatter.write_str(
                "remote log endpoint must be an HTTPS URL without credentials, query, or fragment",
            ),
            Self::InvalidRemoteConfiguration => {
                formatter.write_str("invalid remote log configuration")
            }
            Self::InvalidBearerToken => formatter.write_str("invalid remote log bearer token"),
            Self::RemoteQueueFull => formatter.write_str("remote log queue is full"),
            Self::RemoteFailure(error) => error.fmt(formatter),
            Self::FlushTimeout => formatter.write_str("timed out flushing observability output"),
            Self::AlreadyShutdown => formatter.write_str("observability logger is shut down"),
            Self::Io { operation, .. } => {
                write!(formatter, "observability I/O failed during {operation}")
            }
            Self::HttpClient(_) => formatter.write_str("failed to initialize remote log transport"),
        }
    }
}

impl std::error::Error for ObservabilityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::HttpClient(source) => Some(source),
            _ => None,
        }
    }
}

pub type ObservabilityResult<T> = std::result::Result<T, ObservabilityError>;

/// A synchronous local logger with an optional bounded asynchronous HTTPS sink.
/// Share it across workers with `Arc<EventLogger>`.
pub struct EventLogger {
    level: LogLevel,
    stderr: Option<(LogFormat, Mutex<Box<dyn Write + Send>>)>,
    file: Option<(LogFormat, Mutex<RotatingFile>)>,
    remote: Option<RemoteSink>,
}

impl EventLogger {
    pub fn new(config: LoggerConfig) -> ObservabilityResult<Self> {
        Self::with_stderr_writer(config, io::stderr())
    }

    pub fn with_stderr_writer<W>(config: LoggerConfig, writer: W) -> ObservabilityResult<Self>
    where
        W: Write + Send + 'static,
    {
        let file = config
            .file
            .map(|file| {
                let format = file.format;
                RotatingFile::open(file).map(|sink| (format, Mutex::new(sink)))
            })
            .transpose()?;
        let remote = config.remote.map(RemoteSink::from_config).transpose()?;
        Ok(Self {
            level: config.level,
            stderr: config.stderr.map(|format| {
                (
                    format,
                    Mutex::new(Box::new(writer) as Box<dyn Write + Send>),
                )
            }),
            file,
            remote,
        })
    }

    pub fn emit(&self, event: LogEvent) -> ObservabilityResult<()> {
        if !self.level.permits(event.level) {
            return Ok(());
        }
        if let Some((format, sink)) = &self.stderr {
            write_event(&mut *lock(sink)?, *format, event, "stderr log write")?;
        }
        if let Some((format, sink)) = &self.file {
            let line = format_event(*format, event);
            lock(sink)?.write_line(&line)?;
        }
        if let Some(remote) = &self.remote {
            remote.enqueue(event.json_value().to_string())?;
        }
        Ok(())
    }

    /// Flush local writers and wait until every earlier remote event has been attempted.
    pub fn flush(&self, wait: Duration) -> ObservabilityResult<()> {
        self.flush_local()?;
        if let Some(remote) = &self.remote {
            remote.flush(wait)?;
        }
        Ok(())
    }

    fn flush_local(&self) -> ObservabilityResult<()> {
        if let Some((_, sink)) = &self.stderr {
            lock(sink)?
                .flush()
                .map_err(|source| ObservabilityError::Io {
                    operation: "stderr log flush",
                    source,
                })?;
        }
        if let Some((_, sink)) = &self.file {
            lock(sink)?.flush()?;
        }
        Ok(())
    }

    /// Flush, stop, and join the remote worker. In required mode, any rejected or failed
    /// delivery is returned. Best-effort mode reports failures without failing shutdown.
    pub fn shutdown(&self, wait: Duration) -> ObservabilityResult<ShutdownReport> {
        // A Shutdown command is itself a FIFO barrier for every earlier remote event. Flush only
        // local writers here so the remote side gets one overall caller-supplied deadline.
        let flush_result = self.flush_local();
        let shutdown_result = match &self.remote {
            Some(remote) => remote.shutdown(wait),
            None => Ok(()),
        };
        let report = self
            .remote
            .as_ref()
            .map(RemoteSink::report)
            .unwrap_or_default();
        flush_result.and(shutdown_result).map(|()| report)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> ObservabilityResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| ObservabilityError::Io {
        operation: "log sink lock",
        source: io::Error::other("log sink lock was poisoned"),
    })
}

fn write_event(
    writer: &mut dyn Write,
    format: LogFormat,
    event: LogEvent,
    operation: &'static str,
) -> ObservabilityResult<()> {
    writeln!(writer, "{}", format_event(format, event))
        .map_err(|source| ObservabilityError::Io { operation, source })
}

fn format_event(format: LogFormat, event: LogEvent) -> String {
    match format {
        LogFormat::Human => event.human_line(),
        LogFormat::Json => event.json_value().to_string(),
    }
}

struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    length: u64,
    max_bytes: u64,
    backups: usize,
}

impl RotatingFile {
    fn open(config: FileLogConfig) -> ObservabilityResult<Self> {
        if config.path.as_os_str().is_empty()
            || config.max_bytes == 0
            || config.backups > MAX_FILE_BACKUPS
        {
            return Err(ObservabilityError::InvalidFileConfiguration);
        }
        let file = open_private_append(&config.path)?;
        let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Ok(Self {
            path: config.path,
            file: Some(file),
            length,
            max_bytes: config.max_bytes,
            backups: config.backups,
        })
    }

    fn write_line(&mut self, line: &str) -> ObservabilityResult<()> {
        let added = u64::try_from(line.len().saturating_add(1)).unwrap_or(u64::MAX);
        if self.length > 0 && self.length.saturating_add(added) > self.max_bytes {
            self.rotate()?;
        }
        let file = self.file.as_mut().ok_or_else(|| ObservabilityError::Io {
            operation: "file log write",
            source: io::Error::other("file log is closed"),
        })?;
        writeln!(file, "{line}").map_err(|source| ObservabilityError::Io {
            operation: "file log write",
            source,
        })?;
        self.length = self.length.saturating_add(added);
        Ok(())
    }

    fn flush(&mut self) -> ObservabilityResult<()> {
        self.file
            .as_mut()
            .ok_or_else(|| ObservabilityError::Io {
                operation: "file log flush",
                source: io::Error::other("file log is closed"),
            })?
            .flush()
            .map_err(|source| ObservabilityError::Io {
                operation: "file log flush",
                source,
            })
    }

    fn rotate(&mut self) -> ObservabilityResult<()> {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }
        if self.backups == 0 {
            let file = open_private_truncate(&self.path)?;
            self.file = Some(file);
            self.length = 0;
            return Ok(());
        }
        let oldest = rotated_path(&self.path, self.backups);
        if oldest.exists() {
            fs::remove_file(&oldest).map_err(|source| ObservabilityError::Io {
                operation: "old file log removal",
                source,
            })?;
        }
        for index in (1..self.backups).rev() {
            let source_path = rotated_path(&self.path, index);
            if source_path.exists() {
                fs::rename(&source_path, rotated_path(&self.path, index + 1)).map_err(
                    |source| ObservabilityError::Io {
                        operation: "file log rotation",
                        source,
                    },
                )?;
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, rotated_path(&self.path, 1)).map_err(|source| {
                ObservabilityError::Io {
                    operation: "file log rotation",
                    source,
                }
            })?;
        }
        self.file = Some(open_private_truncate(&self.path)?);
        self.length = 0;
        Ok(())
    }
}

fn open_private_append(path: &Path) -> ObservabilityResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    set_private_mode(&mut options);
    options.open(path).map_err(|source| ObservabilityError::Io {
        operation: "file log open",
        source,
    })
}

fn open_private_truncate(path: &Path) -> ObservabilityResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    set_private_mode(&mut options);
    options.open(path).map_err(|source| ObservabilityError::Io {
        operation: "file log open",
        source,
    })
}

#[cfg(unix)]
fn set_private_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_mode(_options: &mut OpenOptions) {}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

trait RemoteTransport: Send + 'static {
    fn send(&mut self, body: &str) -> std::result::Result<(), RemoteFailure>;
}

struct HttpRemoteTransport {
    client: Client,
    endpoint: Url,
    bearer_token: Option<Zeroizing<String>>,
}

impl RemoteTransport for HttpRemoteTransport {
    fn send(&mut self, body: &str) -> std::result::Result<(), RemoteFailure> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_owned());
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token.as_str());
        }
        let response = request.send().map_err(|_| RemoteFailure::Transport)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(RemoteFailure::Rejected)
        }
    }
}

enum RemoteCommand {
    Event(String),
    Flush(mpsc::Sender<std::result::Result<(), RemoteFailure>>),
    Shutdown(mpsc::Sender<std::result::Result<(), RemoteFailure>>),
}

#[derive(Default)]
struct RemoteStatus {
    first_failure: Mutex<Option<RemoteFailure>>,
    dropped: AtomicU64,
    failures: AtomicU64,
}

struct RemoteSink {
    sender: Mutex<Option<SyncSender<RemoteCommand>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    status: std::sync::Arc<RemoteStatus>,
    delivery: RemoteDelivery,
    closed: AtomicBool,
}

impl RemoteSink {
    fn from_config(config: RemoteLogConfig) -> ObservabilityResult<Self> {
        if config.queue_capacity == 0
            || config.queue_capacity > MAX_REMOTE_QUEUE
            || config.timeout.is_zero()
        {
            return Err(ObservabilityError::InvalidRemoteConfiguration);
        }
        let endpoint = validate_remote_endpoint(&config.endpoint)?;
        let bearer_token = config
            .bearer_token
            .as_ref()
            .map(load_bearer_token)
            .transpose()?;
        let client = crate::blocking_client_builder()
            .map_err(|_| ObservabilityError::InvalidRemoteConfiguration)?
            .connect_timeout(config.timeout)
            .timeout(config.timeout)
            // Never forward an observability payload or bearer credential to a redirect
            // target. Operators must configure the final HTTPS endpoint explicitly.
            .redirect(Policy::none())
            .build()
            .map_err(ObservabilityError::HttpClient)?;
        let transport = HttpRemoteTransport {
            client,
            endpoint,
            bearer_token,
        };
        Self::spawn(Box::new(transport), config.queue_capacity, config.delivery)
    }

    fn spawn(
        transport: Box<dyn RemoteTransport>,
        capacity: usize,
        delivery: RemoteDelivery,
    ) -> ObservabilityResult<Self> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let status = std::sync::Arc::new(RemoteStatus::default());
        let worker_status = std::sync::Arc::clone(&status);
        let join = thread::Builder::new()
            .name("sdsync-log-delivery".to_owned())
            .spawn(move || remote_worker(receiver, transport, worker_status))
            .map_err(|source| ObservabilityError::Io {
                operation: "remote log worker start",
                source,
            })?;
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            join: Mutex::new(Some(join)),
            status,
            delivery,
            closed: AtomicBool::new(false),
        })
    }

    fn enqueue(&self, body: String) -> ObservabilityResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return self.delivery_result(Err(RemoteFailure::WorkerStopped));
        }
        if self.delivery == RemoteDelivery::Required
            && let Some(failure) = self.first_failure()
        {
            return Err(ObservabilityError::RemoteFailure(failure));
        }
        let sender = self.sender()?;
        match sender.try_send(RemoteCommand::Event(body)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.status.dropped.fetch_add(1, Ordering::Relaxed);
                if self.delivery == RemoteDelivery::Required {
                    Err(ObservabilityError::RemoteQueueFull)
                } else {
                    Ok(())
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.delivery_result(Err(RemoteFailure::WorkerStopped))
            }
        }
    }

    fn flush(&self, wait: Duration) -> ObservabilityResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ObservabilityError::AlreadyShutdown);
        }
        self.control(wait, false)
    }

    fn shutdown(&self, wait: Duration) -> ObservabilityResult<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let started = Instant::now();
        let result = self.control(wait, true);
        lock(&self.sender)?.take();
        let remaining = wait.saturating_sub(started.elapsed());
        let join_result = self.finish_worker_within(remaining);
        result.and(join_result)
    }

    fn control(&self, wait: Duration, shutdown: bool) -> ObservabilityResult<()> {
        let started = Instant::now();
        let (acknowledge, result) = mpsc::channel();
        let mut command = Some(if shutdown {
            RemoteCommand::Shutdown(acknowledge)
        } else {
            RemoteCommand::Flush(acknowledge)
        });
        loop {
            let sender = self.sender()?;
            match sender.try_send(command.take().expect("control command is present")) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    command = Some(returned);
                    if started.elapsed() >= wait {
                        return Err(ObservabilityError::FlushTimeout);
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return self.delivery_result(Err(RemoteFailure::WorkerStopped));
                }
            }
        }
        let remaining = wait.saturating_sub(started.elapsed());
        let delivered = result
            .recv_timeout(remaining)
            .map_err(|_| ObservabilityError::FlushTimeout)?;
        self.delivery_result(delivered)
    }

    fn sender(&self) -> ObservabilityResult<SyncSender<RemoteCommand>> {
        lock(&self.sender)?
            .as_ref()
            .cloned()
            .ok_or(ObservabilityError::AlreadyShutdown)
    }

    fn first_failure(&self) -> Option<RemoteFailure> {
        self.status
            .first_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .copied()
    }

    fn delivery_result(
        &self,
        result: std::result::Result<(), RemoteFailure>,
    ) -> ObservabilityResult<()> {
        match (self.delivery, result) {
            (_, Ok(())) | (RemoteDelivery::BestEffort, Err(_)) => Ok(()),
            (RemoteDelivery::Required, Err(error)) => Err(ObservabilityError::RemoteFailure(error)),
        }
    }

    fn report(&self) -> ShutdownReport {
        ShutdownReport {
            remote_events_dropped: self.status.dropped.load(Ordering::Relaxed),
            remote_delivery_failures: self.status.failures.load(Ordering::Relaxed),
        }
    }

    /// Join a completed worker, waiting at most `wait`. Taking and dropping an unfinished
    /// handle detaches it; this preserves the shutdown deadline even if a transport is stuck.
    fn finish_worker_within(&self, wait: Duration) -> ObservabilityResult<()> {
        let started = Instant::now();
        let join = lock(&self.join)?.take();
        let Some(join) = join else {
            return Ok(());
        };
        while !join.is_finished() {
            if started.elapsed() >= wait {
                drop(join);
                return Err(ObservabilityError::FlushTimeout);
            }
            thread::sleep(Duration::from_millis(1));
        }
        join.join()
            .map_err(|_| ObservabilityError::RemoteFailure(RemoteFailure::WorkerStopped))
    }
}

impl Drop for RemoteSink {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let sender = self
            .sender
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(sender);
        if let Some(join) = self
            .join
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            && join.is_finished()
        {
            let _ = join.join();
        }
        // Dropping an unfinished JoinHandle detaches it. Normal destruction must not turn a
        // best-effort network sink into an unbounded application shutdown delay.
    }
}

fn remote_worker(
    receiver: Receiver<RemoteCommand>,
    mut transport: Box<dyn RemoteTransport>,
    status: std::sync::Arc<RemoteStatus>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            RemoteCommand::Event(body) => {
                if let Err(failure) = transport.send(&body) {
                    status.failures.fetch_add(1, Ordering::Relaxed);
                    let mut first = status
                        .first_failure
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if first.is_none() {
                        *first = Some(failure);
                    }
                }
            }
            RemoteCommand::Flush(acknowledge) => {
                let result = status
                    .first_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .map_or(Ok(()), Err);
                let _ = acknowledge.send(result);
            }
            RemoteCommand::Shutdown(acknowledge) => {
                let result = status
                    .first_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .map_or(Ok(()), Err);
                let _ = acknowledge.send(result);
                break;
            }
        }
    }
}

fn validate_remote_endpoint(input: &str) -> ObservabilityResult<Url> {
    let url = Url::parse(input).map_err(|_| ObservabilityError::InvalidRemoteEndpoint)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ObservabilityError::InvalidRemoteEndpoint);
    }
    Ok(url)
}

fn load_bearer_token(source: &BearerTokenSource) -> ObservabilityResult<Zeroizing<String>> {
    let token = match source {
        BearerTokenSource::Environment(name) => {
            if name.is_empty() {
                return Err(ObservabilityError::InvalidBearerToken);
            }
            env::var(name)
                .map(Zeroizing::new)
                .map_err(|_| ObservabilityError::InvalidBearerToken)?
        }
        BearerTokenSource::File(path) => {
            let mut file = File::open(path).map_err(|source| ObservabilityError::Io {
                operation: "bearer token file open",
                source,
            })?;
            let mut bytes = Zeroizing::new(Vec::new());
            Read::by_ref(&mut file)
                .take(MAX_BEARER_TOKEN_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|source| ObservabilityError::Io {
                    operation: "bearer token file read",
                    source,
                })?;
            if bytes.len() as u64 > MAX_BEARER_TOKEN_BYTES {
                return Err(ObservabilityError::InvalidBearerToken);
            }
            let decoded = String::from_utf8(std::mem::take(&mut *bytes)).map_err(|error| {
                let mut bytes = error.into_bytes();
                bytes.zeroize();
                ObservabilityError::InvalidBearerToken
            })?;
            Zeroizing::new(decoded)
        }
    };
    normalize_bearer_token(token)
}

fn normalize_bearer_token(mut token: Zeroizing<String>) -> ObservabilityResult<Zeroizing<String>> {
    while token.ends_with(['\r', '\n']) {
        token.pop();
    }
    if token.is_empty()
        || token.len() as u64 > MAX_BEARER_TOKEN_BYTES
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"')
    {
        return Err(ObservabilityError::InvalidBearerToken);
    }
    Ok(token)
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn explicit_log_level_has_strict_precedence() {
        assert_eq!(
            resolve_log_level(Some("debug"), Some("not-a-level"), Verbosity::Quiet).unwrap(),
            LogLevel::Debug
        );
        assert_eq!(
            resolve_log_level(None, Some("trace"), Verbosity::Quiet).unwrap(),
            LogLevel::Trace
        );
        assert_eq!(
            resolve_log_level(None, None, Verbosity::Verbose).unwrap(),
            LogLevel::Debug
        );
    }

    #[test]
    fn structured_events_have_no_free_form_secret_field() {
        let marker = "LEAK-ME-NOT";
        let event = LogEvent::new(LogLevel::Info, EventCode::UploadCompleted)
            .operation(7)
            .metrics(EventMetrics {
                files: 1,
                bytes: 42,
                ..EventMetrics::default()
            });
        let json = event.json_value().to_string();
        let human = event.human_line();
        assert!(!json.contains(marker));
        assert!(!human.contains(marker));
        assert!(!json.contains("token"));
        assert!(!json.contains("password"));
        assert!(!json.contains("url"));
    }

    /// A populated record of every kind, for the leak and shape guards below.
    fn fully_populated_details() -> (BuildIdentity, ApiCallDetail, ConnectionDetail, SessionShape) {
        let mut call = ApiCallDetail::started(
            "SYNO.FileStation.List",
            "getinfo",
            2,
            BoundedText::sanitized("/webapi/entry.cgi"),
            RequestTransport::Form,
        );
        call.attempt = 2;
        call.max_attempts = 4;
        call.session = SessionTransport {
            cookie_header: true,
            syno_token_header: true,
            sid_field: true,
            syno_token_field: true,
        };
        call.request_fields = 9;
        call.request_bytes = 4096;
        call.timeout_ms = 10_000;
        call.outcome = RequestOutcome::DsmError;
        call.http_status = Some(200);
        call.dsm_code = Some(119);
        call.dsm_description = Some("session is invalid; rerun to authenticate again");
        call.set_cookie_count = 2;
        call.set_cookie_names = BoundedText::sanitized("id,stay_login");
        call.cookies.push(CookieFact {
            name: ShortToken::sanitized("id"),
            fingerprint: 0xdead_beef,
            value_length: 43,
            persistence: CookiePersistence::Session,
            secure: true,
            http_only: true,
            same_site: CookieSameSite::Lax,
            path_present: true,
            domain_present: false,
            expires_present: false,
            max_age_present: false,
        });
        call.cookies.push(CookieFact {
            name: ShortToken::sanitized("stay_login"),
            fingerprint: 0x0000_0001,
            value_length: 1,
            persistence: CookiePersistence::Persistent,
            secure: false,
            http_only: false,
            same_site: CookieSameSite::Absent,
            path_present: true,
            domain_present: true,
            expires_present: true,
            max_age_present: false,
        });
        call.cookies.push_undescribed();
        call.intermediary = IntermediaryFacts {
            via: ShortToken::sanitized("1.1 relay"),
            server: ShortToken::sanitized("nginx"),
            powered_by: ShortToken::sanitized("PHP/8.2"),
            forwarded_for_reflected: true,
            real_ip_reflected: true,
            cdn_marker: CdnMarker::Cloudflare,
            foreign_cookie_count: 1,
        };
        call.response_bytes = 88;
        call.elapsed_ms = 79;
        call.retry_backoff_ms = Some(500);
        call.redirect_host = Some(BoundedText::sanitized("relay.example.test"));
        call.decode = Some(DecodeFault {
            kind: DecodeFaultKind::TypeMismatch,
            path: BoundedText::sanitized("data.files.0.additional.time.mtime"),
            field: ShortToken::sanitized("mtime"),
            expected: ShortToken::sanitized("i64"),
            found: JsonKind::String,
            line: 1,
            column: 212,
        });
        (
            BUILD,
            call,
            ConnectionDetail {
                scheme: UrlScheme::Https,
                host: BoundedText::sanitized("nas.example.test"),
                port: Some(5001),
                base_path: BoundedText::sanitized("/"),
                certificate_verification: CertificateVerification::CustomCa,
            },
            SessionShape {
                sid_length: 24,
                token_present: true,
                login_format: LoginFormat::Sid,
                server_set_cookie: false,
            },
        )
    }

    /// The closed key set is the actual redaction guarantee, so it is asserted verbatim.
    ///
    /// A new member added to any detail type fails here, which forces a deliberate decision about
    /// whether it can carry secret-bearing data before it can ever be emitted.
    #[test]
    fn every_log_record_key_is_in_the_documented_closed_set() {
        // Arrays are walked as well as objects. A record member that holds a list of objects --
        // the described cookies do -- would otherwise put its keys outside the closed set
        // entirely, which is exactly the gap this test exists to close.
        fn collect_keys(value: &serde_json::Value, into: &mut std::collections::BTreeSet<String>) {
            match value {
                serde_json::Value::Object(object) => {
                    for (key, nested) in object {
                        into.insert(key.clone());
                        collect_keys(nested, into);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        collect_keys(item, into);
                    }
                }
                _ => {}
            }
        }

        let (build, call, connection, session) = fully_populated_details();
        let mut keys = std::collections::BTreeSet::new();
        for code in EventCode::ALL {
            let event = LogEvent::new(LogLevel::Debug, code)
                .operation(1)
                .attempt(2)
                .metrics(EventMetrics::default())
                .build(build)
                .call(call)
                .connection(connection)
                .session(session);
            collect_keys(&event.json_value(), &mut keys);
        }

        let expected: std::collections::BTreeSet<String> = [
            // Envelope.
            "schema",
            "timestamp_ms",
            "level",
            "event",
            "operation_id",
            "attempt",
            "metrics",
            "build",
            "call",
            "connection",
            "session",
            // Metrics.
            "operations",
            "files",
            "bytes",
            "elapsed_ms",
            "throughput_bytes_per_second",
            "eta_ms",
            // Build identity.
            "name",
            "version",
            "target",
            "profile",
            "commit",
            // API call.
            "api",
            "method",
            "route",
            "transport",
            "max_attempts",
            "request_fields",
            "request_bytes",
            "timeout_ms",
            "outcome",
            "http_status",
            "dsm_code",
            "dsm_description",
            "set_cookie_count",
            "set_cookie_names",
            "response_bytes",
            "retry_backoff_ms",
            "redirect_host",
            // Decode fault. A classification, a dotted member path built from keys and indices,
            // the member name, the deserializer's own expectation, and a JSON type. No value.
            "decode",
            "kind",
            "path",
            "field",
            "expected",
            "found",
            "line",
            "column",
            // Described cookies. Names, a salted digest, and attribute presence; no value.
            "cookies",
            "described",
            "undescribed",
            "fingerprint",
            "value_length",
            "persistence",
            "secure",
            "http_only",
            "same_site",
            "path_present",
            "domain_present",
            "expires_present",
            "max_age_present",
            // Intermediary fingerprint.
            "intermediary",
            "via",
            "server",
            "powered_by",
            "forwarded_for_reflected",
            "real_ip_reflected",
            "cdn_marker",
            "foreign_cookie_count",
            // Session transport.
            "cookie_header",
            "syno_token_header",
            "sid_field",
            "syno_token_field",
            // Connection.
            "scheme",
            "host",
            "port",
            "base_path",
            "redirects",
            "certificate_verification",
            // Session shape.
            "sid_length",
            "token_present",
            "login_format",
            "server_set_cookie",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        assert_eq!(
            keys, expected,
            "the log record key set changed; confirm no new member can carry secret-bearing data"
        );
    }

    /// No populated record of any kind may render credential material.
    #[test]
    fn populated_records_render_no_credential_material() {
        let (build, call, connection, session) = fully_populated_details();
        for code in EventCode::ALL {
            let event = LogEvent::new(LogLevel::Trace, code)
                .build(build)
                .call(call)
                .connection(connection)
                .session(session);
            for rendered in [event.json_value().to_string(), event.human_line()] {
                for forbidden in [
                    "passwd",
                    "otp_code",
                    "_sid",
                    "SynoToken",
                    "account",
                    "Cookie:",
                    "X-SYNO-TOKEN",
                    "password",
                ] {
                    assert!(
                        !rendered.contains(forbidden),
                        "{code:?} rendered {forbidden:?}: {rendered}"
                    );
                }
            }
        }
    }

    /// The sanitizer is the only way runtime text enters a record, so its bounds are pinned here.
    #[test]
    fn inline_ascii_sanitizes_replaces_and_bounds() {
        // Everything outside the retained set becomes `_`, so a substitution stays visible
        // instead of silently closing a gap.
        assert_eq!(
            InlineAscii::<64>::sanitized("/webapi/entry.cgi?_sid=abc#frag").as_str(),
            "/webapi/entry.cgi__sid_abc_frag"
        );
        assert_eq!(
            InlineAscii::<64>::sanitized("nas.example.test:5001").as_str(),
            "nas.example.test:5001"
        );
        // Control characters cannot forge a second log line.
        assert_eq!(
            InlineAscii::<32>::sanitized("a\r\nINFO fake").as_str(),
            "a__INFO_fake"
        );
        // Non-ASCII is replaced byte by byte and never yields invalid UTF-8.
        assert_eq!(InlineAscii::<16>::sanitized("héllo").as_str(), "h__llo");
        assert!(InlineAscii::<16>::sanitized("").is_empty());
        // Over-length input is truncated and explicitly marked.
        let long = InlineAscii::<8>::sanitized("abcdefghijklmnop");
        assert_eq!(long.as_str(), "abcdefg~");
        assert_eq!(long.as_str().len(), 8);
    }

    /// Absent details must be omitted, not rendered as `null`.
    ///
    /// This is what keeps records for the original event codes byte-identical to the shape
    /// `sdsync.log.v1` consumers already parse.
    #[test]
    fn events_without_details_render_exactly_as_before() {
        let event = LogEvent::new(LogLevel::Info, EventCode::RunStarted);
        let json = event.json_value();
        let object = json.as_object().expect("a JSON object");
        // serde_json orders object keys, so compare the set rather than an emission order the
        // serializer does not preserve.
        assert_eq!(
            object.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "attempt",
                "event",
                "level",
                "metrics",
                "operation_id",
                "schema",
                "timestamp_ms",
            ]
        );
        assert_eq!(event.human_line().split_whitespace().count(), 5);
    }

    #[test]
    fn upload_progress_has_stable_machine_and_human_names() {
        let event = LogEvent::new(LogLevel::Trace, EventCode::UploadProgress).operation(9);
        assert_eq!(event.json_value()["event"], "upload.progress");
        assert!(event.human_line().contains("upload progress"));
    }

    #[test]
    fn every_event_code_has_a_stable_machine_and_human_name() {
        // Driven by `EventCode::ALL` rather than a hand-written list, so a new variant cannot be
        // added without also being given both names: an omission fails the exhaustiveness check
        // below instead of silently escaping this test the way a literal array allowed.
        let mut machine_names = std::collections::BTreeSet::new();
        let mut human_names = std::collections::BTreeSet::new();
        for code in EventCode::ALL {
            let event = LogEvent::new(LogLevel::Info, code);
            let machine = event.json_value()["event"]
                .as_str()
                .expect("every event renders a machine name")
                .to_owned();
            assert!(!machine.is_empty(), "{code:?} has an empty machine name");
            assert!(
                machine
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'.' | b'_')),
                "{code:?} machine name {machine:?} is not a stable lowercase identifier"
            );
            assert!(
                machine_names.insert(machine.clone()),
                "{code:?} reuses machine name {machine:?}"
            );
            let human = code.human().to_owned();
            assert!(!human.is_empty(), "{code:?} has an empty human name");
            assert!(
                event.human_line().contains(&human),
                "{machine} human line omits {human:?}"
            );
            human_names.insert(human);
        }
        assert_eq!(
            machine_names.len(),
            EventCode::ALL.len(),
            "EventCode::ALL must list every variant exactly once"
        );
        // A representative sample is pinned verbatim so a rename is a deliberate, visible change
        // to the published contract rather than a silently accepted one.
        for expected in [
            "run.build",
            "run.started",
            "run.completed",
            "run.failed",
            "connection.established",
            "api_discovery.started",
            "authentication.started",
            "session.established",
            "api_call.started",
            "api_call.completed",
            "api_call.redirected",
            "upload.progress",
            "retry.scheduled",
            "cancellation.requested",
        ] {
            assert!(
                machine_names.contains(expected),
                "the published event name {expected:?} is gone"
            );
        }
    }

    #[test]
    fn log_levels_parse_case_insensitively_and_reject_unknown_values() {
        for (input, expected, rendered) in [
            (" ERROR ", LogLevel::Error, "error"),
            ("warning", LogLevel::Warn, "warn"),
            ("INFO", LogLevel::Info, "info"),
            ("Debug", LogLevel::Debug, "debug"),
            ("trace", LogLevel::Trace, "trace"),
        ] {
            let parsed = LogLevel::parse(input).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), rendered);
        }
        assert!(matches!(
            LogLevel::parse("verbose"),
            Err(ObservabilityError::InvalidLogLevel)
        ));
    }

    #[test]
    fn structured_and_human_events_include_only_bounded_metrics() {
        let event = LogEvent::new(LogLevel::Warn, EventCode::RetryScheduled)
            .operation(17)
            .attempt(3)
            .metrics(EventMetrics {
                operations: 2,
                files: 1,
                bytes: 4096,
                elapsed_ms: 250,
                throughput_bytes_per_second: 1024,
                eta_ms: Some(750),
            });
        let json = event.json_value();
        assert_eq!(json["operation_id"], 17);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["metrics"]["throughput_bytes_per_second"], 1024);
        assert_eq!(json["metrics"]["eta_ms"], 750);
        let human = event.human_line();
        for field in [
            "operation_id=17",
            "attempt=3",
            "operations=2",
            "files=1",
            "bytes=4096",
            "elapsed_ms=250",
        ] {
            assert!(human.contains(field), "{field}");
        }
    }

    #[test]
    fn logger_filters_and_writes_json_lines() {
        let buffer = SharedWriter::default();
        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Info,
                stderr: Some(LogFormat::Json),
                file: None,
                remote: None,
            },
            buffer.clone(),
        )
        .unwrap();
        logger
            .emit(LogEvent::new(LogLevel::Debug, EventCode::UploadStarted))
            .unwrap();
        logger
            .emit(LogEvent::new(LogLevel::Info, EventCode::UploadCompleted))
            .unwrap();
        logger.flush(Duration::from_secs(1)).unwrap();
        let output = buffer.text();
        assert_eq!(output.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(value["event"], "upload.completed");
    }

    #[test]
    fn file_sink_rotates_locally() {
        let directory = unique_test_directory("rotation");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.log");
        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Trace,
                stderr: None,
                file: Some(FileLogConfig {
                    path: path.clone(),
                    format: LogFormat::Json,
                    max_bytes: 180,
                    backups: 2,
                }),
                remote: None,
            },
            Vec::new(),
        )
        .unwrap();
        for id in 0..5 {
            logger
                .emit(LogEvent::new(LogLevel::Info, EventCode::UploadCompleted).operation(id))
                .unwrap();
        }
        logger.flush(Duration::from_secs(1)).unwrap();
        assert!(path.exists());
        assert!(rotated_path(&path, 1).exists());
        drop(logger);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_file_sink_configuration_is_rejected_before_opening_a_file() {
        let directory = unique_test_directory("invalid-file-config");
        fs::create_dir_all(&directory).unwrap();
        for config in [
            FileLogConfig {
                path: PathBuf::new(),
                format: LogFormat::Json,
                max_bytes: 1,
                backups: 1,
            },
            FileLogConfig {
                path: directory.join("zero.log"),
                format: LogFormat::Json,
                max_bytes: 0,
                backups: 1,
            },
            FileLogConfig {
                path: directory.join("too-many.log"),
                format: LogFormat::Json,
                max_bytes: 1,
                backups: MAX_FILE_BACKUPS + 1,
            },
        ] {
            assert!(matches!(
                RotatingFile::open(config),
                Err(ObservabilityError::InvalidFileConfiguration)
            ));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn zero_backup_rotation_truncates_in_place() {
        let directory = unique_test_directory("zero-backup");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.log");
        let mut sink = RotatingFile::open(FileLogConfig {
            path: path.clone(),
            format: LogFormat::Human,
            max_bytes: 6,
            backups: 0,
        })
        .unwrap();

        sink.write_line("first").unwrap();
        sink.write_line("next").unwrap();
        sink.flush().unwrap();
        drop(sink);

        assert_eq!(fs::read_to_string(&path).unwrap(), "next\n");
        assert!(!rotated_path(&path, 1).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bearer_token_files_are_trimmed_bounded_and_ascii_safe() {
        let directory = unique_test_directory("bearer-token");
        fs::create_dir_all(&directory).unwrap();

        let valid = directory.join("valid.token");
        fs::write(&valid, b"header.payload.signature\r\n").unwrap();
        let token = load_bearer_token(&BearerTokenSource::File(valid)).unwrap();
        assert_eq!(token.as_str(), "header.payload.signature");

        for (name, bytes) in [
            ("empty", Vec::new()),
            ("space", b"token with spaces".to_vec()),
            ("quote", b"token\"value".to_vec()),
            ("utf8", vec![0xff, 0xfe]),
            (
                "oversized",
                vec![b'x'; usize::try_from(MAX_BEARER_TOKEN_BYTES + 1).unwrap()],
            ),
        ] {
            let path = directory.join(name);
            fs::write(&path, bytes).unwrap();
            assert!(matches!(
                load_bearer_token(&BearerTokenSource::File(path)),
                Err(ObservabilityError::InvalidBearerToken)
            ));
        }
        let missing = directory.join("missing");
        assert!(matches!(
            load_bearer_token(&BearerTokenSource::File(missing)),
            Err(ObservabilityError::Io {
                operation: "bearer token file open",
                ..
            })
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn remote_endpoint_rejects_credential_bearing_forms() {
        assert!(validate_remote_endpoint("https://logs.example.test/events").is_ok());
        for endpoint in [
            "http://logs.example.test/events",
            "https://user:pass@logs.example.test/events",
            "https://logs.example.test/events?token=secret",
            "https://logs.example.test/events#secret",
        ] {
            assert!(validate_remote_endpoint(endpoint).is_err());
        }
    }

    #[test]
    fn invalid_remote_configuration_is_rejected_without_starting_a_worker() {
        for (queue_capacity, timeout) in [
            (0, Duration::from_secs(1)),
            (MAX_REMOTE_QUEUE + 1, Duration::from_secs(1)),
            (1, Duration::ZERO),
        ] {
            assert!(matches!(
                RemoteSink::from_config(RemoteLogConfig {
                    endpoint: "https://logs.example.test/events".to_owned(),
                    bearer_token: None,
                    queue_capacity,
                    timeout,
                    delivery: RemoteDelivery::Required,
                }),
                Err(ObservabilityError::InvalidRemoteConfiguration)
            ));
        }
    }

    #[test]
    fn required_remote_queue_overflow_is_reported_and_counted() {
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let remote = RemoteSink::spawn(
            Box::new(BlockingTransport {
                started: started_send,
                release: release_receive,
            }),
            1,
            RemoteDelivery::Required,
        )
        .unwrap();
        remote.enqueue("first".to_owned()).unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        remote.enqueue("queued".to_owned()).unwrap();

        assert!(matches!(
            remote.enqueue("overflow".to_owned()),
            Err(ObservabilityError::RemoteQueueFull)
        ));
        assert_eq!(remote.report().remote_events_dropped, 1);

        release_send.send(()).unwrap();
        release_send.send(()).unwrap();
        remote.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn successful_remote_shutdown_is_idempotent_and_closes_the_sink() {
        let calls = Arc::new(AtomicU64::new(0));
        let remote = RemoteSink::spawn(
            Box::new(FakeTransport {
                calls: Arc::clone(&calls),
                fail: false,
            }),
            2,
            RemoteDelivery::Required,
        )
        .unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        remote.flush(Duration::from_secs(1)).unwrap();
        remote.shutdown(Duration::from_secs(1)).unwrap();

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(matches!(
            remote.flush(Duration::from_secs(1)),
            Err(ObservabilityError::AlreadyShutdown)
        ));
        assert!(matches!(
            remote.enqueue("{}".to_owned()),
            Err(ObservabilityError::RemoteFailure(
                RemoteFailure::WorkerStopped
            ))
        ));
        remote.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn required_remote_failure_surfaces_on_flush() {
        let transport = Box::new(FakeTransport {
            calls: Arc::new(AtomicU64::new(0)),
            fail: true,
        });
        let remote = RemoteSink::spawn(transport, 4, RemoteDelivery::Required).unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        assert!(matches!(
            remote.flush(Duration::from_secs(1)),
            Err(ObservabilityError::RemoteFailure(RemoteFailure::Transport))
        ));
        assert_eq!(remote.report().remote_delivery_failures, 1);
        let _ = remote.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn best_effort_remote_failure_is_reported_without_failing() {
        let transport = Box::new(FakeTransport {
            calls: Arc::new(AtomicU64::new(0)),
            fail: true,
        });
        let remote = RemoteSink::spawn(transport, 4, RemoteDelivery::BestEffort).unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        remote.flush(Duration::from_secs(1)).unwrap();
        assert_eq!(remote.report().remote_delivery_failures, 1);
        remote.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn dropping_remote_sink_does_not_wait_for_blocked_transport() {
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let remote = RemoteSink::spawn(
            Box::new(BlockingTransport {
                started: started_send,
                release: release_receive,
            }),
            1,
            RemoteDelivery::BestEffort,
        )
        .unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let started = Instant::now();
        drop(remote);
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = release_send.send(());
    }

    #[test]
    fn explicit_shutdown_observes_its_deadline() {
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let remote = RemoteSink::spawn(
            Box::new(BlockingTransport {
                started: started_send,
                release: release_receive,
            }),
            1,
            RemoteDelivery::BestEffort,
        )
        .unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let started = Instant::now();
        assert!(matches!(
            remote.shutdown(Duration::from_millis(20)),
            Err(ObservabilityError::FlushTimeout)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = release_send.send(());
    }

    #[test]
    fn defaults_public_errors_and_local_shutdown_have_stable_contracts() {
        let config = LoggerConfig::default();
        assert_eq!(config.level, LogLevel::Info);
        assert_eq!(config.stderr, Some(LogFormat::Human));
        assert!(config.file.is_none() && config.remote.is_none());
        let logger = EventLogger::new(config).unwrap();
        assert_eq!(
            logger.shutdown(Duration::from_secs(1)).unwrap(),
            ShutdownReport::default()
        );

        let cases = vec![
            (ObservabilityError::InvalidLogLevel, "invalid log level"),
            (
                ObservabilityError::InvalidFileConfiguration,
                "invalid file log configuration",
            ),
            (
                ObservabilityError::InvalidRemoteEndpoint,
                "must be an HTTPS URL",
            ),
            (
                ObservabilityError::InvalidRemoteConfiguration,
                "invalid remote log configuration",
            ),
            (
                ObservabilityError::InvalidBearerToken,
                "invalid remote log bearer token",
            ),
            (
                ObservabilityError::RemoteQueueFull,
                "remote log queue is full",
            ),
            (
                ObservabilityError::RemoteFailure(RemoteFailure::Transport),
                "transport failed",
            ),
            (
                ObservabilityError::RemoteFailure(RemoteFailure::Rejected),
                "rejected an event",
            ),
            (
                ObservabilityError::RemoteFailure(RemoteFailure::WorkerStopped),
                "worker stopped",
            ),
            (ObservabilityError::FlushTimeout, "timed out flushing"),
            (ObservabilityError::AlreadyShutdown, "logger is shut down"),
        ];
        for (error, expected) in cases {
            assert!(error.to_string().contains(expected));
            assert!(std::error::Error::source(&error).is_none());
        }
        let io_error = ObservabilityError::Io {
            operation: "test write",
            source: io::Error::other("sensitive detail"),
        };
        assert_eq!(
            io_error.to_string(),
            "observability I/O failed during test write"
        );
        assert_eq!(
            std::error::Error::source(&io_error).unwrap().to_string(),
            "sensitive detail"
        );
    }

    #[test]
    fn writer_and_closed_file_failures_preserve_sink_operation_context() {
        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Trace,
                stderr: Some(LogFormat::Human),
                file: None,
                remote: None,
            },
            FailingWriter { fail_write: true },
        )
        .unwrap();
        assert!(matches!(
            logger.emit(LogEvent::new(LogLevel::Info, EventCode::RunStarted)),
            Err(ObservabilityError::Io {
                operation: "stderr log write",
                ..
            })
        ));

        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Trace,
                stderr: Some(LogFormat::Json),
                file: None,
                remote: None,
            },
            FailingWriter { fail_write: false },
        )
        .unwrap();
        assert!(matches!(
            logger.flush(Duration::ZERO),
            Err(ObservabilityError::Io {
                operation: "stderr log flush",
                ..
            })
        ));

        let directory = unique_test_directory("closed-file");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.log");
        let mut sink = RotatingFile::open(FileLogConfig {
            path: path.clone(),
            format: LogFormat::Human,
            max_bytes: 64,
            backups: 1,
        })
        .unwrap();
        sink.file = None;
        assert!(matches!(
            sink.write_line("event"),
            Err(ObservabilityError::Io {
                operation: "file log write",
                ..
            })
        ));
        assert!(matches!(
            sink.flush(),
            Err(ObservabilityError::Io {
                operation: "file log flush",
                ..
            })
        ));
        drop(sink);

        assert!(matches!(
            RotatingFile::open(FileLogConfig {
                path: directory.clone(),
                format: LogFormat::Json,
                max_bytes: 64,
                backups: 1,
            }),
            Err(ObservabilityError::Io {
                operation: "file log open",
                ..
            })
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn configured_https_remote_sink_reports_transport_failure_on_shutdown() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("https://{}/events", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let acceptor = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        drop(stream);
                        return true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("ephemeral TLS listener failed: {error}"),
                }
            }
        });
        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Trace,
                stderr: None,
                file: None,
                remote: Some(RemoteLogConfig {
                    endpoint,
                    bearer_token: None,
                    queue_capacity: 2,
                    timeout: Duration::from_millis(100),
                    delivery: RemoteDelivery::Required,
                }),
            },
            Vec::new(),
        )
        .unwrap();
        logger
            .emit(LogEvent::new(LogLevel::Info, EventCode::RunFailed))
            .unwrap();
        assert!(matches!(
            logger.shutdown(Duration::from_secs(2)),
            Err(ObservabilityError::RemoteFailure(RemoteFailure::Transport))
        ));
        assert!(acceptor.join().unwrap(), "HTTPS transport never connected");
    }

    #[test]
    fn best_effort_queue_overflow_is_counted_without_failing_the_caller() {
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let remote = RemoteSink::spawn(
            Box::new(BlockingTransport {
                started: started_send,
                release: release_receive,
            }),
            1,
            RemoteDelivery::BestEffort,
        )
        .unwrap();
        remote.enqueue("first".to_owned()).unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        remote.enqueue("queued".to_owned()).unwrap();
        remote.enqueue("dropped".to_owned()).unwrap();
        assert_eq!(remote.report().remote_events_dropped, 1);
        release_send.send(()).unwrap();
        release_send.send(()).unwrap();
        remote.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeTransport {
        calls: Arc<AtomicU64>,
        fail: bool,
    }

    struct FailingWriter {
        fail_write: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                Err(io::Error::other("write failed"))
            } else {
                Ok(buffer.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush failed"))
        }
    }

    impl RemoteTransport for FakeTransport {
        fn send(&mut self, _body: &str) -> std::result::Result<(), RemoteFailure> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                Err(RemoteFailure::Transport)
            } else {
                Ok(())
            }
        }
    }

    struct BlockingTransport {
        started: mpsc::Sender<()>,
        release: Receiver<()>,
    }

    impl RemoteTransport for BlockingTransport {
        fn send(&mut self, _body: &str) -> std::result::Result<(), RemoteFailure> {
            let _ = self.started.send(());
            let _ = self.release.recv();
            Ok(())
        }
    }

    fn unique_test_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "sdsync-observability-{label}-{}-{nonce}",
            std::process::id()
        ))
    }
}
