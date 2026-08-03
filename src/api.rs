use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use reqwest::blocking::multipart::{Form, Part};
use reqwest::blocking::{Client as HttpClient, Response};
use reqwest::redirect::Policy;
use reqwest::{Certificate, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use zeroize::Zeroizing;

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
];
const LIST_PAGE_SIZE: usize = 500;
const MAX_JSON_RESPONSE: u64 = 32 * 1024 * 1024;

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
}

#[derive(Debug)]
pub struct RemoteInventory {
    pub root_exists: bool,
    pub entries: BTreeMap<String, RemoteEntry>,
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
        };
        client.apis = client.discover()?;
        client.validate_api("SYNO.API.Auth", 3)?;
        for (api, version) in [
            ("SYNO.FileStation.List", 2),
            ("SYNO.FileStation.CreateFolder", 2),
            ("SYNO.FileStation.Upload", 2),
        ] {
            client.validate_api(api, version)?;
        }
        Ok(client)
    }

    pub fn require_delete_api(&self) -> Result<()> {
        self.validate_api("SYNO.FileStation.Delete", 2)
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
        self.upload_observed(local, remote_file, None)
    }

    pub fn upload_observed(
        &self,
        local: &LocalEntry,
        remote_file: &str,
        observer: Option<UploadObserver>,
    ) -> Result<()> {
        let result = self.upload_observed_inner(local, remote_file, observer.clone());
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
    ) -> Result<()> {
        let (remote_parent, remote_name) = parent_and_name(remote_file)?;
        let observer_cancelled = Arc::new(AtomicBool::new(false));
        for attempt in 0..=self.retries {
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
                .text("create_parents", "true")
                .text("overwrite", "true")
                .text("mtime", local.mtime_ms.to_string())
                .text("_sid", session.sid.to_string());
            if let Some(token) = &session.syno_token {
                form = form.text("SynoToken", token.to_string());
            }
            // Synology requires the binary part to be last.
            form = form.part("file", part);

            let url = self.api_url("SYNO.FileStation.Upload")?;
            let operation = format!("uploading {}", local.relative);
            let result = match self.http.post(url).multipart(form).send() {
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
                    return Ok(());
                }
                Err(error) if attempt < self.retries && retryable(&error) => {
                    verify_local_snapshot(local)?;
                    retry_pause(attempt);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("retry loop always returns")
    }

    /// Validate and open an upload source before any destructive type replacement begins.
    pub fn preflight_upload_source(&self, local: &LocalEntry) -> Result<()> {
        verify_local_snapshot(local)?;
        let file = File::open(&local.full_path).map_err(|source| Error::FileIo {
            path: local.full_path.clone(),
            source,
        })?;
        verify_open_file_snapshot(local, &file)
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
        let parameters = vec![
            pair("path", json_array([path])?),
            pair("additional", json_array(["mount_point_type"])?),
        ];
        let mut data: GetInfoData = self
            .call("SYNO.FileStation.List", 2, "getinfo", parameters, true)?
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
        // Passwords, OTPs, and session values enter this owned form field list. reqwest must
        // still serialize its own request-body copy, but this caller-owned copy is short-lived
        // and explicitly erased.
        let fields = Zeroizing::new(fields);
        let response = self
            .http
            .post(url)
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

fn retry_pause(attempt: u32) {
    let multiplier = 1_u64 << attempt.min(4);
    thread::sleep(Duration::from_millis(250 * multiplier));
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
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread::JoinHandle;
    use std::time::SystemTime;

    use super::*;

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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, response_body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut received = Vec::new();
                let header_end = loop {
                    let mut buffer = [0_u8; 4096];
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0, "connection closed before request headers");
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
                    .map(|(name, value)| {
                        (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                    })
                    .collect();
                let content_length = headers
                    .iter()
                    .find(|(name, _)| name == "content-length")
                    .and_then(|(_, value)| value.parse::<usize>().ok())
                    .unwrap_or(0);
                while received.len() - header_end < content_length {
                    let mut buffer = [0_u8; 8192];
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0, "connection closed before complete request body");
                    received.extend_from_slice(&buffer[..count]);
                }
                requests.push(CapturedRequest {
                    request_line,
                    headers,
                    body: received[header_end..header_end + content_length].to_vec(),
                });

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
            requests
        });
        (format!("http://{address}/prefix/"), handle)
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
    fn full_flow_keeps_secrets_out_of_urls_and_streams_known_length_upload() {
        let discovery = serde_json::json!({
            "success": true,
            "data": {
                "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
                "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2, "requestFormat": "JSON"},
                "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2, "requestFormat": "JSON"},
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
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
        };
        client
            .upload(&local, "/share/root/folder/upload.bin")
            .unwrap();
        client.logout().unwrap();
        fs::remove_file(path).unwrap();

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 10);
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
                "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
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
