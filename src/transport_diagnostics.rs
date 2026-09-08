//! Transport-level diagnostics: reachability, intermediaries, and cookie permanence.
//!
//! These checks answer a question the authenticated checks cannot: *what is between this client
//! and DSM, and does the path stay the same from one request to the next?* A DSM session that is
//! accepted on one call and rejected on the next has two plausible explanations -- the client is
//! presenting the session wrongly, or consecutive requests are landing on different DSM hosts --
//! and separating them needs evidence about the transport rather than about the session.
//!
//! Nothing in this module reads a cookie value, a request header, or a response body. Cookie
//! values reach it only as the salted digests [`crate::api`] computed and discarded the value
//! behind; resolved IP addresses are counted and numbered rather than printed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use reqwest::header::LOCATION;
use serde_json::{Value, json};

use crate::api::{self, ClientOptions};
use crate::cancel::CancellationToken;
use crate::observability::{
    ApiCallDetail, CdnMarker, CookieFacts, CookiePersistence, CookieSameSite, IntermediaryFacts,
    RequestOutcome,
};

/// Hostname suffixes Synology hands out for DDNS, which always resolve to the NAS itself.
const SYNOLOGY_DDNS_SUFFIXES: &[&str] = &[
    ".synology.me",
    ".myds.me",
    ".diskstation.me",
    ".dscloud.me",
    ".dsmynas.com",
    ".familyds.com",
    ".i234.me",
    ".synology.wtf",
];

/// The QuickConnect apex. Everything below it is either a relay or a direct address.
const QUICKCONNECT_SUFFIX: &str = ".quickconnect.to";

/// The label that marks the direct QuickConnect forms.
const QUICKCONNECT_DIRECT_LABEL: &str = "direct";

/// The CGI endpoints the HTTP timing probe measures against, in the order discovery tries them.
///
/// Both, and in this order, because [`crate::api::ApiClient`] discovery tries `entry.cgi` first
/// and falls back to `query.cgi`: a host that answers one need not answer the other, and a probe
/// pinned to the fallback measures a route the client may never use. The probe walks the same
/// ladder, so what it times is what the run actually opens with.
const PROBE_CGI_ROUTES: [&str; 2] = ["entry.cgi", "query.cgi"];

/// The unauthenticated query every probe route carries.
const PROBE_QUERY: &str = "?api=SYNO.API.Info&version=1&method=query";

/// Which timing phases this client can and cannot separate, stated once.
///
/// The blocking HTTP client exposes no hook between opening a socket and receiving headers, so
/// the TLS handshake, the server's own processing, and the wait for the first response byte
/// arrive as one measurement. Rather than divide that figure by a guess, the report names it for
/// what it is and leaves the operator to read the parts it *can* separate.
pub const PHASE_SEPARABILITY_NOTE: &str = "DNS resolution and TCP connect are measured directly; the TLS handshake is not separable \
     from the server's own first-byte latency through the blocking HTTP client, so the two are \
     reported together rather than split by estimate";

/// The synthetic API name transport probes are recorded under.
const PROBE_API: &str = "transport.probe";
/// The synthetic method name transport probes are recorded under.
const PROBE_METHOD: &str = "reachability";

// ---------------------------------------------------------------------------------------------
// Endpoint hostname classification
// ---------------------------------------------------------------------------------------------

/// What kind of address the operator pointed this run at.
///
/// The QuickConnect distinction is the one that matters most here. A relay hostname terminates
/// the connection at Synology's relay infrastructure and forwards it on; a direct hostname
/// resolves to the NAS. Only the relay form can put consecutive requests on different backends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointForm {
    /// `[alias].[relay_id].quickconnect.to` -- traffic is relayed by Synology.
    QuickConnectRelay,
    /// `[alias].direct.quickconnect.to` or `[ip].[alias].direct.quickconnect.to`.
    QuickConnectDirect,
    /// Under `quickconnect.to` but matching neither documented form.
    QuickConnectUnrecognized,
    /// A Synology DDNS name, which resolves to the NAS.
    SynologyDdns,
    /// A literal IPv4 or IPv6 address.
    AddressLiteral,
    /// Any other hostname: a LAN name, an own domain, or a self-hosted reverse proxy.
    PlainHost,
}

impl EndpointForm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QuickConnectRelay => "quickconnect-relay",
            Self::QuickConnectDirect => "quickconnect-direct",
            Self::QuickConnectUnrecognized => "quickconnect-unrecognized",
            Self::SynologyDdns => "synology-ddns",
            Self::AddressLiteral => "address-literal",
            Self::PlainHost => "plain-host",
        }
    }

    /// Whether this form is known to put a third party in the data path.
    pub fn is_relayed(self) -> bool {
        self == Self::QuickConnectRelay
    }
}

/// A classified endpoint hostname.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointClassification {
    pub form: EndpointForm,
    /// The relay identifier label of a relay hostname, such as `fr3`. Part of the hostname the
    /// operator typed, so it is reported rather than withheld.
    pub relay_label: Option<String>,
}

impl EndpointClassification {
    fn plain(form: EndpointForm) -> Self {
        Self {
            form,
            relay_label: None,
        }
    }

    /// A sentence an operator can act on, naming what the form implies for session affinity.
    pub fn describe(&self) -> String {
        match self.form {
            EndpointForm::QuickConnectRelay => {
                let label = self.relay_label.as_deref().unwrap_or("unknown");
                format!(
                    "QuickConnect relay hostname (relay id {label}); every request is forwarded \
                     by Synology relay infrastructure and nothing in the hostname pins it to one \
                     DSM backend"
                )
            }
            EndpointForm::QuickConnectDirect => {
                "QuickConnect direct hostname; it resolves to the NAS itself, so no relay sits in \
                 the data path"
                    .to_owned()
            }
            EndpointForm::QuickConnectUnrecognized => {
                "a quickconnect.to hostname in neither the documented relay nor the documented \
                 direct form; treat its routing as unknown"
                    .to_owned()
            }
            EndpointForm::SynologyDdns => {
                "a Synology DDNS hostname, which resolves to the NAS itself".to_owned()
            }
            EndpointForm::AddressLiteral => {
                "a literal IP address, so no name resolution stands between this client and the \
                 host"
                    .to_owned()
            }
            EndpointForm::PlainHost => {
                "an ordinary hostname; whether anything sits in front of DSM depends on the \
                 network it resolves into"
                    .to_owned()
            }
        }
    }

    fn json_value(&self) -> Value {
        json!({
            "form": self.form.as_str(),
            "relay_label": self.relay_label,
            "relayed": self.form.is_relayed(),
        })
    }
}

/// Classify an endpoint hostname.
///
/// The QuickConnect rule is positional rather than pattern-matched: whatever label sits directly
/// in front of `quickconnect.to` is the routing decision. `direct` means the name resolves to the
/// NAS, and anything else is a relay identifier. That covers both documented direct forms --
/// `[alias].direct.quickconnect.to` and `[ip].[alias].direct.quickconnect.to` -- without having
/// to enumerate them.
pub fn classify_endpoint(host: &str) -> EndpointClassification {
    let host = host.trim().trim_matches(['[', ']']).trim_end_matches('.');
    let lowercase = host.to_ascii_lowercase();
    if lowercase.parse::<IpAddr>().is_ok() {
        return EndpointClassification::plain(EndpointForm::AddressLiteral);
    }
    if let Some(prefix) = lowercase.strip_suffix(QUICKCONNECT_SUFFIX) {
        let labels: Vec<&str> = prefix
            .split('.')
            .filter(|label| !label.is_empty())
            .collect();
        return match labels.last() {
            // `[alias].direct.` and `[ip].[alias].direct.` both end in the direct label.
            Some(&last) if last == QUICKCONNECT_DIRECT_LABEL && labels.len() >= 2 => {
                EndpointClassification::plain(EndpointForm::QuickConnectDirect)
            }
            // `[alias].[relay_id].` -- the trailing label is the relay that will carry the run.
            Some(&last) if labels.len() >= 2 => EndpointClassification {
                form: EndpointForm::QuickConnectRelay,
                relay_label: Some(last.to_owned()),
            },
            _ => EndpointClassification::plain(EndpointForm::QuickConnectUnrecognized),
        };
    }
    if SYNOLOGY_DDNS_SUFFIXES
        .iter()
        .any(|suffix| lowercase.ends_with(suffix))
    {
        return EndpointClassification::plain(EndpointForm::SynologyDdns);
    }
    EndpointClassification::plain(EndpointForm::PlainHost)
}

// ---------------------------------------------------------------------------------------------
// Latency samples
// ---------------------------------------------------------------------------------------------

/// A bounded set of timing samples, summarised without pretending to more precision than it has.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LatencySamples {
    micros: Vec<u64>,
}

impl LatencySamples {
    pub fn push(&mut self, elapsed: Duration) {
        self.micros
            .push(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
    }

    pub fn len(&self) -> usize {
        self.micros.len()
    }

    pub fn is_empty(&self) -> bool {
        self.micros.is_empty()
    }

    fn sorted(&self) -> Vec<u64> {
        let mut sorted = self.micros.clone();
        sorted.sort_unstable();
        sorted
    }

    pub fn min_micros(&self) -> Option<u64> {
        self.micros.iter().copied().min()
    }

    pub fn max_micros(&self) -> Option<u64> {
        self.micros.iter().copied().max()
    }

    pub fn median_micros(&self) -> Option<u64> {
        let sorted = self.sorted();
        let middle = sorted.len().checked_sub(1)? / 2;
        if sorted.len() % 2 == 1 {
            sorted.get(middle).copied()
        } else {
            // Even sample counts average the two central values rather than picking a side.
            Some((sorted.get(middle)? + sorted.get(middle + 1)?) / 2)
        }
    }

    pub fn mean_micros(&self) -> Option<u64> {
        if self.micros.is_empty() {
            return None;
        }
        let total: u128 = self.micros.iter().map(|value| u128::from(*value)).sum();
        u64::try_from(total / self.micros.len() as u128).ok()
    }

    /// `max - min`, which is the figure that says whether the path is behaving consistently.
    pub fn spread_micros(&self) -> Option<u64> {
        Some(self.max_micros()?.saturating_sub(self.min_micros()?))
    }

    /// Whether the samples are spread widely enough to suggest more than one path.
    ///
    /// Both a floor and a ratio have to be met. A ratio alone flags sub-millisecond noise on a
    /// LAN, and a floor alone flags an ordinarily slow but perfectly consistent link. Together
    /// they select for what a relay without session affinity actually looks like: some samples
    /// arriving at one cost and some at a visibly different one.
    pub fn is_widely_spread(&self) -> bool {
        const FLOOR_MICROS: u64 = 25_000;
        let (Some(min), Some(max)) = (self.min_micros(), self.max_micros()) else {
            return false;
        };
        self.micros.len() >= 3 && max.saturating_sub(min) >= FLOOR_MICROS && max >= min * 2
    }

    /// `min / median / max ms over N samples`, or a plain statement that there are none.
    pub fn describe(&self) -> String {
        let (Some(min), Some(median), Some(max)) =
            (self.min_micros(), self.median_micros(), self.max_micros())
        else {
            return "no samples".to_owned();
        };
        format!(
            "min {} / median {} / max {} over {} samples",
            format_micros(min),
            format_micros(median),
            format_micros(max),
            self.micros.len(),
        )
    }

    fn json_value(&self) -> Value {
        json!({
            "samples": self.micros.len(),
            "min_us": self.min_micros(),
            "median_us": self.median_micros(),
            "mean_us": self.mean_micros(),
            "max_us": self.max_micros(),
            "spread_us": self.spread_micros(),
            "widely_spread": self.is_widely_spread(),
        })
    }
}

/// Microseconds as milliseconds, to one decimal.
pub fn format_micros(micros: u64) -> String {
    format!("{:.1} ms", micros as f64 / 1000.0)
}

// ---------------------------------------------------------------------------------------------
// Reachability measurement
// ---------------------------------------------------------------------------------------------

/// How much probing one diagnostic level is willing to pay for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReachabilityBudget {
    pub tcp_samples: usize,
    pub http_samples: usize,
    /// A ceiling on the whole probe, so a slow or blackholed host cannot stall the run.
    pub total: Duration,
    /// A ceiling on one HTTP sample.
    ///
    /// Generous on purpose. A QuickConnect relay answers the same unauthenticated discovery
    /// request the control client makes in around 3.8 seconds, so the four-second ceiling this
    /// once hard-coded expired on the relayed paths the probe exists to characterise -- reporting
    /// "no HTTP sample completed" for a host the very next section authenticated against. A probe
    /// that gives up sooner than the client it explains measures nothing but its own impatience.
    pub http_request_timeout: Duration,
}

impl ReachabilityBudget {
    /// The default budget: enough samples for a median to mean something, short enough that an
    /// operator does not notice the wait.
    ///
    /// The total ceiling is checked before each sample rather than during one, so the true worst
    /// case is `total` plus one `http_request_timeout`.
    pub const fn standard() -> Self {
        Self {
            tcp_samples: 5,
            http_samples: 3,
            total: Duration::from_secs(10),
            http_request_timeout: Duration::from_secs(8),
        }
    }

    pub const fn quick() -> Self {
        Self {
            tcp_samples: 3,
            http_samples: 2,
            total: Duration::from_secs(6),
            http_request_timeout: Duration::from_secs(5),
        }
    }

    /// Extensive buys enough TCP samples that a bimodal connect time is unmistakable, and the
    /// fewest HTTP samples that can still reach a verdict.
    ///
    /// **Three is a floor, not a preference.** The only finding the HTTP samples uniquely produce
    /// is [`LatencySamples::is_widely_spread`], which requires `len() >= 3` before it can fire at
    /// all. Trimming this to 2 would not make the probe cheaper so much as make that finding
    /// unreachable, and the section would report a consistent path because it never had enough
    /// samples to say otherwise.
    ///
    /// It was 5, and the fourth and fifth samples were what made this the most expensive section
    /// of the run. On a QuickConnect relay each HTTP sample costs the ~3.8 s a relayed discovery
    /// request costs, so five of them approach the 20 s `total` on their own -- before the client
    /// this probe explains has even been constructed. Those two extra samples bought confidence
    /// about a conclusion the run already had from elsewhere: `suggests_multiple_paths` is
    /// `dns.address_count > 1 || ...`, which a relay hostname satisfies on almost every run, and
    /// the intermediary section warns on a relayed endpoint independently. Three keeps the
    /// verdict reachable and leaves the ceiling real headroom rather than about a second of it.
    ///
    /// This is also why the ceiling is not the thing to adjust: the worst case is `total` plus one
    /// `http_request_timeout`, so buying margin by raising `total` would have re-inflated exactly
    /// the number the margin was for.
    pub const fn extensive() -> Self {
        Self {
            tcp_samples: 9,
            http_samples: 3,
            total: Duration::from_secs(20),
            http_request_timeout: Duration::from_secs(12),
        }
    }
}

/// What name resolution reported for the endpoint host.
#[derive(Clone, Debug, Default)]
pub struct DnsObservation {
    pub elapsed: Option<Duration>,
    pub address_count: usize,
    pub ipv4_count: usize,
    pub ipv6_count: usize,
    /// The host was already an address literal, so nothing was resolved.
    pub literal: bool,
    pub error: Option<String>,
}

impl DnsObservation {
    fn json_value(&self) -> Value {
        json!({
            "elapsed_us": self.elapsed.map(|elapsed| u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)),
            "address_count": self.address_count,
            "ipv4_count": self.ipv4_count,
            "ipv6_count": self.ipv6_count,
            "address_literal": self.literal,
            "error": self.error,
        })
    }
}

/// HTTP round-trip timing against the unauthenticated discovery route.
#[derive(Clone, Debug, Default)]
pub struct HttpTimingObservation {
    /// Connect through to the response headers. Includes the TCP connect measured separately.
    pub first_byte: LatencySamples,
    /// Reading the response body to completion, once headers have arrived.
    pub body: LatencySamples,
    pub total: LatencySamples,
    pub statuses: BTreeSet<u16>,
    pub failures: u32,
    pub failure_reason: Option<String>,
    /// Whether at least one sample was abandoned at its own ceiling rather than refused.
    ///
    /// The distinction is the whole difference between "nothing is listening on that path" and
    /// "the path is slower than the probe was willing to wait", and only the second one is
    /// answered by measuring for longer.
    pub timed_out: bool,
    /// The per-sample ceiling these measurements were taken under.
    pub request_timeout: Option<Duration>,
    /// Which CGI finally answered, of the routes discovery itself would try.
    pub route: Option<&'static str>,
    /// How many routes were rejected with an HTTP status before one answered.
    pub routes_rejected: u32,
}

impl HttpTimingObservation {
    /// What is left of the first-byte time once the separately measured TCP connect is removed.
    ///
    /// Named for everything it contains, because it contains all of it: the TLS handshake, the
    /// request write, DSM's own handling, and the wait for the first byte back. It is a
    /// subtraction of two medians, not a measured phase, and the report says so.
    pub fn handshake_and_service_micros(&self, tcp_median_micros: u64) -> Option<u64> {
        Some(
            self.first_byte
                .median_micros()?
                .saturating_sub(tcp_median_micros),
        )
    }

    fn json_value(&self) -> Value {
        json!({
            "first_byte": self.first_byte.json_value(),
            "body": self.body.json_value(),
            "total": self.total.json_value(),
            "statuses": self.statuses.iter().copied().collect::<Vec<_>>(),
            "failures": self.failures,
            "failure_reason": self.failure_reason,
            "timed_out": self.timed_out,
            "request_timeout_ms": self
                .request_timeout
                .map(|timeout| u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
            "route": self.route,
            "routes_rejected": self.routes_rejected,
        })
    }
}

/// Everything the transport probe learned without authenticating.
#[derive(Clone, Debug)]
pub struct ReachabilityReport {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub dns: DnsObservation,
    pub tcp_connect: LatencySamples,
    /// Per resolved address, in the order resolution returned them. Addresses are numbered rather
    /// than printed: the index is what makes a split between two backends visible, and the
    /// address itself adds nothing a `nslookup` would not.
    pub tcp_per_address: Vec<LatencySamples>,
    pub tcp_failures: u32,
    pub tcp_failure_reason: Option<String>,
    pub http: HttpTimingObservation,
    /// Set when the probe stopped early against its own ceiling.
    pub budget_exhausted: bool,
    pub cancelled: bool,
}

impl ReachabilityReport {
    fn empty(host: String, port: u16, tls: bool) -> Self {
        Self {
            host,
            port,
            tls,
            dns: DnsObservation::default(),
            tcp_connect: LatencySamples::default(),
            tcp_per_address: Vec::new(),
            tcp_failures: 0,
            tcp_failure_reason: None,
            http: HttpTimingObservation::default(),
            budget_exhausted: false,
            cancelled: false,
        }
    }

    /// Whether any measurement at all came back.
    pub fn reached(&self) -> bool {
        !self.tcp_connect.is_empty()
    }

    /// Whether sockets open but no HTTP round trip completes.
    ///
    /// Worth separating from "unreachable": a host that accepts connections and then answers
    /// nothing is a different fault from one that refuses them, and the two point at different
    /// parts of the path.
    pub fn connects_but_does_not_answer(&self) -> bool {
        self.reached() && self.http.first_byte.is_empty() && self.http.failures > 0
    }

    /// Whether the samples argue that consecutive connections are not landing in one place.
    ///
    /// Two independent signals, either of which is enough to be worth saying out loud: more than
    /// one address behind the name, or a connect time that varies more than a single path should.
    pub fn suggests_multiple_paths(&self) -> bool {
        self.dns.address_count > 1
            || self.tcp_connect.is_widely_spread()
            || self.http.first_byte.is_widely_spread()
    }

    pub fn json_value(&self) -> Value {
        json!({
            "host": self.host,
            "port": self.port,
            "tls": self.tls,
            "method": "tcp-connect",
            "dns": self.dns.json_value(),
            "tcp_connect": self.tcp_connect.json_value(),
            "tcp_per_address": self
                .tcp_per_address
                .iter()
                .enumerate()
                .map(|(index, samples)| json!({
                    "address_index": index + 1,
                    "timing": samples.json_value(),
                }))
                .collect::<Vec<_>>(),
            "tcp_failures": self.tcp_failures,
            "tcp_failure_reason": self.tcp_failure_reason,
            "http": self.http.json_value(),
            "handshake_and_service_us": self
                .tcp_connect
                .median_micros()
                .and_then(|median| self.http.handshake_and_service_micros(median)),
            "phase_separability": PHASE_SEPARABILITY_NOTE,
            "budget_exhausted": self.budget_exhausted,
            "cancelled": self.cancelled,
            "suggests_multiple_paths": self.suggests_multiple_paths(),
        })
    }

    /// The report body, one line per fact, without indentation.
    pub fn human_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "endpoint: {}:{} over {}",
            self.host,
            self.port,
            if self.tls { "TLS" } else { "cleartext HTTP" }
        ));
        lines.push(
            "reachability is measured by opening TCP connections, not by ICMP echo; no ping \
             packet is sent"
                .to_owned(),
        );
        if self.dns.literal {
            lines.push("DNS: not consulted; the endpoint is an address literal".to_owned());
        } else if let Some(error) = &self.dns.error {
            lines.push(format!("DNS: resolution failed: {error}"));
        } else {
            let mut line = format!(
                "DNS: {} in {}; {} IPv4 and {} IPv6",
                if self.dns.address_count == 1 {
                    "1 address".to_owned()
                } else {
                    format!("{} addresses", self.dns.address_count)
                },
                self.dns
                    .elapsed
                    .map(|elapsed| format_micros(
                        u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
                    ))
                    .unwrap_or_else(|| "an unmeasured time".to_owned()),
                self.dns.ipv4_count,
                self.dns.ipv6_count,
            );
            if self.dns.address_count > 1 {
                let _ = write!(
                    line,
                    " -- consecutive connections can therefore land on different hosts"
                );
            }
            lines.push(line);
        }
        lines.push(format!("TCP connect: {}", self.tcp_connect.describe()));
        if self.tcp_per_address.len() > 1 {
            for (index, samples) in self.tcp_per_address.iter().enumerate() {
                if !samples.is_empty() {
                    lines.push(format!(
                        "  address {} of {}: {}",
                        index + 1,
                        self.tcp_per_address.len(),
                        samples.describe()
                    ));
                }
            }
        }
        if self.tcp_failures > 0 {
            lines.push(format!(
                "TCP connect failures: {}{}",
                self.tcp_failures,
                self.tcp_failure_reason
                    .as_ref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default(),
            ));
        }
        // Which route answered and how long each sample was given are part of reading the timing:
        // a figure measured against `query.cgi` describes a different handler from one measured
        // against `entry.cgi`, and "no sample completed" means something else at 5 s than at 12 s.
        if let Some(route) = self.http.route {
            let mut line = format!("probe route: webapi/{route}");
            if let Some(timeout) = self.http.request_timeout {
                let _ = write!(line, ", {:.1} s per sample", timeout.as_secs_f64());
            }
            if self.http.routes_rejected > 0 {
                let _ = write!(
                    line,
                    " (after {} route(s) answered with an HTTP error status, the same fallback \
                     API discovery makes)",
                    self.http.routes_rejected
                );
            }
            lines.push(line);
        }
        if self.http.first_byte.is_empty() {
            lines.push(format!(
                "HTTP timing: no sample completed{}{}",
                if self.http.timed_out {
                    " (abandoned at the per-sample ceiling, not refused)"
                } else {
                    ""
                },
                self.http
                    .failure_reason
                    .as_ref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default(),
            ));
        } else {
            lines.push(format!(
                "connect to first byte: {}",
                self.http.first_byte.describe()
            ));
            lines.push(format!("body read: {}", self.http.body.describe()));
            lines.push(format!("total round trip: {}", self.http.total.describe()));
            if let Some(remainder) = self
                .tcp_connect
                .median_micros()
                .and_then(|median| self.http.handshake_and_service_micros(median))
            {
                lines.push(format!(
                    "TLS handshake plus DSM service time: {} (median first byte minus median TCP \
                     connect; a derived remainder, not a measured phase)",
                    format_micros(remainder)
                ));
            }
            if !self.http.statuses.is_empty() {
                lines.push(format!(
                    "probe HTTP statuses: {}",
                    self.http
                        .statuses
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        lines.push(PHASE_SEPARABILITY_NOTE.to_owned());
        if self.budget_exhausted {
            lines.push(
                "the probe stopped early against its own time ceiling; fewer samples than \
                 requested were taken"
                    .to_owned(),
            );
        }
        lines
    }
}

/// One probe response, in the shape the transcript records API calls in.
#[derive(Clone, Debug)]
pub struct ProbeObservation {
    pub http_status: Option<u16>,
    pub cookies: CookieFacts,
    pub intermediary: IntermediaryFacts,
    pub redirect_host: Option<String>,
}

/// Measure how the endpoint behaves at the transport layer, without authenticating.
///
/// Returns the timing report and the probe responses, so the caller can fold the latter into the
/// same transcript the authenticated calls feed. Probing never fails the run: an unreachable host
/// produces a report that says so, because "the transport check itself errored" is exactly the
/// kind of answer a diagnostic exists to record rather than to propagate.
pub fn measure_reachability(
    options: &ClientOptions,
    budget: ReachabilityBudget,
    cancellation: &CancellationToken,
) -> (ReachabilityReport, Vec<ProbeObservation>) {
    let started = Instant::now();
    let Ok(base) = api::normalize_base_url(&options.base_url, options.allow_http) else {
        // The URL is rejected by the routing section with a better message than this one could
        // give; there is nothing to measure against an address that cannot be parsed.
        return (
            ReachabilityReport::empty("<unparseable>".to_owned(), 0, false),
            Vec::new(),
        );
    };
    let host = base.host_str().unwrap_or_default().to_owned();
    let tls = base.scheme() == "https";
    let port = base
        .port_or_known_default()
        .unwrap_or(if tls { 443 } else { 80 });
    let mut report = ReachabilityReport::empty(host.clone(), port, tls);

    // Resolution first: everything after it needs an address, and how many came back is itself
    // one of the two signals that a name is not pinned to a single host.
    report.dns.literal = host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok();
    let resolution_started = Instant::now();
    let addresses: Vec<SocketAddr> = match (host.as_str(), port).to_socket_addrs() {
        Ok(addresses) => {
            report.dns.elapsed = Some(resolution_started.elapsed());
            addresses.collect()
        }
        Err(error) => {
            report.dns.elapsed = Some(resolution_started.elapsed());
            report.dns.error = Some(error.to_string());
            Vec::new()
        }
    };
    report.dns.address_count = addresses.len();
    report.dns.ipv4_count = addresses.iter().filter(|address| address.is_ipv4()).count();
    report.dns.ipv6_count = addresses.iter().filter(|address| address.is_ipv6()).count();
    if addresses.is_empty() {
        return (report, Vec::new());
    }
    report.tcp_per_address = vec![LatencySamples::default(); addresses.len()];

    // TCP samples are spread round-robin across the resolved addresses rather than pinned to the
    // first, so a name that fans out to several hosts is sampled at each of them.
    for sample in 0..budget.tcp_samples {
        if cancellation.is_cancelled() {
            report.cancelled = true;
            break;
        }
        if started.elapsed() >= budget.total {
            report.budget_exhausted = true;
            break;
        }
        let index = sample % addresses.len();
        let connect_started = Instant::now();
        match TcpStream::connect_timeout(&addresses[index], options.connect_timeout) {
            Ok(stream) => {
                let elapsed = connect_started.elapsed();
                report.tcp_connect.push(elapsed);
                report.tcp_per_address[index].push(elapsed);
                // Closed immediately: the measurement is the handshake, and leaving sockets in
                // TIME_WAIT on a diagnostic run helps nobody.
                drop(stream);
            }
            Err(error) => {
                report.tcp_failures = report.tcp_failures.saturating_add(1);
                report
                    .tcp_failure_reason
                    .get_or_insert_with(|| error.to_string());
            }
        }
    }

    let mut observations = Vec::new();
    if report.cancelled || budget.http_samples == 0 {
        return (report, observations);
    }
    report.http.request_timeout = Some(budget.http_request_timeout);
    let client = match api::probe_client(options, budget.http_request_timeout) {
        Ok(client) => client,
        Err(error) => {
            report.http.failure_reason = Some(error.to_string());
            return (report, observations);
        }
    };
    let probe_urls: Vec<_> = PROBE_CGI_ROUTES
        .iter()
        .filter_map(|cgi| base.join(&format!("webapi/{cgi}{PROBE_QUERY}")).ok())
        .collect();
    if probe_urls.len() != PROBE_CGI_ROUTES.len() {
        report.http.failure_reason =
            Some("the discovery route could not be derived from the base URL".to_owned());
        return (report, observations);
    }

    let mut route = 0_usize;
    let mut samples = 0_usize;
    report.http.route = Some(PROBE_CGI_ROUTES[route]);
    while samples < budget.http_samples {
        if cancellation.is_cancelled() {
            report.cancelled = true;
            break;
        }
        if started.elapsed() >= budget.total {
            report.budget_exhausted = true;
            break;
        }
        let request_started = Instant::now();
        match client.get(probe_urls[route].clone()).send() {
            // A status rather than a timing: this route is not the one this host serves WebAPI
            // on, and measuring it would characterise the proxy's 404 handler. Move to the route
            // discovery would move to and do not spend a sample on the answer. Bounded by there
            // being two routes, so this can advance at most once.
            Ok(response) if response.status().as_u16() >= 400 && route + 1 < probe_urls.len() => {
                report.http.routes_rejected = report.http.routes_rejected.saturating_add(1);
                route += 1;
                report.http.route = Some(PROBE_CGI_ROUTES[route]);
            }
            Ok(response) => {
                samples += 1;
                let first_byte = request_started.elapsed();
                // Headers are read before the body is touched, because consuming the body moves
                // the response and takes them with it.
                let status = response.status().as_u16();
                let observation = ProbeObservation {
                    http_status: Some(status),
                    cookies: api::cookie_facts(response.headers()),
                    intermediary: api::intermediary_facts(response.headers()),
                    redirect_host: response
                        .headers()
                        .get(LOCATION)
                        .and_then(|location| location.to_str().ok())
                        .and_then(|location| reqwest::Url::parse(location).ok())
                        .and_then(|location| location.host_str().map(str::to_owned)),
                };
                let body_started = Instant::now();
                let body = response.bytes();
                let body_elapsed = body_started.elapsed();
                report.http.statuses.insert(status);
                report.http.first_byte.push(first_byte);
                if body.is_ok() {
                    report.http.body.push(body_elapsed);
                    report.http.total.push(first_byte + body_elapsed);
                } else {
                    report.http.failures = report.http.failures.saturating_add(1);
                }
                observations.push(observation);
            }
            Err(error) => {
                samples += 1;
                report.http.failures = report.http.failures.saturating_add(1);
                report.http.timed_out |= error.is_timeout();
                report
                    .http
                    .failure_reason
                    .get_or_insert_with(|| transport_failure_reason(&error));
                // Nothing has ever completed, and each further attempt costs the full probe
                // timeout to learn the same thing. One failure is the answer.
                if report.http.first_byte.is_empty() {
                    break;
                }
            }
        }
    }
    (report, observations)
}

/// How far the failure reason may run before it is clipped.
const MAX_FAILURE_REASON_CHARS: usize = 240;

/// A transport failure described by its whole cause chain rather than by its outermost layer.
///
/// `reqwest` renders a request-level failure as `error sending request for url (...)` and puts
/// everything that distinguishes one from another -- `operation timed out`, a TLS verification
/// message, a DNS failure -- in the source chain, which `Display` drops. Reporting only the outer
/// text told an operator that *something* went wrong on a URL they could already see, which is
/// what made a probe timeout read as an unexplained transport fault.
///
/// The chain carries no credential: this probe is unauthenticated, sends no session channel, and
/// the URL it names is the public discovery route.
fn transport_failure_reason(error: &reqwest::Error) -> String {
    cause_chain_reason(error)
}

/// The generic half of [`transport_failure_reason`], separated so it can be exercised directly:
/// a `reqwest::Error` cannot be constructed outside its own crate.
fn cause_chain_reason(error: &dyn std::error::Error) -> String {
    let mut reason = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && !reason.contains(&text) {
            let _ = write!(reason, ": {text}");
        }
        source = cause.source();
        if reason.chars().count() >= MAX_FAILURE_REASON_CHARS {
            break;
        }
    }
    if reason.chars().count() > MAX_FAILURE_REASON_CHARS {
        reason = reason
            .chars()
            .take(MAX_FAILURE_REASON_CHARS.saturating_sub(1))
            .chain(['~'])
            .collect();
    }
    reason
}

// ---------------------------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------------------------

/// Where in the run one observation happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallRef {
    /// 1-based position in the run's request sequence, matching the per-section call listing.
    pub sequence: u32,
    pub api: &'static str,
    pub method: &'static str,
}

impl CallRef {
    pub fn describe(self) -> String {
        format!("call {} ({}.{})", self.sequence, self.api, self.method)
    }

    fn json_value(self) -> Value {
        json!({
            "sequence": self.sequence,
            "api": self.api,
            "method": self.method,
        })
    }
}

/// One response, reduced to what the transport checks reason over.
#[derive(Clone, Debug)]
pub struct CallObservation {
    pub at: CallRef,
    pub outcome: RequestOutcome,
    pub http_status: Option<u16>,
    pub cookies: CookieFacts,
    pub intermediary: IntermediaryFacts,
    pub redirect_host: Option<String>,
}

/// Every response of the run, in order, retained for cross-call reasoning.
///
/// This is deliberately separate from the per-section call log the report prints. That log is
/// drained by each section so a section owns its own requests; cookie permanence is a property of
/// the whole run and cannot be answered from a drained list.
#[derive(Clone, Debug, Default)]
pub struct TransportTranscript {
    observations: Vec<CallObservation>,
}

impl TransportTranscript {
    fn next_sequence(&self) -> u32 {
        u32::try_from(self.observations.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    }

    /// Record a completed API call, returning the sequence number it was given.
    pub fn record_call(&mut self, call: &ApiCallDetail) -> u32 {
        let at = CallRef {
            sequence: self.next_sequence(),
            api: call.api,
            method: call.method,
        };
        self.observations.push(CallObservation {
            at,
            outcome: call.outcome,
            http_status: call.http_status,
            cookies: call.cookies,
            intermediary: call.intermediary,
            redirect_host: call.redirect_host.map(|host| host.as_str().to_owned()),
        });
        at.sequence
    }

    /// Record an unauthenticated transport probe response.
    pub fn record_probe(&mut self, probe: ProbeObservation) {
        let at = CallRef {
            sequence: self.next_sequence(),
            api: PROBE_API,
            method: PROBE_METHOD,
        };
        self.observations.push(CallObservation {
            at,
            outcome: RequestOutcome::Ok,
            http_status: probe.http_status,
            cookies: probe.cookies,
            intermediary: probe.intermediary,
            redirect_host: probe.redirect_host,
        });
    }

    pub fn observations(&self) -> &[CallObservation] {
        &self.observations
    }

    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }

    /// Fold every response into one cookie ledger for the run.
    pub fn cookie_ledger(&self) -> CookieLedger {
        let mut ledger = CookieLedger {
            client_maintains_cookie_jar: api::CLIENT_MAINTAINS_COOKIE_JAR,
            ..CookieLedger::default()
        };
        let mut order: Vec<String> = Vec::new();
        let mut entries: BTreeMap<String, CookieLedgerEntry> = BTreeMap::new();
        for observation in &self.observations {
            ledger.undescribed_headers = ledger
                .undescribed_headers
                .saturating_add(u32::from(observation.cookies.undescribed()));
            if !observation.cookies.is_empty() {
                ledger.responses_setting_cookies =
                    ledger.responses_setting_cookies.saturating_add(1);
            }
            for cookie in observation.cookies.described() {
                let name = cookie.name.as_str().to_owned();
                let entry = entries.entry(name.clone()).or_insert_with(|| {
                    order.push(name.clone());
                    CookieLedgerEntry::first(&name, observation.at, cookie.value_length)
                });
                entry.observe(cookie, observation.at, observation.outcome);
            }
        }
        ledger.entries = order
            .into_iter()
            .filter_map(|name| entries.remove(&name))
            .collect();
        ledger
    }

    /// Fold every response into one intermediary fingerprint for the run.
    pub fn intermediary_summary(&self, endpoint: EndpointClassification) -> IntermediarySummary {
        let mut summary = IntermediarySummary {
            endpoint,
            ..IntermediarySummary::default()
        };
        for observation in &self.observations {
            // A request that never received a response carries no headers, and counting it would
            // let the report claim it fingerprinted something it never saw.
            if observation.http_status.is_none() {
                continue;
            }
            summary.responses_observed = summary.responses_observed.saturating_add(1);
            let facts = observation.intermediary;
            if !facts.via.is_empty() {
                summary.via.insert(facts.via.as_str().to_owned());
            }
            if !facts.server.is_empty() {
                summary.server.insert(facts.server.as_str().to_owned());
            }
            if !facts.powered_by.is_empty() {
                summary
                    .powered_by
                    .insert(facts.powered_by.as_str().to_owned());
            }
            if facts.cdn_marker != CdnMarker::None {
                summary.cdn_markers.insert(facts.cdn_marker);
            }
            summary.forwarded_for_reflected |= facts.forwarded_for_reflected;
            summary.real_ip_reflected |= facts.real_ip_reflected;
            for cookie in observation.cookies.described() {
                if !api::is_dsm_cookie_name(cookie.name.as_str()) {
                    summary
                        .foreign_cookie_names
                        .insert(cookie.name.as_str().to_owned());
                }
            }
            if let Some(host) = &observation.redirect_host {
                summary.redirects_offered = summary.redirects_offered.saturating_add(1);
                summary.redirect_hosts.insert(host.clone());
            }
        }
        summary
    }
}

// ---------------------------------------------------------------------------------------------
// Cookie ledger
// ---------------------------------------------------------------------------------------------

/// One cookie name, followed across the whole run.
#[derive(Clone, Debug)]
pub struct CookieLedgerEntry {
    pub name: String,
    pub dsm_cookie: bool,
    pub first_seen: CallRef,
    pub last_seen: CallRef,
    pub set_count: u32,
    /// Where the server sent this name under a value it had not sent before.
    pub rotations: Vec<Rotation>,
    pub cleared_at: Option<CallRef>,
    pub persistence: CookiePersistence,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: CookieSameSite,
    pub path_present: bool,
    pub domain_present: bool,
    pub value_length: u16,
    /// The attributes changed between two settings of the same name.
    pub attributes_changed: bool,
    /// Salted digests seen, in order of first appearance. Never values.
    pub fingerprints: Vec<u32>,
}

/// One occasion on which a cookie name came back under a changed value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rotation {
    pub at: CallRef,
    /// Whether the response that rotated it was itself successful.
    ///
    /// This is the discriminating case. A rotation on a *failed* response is ordinary: the server
    /// is refusing the session and clearing it. A rotation on a *successful* one means the server
    /// handed out a new identifier that this client, keeping no cookie jar, will never send back.
    pub on_success: bool,
}

impl CookieLedgerEntry {
    fn first(name: &str, at: CallRef, value_length: u16) -> Self {
        Self {
            name: name.to_owned(),
            dsm_cookie: api::is_dsm_cookie_name(name),
            first_seen: at,
            last_seen: at,
            set_count: 0,
            rotations: Vec::new(),
            cleared_at: None,
            persistence: CookiePersistence::Session,
            secure: false,
            http_only: false,
            same_site: CookieSameSite::Absent,
            path_present: false,
            domain_present: false,
            value_length,
            attributes_changed: false,
            fingerprints: Vec::new(),
        }
    }

    fn observe(
        &mut self,
        cookie: crate::observability::CookieFact,
        at: CallRef,
        outcome: RequestOutcome,
    ) {
        let previous_attributes = (
            self.persistence,
            self.secure,
            self.http_only,
            self.same_site,
            self.path_present,
            self.domain_present,
        );
        let known = self.fingerprints.contains(&cookie.fingerprint);
        if !known {
            // The first setting is not a rotation; every later *new* value is.
            if !self.fingerprints.is_empty() {
                self.rotations.push(Rotation {
                    at,
                    on_success: outcome == RequestOutcome::Ok,
                });
            }
            self.fingerprints.push(cookie.fingerprint);
        }
        self.set_count = self.set_count.saturating_add(1);
        self.last_seen = at;
        self.persistence = cookie.persistence;
        self.secure = cookie.secure;
        self.http_only = cookie.http_only;
        self.same_site = cookie.same_site;
        self.path_present = cookie.path_present;
        self.domain_present = cookie.domain_present;
        self.value_length = cookie.value_length;
        if cookie.persistence == CookiePersistence::Deletion {
            self.cleared_at.get_or_insert(at);
        }
        if self.set_count > 1
            && previous_attributes
                != (
                    self.persistence,
                    self.secure,
                    self.http_only,
                    self.same_site,
                    self.path_present,
                    self.domain_present,
                )
        {
            self.attributes_changed = true;
        }
    }

    /// A rotation that arrived on a response the server also reported as successful.
    pub fn rotation_on_success(&self) -> Option<Rotation> {
        self.rotations
            .iter()
            .copied()
            .find(|rotation| rotation.on_success)
    }

    /// `session` or `persistent`, in the words the operator asked the question in.
    pub fn persistence_description(&self) -> &'static str {
        match self.persistence {
            CookiePersistence::Session => {
                "a session cookie: no Expires or Max-Age, so it lives only as long as a client \
                 that stores it"
            }
            CookiePersistence::Persistent => {
                "a persistent cookie: Expires or Max-Age asks a client to store it on disk"
            }
            CookiePersistence::Deletion => {
                "a deletion: the server is clearing this cookie rather than setting one"
            }
        }
    }

    /// The attribute names present, as a compact list. Never an attribute value except SameSite,
    /// which is a fixed enum rather than server-chosen text.
    pub fn attribute_list(&self) -> String {
        let mut parts = Vec::new();
        if self.secure {
            parts.push("Secure".to_owned());
        }
        if self.http_only {
            parts.push("HttpOnly".to_owned());
        }
        if self.same_site != CookieSameSite::Absent {
            parts.push(format!("SameSite={}", self.same_site.as_str()));
        }
        if self.path_present {
            parts.push("Path".to_owned());
        }
        if self.domain_present {
            parts.push("Domain".to_owned());
        }
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join(", ")
        }
    }

    fn json_value(&self) -> Value {
        json!({
            "name": self.name,
            "dsm_cookie": self.dsm_cookie,
            "first_seen": self.first_seen.json_value(),
            "last_seen": self.last_seen.json_value(),
            "set_count": self.set_count,
            "distinct_values": self.fingerprints.len(),
            "fingerprints": self
                .fingerprints
                .iter()
                .map(|fingerprint| format!("{fingerprint:08x}"))
                .collect::<Vec<_>>(),
            "rotations": self
                .rotations
                .iter()
                .map(|rotation| json!({
                    "at": rotation.at.json_value(),
                    "on_success": rotation.on_success,
                }))
                .collect::<Vec<_>>(),
            "rotated_on_success": self.rotation_on_success().is_some(),
            "cleared_at": self.cleared_at.map(CallRef::json_value),
            "persistence": self.persistence.as_str(),
            "secure": self.secure,
            "http_only": self.http_only,
            "same_site": self.same_site.as_str(),
            "path_present": self.path_present,
            "domain_present": self.domain_present,
            "value_length": self.value_length,
            "attributes_changed": self.attributes_changed,
        })
    }
}

/// Every cookie the server set during the run, and what happened to it afterwards.
#[derive(Clone, Debug, Default)]
pub struct CookieLedger {
    /// Whether this client replays what a server sets. It does not, and that is load-bearing.
    pub client_maintains_cookie_jar: bool,
    pub entries: Vec<CookieLedgerEntry>,
    pub responses_setting_cookies: u32,
    pub undescribed_headers: u32,
}

impl CookieLedger {
    /// The one entry that would confirm a cookie-handling fault, if there is one.
    pub fn rotated_on_success(&self) -> Option<&CookieLedgerEntry> {
        self.entries
            .iter()
            .find(|entry| entry.rotation_on_success().is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.undescribed_headers == 0
    }

    pub fn json_value(&self) -> Value {
        json!({
            "client_maintains_cookie_jar": self.client_maintains_cookie_jar,
            "responses_setting_cookies": self.responses_setting_cookies,
            "undescribed_set_cookie_headers": self.undescribed_headers,
            "rotated_on_success": self.rotated_on_success().map(|entry| entry.name.clone()),
            "cookies": self
                .entries
                .iter()
                .map(CookieLedgerEntry::json_value)
                .collect::<Vec<_>>(),
        })
    }

    pub fn human_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.entries.is_empty() {
            lines.push("the server set no cookies at any point in this run".to_owned());
        }
        for entry in &self.entries {
            let mut line = format!(
                "{} ({}): set {} time{} ({} to {}), {} distinct value{}; {}",
                entry.name,
                if entry.dsm_cookie {
                    "a DSM cookie"
                } else {
                    "not a DSM cookie name"
                },
                entry.set_count,
                if entry.set_count == 1 { "" } else { "s" },
                entry.first_seen.describe(),
                entry.last_seen.describe(),
                entry.fingerprints.len(),
                if entry.fingerprints.len() == 1 {
                    ""
                } else {
                    "s"
                },
                entry.persistence_description(),
            );
            let _ = write!(line, "; attributes: {}", entry.attribute_list());
            lines.push(line);
            if let Some(rotation) = entry.rotation_on_success() {
                lines.push(format!(
                    "  ROTATED ON A SUCCESSFUL RESPONSE at {}: the server issued a new value for \
                     {} on a call it also reported as successful",
                    rotation.at.describe(),
                    entry.name,
                ));
            } else if entry.rotations.len() == 1 {
                lines.push(format!(
                    "  changed value once, at {}, on a call that did not succeed",
                    entry.rotations[0].at.describe()
                ));
            } else if entry.rotations.len() > 1 {
                lines.push(format!(
                    "  changed value {} times, none of them on a successful call",
                    entry.rotations.len()
                ));
            }
            if let Some(cleared) = entry.cleared_at {
                lines.push(format!("  cleared by the server at {}", cleared.describe()));
            }
            if entry.attributes_changed {
                lines.push(
                    "  its attributes were not the same on every setting, which a single \
                     consistent origin would not do"
                        .to_owned(),
                );
            }
        }
        if self.undescribed_headers > 0 {
            lines.push(format!(
                "{} Set-Cookie header(s) carried no parseable name and are counted but not \
                 described",
                self.undescribed_headers
            ));
        }
        lines.push(if self.client_maintains_cookie_jar {
            "this client keeps a cookie jar, so a cookie the server sets is sent back on later \
             requests"
                .to_owned()
        } else {
            "this client keeps no cookie jar: every Set-Cookie above was observed and discarded, \
             and the only cookie it ever sends is the id header it synthesises from the login SID"
                .to_owned()
        });
        lines
    }
}

// ---------------------------------------------------------------------------------------------
// Intermediary summary
// ---------------------------------------------------------------------------------------------

/// What sat between this client and DSM, across every response of the run.
#[derive(Clone, Debug, Default)]
pub struct IntermediarySummary {
    pub endpoint: EndpointClassification,
    pub responses_observed: u32,
    pub via: BTreeSet<String>,
    pub server: BTreeSet<String>,
    pub powered_by: BTreeSet<String>,
    pub cdn_markers: BTreeSet<CdnMarker>,
    pub forwarded_for_reflected: bool,
    pub real_ip_reflected: bool,
    pub foreign_cookie_names: BTreeSet<String>,
    pub redirects_offered: u32,
    pub redirect_hosts: BTreeSet<String>,
}

impl Default for EndpointClassification {
    fn default() -> Self {
        Self::plain(EndpointForm::PlainHost)
    }
}

impl IntermediarySummary {
    /// Whether responses came back under more than one `Server` banner.
    ///
    /// One banner is what a single origin produces. Two, within one short run, means two origins
    /// answered, which is the direct observation a session-affinity failure is made of.
    pub fn distinct_server_banners(&self) -> usize {
        self.server.len()
    }

    /// Whether anything at all was detected between this client and DSM.
    pub fn intermediary_detected(&self) -> bool {
        !self.via.is_empty()
            || !self.cdn_markers.is_empty()
            || self.forwarded_for_reflected
            || self.real_ip_reflected
            || !self.foreign_cookie_names.is_empty()
            || self.endpoint.form.is_relayed()
    }

    pub fn json_value(&self) -> Value {
        json!({
            "endpoint": self.endpoint.json_value(),
            "responses_observed": self.responses_observed,
            "via": self.via.iter().cloned().collect::<Vec<_>>(),
            "server": self.server.iter().cloned().collect::<Vec<_>>(),
            "powered_by": self.powered_by.iter().cloned().collect::<Vec<_>>(),
            "cdn_markers": self
                .cdn_markers
                .iter()
                .map(|marker| marker.as_str())
                .collect::<Vec<_>>(),
            "forwarded_for_reflected": self.forwarded_for_reflected,
            "real_ip_reflected": self.real_ip_reflected,
            "non_dsm_cookie_names": self.foreign_cookie_names.iter().cloned().collect::<Vec<_>>(),
            "redirects_offered": self.redirects_offered,
            "redirect_hosts": self.redirect_hosts.iter().cloned().collect::<Vec<_>>(),
            "distinct_server_banners": self.distinct_server_banners(),
            "intermediary_detected": self.intermediary_detected(),
        })
    }

    pub fn human_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("endpoint form: {}", self.endpoint.describe())];
        if self.responses_observed == 0 {
            // Absences are only evidence once something answered. Listing them here would read
            // as "nothing is in the way" when the truth is that nothing was seen at all.
            lines.push(
                "no response reached this client, so there was nothing to fingerprint".to_owned(),
            );
            return lines;
        }
        lines.push(format!(
            "{} response{} fingerprinted",
            self.responses_observed,
            if self.responses_observed == 1 {
                ""
            } else {
                "s"
            }
        ));
        if self.server.is_empty() {
            lines.push("no Server banner was returned on any response".to_owned());
        } else if self.server.len() == 1 {
            lines.push(format!(
                "Server banner: {} (the same on every response)",
                self.server.iter().next().cloned().unwrap_or_default()
            ));
        } else {
            lines.push(format!(
                "MORE THAN ONE Server banner across this run: {} -- responses came back from more \
                 than one origin, so consecutive requests did not all reach the same host",
                self.server.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.via.is_empty() {
            lines.push(format!(
                "Via chain: {}",
                self.via.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.powered_by.is_empty() {
            lines.push(format!(
                "X-Powered-By: {}",
                self.powered_by
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.cdn_markers.is_empty() {
            lines.push(format!(
                "cache or CDN markers: {}",
                self.cdn_markers
                    .iter()
                    .map(|marker| marker.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if self.forwarded_for_reflected || self.real_ip_reflected {
            lines.push(format!(
                "a proxy reflected client-address headers back in its responses ({}), so it is \
                 rewriting them on the way in",
                [
                    self.forwarded_for_reflected.then_some("X-Forwarded-For"),
                    self.real_ip_reflected.then_some("X-Real-IP"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" and ")
            ));
        }
        if self.foreign_cookie_names.is_empty() {
            lines.push(
                "no cookie was set under a name DSM does not use, so nothing on the path is \
                 pinning this client with an affinity cookie"
                    .to_owned(),
            );
        } else {
            lines.push(format!(
                "cookies set under non-DSM names: {} -- an intermediary is tracking this client \
                 itself, most likely for load-balancer affinity",
                self.foreign_cookie_names
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if self.redirects_offered > 0 {
            lines.push(format!(
                "{} redirect(s) were offered and refused by transport policy; target host(s): {}",
                self.redirects_offered,
                self.redirect_hosts
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            lines.push("no response offered a redirect".to_owned());
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{BoundedText, CookieFact, RequestTransport, ShortToken};

    fn call(
        api: &'static str,
        method: &'static str,
        outcome: RequestOutcome,
        cookies: CookieFacts,
    ) -> ApiCallDetail {
        let mut call = ApiCallDetail::started(
            api,
            method,
            2,
            BoundedText::sanitized("/webapi/entry.cgi"),
            RequestTransport::Form,
        );
        call.outcome = outcome;
        call.http_status = Some(200);
        call.cookies = cookies;
        call
    }

    fn session_cookie(name: &str, fingerprint: u32) -> CookieFact {
        CookieFact {
            name: ShortToken::sanitized(name),
            fingerprint,
            value_length: 43,
            persistence: CookiePersistence::Session,
            secure: true,
            http_only: true,
            same_site: CookieSameSite::Lax,
            path_present: true,
            domain_present: false,
            expires_present: false,
            max_age_present: false,
        }
    }

    fn facts(cookies: &[CookieFact]) -> CookieFacts {
        let mut facts = CookieFacts::default();
        for cookie in cookies {
            facts.push(*cookie);
        }
        facts
    }

    #[test]
    fn the_relay_and_direct_quickconnect_forms_are_told_apart() {
        // The host from the live report this check was built for.
        let relay = classify_endpoint("nascheckoffice.fr3.quickconnect.to");
        assert_eq!(relay.form, EndpointForm::QuickConnectRelay);
        assert_eq!(relay.relay_label.as_deref(), Some("fr3"));
        assert!(relay.form.is_relayed());

        for direct in [
            "nascheckoffice.direct.quickconnect.to",
            "203-0-113-7.nascheckoffice.direct.quickconnect.to",
        ] {
            let classification = classify_endpoint(direct);
            assert_eq!(
                classification.form,
                EndpointForm::QuickConnectDirect,
                "{direct} should be a direct form"
            );
            assert!(!classification.form.is_relayed());
        }

        // Case and a trailing root dot must not change the verdict.
        assert_eq!(
            classify_endpoint("NASCheckOffice.FR3.QuickConnect.to.").form,
            EndpointForm::QuickConnectRelay
        );
        // A bare alias under the apex matches neither documented form.
        assert_eq!(
            classify_endpoint("nascheckoffice.quickconnect.to").form,
            EndpointForm::QuickConnectUnrecognized
        );
        assert_eq!(
            classify_endpoint("nas.synology.me").form,
            EndpointForm::SynologyDdns
        );
        assert_eq!(
            classify_endpoint("192.168.1.10").form,
            EndpointForm::AddressLiteral
        );
        assert_eq!(
            classify_endpoint("[::1]").form,
            EndpointForm::AddressLiteral
        );
        assert_eq!(
            classify_endpoint("nas.example.test").form,
            EndpointForm::PlainHost
        );
    }

    #[test]
    fn a_changed_value_on_a_successful_call_is_recorded_as_a_rotation() {
        let mut transcript = TransportTranscript::default();
        transcript.record_call(&call(
            "SYNO.API.Auth",
            "login",
            RequestOutcome::Ok,
            facts(&[session_cookie("id", 0x1111_1111)]),
        ));
        transcript.record_call(&call(
            "SYNO.FileStation.List",
            "list_share",
            RequestOutcome::Ok,
            CookieFacts::default(),
        ));
        transcript.record_call(&call(
            "SYNO.FileStation.List",
            "getinfo",
            RequestOutcome::Ok,
            facts(&[session_cookie("id", 0x2222_2222)]),
        ));

        let ledger = transcript.cookie_ledger();
        let entry = ledger
            .rotated_on_success()
            .expect("a rotation was recorded");
        assert_eq!(entry.name, "id");
        assert_eq!(entry.set_count, 2);
        assert_eq!(entry.fingerprints.len(), 2);
        let rotation = entry.rotation_on_success().expect("rotation on success");
        assert_eq!(rotation.at.sequence, 3);
        assert_eq!(rotation.at.method, "getinfo");
        assert!(!ledger.client_maintains_cookie_jar);

        let rendered = ledger.human_lines().join("\n");
        assert!(rendered.contains("ROTATED ON A SUCCESSFUL RESPONSE"));
    }

    #[test]
    fn resending_the_same_value_is_not_a_rotation() {
        let mut transcript = TransportTranscript::default();
        for _ in 0..3 {
            transcript.record_call(&call(
                "SYNO.API.Auth",
                "login",
                RequestOutcome::Ok,
                facts(&[session_cookie("id", 0x1111_1111)]),
            ));
        }
        let ledger = transcript.cookie_ledger();
        assert!(ledger.rotated_on_success().is_none());
        let entry = &ledger.entries[0];
        assert_eq!(entry.set_count, 3);
        assert_eq!(entry.fingerprints.len(), 1);
        assert!(entry.rotations.is_empty());
    }

    #[test]
    fn a_rotation_on_a_failed_call_is_not_reported_as_one_on_a_successful_call() {
        let mut transcript = TransportTranscript::default();
        transcript.record_call(&call(
            "SYNO.API.Auth",
            "login",
            RequestOutcome::Ok,
            facts(&[session_cookie("id", 0x1111_1111)]),
        ));
        transcript.record_call(&call(
            "SYNO.FileStation.List",
            "getinfo",
            RequestOutcome::DsmError,
            facts(&[session_cookie("id", 0x3333_3333)]),
        ));
        let ledger = transcript.cookie_ledger();
        assert!(ledger.rotated_on_success().is_none());
        assert_eq!(ledger.entries[0].rotations.len(), 1);
        assert!(!ledger.entries[0].rotations[0].on_success);
    }

    #[test]
    fn session_and_persistent_cookies_are_classified_apart() {
        let mut persistent = session_cookie("stay_login", 0x4444_4444);
        persistent.persistence = CookiePersistence::Persistent;
        persistent.expires_present = true;
        let mut cleared = session_cookie("id", 0x5555_5555);
        cleared.persistence = CookiePersistence::Deletion;
        cleared.max_age_present = true;

        let mut transcript = TransportTranscript::default();
        transcript.record_call(&call(
            "SYNO.API.Auth",
            "login",
            RequestOutcome::Ok,
            facts(&[session_cookie("id", 0x1111_1111), persistent]),
        ));
        transcript.record_call(&call(
            "SYNO.API.Auth",
            "logout",
            RequestOutcome::Ok,
            facts(&[cleared]),
        ));

        let ledger = transcript.cookie_ledger();
        let id = ledger
            .entries
            .iter()
            .find(|entry| entry.name == "id")
            .expect("the id cookie");
        assert_eq!(id.persistence, CookiePersistence::Deletion);
        assert_eq!(id.cleared_at.map(|at| at.sequence), Some(2));
        assert!(id.persistence_description().contains("clearing"));

        let stay = ledger
            .entries
            .iter()
            .find(|entry| entry.name == "stay_login")
            .expect("the stay_login cookie");
        assert_eq!(stay.persistence, CookiePersistence::Persistent);
        assert!(stay.persistence_description().contains("persistent cookie"));
        assert!(stay.attribute_list().contains("HttpOnly"));
        assert!(stay.attribute_list().contains("SameSite=lax"));
        // Order of first appearance is preserved, so the report reads in the order it happened.
        assert_eq!(
            ledger
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "stay_login"]
        );
    }

    #[test]
    fn a_non_dsm_cookie_name_is_surfaced_as_an_intermediary_fingerprint() {
        let mut affinity = session_cookie("AWSALB", 0x6666_6666);
        affinity.persistence = CookiePersistence::Persistent;
        let mut transcript = TransportTranscript::default();
        let mut detail = call(
            "SYNO.API.Info",
            "query",
            RequestOutcome::Ok,
            facts(&[session_cookie("id", 0x1111_1111), affinity]),
        );
        detail.intermediary = IntermediaryFacts {
            via: ShortToken::sanitized("1.1 relay"),
            server: ShortToken::sanitized("nginx"),
            forwarded_for_reflected: true,
            cdn_marker: CdnMarker::Cloudflare,
            foreign_cookie_count: 1,
            ..IntermediaryFacts::default()
        };
        transcript.record_call(&detail);

        let summary = transcript
            .intermediary_summary(classify_endpoint("nascheckoffice.fr3.quickconnect.to"));
        assert!(summary.intermediary_detected());
        assert!(summary.foreign_cookie_names.contains("AWSALB"));
        assert!(!summary.foreign_cookie_names.contains("id"));
        assert_eq!(summary.distinct_server_banners(), 1);
        let rendered = summary.human_lines().join("\n");
        assert!(rendered.contains("AWSALB"));
        assert!(rendered.contains("relay id fr3"));
    }

    #[test]
    fn two_server_banners_in_one_run_are_called_out() {
        let mut transcript = TransportTranscript::default();
        for banner in ["nginx", "Apache"] {
            let mut detail = call(
                "SYNO.FileStation.List",
                "getinfo",
                RequestOutcome::Ok,
                CookieFacts::default(),
            );
            detail.intermediary = IntermediaryFacts {
                server: ShortToken::sanitized(banner),
                ..IntermediaryFacts::default()
            };
            transcript.record_call(&detail);
        }
        let summary = transcript.intermediary_summary(classify_endpoint("nas.example.test"));
        assert_eq!(summary.distinct_server_banners(), 2);
        assert!(
            summary
                .human_lines()
                .join("\n")
                .contains("MORE THAN ONE Server banner")
        );
    }

    #[test]
    fn latency_summaries_report_only_what_the_samples_support() {
        let empty = LatencySamples::default();
        assert_eq!(empty.describe(), "no samples");
        assert_eq!(empty.median_micros(), None);
        assert!(!empty.is_widely_spread());

        let mut even = LatencySamples::default();
        for micros in [10_000, 20_000, 30_000, 40_000] {
            even.push(Duration::from_micros(micros));
        }
        assert_eq!(even.min_micros(), Some(10_000));
        assert_eq!(even.max_micros(), Some(40_000));
        // Four samples average the two central values rather than picking a side.
        assert_eq!(even.median_micros(), Some(25_000));
        assert_eq!(even.mean_micros(), Some(25_000));
        assert_eq!(even.spread_micros(), Some(30_000));
        assert!(even.is_widely_spread());

        // Consistent samples, however slow, are not a split path.
        let mut consistent = LatencySamples::default();
        for micros in [200_000, 205_000, 210_000] {
            consistent.push(Duration::from_micros(micros));
        }
        assert!(!consistent.is_widely_spread());

        // Fast but proportionally noisy samples are not either: the floor rules them out.
        let mut noisy = LatencySamples::default();
        for micros in [500, 900, 4_000] {
            noisy.push(Duration::from_micros(micros));
        }
        assert!(!noisy.is_widely_spread());
        assert_eq!(format_micros(1_500), "1.5 ms");
    }

    #[test]
    fn a_probe_response_joins_the_same_transcript_as_an_api_call() {
        let mut transcript = TransportTranscript::default();
        transcript.record_probe(ProbeObservation {
            http_status: Some(200),
            cookies: facts(&[session_cookie("id", 0x7777_7777)]),
            intermediary: IntermediaryFacts::default(),
            redirect_host: Some("relay.example.test".to_owned()),
        });
        let sequence = transcript.record_call(&call(
            "SYNO.API.Auth",
            "login",
            RequestOutcome::Ok,
            CookieFacts::default(),
        ));
        assert_eq!(sequence, 2);
        let ledger = transcript.cookie_ledger();
        assert_eq!(ledger.entries[0].first_seen.sequence, 1);
        assert_eq!(ledger.entries[0].first_seen.api, PROBE_API);
        let summary = transcript.intermediary_summary(EndpointClassification::default());
        assert_eq!(summary.redirects_offered, 1);
        assert!(summary.redirect_hosts.contains("relay.example.test"));
    }

    /// A minimal HTTP server for the probe: answers each connection with one canned status.
    ///
    /// `answers` is consumed in order and each entry names the status and body for one request.
    /// A connection arriving after the list is exhausted is accepted and left unanswered, which
    /// is what the timing probe experiences as a stall.
    fn probe_server(answers: Vec<u16>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("probe listener");
        let url = format!("http://{}/", listener.local_addr().expect("probe address"));
        let handle = std::thread::spawn(move || {
            let mut routes = Vec::new();
            // Held so a stalled connection stays open instead of being closed by its own drop;
            // a closed socket is a different fault from a server that never answers.
            let mut stalled = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().expect("probe stream clone"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    break;
                }
                // The TCP timing samples connect and close without sending anything. They are
                // not requests and must not consume an answer.
                if request_line.trim().is_empty() {
                    continue;
                }
                routes.push(request_line.trim().to_owned());
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
                    header.clear();
                }
                let Some(status) = answers.get(routes.len() - 1).copied() else {
                    stalled.push(stream);
                    continue;
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                );
                let _ = stream.flush();
                // Every scripted answer has been given, so the thread ends and can be joined. A
                // server with no answers at all is the stalling one and never reaches here.
                if routes.len() >= answers.len() {
                    break;
                }
            }
            routes
        });
        (url, handle)
    }

    fn probe_options(base_url: String) -> ClientOptions {
        ClientOptions {
            base_url,
            allow_http: true,
            accept_invalid_certs: false,
            ca_certificate: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            retries: 0,
        }
    }

    /// The probe must time the route API discovery actually uses, and follow discovery's own
    /// fallback when the first one is not served.
    ///
    /// Pinning the probe to `query.cgi` measured a route the client need never open: discovery
    /// tries `entry.cgi` first, so a host serving only that one made the probe characterise a
    /// handler that does not exist there.
    #[test]
    fn the_probe_follows_the_same_cgi_ladder_as_api_discovery() {
        // `entry.cgi` answers, so the fallback is never reached.
        let (url, server) = probe_server(vec![200]);
        let budget = ReachabilityBudget {
            tcp_samples: 1,
            http_samples: 1,
            total: Duration::from_secs(5),
            http_request_timeout: Duration::from_secs(3),
        };
        let (report, observations) = measure_reachability(
            &probe_options(url),
            budget,
            &crate::cancel::CancellationToken::default(),
        );
        assert_eq!(report.http.route, Some("entry.cgi"));
        assert_eq!(report.http.routes_rejected, 0);
        assert_eq!(report.http.request_timeout, Some(Duration::from_secs(3)));
        assert_eq!(observations.len(), 1);
        drop(server);

        // `entry.cgi` answers 404, so the probe moves to the route discovery would move to and
        // does not spend a sample characterising the 404 handler.
        let (url, server) = probe_server(vec![404, 200]);
        let (report, observations) = measure_reachability(
            &probe_options(url),
            budget,
            &crate::cancel::CancellationToken::default(),
        );
        assert_eq!(report.http.route, Some("query.cgi"));
        assert_eq!(report.http.routes_rejected, 1);
        assert_eq!(
            report.http.statuses.iter().copied().collect::<Vec<_>>(),
            vec![200],
            "the rejected route must not pollute the measured statuses"
        );
        assert_eq!(observations.len(), 1);
        let requests = server.join().expect("probe server");
        assert!(
            requests[0].contains("/webapi/entry.cgi"),
            "unexpected first probe request: {requests:?}"
        );
        assert!(
            requests[1].contains("/webapi/query.cgi"),
            "unexpected fallback request: {requests:?}"
        );
    }

    /// A probe that gives up before the host answers must say that is what happened.
    ///
    /// Against a QuickConnect relay the client's own discovery request takes around 3.8 seconds,
    /// and the four-second ceiling the probe once hard-coded expired on it -- reporting "no HTTP
    /// sample completed" with a reqwest message whose `Display` hides the timeout in its source
    /// chain, for a host the very next section authenticated against successfully.
    #[test]
    fn a_probe_timeout_is_reported_as_a_timeout_and_names_its_cause() {
        let (url, _server) = probe_server(Vec::new());
        let budget = ReachabilityBudget {
            tcp_samples: 1,
            http_samples: 2,
            total: Duration::from_secs(10),
            http_request_timeout: Duration::from_millis(250),
        };
        let (report, observations) = measure_reachability(
            &probe_options(url),
            budget,
            &crate::cancel::CancellationToken::default(),
        );
        assert!(report.reached(), "TCP connects; only HTTP does not answer");
        assert!(report.connects_but_does_not_answer());
        assert!(report.http.timed_out, "the failure was a timeout");
        let reason = report
            .http
            .failure_reason
            .clone()
            .expect("a failure reason");
        assert!(
            reason.contains("timed out") || reason.contains("timeout"),
            "the cause chain must survive into the reason: {reason}"
        );
        assert!(observations.is_empty());
        let lines = report.human_lines().join("\n");
        assert!(lines.contains("abandoned at the per-sample ceiling, not refused"));
        assert!(lines.contains("probe route: webapi/entry.cgi"));
    }

    /// The cause chain is what distinguishes one transport failure from another, and it is bounded.
    #[test]
    fn a_failure_reason_carries_the_cause_chain_within_a_bound() {
        #[derive(Debug)]
        struct Layer(String, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1
                    .as_deref()
                    .map(|layer| layer as &dyn std::error::Error)
            }
        }
        let layer = |text: &str, inner: Option<Layer>| Layer(text.to_owned(), inner.map(Box::new));

        // The shape reqwest produces for a probe timeout: the distinguishing text is one level
        // down, where `Display` alone never reaches it.
        let timed_out = layer(
            "error sending request for url (https://host/webapi/entry.cgi)",
            Some(layer("operation timed out", None)),
        );
        assert_eq!(
            cause_chain_reason(&timed_out),
            "error sending request for url (https://host/webapi/entry.cgi): operation timed out"
        );

        // A layer that only repeats its cause adds nothing and is not repeated.
        let echoing = layer(
            "operation timed out",
            Some(layer("operation timed out", None)),
        );
        assert_eq!(cause_chain_reason(&echoing), "operation timed out");

        // A pathological chain is clipped, visibly, rather than filling the report.
        let mut deep = layer("z", None);
        for index in 0..60 {
            deep = layer(&format!("layer-{index:03}-padding"), Some(deep));
        }
        let reason = cause_chain_reason(&deep);
        assert_eq!(reason.chars().count(), MAX_FAILURE_REASON_CHARS);
        assert!(reason.ends_with('~'), "a clipped reason says so: {reason}");
    }

    /// Every level's per-sample ceiling has to outlast a relayed round trip, which is the case
    /// the probe most needs to measure and the one it used to give up on.
    #[test]
    fn every_budget_waits_longer_than_a_relayed_round_trip() {
        // The measured figure from the run that exposed this: a QuickConnect relay answered the
        // unauthenticated discovery request in 3.788 s.
        let observed_relay = Duration::from_millis(3_788);
        for budget in [
            ReachabilityBudget::quick(),
            ReachabilityBudget::standard(),
            ReachabilityBudget::extensive(),
        ] {
            assert!(
                budget.http_request_timeout > observed_relay,
                "a ceiling of {:?} expires on a relay that answers in {observed_relay:?}",
                budget.http_request_timeout
            );
            assert!(budget.http_samples > 0);
        }
    }

    /// The HTTP sample count is a floor, not a preference.
    ///
    /// `is_widely_spread` cannot fire below three samples, so a level that means to report an
    /// HTTP-spread finding has to take at least three. This binds the budgets to the predicate
    /// rather than to a number someone typed: trim `extensive` to 2 and the finding becomes
    /// unreachable, and the section would report a consistent path because it never had the
    /// samples to say otherwise -- a silent loss no timing test would catch.
    ///
    /// Quick is deliberately exempt. It is the level an operator reaches for when something is
    /// already wrong, and it leans on DNS address count and TCP spread instead.
    #[test]
    fn levels_that_report_http_spread_take_enough_samples_to_reach_the_verdict() {
        // Derived here rather than asserted from memory: two samples as far apart as the
        // predicate's own floor and ratio allow still does not qualify, and a third identical
        // sample tips it. Whatever `is_widely_spread` requires, this reads it back.
        let mut two = LatencySamples::default();
        two.push(Duration::from_millis(10));
        two.push(Duration::from_millis(100));
        assert!(
            !two.is_widely_spread(),
            "two samples cannot establish a spread however far apart they are"
        );
        let mut three = two.clone();
        three.push(Duration::from_millis(100));
        assert!(
            three.is_widely_spread(),
            "three samples over the floor and ratio is the point at which the finding fires"
        );
        let spread_floor = three.len();

        for (level, budget) in [
            ("standard", ReachabilityBudget::standard()),
            ("extensive", ReachabilityBudget::extensive()),
        ] {
            assert!(
                budget.http_samples >= spread_floor,
                "{level} takes {} HTTP samples, below the {spread_floor} `is_widely_spread` \
                 needs; the level would report a consistent path it never measured",
                budget.http_samples
            );
        }
    }
}
