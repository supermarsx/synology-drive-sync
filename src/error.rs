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

    #[error("HTTP request failed during {operation}: {source}")]
    Http {
        operation: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("HTTP response body failed during {operation}: {source}")]
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

    #[error(
        "final reconciliation found {operations} pending in-scope operations; inspect a fresh plan before rerunning"
    )]
    ReconciliationPending { operations: usize },

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

    /// For a refusal of one path, what it shares with other refusals of the same kind: the
    /// operation and DSM's code (`"SYNO.FileStation.Upload.upload:418"`). A run that sees many
    /// refusals with one key is looking at one cause, not many paths, and can say so once.
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
    }
}
