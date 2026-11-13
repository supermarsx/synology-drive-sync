//! A DSM session lent by the package daemon to one run, and handed back by the run (fix9 P7).
//!
//! The run that logs in hands its session to the daemon instead of logging out; the daemon keeps
//! it for at most an idle TTL and lends it to the next run of the same profile, which then skips
//! discovery, login and logout. Every byte of a handover is secret, because it carries the SID and
//! the `SynoToken`:
//!
//! - it only ever travels over an anonymous socket (the run's standard input), never argv, the
//!   environment or a file;
//! - [`SessionHandover::encode`] writes into one buffer of fixed capacity, so no reallocation
//!   leaves a copy behind, and the buffer is zeroized when dropped;
//! - [`SessionHandover`] has no `Serialize`, no `Display`, and a `Debug` that names only the
//!   binding and the endpoint;
//! - decoding moves the SID straight from the parser into a zeroizing string.
//!
//! The binding is a digest the run computes from what its credentials are (URL, account and the
//! identity of the secret files). A lent session whose binding is not the run's own is never used.

use std::borrow::Cow;

use subtle::ConstantTimeEq;
use zeroize::Zeroize;

use super::*;

/// The `schema` of a handover document.
pub const HANDOVER_SCHEMA: &str = "sdsync.session-handover.v1";

/// What a lender writes on the channel when it has nothing to lend: the run logs in itself and
/// hands back the session it made.
pub const LENT_NOTHING: &[u8] = b"none\n";

/// The first line a run writes back on the channel: this schema, one space, and what became of
/// the lent session (`none`, `used`, `rejected` or `ignored`). The session it now owns, if any,
/// follows as one encoded handover.
pub const HANDBACK_SCHEMA: &str = "sdsync.session-handback.v1";

/// The longest handover a reader accepts and a writer produces. A real one is about 2 KiB: the
/// API map is the bulk of it.
pub const MAX_HANDOVER_BYTES: usize = 64 * 1024;

/// A DSM session in transit, with what a holder needs to use it or end it: the endpoint and its
/// transport options, the discovered API map, and the session itself.
pub struct SessionHandover {
    binding: String,
    options: ClientOptions,
    apis: HashMap<String, ApiSpec>,
    session: Session,
}

impl fmt::Debug for SessionHandover {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the SID or the token: this is what a panic message or a `{:?}` would print.
        formatter
            .debug_struct("SessionHandover")
            .field("binding", &self.binding)
            .field("base_url", &self.options.base_url)
            .field("apis", &self.apis.len())
            .field("session", &"<redacted>")
            .finish()
    }
}

/// One API of the map: its CGI path and version range.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireApi<'a> {
    path: Cow<'a, str>,
    min: u32,
    max: u32,
}

#[derive(Serialize)]
struct WireOut<'a> {
    schema: &'static str,
    binding: &'a str,
    base_url: &'a str,
    allow_http: bool,
    accept_invalid_certs: bool,
    ca_certificate: Option<&'a std::path::Path>,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    retries: u32,
    api_map: BTreeMap<&'a str, WireApi<'a>>,
    sid: &'a str,
    syno_token: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIn {
    schema: String,
    binding: String,
    base_url: String,
    allow_http: bool,
    accept_invalid_certs: bool,
    ca_certificate: Option<PathBuf>,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    retries: u32,
    api_map: BTreeMap<String, WireApi<'static>>,
    sid: String,
    syno_token: Option<String>,
}

impl Drop for WireIn {
    fn drop(&mut self) {
        // Moved out on success; on any early return the secret still dies here.
        self.sid.zeroize();
        if let Some(token) = self.syno_token.as_mut() {
            token.zeroize();
        }
    }
}

/// An `io::Write` over a buffer that never reallocates: a write past its capacity fails instead,
/// so the encoded secret exists in exactly one allocation, which is zeroized on drop.
struct FixedBuffer(Zeroizing<Vec<u8>>);

impl Write for FixedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > self.0.capacity() {
            return Err(std::io::Error::other(
                "the session handover exceeds its bound",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn handover_error(message: impl Into<String>) -> Error {
    Error::InvalidResponse {
        operation: "session handover".to_owned(),
        message: message.into(),
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn same_sid(left: &Session, right: &Session) -> bool {
    bool::from(left.sid.as_bytes().ct_eq(right.sid.as_bytes()))
}

impl SessionHandover {
    /// The binding the session was established under (a digest, not a secret).
    pub fn binding(&self) -> &str {
        &self.binding
    }

    /// Whether `other` carries the same DSM session, compared in constant time.
    pub fn same_session(&self, other: &Self) -> bool {
        same_sid(&self.session, &other.session)
    }

    /// One line of JSON, newline included, in a single zeroizing allocation of
    /// [`MAX_HANDOVER_BYTES`].
    pub fn encode(&self) -> Result<Zeroizing<Vec<u8>>> {
        let wire = WireOut {
            schema: HANDOVER_SCHEMA,
            binding: &self.binding,
            base_url: &self.options.base_url,
            allow_http: self.options.allow_http,
            accept_invalid_certs: self.options.accept_invalid_certs,
            ca_certificate: self.options.ca_certificate.as_deref(),
            connect_timeout_ms: millis(self.options.connect_timeout),
            request_timeout_ms: millis(self.options.request_timeout),
            retries: self.options.retries,
            api_map: self
                .apis
                .iter()
                .map(|(name, spec)| {
                    (
                        name.as_str(),
                        WireApi {
                            path: Cow::Borrowed(spec.path.as_str()),
                            min: spec.min_version,
                            max: spec.max_version,
                        },
                    )
                })
                .collect(),
            sid: self.session.sid.as_str(),
            syno_token: self.session.syno_token.as_ref().map(|token| token.as_str()),
        };
        let mut buffer = FixedBuffer(Zeroizing::new(Vec::with_capacity(MAX_HANDOVER_BYTES)));
        serde_json::to_writer(&mut buffer, &wire)
            .and_then(|()| buffer.write_all(b"\n").map_err(serde_json::Error::io))
            .map_err(|_| handover_error("the session could not be encoded within its bound"))?;
        Ok(buffer.0)
    }

    /// Read one handover document. Every field is checked: the schema, the API map's ranges and
    /// the session's header shape, so a damaged document is refused rather than half-used.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_HANDOVER_BYTES {
            return Err(handover_error("the session handover exceeds its bound"));
        }
        let mut wire: WireIn = serde_json::from_slice(bytes).map_err(|error| {
            // Only the category: the parser's message can quote the input.
            handover_error(format!(
                "the session handover is not a valid document ({:?} error)",
                error.classify()
            ))
        })?;
        if wire.schema != HANDOVER_SCHEMA {
            return Err(handover_error("the session handover has an unknown schema"));
        }
        if wire.binding.is_empty() || wire.binding.len() > 128 {
            return Err(handover_error("the session handover has no usable binding"));
        }
        let mut apis = HashMap::with_capacity(wire.api_map.len());
        for (name, entry) in std::mem::take(&mut wire.api_map) {
            if entry.min > entry.max || entry.path.is_empty() {
                return Err(handover_error(format!(
                    "the session handover's {name} entry is not a valid API range"
                )));
            }
            apis.insert(
                name,
                ApiSpec {
                    path: entry.path.into_owned(),
                    min_version: entry.min,
                    max_version: entry.max,
                    _request_format: None,
                },
            );
        }
        let session = Session {
            sid: Zeroizing::new(std::mem::take(&mut wire.sid)),
            syno_token: wire.syno_token.take().map(Zeroizing::new),
        };
        if session.sid.is_empty() {
            return Err(handover_error("the session handover carries no session"));
        }
        // The same shape check a login applies before a session is used.
        session
            .request_headers()
            .map_err(|_| handover_error("the session handover carries a malformed session"))?;
        Ok(Self {
            binding: std::mem::take(&mut wire.binding),
            options: ClientOptions {
                base_url: std::mem::take(&mut wire.base_url),
                allow_http: wire.allow_http,
                accept_invalid_certs: wire.accept_invalid_certs,
                ca_certificate: wire.ca_certificate.take(),
                connect_timeout: Duration::from_millis(wire.connect_timeout_ms),
                request_timeout: Duration::from_millis(wire.request_timeout_ms),
                retries: wire.retries,
            },
            apis,
            session,
        })
    }

    /// End this session at DSM, bounded by `timeout`. For a holder that never connected: the
    /// daemon, when the TTL lapses or the service stops. Builds the transport from the options
    /// the session was established with, and makes exactly one request.
    pub fn log_out(&self, timeout: Duration) -> Result<()> {
        let mut client = ApiClient::undiscovered(&self.options, None, None)?;
        client.apis = self.apis.clone();
        client.session = Arc::new(RwLock::new(Some(self.session.clone())));
        client.logout_bounded(timeout)
    }
}

impl ApiClient {
    /// The current session, packaged for a holder that will lend it to a later run. `None` when
    /// the client holds no session. The client keeps its session: the caller decides whether to
    /// hand it over or log it out.
    pub fn hand_over_session(
        &self,
        binding: &str,
        options: &ClientOptions,
    ) -> Option<SessionHandover> {
        let session = self.current_session()?;
        Some(SessionHandover {
            binding: binding.to_owned(),
            options: options.clone(),
            apis: self.apis.clone(),
            session,
        })
    }

    /// Connect with a lent session: no discovery and no login. The lent API map is checked against
    /// what a run requires exactly as a discovered one would be.
    pub fn resume_observed(
        options: &ClientOptions,
        handover: &SessionHandover,
        observer: Option<RequestObserver>,
    ) -> Result<Self> {
        let mut client = Self::undiscovered(options, None, observer)?;
        client.apis = handover.apis.clone();
        client.validate_requirements(RUN_REQUIRED_APIS)?;
        client.session = Arc::new(RwLock::new(Some(handover.session.clone())));
        Ok(client)
    }

    /// Whether this client still sends the session `handover` carries, compared in constant time.
    pub fn holds_session_of(&self, handover: &SessionHandover) -> bool {
        self.current_session()
            .is_some_and(|session| same_sid(&session, &handover.session))
    }

    /// Forget the session without telling DSM: for a holder that has handed it to someone else.
    pub fn forget_session(&mut self) {
        self.session = Arc::new(RwLock::new(None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handover(sid: &str, token: Option<&str>) -> SessionHandover {
        let mut apis = HashMap::new();
        for (name, path) in [
            ("SYNO.API.Auth", "entry.cgi"),
            ("SYNO.FileStation.List", "entry.cgi"),
        ] {
            apis.insert(
                name.to_owned(),
                ApiSpec {
                    path: path.to_owned(),
                    min_version: 1,
                    max_version: 7,
                    _request_format: None,
                },
            );
        }
        SessionHandover {
            binding: "b".repeat(64),
            options: ClientOptions {
                base_url: "https://nas.example:5001".to_owned(),
                allow_http: false,
                accept_invalid_certs: false,
                ca_certificate: Some(PathBuf::from("/etc/ca.pem")),
                connect_timeout: Duration::from_secs(10),
                request_timeout: Duration::from_secs(60),
                retries: 2,
            },
            apis,
            session: Session {
                sid: Zeroizing::new(sid.to_owned()),
                syno_token: token.map(|token| Zeroizing::new(token.to_owned())),
            },
        }
    }

    #[test]
    fn a_handover_round_trips_every_field() {
        let original = handover("sid-secret-value", Some("token-secret-value"));
        let bytes = original.encode().unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        let decoded = SessionHandover::decode(&bytes).unwrap();
        assert!(decoded.same_session(&original));
        assert_eq!(decoded.binding(), original.binding());
        assert_eq!(decoded.options.base_url, original.options.base_url);
        assert_eq!(
            decoded.options.ca_certificate,
            original.options.ca_certificate
        );
        assert_eq!(decoded.options.request_timeout, Duration::from_secs(60));
        assert_eq!(decoded.options.retries, 2);
        assert_eq!(
            decoded.session.syno_token.as_deref().map(String::as_str),
            Some("token-secret-value")
        );
        let auth = &decoded.apis["SYNO.API.Auth"];
        assert_eq!(
            (auth.path.as_str(), auth.min_version, auth.max_version),
            ("entry.cgi", 1, 7)
        );
    }

    #[test]
    fn debug_never_prints_the_session() {
        let printed = format!(
            "{:?}",
            handover("sid-secret-value", Some("token-secret-value"))
        );
        assert!(!printed.contains("sid-secret-value"), "{printed}");
        assert!(!printed.contains("token-secret-value"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
    }

    #[test]
    fn a_refused_document_never_quotes_its_input() {
        let original = handover("sid-secret-value", None);
        let bytes = original.encode().unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        for damaged in [
            text.replace(HANDOVER_SCHEMA, "sdsync.session-handover.v0"),
            text.replace(
                "\"retries\":2",
                "\"retries\":2,\"extra\":\"sid-secret-value\"",
            ),
            text.replace("\"min\":1", "\"min\":9"),
            text.replace("sid-secret-value", ""),
            text.replace("sid-secret-value", "sid;secret"),
            text[..text.len() / 2].to_owned(),
        ] {
            let error = SessionHandover::decode(damaged.as_bytes())
                .unwrap_err()
                .to_string();
            assert!(!error.contains("sid-secret-value"), "{error}");
            assert!(!error.contains("secret"), "{error}");
        }
    }

    #[test]
    fn an_oversized_document_is_refused_on_both_sides() {
        let huge = handover(&"s".repeat(MAX_HANDOVER_BYTES), None);
        assert!(huge.encode().is_err());
        assert!(SessionHandover::decode(&vec![b' '; MAX_HANDOVER_BYTES + 1]).is_err());
    }

    #[test]
    fn the_encoded_buffer_is_one_allocation_of_the_bound() {
        let bytes = handover("sid-secret-value", None).encode().unwrap();
        assert!(bytes.capacity() >= MAX_HANDOVER_BYTES);
    }
}
