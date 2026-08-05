use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
        let additional = item.additional.unwrap_or_default();
        let actual_size = additional.size.unwrap_or(0);
        let actual_mtime_seconds = additional.time.map_or(0, |time| time.mtime);
        if actual_kind != expected_kind
            || actual_size != expected_size
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
                let mount_point_type = additional
                    .mount_point_type
                    .filter(|value| !value.trim().is_empty());
                let entry = RemoteEntry {
                    relative: relative.clone(),
                    remote_path: item.path.clone(),
                    kind,
                    size: additional.size.unwrap_or(0),
                    mtime_seconds: additional.time.map_or(0, |time| time.mtime),
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
        let count = self.inner.read(buffer)?;
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
    use std::io::{Read as _, Write as _};
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

    fn scripted_server_with_status_hook<F>(
        responses: Vec<(StatusCode, String)>,
        mut after_response: F,
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
                write_scripted_response(&mut stream, status, &response_body);
                after_response(index);
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
            {"path":"/share/root/sub/nested.txt","name":"nested.txt","isdir":false,"additional":{"size":9}}
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

    #[test]
    fn remote_inventory_rejects_stalled_or_inconsistent_directory_pages() {
        let bad_pages = [
            serde_json::json!({"success":true,"data":{"total":1,"files":[]}}),
            serde_json::json!({"success":true,"data":{"total":1,"files":[{"path":"/share/root","name":"root","isdir":true}]}}),
            serde_json::json!({"success":true,"data":{"total":1,"files":[{"path":"/share/root/a","name":"wrong","isdir":false}]}}),
            serde_json::json!({"success":true,"data":{"total":2,"files":[
                {"path":"/share/root/a","name":"a","isdir":false},
                {"path":"/share/root/a","name":"a","isdir":false}
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
        assert!(matches!(
            ApiClient::connect(&options(invalid.clone())),
            Err(Error::Http { .. })
        ));
        fs::remove_file(invalid).unwrap();

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
        let cancel_after_create = cancellation.clone();
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
                cancel_after_create.cancel();
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
}
