use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use md5::{Digest, Md5};
use reqwest::blocking::multipart::{Form, Part};
use reqwest::blocking::{Client as HttpClient, Response};
use reqwest::redirect::Policy;
use reqwest::{Certificate, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::cancel::CancellationToken;
use crate::integrity::ContentMd5;
use crate::local::{EntryKind, LocalEntry};
use crate::path::{RemoteRoot, parent_and_name};
use crate::{Error, Result};

const DISCOVERY_APIS: &[&str] = &[
    "SYNO.API.Auth",
    "SYNO.FileStation.Info",
    "SYNO.FileStation.List",
    "SYNO.FileStation.CreateFolder",
    "SYNO.FileStation.Upload",
    "SYNO.FileStation.Delete",
    "SYNO.FileStation.MD5",
    "SYNO.FileStation.CopyMove",
    "SYNO.FileStation.CheckPermission",
];
const LIST_PAGE_SIZE: usize = 500;
const MAX_JSON_RESPONSE: u64 = 32 * 1024 * 1024;
const MAX_CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest a rate-limited reader sleeps before it looks at the cancellation token again.
const RATE_LIMIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Rate-limit tokens are held scaled by this factor so refill stays exact integer arithmetic.
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const WRITE_PROBE_PAYLOAD: &[u8] = b"synology-drive-sync disposable write probe v1\n";
const WRITE_PROBE_FILE_NAME: &str = "probe.bin";
const WRITE_PROBE_COPY_DIRECTORY: &str = "copy";
static WRITE_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    base: Url,
    apis: HashMap<String, ApiSpec>,
    session: Option<Session>,
    retries: u32,
    control_timeout: Duration,
    upload_timeout: Duration,
    operation_timeout: Duration,
    upload_rate_limit: Option<Arc<Mutex<TokenBucket>>>,
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

#[derive(Debug)]
pub struct RemoteInventory {
    pub root_exists: bool,
    pub entries: BTreeMap<String, RemoteEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationWriteCheck {
    /// The existing directory whose child-create permission was checked.
    pub checked_directory: String,
    /// Whether the configured destination itself existed when it was checked.
    pub destination_exists: bool,
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

impl ApiClient {
    pub fn connect(options: &ClientOptions) -> Result<Self> {
        let base = normalize_base_url(&options.base_url, options.allow_http)?;
        let mut builder = HttpClient::builder()
            .connect_timeout(options.connect_timeout)
            .timeout(options.request_timeout)
            .redirect(Policy::none())
            .user_agent(concat!("synology-drive-sync/", env!("SDSYNC_VERSION")));

        if options.accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(path) = &options.ca_certificate {
            let pem = fs::read(path).map_err(|source| Error::FileIo {
                path: path.clone(),
                source,
            })?;
            // The rustls backend only stores the PEM bytes here and parses them when the client
            // is built, so a file holding no CERTIFICATE section at all -- an empty, truncated,
            // or simply mistaken file -- would be accepted in silence and leave the operator
            // believing a CA was pinned when nothing was added to the trust store. Counting the
            // sections up front makes that loud; an unreadable payload is deliberately left to
            // surface where reqwest actually rejects it, when the client is built.
            if Certificate::from_pem_bundle(&pem).is_ok_and(|certificates| certificates.is_empty())
            {
                return Err(Error::Message(format!(
                    "CA certificate file {path:?} contains no certificate; --ca-certificate must name a PEM file with at least one CERTIFICATE block"
                )));
            }
            let certificate = Certificate::from_pem(&pem).map_err(|source| Error::Http {
                operation: format!("loading CA certificate {path:?}"),
                source,
            })?;
            builder = builder.add_root_certificate(certificate);
        }
        let http = builder.build().map_err(|source| Error::Http {
            operation: "building HTTP client".to_owned(),
            source,
        })?;

        let mut client = Self {
            http,
            base,
            apis: HashMap::new(),
            session: None,
            retries: options.retries,
            control_timeout: control_request_timeout(options.request_timeout),
            upload_timeout: options.request_timeout,
            operation_timeout: options.request_timeout,
            upload_rate_limit: None,
        };
        client.apis = client.discover()?;
        client.validate_api("SYNO.API.Auth", 3)?;
        for (api, version) in [
            ("SYNO.FileStation.List", 2),
            ("SYNO.FileStation.CreateFolder", 2),
            ("SYNO.FileStation.Upload", 2),
            ("SYNO.FileStation.CheckPermission", 3),
        ] {
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

    fn stop_task(&self, api: &str, version: u32, taskid: &str) -> Result<()> {
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
    /// the MD5 API so a delayed successful response cannot hide a replacement.
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
        let Some(actual_size) = self.remote_file_size(remote_path)? else {
            return Ok(false);
        };
        if actual_size != expected_size {
            return Ok(false);
        }
        Ok(self.remote_content_md5(remote_path, cancellation)? == expected_md5)
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
        if auth_version >= 6 {
            fields.push(pair("enable_syno_token", "yes"));
        }
        if let Some(code) = otp {
            fields.push(pair("otp_code", code));
        }

        let url = self.api_url("SYNO.API.Auth")?;
        let data: LoginData = self
            .send_form_once(url, fields, "SYNO.API.Auth", "login")?
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
        self.session = Some(Session {
            sid: Zeroizing::new(data.sid),
            syno_token: data.synotoken.map(Zeroizing::new),
        });
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
        let result = self.send_form_once::<Value>(url, fields, "SYNO.API.Auth", "logout");
        self.session = None;
        result.map(|_| ())
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

    /// Verify write permission at the configured destination without changing remote state.
    ///
    /// File Station's CheckPermission API checks permission to create a named child within an
    /// existing directory. For an existing destination, use a collision-resistant probe name in
    /// that exact directory. If the destination is absent, check creation of the first missing
    /// path component in its nearest existing ancestor; later components do not exist yet and
    /// therefore cannot have independent ACLs to inspect without mutating the NAS.
    pub fn verify_destination_writable(&self, root: &RemoteRoot) -> Result<DestinationWriteCheck> {
        self.validate_api("SYNO.FileStation.CheckPermission", 3)?;

        let mut nearest_existing = None;
        for path in absolute_prefixes(root.as_str()) {
            match self.get_info(&path) {
                Ok(item) => {
                    if !item.isdir {
                        return Err(Error::Message(format!(
                            "remote destination ancestor {path} exists but is not a directory"
                        )));
                    }
                    nearest_existing = Some(path);
                }
                Err(error) if error.api_code() == Some(408) => {
                    let Some(existing) = nearest_existing else {
                        return Err(Error::ShareNotWritable(root.share_name().to_owned()));
                    };
                    let (_, missing_name) = parent_and_name(&path)?;
                    self.check_write_permission(&existing, missing_name)?;
                    return Ok(DestinationWriteCheck {
                        checked_directory: existing,
                        destination_exists: false,
                    });
                }
                Err(error) => return Err(error),
            }
        }

        let probe_name = permission_probe_name();
        self.check_write_permission(root.as_str(), &probe_name)?;
        Ok(DestinationWriteCheck {
            checked_directory: root.as_str().to_owned(),
            destination_exists: true,
        })
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
        let expected_md5 = write_probe_md5();
        let local = match ProbeLocalFile::create(expected_md5) {
            Ok(local) => local,
            Err(cause) => {
                let mut report = initial_write_probe_report(
                    root,
                    probe_path,
                    WRITE_PROBE_PAYLOAD.len() as u64,
                    expected_md5,
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
            self.require_content_api()?;
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
            self.verify_empty_probe_directory(probe_path)?;
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
                self.verify_empty_probe_directory(&copy_directory)?;
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

    fn verify_empty_probe_directory(&self, remote_path: &str) -> Result<()> {
        let item = self.get_info_with_retry(remote_path, false)?;
        if !item.isdir {
            return Err(Error::Message(format!(
                "write-probe path {remote_path:?} was not created as a directory"
            )));
        }
        let children = self.list_directory(remote_path)?;
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

    pub fn remote_inventory(&self, root: &RemoteRoot) -> Result<RemoteInventory> {
        let mut entries = BTreeMap::new();
        let mut pending = vec![root.as_str().to_owned()];
        let mut root_exists = true;

        // Inspect every ancestor before traversing. This also catches a destination below
        // a mounted remote folder, including when the final destination does not exist yet.
        for path in absolute_prefixes(root.as_str()) {
            let info = match self.get_info(&path) {
                Ok(info) => info,
                Err(error) if error.api_code() == Some(408) => {
                    return Ok(RemoteInventory {
                        root_exists: false,
                        entries,
                    });
                }
                Err(error) => return Err(error),
            };
            if !info.isdir {
                return Err(Error::Message(format!(
                    "remote destination ancestor {path} exists but is not a directory"
                )));
            }
            if let Some(mount_type) = info
                .additional
                .and_then(|additional| additional.mount_point_type)
                .filter(|value| !value.trim().is_empty())
            {
                return Err(Error::RemoteMountRoot { path, mount_type });
            }
        }

        while let Some(folder) = pending.pop() {
            let files = match self.list_directory(&folder) {
                Ok(files) => files,
                Err(error) if folder == root.as_str() && error.api_code() == Some(408) => {
                    root_exists = false;
                    break;
                }
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

        Ok(RemoteInventory {
            root_exists,
            entries,
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
            if let Some(token) = &session.syno_token {
                form = form.text("SynoToken", token.to_string());
            }
            // Synology requires the binary part to be last.
            form = form.part("file", part);

            let url = self.api_url("SYNO.FileStation.Upload")?;
            let operation = format!("uploading {}", local.relative);
            let result = match self
                .http
                .post(url)
                .timeout(self.upload_timeout)
                .multipart(form)
                .send()
            {
                Ok(response) => {
                    decode_response::<Value>(response, "SYNO.FileStation.Upload", "upload")
                        .map(|_| ())
                }
                Err(source) => Err(Error::Http {
                    operation: operation.clone(),
                    source,
                }),
            };
            let result = prioritize_observer_cancellation(&observer_cancelled, result);
            match result {
                Ok(()) => {
                    verify_local_snapshot(local)?;
                    if let Some(expected) = local.content_md5 {
                        let actual_local = crate::local::hash_file_snapshot(local, cancellation)?;
                        if actual_local != expected {
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
                        if actual_local != expected {
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
        if let Some(expected) = local.content_md5
            && crate::local::hash_file_snapshot(local, cancellation)? != expected
        {
            return Err(Error::SourceChanged(local.full_path.clone()));
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
                Err(second_error) => Err(Error::Message(format!(
                    "File Station API discovery failed through the reverse proxy; entry.cgi: {first_error}; query.cgi fallback: {second_error}"
                ))),
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
        self.send_form_with_retry(url, fields, "SYNO.API.Info", "query", true)?
            .ok_or_else(|| Error::InvalidResponse {
                operation: "SYNO.API.Info.query".to_owned(),
                message: "successful response contained no API map".to_owned(),
            })
    }

    fn list_directory(&self, folder: &str) -> Result<Vec<RemoteItemWire>> {
        let mut offset = 0_usize;
        let mut output = Vec::new();
        loop {
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
            output.extend(data.files);
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

    fn get_info(&self, path: &str) -> Result<RemoteItemWire> {
        self.get_info_with_retry(path, true)
    }

    fn get_info_with_retry(&self, path: &str, allow_retry: bool) -> Result<RemoteItemWire> {
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
        Ok(item)
    }

    fn call<T: DeserializeOwned>(
        &self,
        api: &str,
        version: u32,
        method: &str,
        parameters: Vec<(String, String)>,
        allow_retry: bool,
    ) -> Result<Option<T>> {
        self.validate_api(api, version)?;
        let fields = self.authenticated_fields(api, version, method, parameters)?;
        let url = self.api_url(api)?;
        self.send_form_with_retry(url, fields, api, method, allow_retry)
    }

    fn call_bounded<T: DeserializeOwned>(
        &self,
        api: &str,
        version: u32,
        method: &str,
        parameters: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<Option<T>> {
        self.validate_api(api, version)?;
        let fields = self.authenticated_fields(api, version, method, parameters)?;
        let url = self.api_url(api)?;
        self.send_form_once_with_timeout(url, fields, api, method, timeout)
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
        api: &str,
        method: &str,
        allow_retry: bool,
    ) -> Result<Option<T>> {
        // These owned copies can include SID/SynoToken values. Erase them after the final
        // attempt; each per-attempt clone is erased by `send_form_once` as well.
        let fields = Zeroizing::new(fields);
        let attempts = if allow_retry { self.retries } else { 0 };
        for attempt in 0..=attempts {
            let result = self.send_form_once(url.clone(), fields.to_vec(), api, method);
            match result {
                Ok(value) => return Ok(value),
                Err(error) if attempt < attempts && retryable(&error) => retry_pause(attempt),
                Err(error) => return Err(error),
            }
        }
        unreachable!("retry loop always returns")
    }

    fn send_form_once<T: DeserializeOwned>(
        &self,
        url: Url,
        fields: Vec<(String, String)>,
        api: &str,
        method: &str,
    ) -> Result<Option<T>> {
        self.send_form_once_with_timeout(url, fields, api, method, self.control_timeout)
    }

    fn send_form_once_with_timeout<T: DeserializeOwned>(
        &self,
        url: Url,
        fields: Vec<(String, String)>,
        api: &str,
        method: &str,
        timeout: Duration,
    ) -> Result<Option<T>> {
        // Passwords, OTPs, and session values enter this owned form field list. reqwest must
        // still serialize its own request-body copy, but this caller-owned copy is short-lived
        // and explicitly erased.
        let fields = Zeroizing::new(fields);
        let response = self
            .http
            .post(url)
            .timeout(timeout)
            .form(&*fields)
            .send()
            .map_err(|source| Error::Http {
                operation: format!("{api}.{method}"),
                source,
            })?;
        decode_response(response, api, method)
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

    fn api_url(&self, api: &str) -> Result<Url> {
        endpoint_url(&self.base, &self.required_spec(api)?.path)
    }
}

#[derive(Debug, Deserialize)]
struct LoginData {
    sid: String,
    #[serde(default)]
    synotoken: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListShareData {
    #[serde(default)]
    shares: Vec<ShareWire>,
}

#[derive(Debug, Deserialize)]
struct ShareWire {
    path: String,
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
    name: String,
    isdir: bool,
    #[serde(default)]
    additional: Option<RemoteAdditionalWire>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteAdditionalWire {
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    time: Option<RemoteTimeWire>,
    #[serde(default)]
    mount_point_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteTimeWire {
    mtime: i64,
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
    let mtime_seconds = additional.time.as_ref().map(|time| time.mtime);
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

fn decode_response<T: DeserializeOwned>(
    mut response: Response,
    api: &str,
    method: &str,
) -> Result<Option<T>> {
    let status = response.status();
    // API discovery is the only unauthenticated response decoded here. Every other API either
    // receives login material or an authenticated SID/SynoToken. Default to withholding those
    // response bodies so a diagnostic proxy cannot reflect request secrets into user-visible
    // errors or logs.
    let withhold_response_body = api != "SYNO.API.Info";
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
    if body.len() as u64 > MAX_JSON_RESPONSE {
        return Err(Error::InvalidResponse {
            operation: format!("{api}.{method}"),
            message: "response exceeded the 32 MiB safety limit".to_owned(),
        });
    }
    if !status.is_success() {
        return Err(Error::HttpStatus {
            operation: format!("{api}.{method}"),
            status,
            message: if withhold_response_body {
                withheld_response_message(api).to_owned()
            } else {
                http_status_hint(status, &body)
            },
        });
    }

    let envelope: Envelope<T> = serde_json::from_slice(&body).map_err(|error| {
        let snippet = if withhold_response_body {
            format!("[{}]", withheld_response_message(api))
        } else {
            response_snippet(&body)
        };
        let route_hint = if looks_like_html(&body) {
            " (the proxy returned HTML, so /webapi/* is probably routed to the File Station UI instead of WebAPI)"
        } else {
            ""
        };
        Error::InvalidResponse {
            operation: format!("{api}.{method}"),
            message: format!("expected a DSM JSON envelope: {error}; response: {snippet}{route_hint}"),
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

pub(crate) fn normalize_base_url(input: &str, allow_http: bool) -> Result<Url> {
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

fn write_probe_md5() -> ContentMd5 {
    ContentMd5::from_bytes(Md5::digest(WRITE_PROBE_PAYLOAD).into())
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

fn pair(key: impl Into<String>, value: impl Into<String>) -> (String, String) {
    (key.into(), value.into())
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

fn retry_pause(attempt: u32) {
    let multiplier = 1_u64 << attempt.min(4);
    thread::sleep(Duration::from_millis(250 * multiplier));
}

fn retry_pause_cancellable(attempt: u32, cancellation: &CancellationToken) -> Result<()> {
    let multiplier = 1_u64 << attempt.min(4);
    sleep_cancellable(Duration::from_millis(250 * multiplier), cancellation)
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

fn http_status_hint(status: StatusCode, body: &[u8]) -> String {
    match status.as_u16() {
        301 | 302 | 303 | 307 | 308 => {
            "redirects are disabled to prevent credentials crossing origins; expose /webapi/* directly at the configured HTTPS URL".to_owned()
        }
        413 => "request body is larger than the reverse proxy permits; raise its upload/body-size limit".to_owned(),
        502 => "reverse proxy could not reach the File Station backend".to_owned(),
        504 => "reverse proxy timed out; raise its send/read timeout for large uploads".to_owned(),
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

    const SCRIPTED_SERVER_TIMEOUT: Duration = Duration::from_secs(5);

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

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn decode_scripted_error(status: StatusCode, body: String, api: &str, method: &str) -> Error {
        let (base, server) = scripted_server_with_status(vec![(status, body)]);
        let url = Url::parse(&base).unwrap().join("webapi/entry.cgi").unwrap();
        let response = HttpClient::new().post(url).send().unwrap();
        let error = decode_response::<Value>(response, api, method).unwrap_err();
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

    fn task_start_response(taskid: &str) -> String {
        serde_json::json!({"success": true, "data": {"taskid": taskid}}).to_string()
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
            .remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
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
                .remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
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
            .remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
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
            time: Some(RemoteTimeWire { mtime: 13 }),
            mount_point_type: None,
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
                client.remote_inventory(&RemoteRoot::parse("/share/root").unwrap()),
                Err(Error::InvalidResponse { .. })
            ));
            assert_eq!(server.join().unwrap().len(), 5);
        }
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
            .remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
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
                .remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
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
            .remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
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
        let root_digest = ContentMd5::from_bytes([0_u8; 16]);
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
            http: HttpClient::new(),
            base: Url::parse("https://files.example.test/webapi/").unwrap(),
            apis: HashMap::new(),
            session: None,
            retries: 0,
            control_timeout: Duration::from_secs(1),
            upload_timeout: Duration::from_secs(1),
            operation_timeout: Duration::from_secs(1),
            upload_rate_limit: None,
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
        let local = ProbeLocalFile::create(write_probe_md5()).unwrap();
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
            r#"{"success":true,"data":{"taskid":"upload-md5"}}"#.to_owned(),
            format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#),
            getinfo_file(&upload_path, size, Some(mtime_seconds)),
            r#"{"success":true}"#.to_owned(),
            getinfo_directory(&copy_directory),
            r#"{"success":true,"data":{"total":0,"offset":0,"files":[]}}"#.to_owned(),
            r#"{"success":false,"error":{"code":408}}"#.to_owned(),
            r#"{"success":true,"data":{"taskid":"copy-task"}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":true}}"#.to_owned(),
            getinfo_file(&copy_path, size, None),
            r#"{"success":true,"data":{"taskid":"copy-md5"}}"#.to_owned(),
            format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#),
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
        assert_eq!(requests.len(), 29);
        for index in [5, 13] {
            let create = String::from_utf8_lossy(&requests[index].body);
            assert!(create.contains("api=SYNO.FileStation.CreateFolder"));
            assert!(create.contains("force_parent=false"));
        }
        let upload = &requests[8];
        assert!(find_bytes(&upload.body, b"name=\"overwrite\"").is_some());
        assert!(find_bytes(&upload.body, b"\r\n\r\nfalse\r\n").is_some());
        assert!(find_bytes(&upload.body, WRITE_PROBE_PAYLOAD).is_some());
        let copy = String::from_utf8_lossy(&requests[17].body);
        assert!(copy.contains("api=SYNO.FileStation.CopyMove"));
        assert!(copy.contains("remove_src=false"));
        assert!(!copy.contains("overwrite"));
        for delete in &requests[23..=26] {
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
        let local = ProbeLocalFile::create(write_probe_md5()).unwrap();
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
        let local = ProbeLocalFile::create(write_probe_md5()).unwrap();
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
                "SYNO.FileStation.MD5": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
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
            r#"{"success":true,"data":{"taskid":"upload-md5-task"}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":true,"md5":"28d24f2b9feacb26cfebfe4f01ba3aed"}}"#.to_owned(),
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
        let inventory = client.remote_inventory(&root).unwrap();
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
            content_md5: Some(ContentMd5::parse_hex("28d24f2b9feacb26cfebfe4f01ba3aed").unwrap()),
        };
        client
            .upload(&local, "/share/root/folder/upload.bin")
            .unwrap();
        client.logout().unwrap();
        fs::remove_file(path).unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 13);
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
        assert!(verification.contains("SYNO.FileStation.MD5"));
        assert!(verification.contains("%2Fshare%2Froot%2Ffolder%2Fupload.bin"));
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
                "SYNO.FileStation.MD5": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
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
            (
                StatusCode::OK,
                r#"{"success":true,"data":{"taskid":"reconcile-md5"}}"#.to_owned(),
            ),
            (
                StatusCode::OK,
                r#"{"success":true,"data":{"finished":true,"md5":"900150983cd24fb0d6963f7d28e17f72"}}"#.to_owned(),
            ),
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
            content_md5: Some(ContentMd5::parse_hex("900150983cd24fb0d6963f7d28e17f72").unwrap()),
        };
        client.upload(&local, "/share/root/abc.bin").unwrap();
        client.logout().unwrap();
        fs::remove_file(path).unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
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
                "SYNO.FileStation.CopyMove": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 3}
            }
        })
        .to_string();
        let digest = "900150983cd24fb0d6963f7d28e17f72";
        let responses = vec![
            discovery,
            r#"{"success":true,"data":{"sid":"secret-sid","synotoken":"csrf-secret"}}"#.to_owned(),
            r#"{"success":true,"data":{"taskid":"copy-task"}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":true}}"#.to_owned(),
            r#"{"success":true,"data":{"files":[{"path":"/share/root/new/report.bin","name":"report.bin","isdir":false,"additional":{"size":3}}]}}"#.to_owned(),
            r#"{"success":true,"data":{"taskid":"md5-task"}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":false}}"#.to_owned(),
            format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#),
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
                ContentMd5::parse_hex(digest).unwrap(),
                &CancellationToken::default(),
            )
            .unwrap();
        client.logout().unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 9);
        let copy_start = String::from_utf8_lossy(&requests[2].body);
        assert!(copy_start.contains("SYNO.FileStation.CopyMove"));
        assert!(copy_start.contains("remove_src=false"));
        assert!(!copy_start.contains("overwrite"));
        let size_check = String::from_utf8_lossy(&requests[4].body);
        assert!(size_check.contains("method=getinfo"));
        let md5_start = String::from_utf8_lossy(&requests[5].body);
        assert!(md5_start.contains("SYNO.FileStation.MD5"));
        assert!(md5_start.contains("%2Fshare%2Froot%2Fnew%2Freport.bin"));
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
            .remote_inventory(&RemoteRoot::parse("/share/mounted/child").unwrap())
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
            .remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
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
            content_md5: Some(ContentMd5::from_bytes(Md5::digest(b"payload").into())),
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
        let local = ProbeLocalFile::create(write_probe_md5()).unwrap();
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

    /// A deterministic name collision (414) is somebody else's directory and must never be
    /// cleaned up. Any other creation failure may have partially landed, so cleanup must run.
    #[test]
    fn write_probe_cleans_up_after_ambiguous_creation_but_never_after_a_collision() {
        let root = RemoteRoot::parse("/share/root").unwrap();
        let probe_path = "/share/root/.synology-drive-sync-probe-test-create";
        let local = ProbeLocalFile::create(write_probe_md5()).unwrap();
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
        let local = ProbeLocalFile::create(write_probe_md5()).unwrap();
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
        let local = ProbeLocalFile::create(write_probe_md5()).unwrap();
        let size = local.entry.size;
        let mtime_seconds = local.entry.mtime_ms.div_euclid(1000);
        let digest = local.entry.content_md5.unwrap().to_string();
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
            task_start_response("leftover-md5"),
            format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#),
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
        assert_eq!(server.join().unwrap().len(), 18);
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
            content_md5: Some(ContentMd5::from_bytes(Md5::digest(contents).into())),
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
        let digest = local.content_md5.unwrap().to_string();
        let mut responses = upload_preamble();
        responses.extend([
            (StatusCode::OK, r#"{"success":true}"#.to_owned()),
            (
                StatusCode::OK,
                getinfo_file("/share/root/abc.bin", local.size, None),
            ),
            (StatusCode::OK, task_start_response("throttled-md5")),
            (
                StatusCode::OK,
                format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#),
            ),
        ]);
        let (client, server) = upload_client(responses, 0);
        // A megabyte per second: the opening burst covers this payload outright, so the limited
        // path is exercised without the test depending on any wall-clock delay.
        let client = client.with_max_upload_rate(Some(1024 * 1024));
        client.upload(&local, "/share/root/abc.bin").unwrap();
        assert_eq!(server.join().unwrap().len(), 6);
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
        let digest = local.content_md5.unwrap().to_string();
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
            (StatusCode::OK, task_start_response("retry-md5")),
            (
                StatusCode::OK,
                format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#),
            ),
        ]);
        let (client, server) = upload_client(responses, 1);
        client.upload(&local, "/share/root/abc.bin").unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 8);
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
        let digest = local.content_md5.unwrap().to_string();
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
            (StatusCode::OK, task_start_response("flaky-md5")),
            (
                StatusCode::OK,
                format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#),
            ),
        ]);
        let (client, server) = upload_client(responses, 1);
        client.upload(&local, "/share/root/abc.bin").unwrap();
        assert_eq!(server.join().unwrap().len(), 8);
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
        let rendered = error.to_string();
        assert!(
            rendered.starts_with("File Station API discovery failed through the reverse proxy")
        );
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
        server.join().unwrap();
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
            .remote_inventory(&RemoteRoot::parse("/share/root").unwrap())
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
        let digest = ContentMd5::from_bytes(Md5::digest(b"report").into());
        let (url, server) = scripted_server(vec![
            write_probe_discovery(true),
            login_response(),
            task_start_response("slow-copy"),
            r#"{"success":true,"data":{"finished":false}}"#.to_owned(),
            r#"{"success":true,"data":{"finished":true}}"#.to_owned(),
            getinfo_file(destination, 6, None),
            task_start_response("slow-copy-md5"),
            format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#),
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
        assert_eq!(requests.len(), 8);
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
        let local = ProbeLocalFile::create(write_probe_md5()).unwrap();
        let size = local.entry.size;
        let mtime_seconds = local.entry.mtime_ms.div_euclid(1000);
        let digest = local.entry.content_md5.unwrap().to_string();
        let md5_finished =
            format!(r#"{{"success":true,"data":{{"finished":true,"md5":"{digest}"}}}}"#);
        let missing = r#"{"success":false,"error":{"code":408}}"#.to_owned();
        let succeeded = r#"{"success":true}"#.to_owned();

        // Responses 0..=11: everything up to and including the uploaded file's MD5 check.
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
                task_start_response("copy-phase-md5"),
                md5_finished.clone(),
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
        assert_eq!(server.join().unwrap().len(), 18);

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
        assert_eq!(server.join().unwrap().len(), 19);

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
        assert_eq!(server.join().unwrap().len(), 23);

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
            task_start_response("probe-copy-md5"),
            md5_finished.clone(),
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
        assert_eq!(server.join().unwrap().len(), 28);
    }
}
