use std::path::PathBuf;

use reqwest::StatusCode;
use serde_json::Value;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid reverse-proxy URL: {0}")]
    InvalidUrl(String),

    #[error(
        "reverse-proxy URL must use HTTPS (pass --allow-http only for a trusted test/LAN endpoint)"
    )]
    HttpsRequired,

    #[error("unsafe remote path {path:?}: {reason}")]
    UnsafeRemotePath { path: String, reason: String },

    #[error("local source is not a readable directory: {0:?}")]
    InvalidSource(PathBuf),

    #[error("unsupported local entry {path:?}: {reason}")]
    UnsupportedLocalEntry { path: PathBuf, reason: String },

    #[error("failed to read {path:?}: {source}")]
    FileIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    // The cause chain, not just reqwest's outer layer: `error sending request for
    // url (...)` alone hides whether it timed out, was refused or failed TLS.
    #[error("HTTP request failed during {operation}: {}", cause_chain(source))]
    Http {
        operation: String,
        #[source]
        source: reqwest::Error,
    },

    /// A body failed in transit: a response body that could not be read, or an upload attempt
    /// one of its bounds ended (a stall, the ceiling, no answer after the last byte), named in
    /// `source`.
    #[error("HTTP transfer failed during {operation}: {source}")]
    HttpBody {
        operation: String,
        #[source]
        source: std::io::Error,
    },

    #[error("reverse proxy returned HTTP {status} during {operation}: {message}")]
    HttpStatus {
        operation: String,
        status: StatusCode,
        message: String,
    },

    #[error("unexpected response during {operation}: {message}")]
    InvalidResponse { operation: String, message: String },

    #[error("Synology API {api}.{operation} failed with code {code}{description}")]
    Api {
        api: String,
        operation: String,
        code: i64,
        description: String,
        details: Vec<Value>,
    },

    #[error(
        "required Synology API {api} version {version} is not available (server offers {min}..={max})"
    )]
    UnsupportedApiVersion {
        api: String,
        version: u32,
        min: u32,
        max: u32,
    },

    /// DSM refused one operation for a reason confined to the path it named: the name (too long,
    /// illegal), this account's permissions on that path, its size for the destination
    /// filesystem, or a clash with an entry already there. Every other path is unaffected, so the
    /// caller defers this one and carries on, rather than failing the run on every run.
    #[error("NAS refused {operation} of {path:?} with code {code}{description}")]
    RemoteRefused {
        operation: String,
        path: String,
        code: i64,
        description: String,
    },

    /// A folder on the requested scope, between the destination and the scope or the scope
    /// itself, is a NAS link: DSM reports a real path for it that is not where it is listed. A
    /// scoped run, plan or resync of it would write and delete wherever the link points, though an
    /// unscoped run refuses to follow it (G-1). Refused by name before anything is planned; a link
    /// strictly beneath the scope is protected by the walk as in an unscoped run.
    #[error(
        "remote path {path:?} on the requested scope is a NAS link ({cause}); a scoped run never \
         writes or deletes through a link: run without --scope, or replace the link with a folder"
    )]
    RemoteLinkInScope { path: String, cause: String },

    /// Most of what a run planned to write was refused by DSM for one class of cause, so the
    /// cause is the share or the account rather than the paths. Every refused path is still named
    /// as its own deferral; this names the class, what DSM answered, and a sample.
    #[error(
        "remote-refused-share-wide: {}: {refused} of the {considered} folders and files this run \
         wrote or found in sync (including {in_sync} already in sync) were refused for one cause \
         ({}); {not_attempted} planned writes were not attempted; the share or this account's \
         rights on it are the likely problem; paths: {}{}",
        .class.as_str(),
        join_refusal_codes(.codes),
        .sample.join(", "),
        and_more(*.more)
    )]
    RemoteRefusalsShareWide {
        class: RefusalClass,
        refused: usize,
        considered: usize,
        /// Entries already in sync, counted in `considered` (BK-1).
        in_sync: usize,
        not_attempted: usize,
        /// `operation:code` with how many refusals answered it, e.g. `SYNO.FileStation.Upload.upload:418`.
        codes: Vec<(String, usize)>,
        /// At most ten of the refused paths.
        sample: Vec<String>,
        /// How many refused paths `sample` leaves out.
        more: usize,
    },

    /// Most entries of one kind that the link guard checked look like links to somewhere else,
    /// which says DSM reports real paths in a shape this tool does not expect rather than that
    /// they are real links (G-3). Nothing was synced; acting on it would skip the tree entry by
    /// entry.
    #[error(
        "remote-real-path-unusable: {linked} of the {checked} NAS {kind} checked look like links \
         to somewhere else, which points to DSM reporting real paths in a shape this tool does not \
         expect rather than to real links; nothing was synced. Exclude the links by name if they \
         are real; {kind}: {}{}",
        .sample.join(", "),
        and_more(*.more)
    )]
    RemoteRealPathUnusable {
        /// `"folders"` or `"files"`.
        kind: &'static str,
        linked: usize,
        checked: usize,
        /// At most ten of the entries that look like links.
        sample: Vec<String>,
        /// How many `sample` leaves out.
        more: usize,
    },

    #[error("required Synology API {0} was not reported by API discovery")]
    MissingApi(String),

    #[error("DSM shared folder /{0} is unavailable or not writable by this account")]
    ShareNotWritable(String),

    #[error("remote path returned by File Station escaped the configured destination: {0:?}")]
    RemoteEscape(String),

    #[error(
        "remote destination {path:?} is a File Station mount point ({mount_type:?}); mounted filesystems are never synchronized"
    )]
    RemoteMountRoot { path: String, mount_type: String },

    #[error(
        "local/remote type conflict at {path}; rerun with --delete to replace the remote {remote_kind} with a local {local_kind}"
    )]
    TypeConflict {
        path: String,
        local_kind: &'static str,
        remote_kind: &'static str,
    },

    #[error(
        "cannot modify remote directory {0} because it contains an excluded, DSM-managed, or mounted path"
    )]
    ProtectedConflict(String),

    #[error(
        "refusing mirror deletion from a local source with no payload files; pass --allow-empty-source if intentional"
    )]
    EmptySourceDeletion,

    #[error(
        "plan would delete {planned} remote entries, exceeding --max-delete {maximum}; inspect --dry-run and raise the limit explicitly"
    )]
    DeleteLimit { planned: usize, maximum: usize },

    #[error("source file changed while it was being synchronized: {0:?}")]
    SourceChanged(PathBuf),

    /// A resync confirmation named a set of files that is no longer what would be overwritten.
    ///
    /// Not a dead end: the caller is shown the new overwrite list and a new ticket alongside this,
    /// so a destination that changes under them is one step to resolve rather than a loop.
    #[error(
        "resync ticket {presented} no longer describes what would be overwritten (now {current}); \
         review the refreshed plan below and confirm it instead"
    )]
    ResyncTicketStale { presented: String, current: String },

    /// A `--scope` argument named a relative path that exists on neither side.
    ///
    /// Distinct from a generic failure on purpose: a caller that asked about one folder or file
    /// needs to tell "that path is not there" apart from "the query failed", and callers render
    /// the two very differently.
    #[error("scope {0:?} does not exist in the local source or under the remote destination")]
    ScopeNotFound(String),

    #[error("DSM returned an invalid MD5 content digest")]
    InvalidContentHash,

    #[error("remote content verification failed for {0:?}")]
    ContentVerificationFailed(String),

    #[error("remote path {0:?} changed since planning; it was preserved")]
    RemoteSnapshotChanged(String),

    #[error(
        "server-side copy was rejected before a task was created; verified upload fallback is safe"
    )]
    ServerCopyNotStarted,

    /// Remote checksums kept failing with one cause, so the cause is the NAS or the link to it
    /// rather than the files. Raised once, naming the files, instead of deferring every file with a
    /// timeout each.
    #[error(
        "{} remote checksums failed with the same cause, so the NAS rather than these files is the likely problem: {cause}; files: {}",
        paths.len(),
        paths.join(", ")
    )]
    RemoteDigestsFailing {
        cause: Box<Error>,
        paths: Vec<String>,
    },

    /// DSM finished a server-side copy task and reported that it failed.
    #[error("server-side copy to {destination:?} finished with an error: {detail}")]
    ServerCopyFailed { destination: String, detail: String },

    #[error("{operation} did not finish within the configured request timeout")]
    OperationTimedOut { operation: &'static str },

    /// The rebuilt plan still held work no change during the run explains. Only those operations
    /// are counted, and the first [`RECONCILIATION_PENDING_NAMED`] are named with their action and
    /// cause, so the result record, the log and the run's failure line say which path and why.
    #[error(
        "final reconciliation did not converge on {operations} in-scope path(s): {}{}",
        pending_list(named),
        and_more(operations.saturating_sub(named.len()))
    )]
    ReconciliationPending {
        operations: usize,
        named: Vec<PendingOperation>,
    },

    #[error("operation cancelled")]
    Cancelled,

    #[error("OS credential vault {operation} failed: {reason}")]
    Vault {
        operation: &'static str,
        reason: &'static str,
    },

    /// Invalid non-secret CLI configuration. The binary maps this class to usage exit 2;
    /// transport, DSM, filesystem, vault, and delivery failures remain operational exit 1.
    #[error("{0}")]
    Configuration(String),

    #[error("{0}")]
    Message(String),
}

impl Error {
    pub fn api_code(&self) -> Option<i64> {
        match self {
            Self::Api { code, .. } | Self::RemoteRefused { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// The remote path DSM refused, when this is a refusal confined to that one path.
    pub fn refused_remote_path(&self) -> Option<&str> {
        match self {
            Self::RemoteRefused { path, .. } => Some(path),
            _ => None,
        }
    }

    /// For a refusal of one path, the class of cause it shares with other refusals, whatever the
    /// operation: a permission refusal of a folder create and of an upload are one cause (G-5).
    ///
    /// Every refusal has a class. A code with no class of its own is `CreateFailed`, which can
    /// trip the share-wide breaker like any other: a refusal that counted towards nothing would
    /// let a share-wide condition end a run green.
    pub fn refusal_class(&self) -> Option<RefusalClass> {
        match self {
            Self::RemoteRefused { code, .. } => Some(RefusalClass::of_code(*code)),
            _ => None,
        }
    }

    /// For a refusal of one path, the operation and DSM's code
    /// (`"SYNO.FileStation.Upload.upload:418"`): what a share-wide failure reports DSM answered.
    /// Refusals are counted by [`Self::refusal_class`]; this only says what they were.
    pub fn refusal_cause_key(&self) -> Option<String> {
        match self {
            Self::RemoteRefused {
                operation, code, ..
            } => Some(format!("{operation}:{code}")),
            _ => None,
        }
    }

    /// Whether DSM refused the session rather than the request: an expired, invalid or displaced
    /// session (106, 107, 119), or a re-login that itself failed. Such a failure concerns every
    /// request after it, so it must never be recorded as a refusal of one entry.
    pub fn is_session_failure(&self) -> bool {
        match self {
            Self::Api { api, code, .. } => {
                api == "SYNO.API.Auth" || matches!(*code, 106 | 107 | 119)
            }
            _ => false,
        }
    }
}

/// An error with every distinct cause in its source chain, the innermost first:
/// `innermost (outer: inner)`.
///
/// `reqwest` puts what distinguishes one transport failure from another
/// (`operation timed out`, `Connection refused`, a TLS verification message) in
/// the source chain, which its `Display` drops; a hung server then read as an
/// unexplained "error sending request". The innermost cause leads because the
/// records that carry this line keep only its head (`run.state` and Activity
/// keep 160 bytes), and the outer layer starts with a URL of any length. The
/// chain names no credential: it is the transport's own text, and every WebAPI
/// call carries its fields in a POST body, never in the URL.
pub fn cause_chain(error: &dyn std::error::Error) -> String {
    let mut layers = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !cause_text.is_empty() && !layers.iter().any(|layer| layer.contains(&cause_text)) {
            layers.push(cause_text);
        }
        source = cause.source();
    }
    match layers.split_last() {
        Some((innermost, outer)) if !outer.is_empty() => {
            format!("{innermost} ({})", outer.join(": "))
        }
        _ => layers.concat(),
    }
}

fn join_refusal_codes(codes: &[(String, usize)]) -> String {
    codes
        .iter()
        .map(|(code, count)| format!("{code} x{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How many unconverged operations a [`Error::ReconciliationPending`] names; the rest are counted.
pub const RECONCILIATION_PENDING_NAMED: usize = 5;

/// One operation the rebuilt plan still held that nothing excused: what it was, where, and why it
/// counts as not converged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperation {
    /// `upload`, `copy`, `create`, `delete`, `type-conflict-delete` or `rename`.
    pub action: &'static str,
    /// Root-relative, `/`-separated.
    pub relative: String,
    pub cause: &'static str,
}

/// The named operations, each path percent-encoded and bounded so a name cannot forge a second
/// line or field of the record that carries this text.
fn pending_list(named: &[PendingOperation]) -> String {
    named
        .iter()
        .map(|operation| {
            // The source root itself is `.`, as every other record names it.
            let relative = match operation.relative.as_str() {
                "" => ".",
                relative => relative,
            };
            format!(
                "{} {} ({})",
                operation.action,
                crate::observability::InlineAscii::<120>::percent_encoded(relative).as_str(),
                operation.cause
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn and_more(more: usize) -> String {
    if more > 0 {
        format!(", and {more} more")
    } else {
        String::new()
    }
}

/// The class of cause a refusal of one path belongs to. See [`Error::refusal_class`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum RefusalClass {
    /// This account may not do it: 407, or 105 once the session was proven alive.
    Permission,
    /// The name or path: 400, 418, 412, 413 (inside a 1100) and 1802.
    Name,
    /// Too large for the destination filesystem: 1804.
    Size,
    /// Something is already there: 414.
    Clash,
    /// The parent folder holds as many items as DSM allows: 1101.
    FolderLimit,
    /// Folder creation failed for no recognised reason (1100), or any per-path code without a
    /// class of its own.
    CreateFailed,
}

impl RefusalClass {
    pub fn of_code(code: i64) -> Self {
        match code {
            105 | 407 => Self::Permission,
            400 | 412 | 413 | 418 | 1802 => Self::Name,
            1804 => Self::Size,
            414 => Self::Clash,
            1101 => Self::FolderLimit,
            _ => Self::CreateFailed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Name => "name",
            Self::Size => "size",
            Self::Clash => "clash",
            Self::FolderLimit => "folder-limit",
            Self::CreateFailed => "create-failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(api: &str, code: i64) -> Error {
        Error::Api {
            api: api.to_owned(),
            operation: "delete".to_owned(),
            code,
            description: String::new(),
            details: Vec::new(),
        }
    }

    /// ADV-R-8: a dead session or a failed re-login concerns every request after it.
    #[test]
    fn session_failures_are_told_apart_from_refusals_of_one_entry() {
        for code in [106, 107, 119] {
            assert!(
                api("SYNO.FileStation.Delete", code).is_session_failure(),
                "{code}"
            );
        }
        assert!(api("SYNO.API.Auth", 400).is_session_failure());
        for code in [105, 400, 407, 408, 900] {
            assert!(
                !api("SYNO.FileStation.Delete", code).is_session_failure(),
                "{code}"
            );
        }
        assert!(!Error::Cancelled.is_session_failure());
    }

    #[test]
    fn a_remote_refusal_names_its_path_and_keeps_its_code() {
        let error = Error::RemoteRefused {
            operation: "SYNO.FileStation.Upload.upload".to_owned(),
            path: "/share/日本.txt".to_owned(),
            code: 418,
            description: ": illegal remote name or path".to_owned(),
        };
        assert_eq!(error.api_code(), Some(418));
        assert_eq!(error.refused_remote_path(), Some("/share/日本.txt"));
        assert_eq!(
            error.to_string(),
            "NAS refused SYNO.FileStation.Upload.upload of \"/share/日本.txt\" with code 418: illegal remote name or path"
        );
        assert!(!error.is_session_failure());
        assert_eq!(
            error.refusal_cause_key().as_deref(),
            Some("SYNO.FileStation.Upload.upload:418")
        );
        assert_eq!(Error::Cancelled.refusal_cause_key(), None);
        assert_eq!(error.refusal_class(), Some(RefusalClass::Name));
        assert_eq!(Error::Cancelled.refusal_class(), None);
    }

    /// G-5: refusals count by class, whatever the operation, and every code has one.
    #[test]
    fn refusals_are_classed_by_cause_not_by_operation() {
        let refused = |operation: &str, code| Error::RemoteRefused {
            operation: operation.to_owned(),
            path: "/share/x".to_owned(),
            code,
            description: String::new(),
        };
        assert_eq!(
            refused("SYNO.FileStation.CreateFolder.create", 407).refusal_class(),
            refused("SYNO.FileStation.Upload.upload", 105).refusal_class()
        );
        for (code, class) in [
            (407, RefusalClass::Permission),
            (105, RefusalClass::Permission),
            (400, RefusalClass::Name),
            (418, RefusalClass::Name),
            (1802, RefusalClass::Name),
            (1804, RefusalClass::Size),
            (414, RefusalClass::Clash),
            (1101, RefusalClass::FolderLimit),
            (1100, RefusalClass::CreateFailed),
            (1999, RefusalClass::CreateFailed),
        ] {
            assert_eq!(RefusalClass::of_code(code), class, "{code}");
        }
    }

    #[test]
    fn a_share_wide_refusal_leads_with_its_token_and_names_the_cause() {
        let error = Error::RemoteRefusalsShareWide {
            class: RefusalClass::Name,
            refused: 4,
            considered: 5,
            in_sync: 0,
            not_attempted: 1,
            codes: vec![("SYNO.FileStation.Upload.upload:418".to_owned(), 4)],
            sample: vec!["a".to_owned(), "b".to_owned()],
            more: 2,
        };
        let text = error.to_string();
        assert!(
            text.starts_with("remote-refused-share-wide: name: "),
            "{text}"
        );
        assert!(text.contains("for one cause"), "{text}");
        assert!(
            text.contains("found in sync (including 0 already in sync)"),
            "{text}"
        );
        assert!(text.contains(":418 x4"), "{text}");
        assert!(text.ends_with("paths: a, b, and 2 more"), "{text}");
        assert!(!text.contains("  "), "{text}");
    }

    #[test]
    fn an_unusable_real_path_shape_leads_with_its_token() {
        let text = Error::RemoteRealPathUnusable {
            kind: "files",
            linked: 5,
            checked: 5,
            sample: vec!["a.txt".to_owned()],
            more: 4,
        }
        .to_string();
        assert!(
            text.starts_with("remote-real-path-unusable: 5 of the 5 NAS files"),
            "{text}"
        );
        assert!(text.contains("nothing was synced"), "{text}");
        assert!(text.ends_with("files: a.txt, and 4 more"), "{text}");
        assert!(!text.contains("  "), "{text}");
    }

    #[test]
    fn a_link_on_the_scope_names_the_path_and_the_way_out() {
        let text = Error::RemoteLinkInScope {
            path: "/team/r5/backup-link".to_owned(),
            cause: "listed under the destination, but its real path is \"/volume1/team/x\""
                .to_owned(),
        }
        .to_string();
        assert!(text.contains("\"/team/r5/backup-link\""), "{text}");
        assert!(text.contains("NAS link"), "{text}");
        assert!(text.contains("run without --scope"), "{text}");
        assert!(!text.contains("  "), "{text}");
    }

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);
    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|l| l as _)
        }
    }

    /// A transport failure names its innermost cause (a black-hole server reads
    /// "operation timed out", not just "error sending request").
    #[test]
    fn cause_chains_name_the_innermost_cause_once() {
        let error = Layer(
            "error sending request for url (http://127.0.0.1:9/webapi/entry.cgi)",
            Some(Box::new(Layer(
                "client error (Connect)",
                Some(Box::new(Layer("operation timed out", None))),
            ))),
        );
        assert_eq!(
            super::cause_chain(&error),
            "operation timed out (error sending request for url (http://127.0.0.1:9/webapi/entry.cgi): client error (Connect))"
        );
        // What a 160-byte record keeps still names the cause, however long the URL.
        let long = Layer(
            Box::leak(
                format!(
                    "error sending request for url (https://nas.example:5001/{}/webapi/entry.cgi)",
                    "x".repeat(200)
                )
                .into_boxed_str(),
            ),
            Some(Box::new(Layer("operation timed out", None))),
        );
        let line = format!(
            "HTTP request failed during File Station list: {}",
            super::cause_chain(&long)
        );
        assert!(line[..160].contains("operation timed out"), "{line}");
        let repeated = Layer("timed out", Some(Box::new(Layer("timed out", None))));
        assert_eq!(super::cause_chain(&repeated), "timed out");
    }
}
