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

    #[error("source file changed while it was being uploaded: {0:?}")]
    SourceChanged(PathBuf),

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
            Self::Api { code, .. } => Some(*code),
            _ => None,
        }
    }
}
