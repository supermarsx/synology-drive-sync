use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use md5::{Digest, Md5};
use serde_json::{Value, json};

const SESSION_ID: &str = "e2e-session-secret";
const SYNO_TOKEN: &str = "e2e-syno-token-secret";

#[derive(Clone, Debug)]
pub struct CapturedRequest {
    pub request_path: String,
    pub headers: BTreeMap<String, String>,
    pub api: String,
    pub method: String,
    pub fields: BTreeMap<String, String>,
    pub upload_filename: Option<String>,
    pub upload_bytes: Option<usize>,
}

impl CapturedRequest {
    pub fn operation(&self) -> String {
        format!("{}.{}", self.api, self.method)
    }
}

#[derive(Clone, Debug)]
struct FileNode {
    contents: Vec<u8>,
    mtime_seconds: i64,
}

#[derive(Debug)]
struct ServerState {
    directories: BTreeSet<String>,
    list_disabled_directories: BTreeSet<String>,
    files: BTreeMap<String, FileNode>,
    requests: Vec<CapturedRequest>,
    expected_account: String,
    expected_password: String,
    reflected_login_failure: Option<String>,
    login_cookie: Option<String>,
    require_header_session_transport: bool,
    require_totp: bool,
    reject_next_valid_otp: bool,
    next_task: u64,
    md5_tasks: BTreeMap<String, String>,
    copy_tasks: BTreeSet<String>,
    mutation_after_listing: Option<PendingMutation>,
    faults: Vec<InjectedFault>,
}

#[derive(Debug)]
struct PendingMutation {
    path: String,
    contents: Vec<u8>,
    mtime_seconds: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultPhase {
    BeforeOperation,
    AfterCommit,
}

#[derive(Clone, Copy, Debug)]
enum FaultResponse {
    HttpStatus(u16),
    ApiError(i64),
}

#[derive(Debug)]
struct InjectedFault {
    operation: String,
    request_path: Option<String>,
    phase: FaultPhase,
    response: FaultResponse,
    remaining: usize,
}

pub struct MockFileStation {
    base_url: String,
    state: Arc<Mutex<ServerState>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MockFileStation {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock File Station");
        listener
            .set_nonblocking(true)
            .expect("make mock listener nonblocking");
        let address = listener.local_addr().expect("read mock address");
        let state = Arc::new(Mutex::new(ServerState {
            directories: BTreeSet::from(["/team".to_owned()]),
            list_disabled_directories: BTreeSet::new(),
            files: BTreeMap::new(),
            requests: Vec::new(),
            expected_account: "e2e-user".to_owned(),
            expected_password: "correct horse battery staple".to_owned(),
            reflected_login_failure: None,
            login_cookie: None,
            require_header_session_transport: false,
            require_totp: false,
            reject_next_valid_otp: false,
            next_task: 1,
            md5_tasks: BTreeMap::new(),
            copy_tasks: BTreeSet::new(),
            mutation_after_listing: None,
            faults: Vec::new(),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if worker_shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        stream
                            .set_nonblocking(false)
                            .expect("make accepted mock connection blocking");
                        handle_connection(stream, &worker_state);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock File Station accept failed: {error}"),
                }
            }
        });
        Self {
            base_url: format!("http://{address}/prefix/"),
            state,
            shutdown,
            worker: Some(worker),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn add_directory(&self, path: &str) {
        self.state
            .lock()
            .expect("mock state lock")
            .directories
            .insert(path.to_owned());
    }

    pub fn remove_directory(&self, path: &str) {
        self.state
            .lock()
            .expect("mock state lock")
            .directories
            .remove(path);
    }

    pub fn disable_directory_listing(&self, path: &str) {
        self.state
            .lock()
            .expect("mock state lock")
            .list_disabled_directories
            .insert(path.to_owned());
    }

    pub fn add_file(&self, path: &str, contents: &[u8], mtime_seconds: i64) {
        self.state.lock().expect("mock state lock").files.insert(
            path.to_owned(),
            FileNode {
                contents: contents.to_vec(),
                mtime_seconds,
            },
        );
    }

    pub fn reflect_login_failure(&self, marker: &str) {
        self.state
            .lock()
            .expect("mock state lock")
            .reflected_login_failure = Some(marker.to_owned());
    }

    /// Model DSM 7 behind a reverse proxy: the login response advertises a
    /// misleading cookie, while every authenticated request must construct the
    /// exact cookie and SynoToken header from the JSON login response.
    pub fn require_header_session_transport(&self, advertised_cookie: &str) {
        let mut state = self.state.lock().expect("mock state lock");
        state.login_cookie = Some(advertised_cookie.to_owned());
        state.require_header_session_transport = true;
    }

    pub fn require_totp(&self) {
        self.state.lock().expect("mock state lock").require_totp = true;
    }

    #[allow(dead_code)]
    pub fn reject_next_valid_otp(&self) {
        self.state
            .lock()
            .expect("mock state lock")
            .reject_next_valid_otp = true;
    }

    pub fn mutate_file_after_next_listing(&self, path: &str, contents: &[u8], mtime_seconds: i64) {
        self.state
            .lock()
            .expect("mock state lock")
            .mutation_after_listing = Some(PendingMutation {
            path: path.to_owned(),
            contents: contents.to_vec(),
            mtime_seconds,
        });
    }

    pub fn fail_entry_discovery_once(&self, status: u16) {
        self.inject_fault(InjectedFault {
            operation: "SYNO.API.Info.query".to_owned(),
            request_path: Some("/prefix/webapi/entry.cgi".to_owned()),
            phase: FaultPhase::BeforeOperation,
            response: FaultResponse::HttpStatus(status),
            remaining: 1,
        });
    }

    pub fn fail_next_http_operation(&self, operation: &str, status: u16) {
        self.inject_fault(InjectedFault {
            operation: operation.to_owned(),
            request_path: None,
            phase: FaultPhase::BeforeOperation,
            response: FaultResponse::HttpStatus(status),
            remaining: 1,
        });
    }

    pub fn fail_next_api_operation(&self, operation: &str, code: i64) {
        self.inject_fault(InjectedFault {
            operation: operation.to_owned(),
            request_path: None,
            phase: FaultPhase::BeforeOperation,
            response: FaultResponse::ApiError(code),
            remaining: 1,
        });
    }

    pub fn fail_next_upload_response_after_commit(&self, status: u16) {
        self.inject_fault(InjectedFault {
            operation: "SYNO.FileStation.Upload.upload".to_owned(),
            request_path: None,
            phase: FaultPhase::AfterCommit,
            response: FaultResponse::HttpStatus(status),
            remaining: 1,
        });
    }

    fn inject_fault(&self, fault: InjectedFault) {
        self.state
            .lock()
            .expect("mock state lock")
            .faults
            .push(fault);
    }

    pub fn pending_faults(&self) -> usize {
        self.state
            .lock()
            .expect("mock state lock")
            .faults
            .iter()
            .map(|fault| fault.remaining)
            .sum()
    }

    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.state.lock().expect("mock state lock").requests.clone()
    }

    pub fn directories(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("mock state lock")
            .directories
            .iter()
            .cloned()
            .collect()
    }

    pub fn file_contents(&self, path: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .expect("mock state lock")
            .files
            .get(path)
            .map(|file| file.contents.clone())
    }

    pub fn file_paths(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("mock state lock")
            .files
            .keys()
            .cloned()
            .collect()
    }
}

impl Drop for MockFileStation {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("mock File Station worker panicked");
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &Arc<Mutex<ServerState>>) {
    let (request_path, headers, body) = read_request(&mut stream);
    let content_type = headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("");
    let (fields, upload) = if content_type.starts_with("application/x-www-form-urlencoded") {
        (parse_urlencoded(&body), None)
    } else if content_type.starts_with("multipart/form-data") {
        parse_multipart(content_type, &body)
    } else {
        (BTreeMap::new(), None)
    };
    let api = fields.get("api").cloned().unwrap_or_default();
    let method = fields.get("method").cloned().unwrap_or_default();
    let captured = CapturedRequest {
        request_path,
        headers: headers.clone(),
        api: api.clone(),
        method: method.clone(),
        fields: fields.clone(),
        upload_filename: upload.as_ref().map(|file| file.filename.clone()),
        upload_bytes: upload.as_ref().map(|file| file.contents.len()),
    };

    let response = {
        let mut state = state.lock().expect("mock state lock");
        let captured_path = captured.request_path.clone();
        state.requests.push(captured);
        route_request(
            &mut state,
            &captured_path,
            &api,
            &method,
            &fields,
            &headers,
            upload,
        )
    };
    write_response(&mut stream, response);
}

enum MockResponse {
    Json(Value),
    JsonWithCookie { value: Value, cookie: String },
    Raw(String),
    Bytes(Vec<u8>),
    HttpStatus { status: u16, body: String },
}

fn route_request(
    state: &mut ServerState,
    request_path: &str,
    api: &str,
    method: &str,
    fields: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
    upload: Option<MultipartFile>,
) -> MockResponse {
    if let Some(response) = take_injected_fault(
        state,
        request_path,
        api,
        method,
        FaultPhase::BeforeOperation,
    ) {
        return response;
    }
    if api == "SYNO.API.Info" && method == "query" {
        return MockResponse::Json(discovery());
    }
    if api == "SYNO.API.Auth" && method == "login" {
        if let Some(marker) = &state.reflected_login_failure {
            return MockResponse::Raw(format!(
                "<html>reflected passwd={marker}&otp_code=654321</html>"
            ));
        }
        if fields.get("account") != Some(&state.expected_account)
            || fields.get("passwd") != Some(&state.expected_password)
        {
            return api_error(400);
        }
        if state.require_totp {
            match fields.get("otp_code") {
                None => return api_error_with_marker(403, "challenge-token-must-not-leak"),
                Some(code) if code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit()) => {
                    if std::mem::take(&mut state.reject_next_valid_otp) {
                        return api_error_with_marker(404, "rejected-otp-must-not-leak");
                    }
                }
                Some(_) => return api_error_with_marker(404, "rejected-otp-must-not-leak"),
            }
        }
        let data = json!({"sid": SESSION_ID, "synotoken": SYNO_TOKEN});
        return match &state.login_cookie {
            Some(cookie) => success_with_cookie(data, cookie),
            None => success(data),
        };
    }
    if api == "SYNO.API.Auth" && method == "logout" {
        return authenticated(state, fields, headers, || success(Value::Null));
    }

    if !valid_session(state, fields, headers) {
        return api_error(119);
    }
    match (api, method) {
        ("SYNO.FileStation.List", "list_share") => {
            let mut shares = state
                .directories
                .iter()
                .filter_map(|path| {
                    let name = path.strip_prefix('/')?;
                    (!name.is_empty() && !name.contains('/')).then(|| {
                        let disable_list = state.list_disabled_directories.contains(path);
                        json!({
                            "name": name,
                            "path": path,
                            "disable_list": disable_list,
                            "additional": {"perm": {
                                "adv_right": {"disable_list": disable_list},
                                "acl": {"read": !disable_list, "exec": !disable_list}
                            }}
                        })
                    })
                })
                .collect::<Vec<_>>();
            shares.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
            let total = shares.len();
            let offset = fields
                .get("offset")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let limit = fields
                .get("limit")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(total);
            let shares = shares
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect::<Vec<_>>();
            success(json!({"total": total, "shares": shares}))
        }
        ("SYNO.FileStation.List", "getinfo") => {
            let path = first_json_string(fields.get("path"));
            match path.and_then(|path| node_value(state, &path)) {
                Some(node) => success(json!({"files": [node]})),
                None => api_error(408),
            }
        }
        ("SYNO.FileStation.List", "list") => {
            let folder = json_string(fields.get("folder_path")).unwrap_or_default();
            if !state.directories.contains(&folder) {
                return api_error(408);
            }
            let files = direct_children(state, &folder);
            let response = success(json!({"total": files.len(), "files": files}));
            if let Some(mutation) = state.mutation_after_listing.take()
                && state.files.contains_key(&mutation.path)
            {
                state.files.insert(
                    mutation.path,
                    FileNode {
                        contents: mutation.contents,
                        mtime_seconds: mutation.mtime_seconds,
                    },
                );
            }
            response
        }
        ("SYNO.FileStation.CheckPermission", "write") => success(Value::Null),
        ("SYNO.FileStation.CreateFolder", "create") => {
            let Some(parent) = first_json_string(fields.get("folder_path")) else {
                return api_error(101);
            };
            let Some(name) = first_json_string(fields.get("name")) else {
                return api_error(101);
            };
            let path = format!("{}/{}", parent.trim_end_matches('/'), name);
            if fields.get("force_parent").map(String::as_str) == Some("true") {
                insert_directory_tree(&mut state.directories, &parent);
            }
            if !state.directories.contains(&parent)
                || state.directories.contains(&path)
                || state.files.contains_key(&path)
            {
                return api_error(414);
            }
            state.directories.insert(path);
            success(Value::Null)
        }
        ("SYNO.FileStation.Upload", "upload") => {
            let Some(upload) = upload else {
                return api_error(1802);
            };
            let Some(parent) = fields.get("path") else {
                return api_error(101);
            };
            if fields.get("create_parents").map(String::as_str) == Some("true") {
                insert_directory_tree(&mut state.directories, parent);
            }
            if !state.directories.contains(parent) {
                return api_error(408);
            }
            let path = format!("{}/{}", parent.trim_end_matches('/'), upload.filename);
            if fields.get("overwrite").map(String::as_str) != Some("true")
                && state.files.contains_key(&path)
            {
                return api_error(414);
            }
            let mtime_seconds = fields
                .get("mtime")
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0)
                .div_euclid(1000);
            state.files.insert(
                path,
                FileNode {
                    contents: upload.contents,
                    mtime_seconds,
                },
            );
            if let Some(response) =
                take_injected_fault(state, request_path, api, method, FaultPhase::AfterCommit)
            {
                return response;
            }
            success(Value::Null)
        }
        ("SYNO.FileStation.MD5", "start") => {
            let Some(path) = json_string(fields.get("file_path")) else {
                return api_error(101);
            };
            if !state.files.contains_key(&path) {
                return api_error(408);
            }
            let task = format!("md5-task-{}", state.next_task);
            state.next_task += 1;
            state.md5_tasks.insert(task.clone(), path);
            success(json!({"taskid": task}))
        }
        ("SYNO.FileStation.MD5", "status") => {
            let Some(task) = json_string(fields.get("taskid")) else {
                return api_error(101);
            };
            let Some(path) = state.md5_tasks.get(&task) else {
                return api_error(408);
            };
            let Some(file) = state.files.get(path) else {
                return api_error(408);
            };
            let digest = Md5::digest(&file.contents);
            let digest = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            success(json!({"finished": true, "md5": digest}))
        }
        ("SYNO.FileStation.Download", "download") => {
            let Some(path) = first_json_string(fields.get("path")) else {
                return api_error(101);
            };
            match state.files.get(&path) {
                Some(file) => MockResponse::Bytes(file.contents.clone()),
                None => api_error(408),
            }
        }
        ("SYNO.FileStation.CopyMove", "start") => {
            let Some(source) = first_json_string(fields.get("path")) else {
                return api_error(101);
            };
            let Some(destination_parent) = json_string(fields.get("dest_folder_path")) else {
                return api_error(101);
            };
            let Some(source_file) = state.files.get(&source).cloned() else {
                return api_error(408);
            };
            if !state.directories.contains(&destination_parent) {
                return api_error(408);
            }
            let name = source.rsplit('/').next().unwrap_or(&source);
            let destination = format!("{}/{}", destination_parent.trim_end_matches('/'), name);
            if state.files.contains_key(&destination) || state.directories.contains(&destination) {
                return api_error(414);
            }
            state.files.insert(destination, source_file);
            let task = format!("copy-task-{}", state.next_task);
            state.next_task += 1;
            state.copy_tasks.insert(task.clone());
            success(json!({"taskid": task}))
        }
        ("SYNO.FileStation.CopyMove", "status") => {
            let Some(task) = json_string(fields.get("taskid")) else {
                return api_error(101);
            };
            if !state.copy_tasks.contains(&task) {
                return api_error(408);
            }
            success(json!({"finished": true}))
        }
        ("SYNO.FileStation.Delete", "delete") => {
            let Some(path) = first_json_string(fields.get("path")) else {
                return api_error(101);
            };
            if state.files.remove(&path).is_some() {
                return success(Value::Null);
            }
            if state.directories.contains(&path) {
                let prefix = format!("{}/", path.trim_end_matches('/'));
                if state
                    .directories
                    .iter()
                    .any(|child| child.starts_with(&prefix))
                    || state.files.keys().any(|child| child.starts_with(&prefix))
                {
                    return api_error(416);
                }
                state.directories.remove(&path);
                return success(Value::Null);
            }
            api_error(408)
        }
        _ => api_error(103),
    }
}

fn take_injected_fault(
    state: &mut ServerState,
    request_path: &str,
    api: &str,
    method: &str,
    phase: FaultPhase,
) -> Option<MockResponse> {
    let operation = format!("{api}.{method}");
    let fault = state.faults.iter_mut().find(|fault| {
        fault.remaining > 0
            && fault.phase == phase
            && fault.operation == operation
            && fault
                .request_path
                .as_deref()
                .is_none_or(|expected| expected == request_path)
    })?;
    fault.remaining -= 1;
    Some(match fault.response {
        FaultResponse::HttpStatus(status) => MockResponse::HttpStatus {
            status,
            body: "injected transient mock failure".to_owned(),
        },
        FaultResponse::ApiError(code) => api_error(code),
    })
}

fn authenticated(
    state: &ServerState,
    fields: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
    operation: impl FnOnce() -> MockResponse,
) -> MockResponse {
    if valid_session(state, fields, headers) {
        operation()
    } else {
        api_error(119)
    }
}

fn valid_session(
    state: &ServerState,
    fields: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
) -> bool {
    let valid_fields = fields.get("_sid").map(String::as_str) == Some(SESSION_ID)
        && fields.get("SynoToken").map(String::as_str) == Some(SYNO_TOKEN);
    let valid_headers = !state.require_header_session_transport
        || (headers.get("cookie").map(String::as_str) == Some("id=e2e-session-secret")
            && headers.get("x-syno-token").map(String::as_str) == Some(SYNO_TOKEN));
    valid_fields && valid_headers
}

fn discovery() -> Value {
    json!({
        "success": true,
        "data": {
            "SYNO.API.Auth": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 7},
            "SYNO.FileStation.Info": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
            "SYNO.FileStation.List": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
            "SYNO.FileStation.CreateFolder": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
            "SYNO.FileStation.Upload": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
            "SYNO.FileStation.Delete": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
            "SYNO.FileStation.MD5": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
            "SYNO.FileStation.Download": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2},
            "SYNO.FileStation.CopyMove": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 3},
            "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3}
        }
    })
}

fn success(data: Value) -> MockResponse {
    MockResponse::Json(json!({"success": true, "data": data}))
}

fn success_with_cookie(data: Value, cookie: &str) -> MockResponse {
    MockResponse::JsonWithCookie {
        value: json!({"success": true, "data": data}),
        cookie: cookie.to_owned(),
    }
}

fn api_error(code: i64) -> MockResponse {
    MockResponse::Json(json!({
        "success": false,
        "error": {"code": code, "errors": {"mock": "redacted authenticated detail"}}
    }))
}

fn api_error_with_marker(code: i64, marker: &str) -> MockResponse {
    MockResponse::Json(json!({
        "success": false,
        "error": {"code": code, "errors": {"token": marker}}
    }))
}

fn node_value(state: &ServerState, path: &str) -> Option<Value> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if state.directories.contains(path) {
        return Some(json!({
            "path": path,
            "name": name,
            "isdir": true,
            "additional": {"mount_point_type": ""}
        }));
    }
    state.files.get(path).map(|file| {
        json!({
            "path": path,
            "name": name,
            "isdir": false,
            "additional": {
                "size": file.contents.len(),
                "time": {"mtime": file.mtime_seconds},
                "mount_point_type": ""
            }
        })
    })
}

fn direct_children(state: &ServerState, folder: &str) -> Vec<Value> {
    let prefix = format!("{}/", folder.trim_end_matches('/'));
    let mut children = state
        .directories
        .iter()
        .filter(|path| path.starts_with(&prefix) && !path[prefix.len()..].contains('/'))
        .filter_map(|path| node_value(state, path))
        .chain(
            state
                .files
                .keys()
                .filter(|path| path.starts_with(&prefix) && !path[prefix.len()..].contains('/'))
                .filter_map(|path| node_value(state, path)),
        )
        .collect::<Vec<_>>();
    children.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    children
}

fn insert_directory_tree(directories: &mut BTreeSet<String>, path: &str) {
    let mut current = String::new();
    for component in path.split('/').filter(|component| !component.is_empty()) {
        current.push('/');
        current.push_str(component);
        directories.insert(current.clone());
    }
}

#[derive(Debug)]
struct MultipartFile {
    filename: String,
    contents: Vec<u8>,
}

fn parse_multipart(
    content_type: &str,
    body: &[u8],
) -> (BTreeMap<String, String>, Option<MultipartFile>) {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("boundary="))
        .map(|value| value.trim_matches('"'))
        .expect("multipart request has a boundary");
    let delimiter = format!("--{boundary}").into_bytes();
    let mut fields = BTreeMap::new();
    let mut upload = None;
    for raw_part in split_bytes(body, &delimiter).into_iter().skip(1) {
        if raw_part.starts_with(b"--") {
            break;
        }
        let part = raw_part.strip_prefix(b"\r\n").unwrap_or(raw_part);
        let part = part.strip_suffix(b"\r\n").unwrap_or(part);
        let Some(header_end) = find_bytes(part, b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8(part[..header_end].to_vec())
            .expect("multipart headers are valid UTF-8");
        let disposition = headers
            .lines()
            .find(|line| {
                line.to_ascii_lowercase()
                    .starts_with("content-disposition:")
            })
            .expect("multipart part has Content-Disposition");
        let name = disposition_parameter(disposition, "name").expect("multipart part has a name");
        let contents = part[header_end + 4..].to_vec();
        if let Some(filename) = disposition_parameter(disposition, "filename") {
            upload = Some(MultipartFile { filename, contents });
        } else {
            fields.insert(
                name,
                String::from_utf8(contents).expect("multipart text field is valid UTF-8"),
            );
        }
    }
    (fields, upload)
}

fn disposition_parameter(line: &str, name: &str) -> Option<String> {
    line.split(';').map(str::trim).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key == name).then(|| value.trim_matches('"').to_owned())
    })
}

fn split_bytes<'a>(haystack: &'a [u8], needle: &[u8]) -> Vec<&'a [u8]> {
    let mut output = Vec::new();
    let mut start = 0;
    while let Some(relative) = find_bytes(&haystack[start..], needle) {
        let end = start + relative;
        output.push(&haystack[start..end]);
        start = end + needle.len();
    }
    output.push(&haystack[start..]);
    output
}

fn read_request(stream: &mut TcpStream) -> (String, BTreeMap<String, String>, Vec<u8>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set mock read timeout");
    let mut received = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 8192];
        let count = stream.read(&mut buffer).expect("read request headers");
        assert!(count > 0, "client closed before request headers completed");
        received.extend_from_slice(&buffer[..count]);
        if let Some(position) = find_bytes(&received, b"\r\n\r\n") {
            break position + 4;
        }
    };
    let header_text = String::from_utf8(received[..header_end].to_vec())
        .expect("request headers are valid UTF-8");
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().expect("request line");
    let request_path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request path")
        .to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while received.len() - header_end < content_length {
        let mut buffer = [0_u8; 8192];
        let count = stream.read(&mut buffer).expect("read request body");
        assert!(count > 0, "client closed before request body completed");
        received.extend_from_slice(&buffer[..count]);
    }
    (
        request_path,
        headers,
        received[header_end..header_end + content_length].to_vec(),
    )
}

fn write_response(stream: &mut TcpStream, response: MockResponse) {
    let (status, reason, content_type, cookie, body) = match response {
        MockResponse::Json(value) => (
            200,
            "OK",
            "application/json",
            None,
            value.to_string().into_bytes(),
        ),
        MockResponse::JsonWithCookie { value, cookie } => (
            200,
            "OK",
            "application/json",
            Some(cookie),
            value.to_string().into_bytes(),
        ),
        MockResponse::Raw(body) => (200, "OK", "text/html", None, body.into_bytes()),
        MockResponse::Bytes(body) => (200, "OK", "application/octet-stream", None, body),
        MockResponse::HttpStatus { status, body } => (
            status,
            http_reason(status),
            "text/plain",
            None,
            body.into_bytes(),
        ),
    };
    let cookie_header = cookie
        .map(|cookie| format!("Set-Cookie: {cookie}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n{cookie_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("write mock response");
    stream.write_all(&body).expect("write mock response body");
    stream.flush().expect("flush mock response");
}

fn http_reason(status: u16) -> &'static str {
    match status {
        408 => "Request Timeout",
        425 => "Too Early",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Injected Failure",
    }
}

fn parse_urlencoded(body: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|field| field.split_once('='))
        .map(|(name, value)| (url_decode(name), url_decode(value)))
        .collect()
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1]);
                let low = hex(bytes[index + 2]);
                if let (Some(high), Some(low)) = (high, low) {
                    output.push(high * 16 + low);
                    index += 3;
                } else {
                    output.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).expect("form field is valid UTF-8")
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn json_string(value: Option<&String>) -> Option<String> {
    serde_json::from_str::<String>(value?).ok()
}

fn first_json_string(value: Option<&String>) -> Option<String> {
    serde_json::from_str::<Vec<String>>(value?)
        .ok()
        .and_then(|mut values| values.drain(..).next())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
