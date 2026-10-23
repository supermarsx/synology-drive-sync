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
    /// Operation whose *successful* response also sets a cookie, as DSM does when it rotates a
    /// session mid-run.
    rotate_session_on: Option<(String, String)>,
    /// Whether the configured rotation is still owed. Armed by each login and spent by the first
    /// matching response after it.
    rotation_pending: bool,
    require_header_session_transport: bool,
    require_totp: bool,
    reject_next_valid_otp: bool,
    next_task: u64,
    md5_tasks: BTreeMap<String, String>,
    /// How many status calls each MD5 task reports unfinished before returning a digest.
    md5_unfinished_statuses: u32,
    /// Status calls already answered per MD5 task, so the count above applies per task.
    md5_status_calls: BTreeMap<String, u32>,
    copy_tasks: BTreeSet<String>,
    mutation_after_listing: Option<PendingMutation>,
    faults: Vec<InjectedFault>,
    /// What `SYNO.FileStation.Info.get` reports as the host serving the request.
    hostname: String,
    /// Whether `query=all` also advertises an entry the documented API map cannot describe.
    advertise_malformed_api: bool,
    /// Whether the cookie-borne session stops being resolvable after its first use.
    reject_cookie_sessions: bool,
    /// How many more cookie-borne requests this login may make. Re-armed by each login.
    cookie_uses_remaining: u32,
    /// Host name to switch to after the next `SYNO.FileStation.Info.get` is answered.
    next_hostname: Option<String>,
    /// Whether a second login invalidates the session the first one established.
    ///
    /// DSM binds one session per (account, session name) and both this client's logins name
    /// `session=FileStation`, so a duplicate login is a real collision with a dedicated error:
    /// `107`, "session interrupted by duplicate login". Modelling it is the only way to prove a
    /// diagnostic that opens a second session cannot break the run it is diagnosing.
    invalidate_on_duplicate_login: bool,
    /// How many logins this server has answered.
    logins_seen: u32,
    /// Incremented each time the session expires. Zero keeps the historical session identifier,
    /// so every existing test sees exactly the identifier it always did; later generations make
    /// each re-login issue a new one, the way DSM does.
    session_generation: u32,
    /// Authenticated requests left before the session expires, when expiry is armed.
    session_expires_after: Option<u32>,
    /// Entries per folder that `List.list` counts in `total` but does not return, the way
    /// Access-Based Enumeration hides what the account may not read.
    hidden_list_entries: BTreeMap<String, usize>,
    /// When set, `CopyMove` stamps the copy with this time instead of keeping the source's.
    copy_stamps_mtime: Option<i64>,
    /// Copy tasks that finish with an error member instead of a result.
    failing_copy_tasks: BTreeSet<String>,
    /// Whether the next `CopyMove.start` creates a task that finishes with an error.
    fail_next_copy: bool,
    /// Whether `Upload` rounds the millisecond `mtime` to the nearest second rather than flooring.
    round_upload_mtime: bool,
    /// ADV: refuse uploads/creates whose final name exceeds this many UTF-8 bytes (ext4/btrfs 255,
    /// eCryptfs ~143), answering with the given DSM code.
    refuse_names_over_bytes: Option<(usize, i64)>,
    /// ADV: copy tasks whose `status` fails with an envelope error, the shape DSM documents for
    /// CopyMove failures (codes 1000-1007), rather than an `error` member inside `data`.
    envelope_failing_copy_tasks: BTreeMap<String, i64>,
    fail_next_copy_envelope: Option<i64>,
    /// ADV: DSM ignores the upload `mtime` and stamps this time instead.
    upload_stamps_mtime: Option<i64>,
    /// ADV: expire the session when the first request for this operation arrives.
    expire_session_on: Option<String>,
    /// Folder symbolic links, link path to target folder, the way a link made over SSH inside a
    /// share appears to File Station: listed as a folder, and every path through it resolved to
    /// the target. Each entry's `real_path` (when asked for) is where it really is.
    symlinks: BTreeMap<String, String>,
    /// Paths whose `real_path` the mock leaves out even when asked, the way an entry DSM cannot
    /// resolve might be described.
    real_path_withheld: BTreeSet<String>,
    /// Whether every `real_path` is wrong: the entry's name under one unrelated folder, the shape a
    /// DSM whose real paths mean something other than the link guard assumes would produce.
    misreport_real_paths: bool,
    /// Actions to run once, when the first request for an operation arrives and before it is
    /// answered: the way a test changes the local tree at a known point in a run.
    request_hooks: Vec<RequestHook>,
}

struct RequestHook {
    operation: String,
    action: Box<dyn FnOnce() + Send>,
}

impl std::fmt::Debug for RequestHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestHook")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
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
            rotate_session_on: None,
            rotation_pending: false,
            require_header_session_transport: false,
            require_totp: false,
            reject_next_valid_otp: false,
            next_task: 1,
            md5_tasks: BTreeMap::new(),
            md5_unfinished_statuses: 0,
            md5_status_calls: BTreeMap::new(),
            copy_tasks: BTreeSet::new(),
            mutation_after_listing: None,
            faults: Vec::new(),
            hostname: "MOCKSTATION".to_owned(),
            advertise_malformed_api: false,
            reject_cookie_sessions: false,
            cookie_uses_remaining: 0,
            next_hostname: None,
            invalidate_on_duplicate_login: false,
            logins_seen: 0,
            session_generation: 0,
            session_expires_after: None,
            hidden_list_entries: BTreeMap::new(),
            copy_stamps_mtime: None,
            failing_copy_tasks: BTreeSet::new(),
            fail_next_copy: false,
            round_upload_mtime: false,
            refuse_names_over_bytes: None,
            envelope_failing_copy_tasks: BTreeMap::new(),
            fail_next_copy_envelope: None,
            upload_stamps_mtime: None,
            expire_session_on: None,
            symlinks: BTreeMap::new(),
            real_path_withheld: BTreeSet::new(),
            misreport_real_paths: false,
            request_hooks: Vec::new(),
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

    /// Stop resolving the cookie-borne session after its first use.
    ///
    /// This reproduces the reported live failure exactly: login succeeds, the first authenticated
    /// call succeeds, and every later call that presents the SID as an `id` cookie answers 119 --
    /// while the documented `_sid` request field keeps working. Without a mode like this the
    /// cookie can only ever *help*, so the whole bug class is untestable by construction.
    pub fn reject_cookie_sessions_after_first_use(&self) {
        self.state
            .lock()
            .expect("mock state lock")
            .reject_cookie_sessions = true;
    }

    /// Invalidate the first session as soon as a second login arrives, the way DSM's error 107
    /// describes.
    ///
    /// The newest session is the one established without `enable_syno_token`, so it is the one
    /// with no `SynoToken`; every later request that still carries one belongs to the session the
    /// duplicate login displaced and is answered `107`.
    pub fn invalidate_session_on_duplicate_login(&self) {
        self.state
            .lock()
            .expect("mock state lock")
            .invalidate_on_duplicate_login = true;
    }

    /// How many logins this server has answered.
    pub fn logins_seen(&self) -> u32 {
        self.state.lock().expect("mock state lock").logins_seen
    }

    /// Expire the session after `requests` more authenticated requests, the way DSM's idle timer
    /// does: every later request carrying it is answered `119`, and the next login issues a new
    /// session identifier.
    #[allow(dead_code)]
    pub fn expire_session_after(&self, requests: u32) {
        self.state
            .lock()
            .expect("mock state lock")
            .session_expires_after = Some(requests);
    }

    /// Count `hidden` entries in `folder`'s `total` without returning them, as Access-Based
    /// Enumeration does for entries the account may not read.
    #[allow(dead_code)]
    pub fn hide_list_entries(&self, folder: &str, hidden: usize) {
        self.state
            .lock()
            .expect("mock state lock")
            .hidden_list_entries
            .insert(folder.to_owned(), hidden);
    }

    /// Make `CopyMove` stamp every copy with `mtime_seconds` rather than keep the source's time.
    #[allow(dead_code)]
    pub fn stamp_copies_with(&self, mtime_seconds: i64) {
        self.state
            .lock()
            .expect("mock state lock")
            .copy_stamps_mtime = Some(mtime_seconds);
    }

    /// Make the next `CopyMove` task finish with an error member, having copied nothing.
    #[allow(dead_code)]
    pub fn fail_next_copy_task(&self) {
        self.state.lock().expect("mock state lock").fail_next_copy = true;
    }

    /// Make `Upload` round the millisecond `mtime` to the nearest second, as a DSM might.
    #[allow(dead_code)]
    pub fn round_upload_mtime(&self) {
        self.state
            .lock()
            .expect("mock state lock")
            .round_upload_mtime = true;
    }

    #[allow(dead_code)]
    pub fn refuse_names_over_bytes(&self, limit: usize, code: i64) {
        self.state
            .lock()
            .expect("mock state lock")
            .refuse_names_over_bytes = Some((limit, code));
    }

    #[allow(dead_code)]
    pub fn fail_next_copy_status_with_envelope(&self, code: i64) {
        self.state
            .lock()
            .expect("mock state lock")
            .fail_next_copy_envelope = Some(code);
    }

    #[allow(dead_code)]
    pub fn expire_session_on(&self, operation: &str) {
        self.state
            .lock()
            .expect("mock state lock")
            .expire_session_on = Some(operation.to_owned());
    }

    #[allow(dead_code)]
    pub fn stamp_uploads_with(&self, mtime_seconds: i64) {
        self.state
            .lock()
            .expect("mock state lock")
            .upload_stamps_mtime = Some(mtime_seconds);
    }

    /// Make `link` a symbolic link to the folder `target`, the way `ln -s` over SSH does. Opt-in:
    /// without one, every path resolves to itself.
    #[allow(dead_code)]
    pub fn add_directory_symlink(&self, link: &str, target: &str) {
        self.state
            .lock()
            .expect("mock state lock")
            .symlinks
            .insert(link.to_owned(), target.to_owned());
    }

    /// Report every entry's `real_path` wrongly. See the field.
    #[allow(dead_code)]
    pub fn misreport_real_paths(&self) {
        self.state
            .lock()
            .expect("mock state lock")
            .misreport_real_paths = true;
    }

    /// Describe `path` without a `real_path`, even when one is asked for.
    #[allow(dead_code)]
    pub fn withhold_real_path(&self, path: &str) {
        self.state
            .lock()
            .expect("mock state lock")
            .real_path_withheld
            .insert(path.to_owned());
    }

    /// Run `action` once, when the next `operation` request arrives and before it is answered.
    #[allow(dead_code)]
    pub fn on_next_request(&self, operation: &str, action: impl FnOnce() + Send + 'static) {
        self.state
            .lock()
            .expect("mock state lock")
            .request_hooks
            .push(RequestHook {
                operation: operation.to_owned(),
                action: Box::new(action),
            });
    }

    /// The modification time the mock holds for a file.
    #[allow(dead_code)]
    pub fn file_mtime(&self, path: &str) -> Option<i64> {
        self.state
            .lock()
            .expect("mock state lock")
            .files
            .get(path)
            .map(|file| file.mtime_seconds)
    }

    /// Advertise an API entry the documented map cannot describe, in the `query=all` response
    /// only. A strict decoder fails the whole map on one of these; the diagnostic read must not.
    pub fn advertise_malformed_api(&self) {
        self.state
            .lock()
            .expect("mock state lock")
            .advertise_malformed_api = true;
    }

    /// Answer the next `SYNO.FileStation.Info.get` under a different host name.
    ///
    /// This is what a path with no session affinity looks like from the client's side, and it is
    /// the one observation that proves consecutive requests reached different DSM hosts.
    pub fn change_hostname_after_next_info_read(&self, hostname: &str) {
        self.state.lock().expect("mock state lock").next_hostname = Some(hostname.to_owned());
    }

    /// Make every MD5 task report unfinished for `count` status calls before it completes.
    ///
    /// Without this the mock answers the first status with a digest, so a client's polling
    /// path is never exercised at all -- the sleep between polls, and any backoff applied to
    /// it, would be dead code in every test that looks like it covers server-side MD5.
    #[allow(dead_code)]
    pub fn defer_md5_completion(&self, count: u32) {
        self.state
            .lock()
            .expect("mock state lock")
            .md5_unfinished_statuses = count;
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

    /// Make `operation`'s successful response carry `cookie`, the way DSM rotates a session.
    ///
    /// This is the case that decides whether a client holding the previous identifier goes stale,
    /// so instrumentation has to observe it on an ordinary authenticated call, not only on login.
    /// Re-issue the session cookie once per login, on the first response to this operation.
    ///
    /// Once per login rather than on every matching response: DSM hands out a new identifier and
    /// then stops setting one. Re-setting it on every later response would make the ledger's
    /// counts a function of how many requests the diagnostic happens to make.
    pub fn rotate_session_on(&self, operation: &str, cookie: &str) {
        let mut state = self.state.lock().expect("mock state lock");
        state.rotate_session_on = Some((operation.to_owned(), cookie.to_owned()));
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

    /// Answer the next `times` requests for `operation` with DSM error `code`, before acting.
    #[allow(dead_code)]
    pub fn fail_api_operation_times(&self, operation: &str, code: i64, times: usize) {
        self.inject_fault(InjectedFault {
            operation: operation.to_owned(),
            request_path: None,
            phase: FaultPhase::BeforeOperation,
            response: FaultResponse::ApiError(code),
            remaining: times,
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

    /// Every DSM API request this server answered.
    ///
    /// The doctor's transport probe issues plain HTTP requests that carry no DSM API fields, so
    /// they are not DSM API requests and do not appear here. [`Self::connections`] returns the
    /// unfiltered list for the checks that are about the probe itself.
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.state
            .lock()
            .expect("mock state lock")
            .requests
            .iter()
            .filter(|request| !request.api.is_empty())
            .cloned()
            .collect()
    }

    /// Every request this server answered, DSM API call and transport probe alike.
    pub fn connections(&self) -> Vec<CapturedRequest> {
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
    // A connection that carried no request is not a request: it is recorded nowhere and answered
    // with nothing, exactly as a real server would leave it.
    let Some((request_path, headers, body)) = read_request(&mut stream) else {
        return;
    };
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
        let operation = format!("{api}.{method}");
        if let Some(index) = state
            .request_hooks
            .iter()
            .position(|hook| hook.operation == operation)
        {
            (state.request_hooks.remove(index).action)();
        }
        let captured_path = captured.request_path.clone();
        state.requests.push(captured);
        let routed = route_request(
            &mut state,
            &captured_path,
            &api,
            &method,
            &fields,
            &headers,
            upload,
        );
        rotate_session_if_configured(&mut state, &api, &method, routed)
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
        // `query=all` is a different question from the ten-name allowlist query, and this mock
        // answers it differently so a test can tell which one the client asked.
        return if fields.get("query").map(String::as_str) == Some("all") {
            MockResponse::Json(full_api_map(state.advertise_malformed_api))
        } else {
            MockResponse::Json(discovery())
        };
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
        state.logins_seen = state.logins_seen.saturating_add(1);
        // A `SynoToken` is issued only to a login that asked for one, the way DSM does it. The
        // ablation's tokenless variant depends on this: a mock that handed out a token regardless
        // would let a client that had stopped omitting `enable_syno_token` pass unnoticed.
        let sid = current_session_id(state);
        let data = if fields.get("enable_syno_token").map(String::as_str) == Some("yes") {
            json!({"sid": sid, "synotoken": SYNO_TOKEN})
        } else {
            json!({"sid": sid})
        };
        state.rotation_pending = state.rotate_session_on.is_some();
        state.cookie_uses_remaining = 1;
        return match &state.login_cookie {
            Some(cookie) => success_with_cookie(data, cookie),
            None => success(data),
        };
    }
    if api == "SYNO.API.Auth" && method == "logout" {
        return authenticated(state, fields, headers, || success(Value::Null));
    }

    // A session displaced by a duplicate login, answered the way DSM answers it. Placed before
    // the ordinary session check so the run sees `107` rather than a generic `119`.
    if state.invalidate_on_duplicate_login
        && state.logins_seen > 1
        && fields.contains_key("SynoToken")
    {
        return api_error(107);
    }
    if state.expire_session_on.as_deref() == Some(&format!("{api}.{method}")) {
        state.expire_session_on = None;
        state.session_generation += 1;
    }
    if !valid_session(state, fields, headers) {
        return api_error(119);
    }
    if let Some(remaining) = state.session_expires_after {
        if remaining == 0 {
            state.session_expires_after = None;
            state.session_generation += 1;
            return api_error(119);
        }
        state.session_expires_after = Some(remaining - 1);
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
            let real_path = wants_real_path(fields);
            match path.and_then(|path| listed_node(state, &path, real_path)) {
                Some(node) => success(json!({"files": [node]})),
                None => api_error(408),
            }
        }
        ("SYNO.FileStation.List", "list") => {
            let folder = json_string(fields.get("folder_path")).unwrap_or_default();
            if !state.directories.contains(&resolve_links(state, &folder)) {
                return api_error(408);
            }
            // Paged the way DSM pages: `offset` and `limit` select from a stable order, and
            // `total` counts the whole directory, including entries DSM withholds from the page.
            let files = listed_children(state, &folder, wants_real_path(fields));
            let hidden = state.hidden_list_entries.get(&folder).copied().unwrap_or(0);
            let total = files.len() + hidden;
            let offset = fields
                .get("offset")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let limit = fields
                .get("limit")
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|limit| *limit > 0)
                .unwrap_or(usize::MAX);
            let page = files
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect::<Vec<_>>();
            let response = success(json!({"total": total, "files": page}));
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
        ("SYNO.FileStation.Info", "get") => {
            let response = success(json!({
                "hostname": state.hostname,
                "is_manager": false,
                "support_sharing": true,
                "support_virtual_protocol": "cifs,nfs,iso",
            }));
            if let Some(hostname) = state.next_hostname.take() {
                state.hostname = hostname;
            }
            response
        }
        ("SYNO.FileStation.VirtualFolder", "list") => success(json!({"total": 0, "folders": []})),
        ("SYNO.FileStation.BackgroundTask", "list") => success(json!({"total": 0, "tasks": []})),
        ("SYNO.FileStation.CreateFolder", "create") => {
            let Some(parent) = first_json_string(fields.get("folder_path")) else {
                return api_error(101);
            };
            let Some(name) = first_json_string(fields.get("name")) else {
                return api_error(101);
            };
            let parent = resolve_links(state, &parent);
            if let Some((limit, code)) = state.refuse_names_over_bytes
                && name.len() > limit
            {
                return api_error(code);
            }
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
            let parent = &resolve_links(state, parent);
            if let Some((limit, code)) = state.refuse_names_over_bytes
                && upload.filename.len() > limit
            {
                return api_error(code);
            }
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
            let mtime_ms = fields
                .get("mtime")
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0);
            let mtime_seconds = if let Some(stamp) = state.upload_stamps_mtime {
                stamp
            } else if state.round_upload_mtime {
                (mtime_ms + 500).div_euclid(1000)
            } else {
                mtime_ms.div_euclid(1000)
            };
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
            let Some(path) = state.md5_tasks.get(&task).cloned() else {
                return api_error(408);
            };
            let answered = state.md5_status_calls.entry(task.clone()).or_insert(0);
            *answered += 1;
            if *answered <= state.md5_unfinished_statuses {
                return success(json!({"finished": false}));
            }
            let Some(file) = state.files.get(&path) else {
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
            let task = format!("copy-task-{}", state.next_task);
            state.next_task += 1;
            state.copy_tasks.insert(task.clone());
            if let Some(code) = state.fail_next_copy_envelope.take() {
                state.envelope_failing_copy_tasks.insert(task.clone(), code);
                return success(json!({"taskid": task}));
            }
            if std::mem::take(&mut state.fail_next_copy) {
                state.failing_copy_tasks.insert(task.clone());
                return success(json!({"taskid": task}));
            }
            let mut copy = source_file;
            if let Some(mtime_seconds) = state.copy_stamps_mtime {
                copy.mtime_seconds = mtime_seconds;
            }
            state.files.insert(destination, copy);
            success(json!({"taskid": task}))
        }
        ("SYNO.FileStation.CopyMove", "status") => {
            let Some(task) = json_string(fields.get("taskid")) else {
                return api_error(101);
            };
            if !state.copy_tasks.contains(&task) {
                return api_error(408);
            }
            if let Some(code) = state.envelope_failing_copy_tasks.get(&task) {
                return api_error(*code);
            }
            if state.failing_copy_tasks.contains(&task) {
                return success(json!({
                    "finished": true,
                    "error": {"code": 1000, "errors": [{"code": 1000, "path": "copy"}]}
                }));
            }
            success(json!({"finished": true}))
        }
        ("SYNO.FileStation.Rename", "rename") => {
            let Some(path) = first_json_string(fields.get("path")) else {
                return api_error(101);
            };
            let Some(name) = first_json_string(fields.get("name")) else {
                return api_error(101);
            };
            let path = resolve_links(state, &path);
            let Some((parent, _)) = path.rsplit_once('/') else {
                return api_error(400);
            };
            if name.is_empty() || name.contains('/') {
                return api_error(418);
            }
            let renamed = format!("{parent}/{name}");
            if renamed == path {
                return success(json!({"files": [{"path": renamed, "name": name}]}));
            }
            // Case-sensitive, as ext4 and btrfs are: a case-only rename is an ordinary rename,
            // and only an entry that already holds the exact new name refuses it.
            if state.files.contains_key(&renamed) || state.directories.contains(&renamed) {
                return api_error(414);
            }
            if let Some(file) = state.files.remove(&path) {
                state.files.insert(renamed.clone(), file);
            } else if state.directories.contains(&path) {
                let prefix = format!("{path}/");
                let moved_directories = state
                    .directories
                    .iter()
                    .filter(|candidate| **candidate == path || candidate.starts_with(&prefix))
                    .cloned()
                    .collect::<Vec<_>>();
                for directory in moved_directories {
                    state.directories.remove(&directory);
                    state
                        .directories
                        .insert(format!("{renamed}{}", &directory[path.len()..]));
                }
                let moved_files = state
                    .files
                    .keys()
                    .filter(|candidate| candidate.starts_with(&prefix))
                    .cloned()
                    .collect::<Vec<_>>();
                for file in moved_files {
                    let node = state.files.remove(&file).expect("listed above");
                    state
                        .files
                        .insert(format!("{renamed}{}", &file[path.len()..]), node);
                }
            } else {
                return api_error(408);
            }
            success(json!({"files": [{"path": renamed, "name": name}]}))
        }
        ("SYNO.FileStation.Delete", "delete") => {
            let Some(path) = first_json_string(fields.get("path")) else {
                return api_error(101);
            };
            let path = resolve_links(state, &path);
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
    state: &mut ServerState,
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

/// Whether this request presents a session the mock still recognises.
///
/// Takes `&mut` because rejecting a cookie-borne session is *destructive*: DSM deleting a session
/// record is what makes the reported failure permanent for the rest of a run, and a mock that only
/// ever answered one request badly could not reproduce it.
fn valid_session(
    state: &mut ServerState,
    fields: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
) -> bool {
    // The live-bug mode: a `format=sid` login is documented as "cookie will not be set", so a
    // request that nonetheless carries an `id` cookie asks DSM to resolve a cookie session it
    // never issued. This models a DSM that honours such a request once and then stops, which is
    // the sequence actually observed against a real NAS.
    if state.reject_cookie_sessions
        && headers
            .get("cookie")
            .is_some_and(|cookie| cookie.starts_with("id="))
    {
        if state.cookie_uses_remaining == 0 {
            return false;
        }
        state.cookie_uses_remaining -= 1;
    }
    let sid = current_session_id(state);
    let valid_fields = fields.get("_sid") == Some(&sid)
        && fields.get("SynoToken").map(String::as_str) == Some(SYNO_TOKEN);
    let valid_headers = !state.require_header_session_transport
        || (headers.get("cookie") == Some(&format!("id={sid}"))
            && headers.get("x-syno-token").map(String::as_str) == Some(SYNO_TOKEN));
    valid_fields && valid_headers
}

/// The session identifier the mock currently honours and issues.
fn current_session_id(state: &ServerState) -> String {
    match state.session_generation {
        0 => SESSION_ID.to_owned(),
        generation => format!("{SESSION_ID}-{generation}"),
    }
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
            "SYNO.FileStation.CheckPermission": {"path": "entry.cgi", "minVersion": 3, "maxVersion": 3},
            "SYNO.FileStation.Rename": {"path": "entry.cgi", "minVersion": 1, "maxVersion": 2}
        }
    })
}

/// The wider map DSM answers `query=all` with, as opposed to the ten-name allowlist query.
///
/// Deliberately a superset of [`discovery`]: it adds the File Station APIs this tool never calls,
/// two other namespaces, and -- when a test asks for it -- one entry whose value is not an object
/// at all, so the lenient enumeration path is exercised against a map a strict decoder would
/// reject outright.
fn full_api_map(advertise_malformed: bool) -> Value {
    let mut map = discovery();
    let entries = map["data"]
        .as_object_mut()
        .expect("discovery data is an object");
    for (name, min, max) in [
        ("SYNO.FileStation.Rename", 1, 2),
        ("SYNO.FileStation.VirtualFolder", 1, 2),
        ("SYNO.FileStation.BackgroundTask", 1, 3),
        ("SYNO.Core.System", 1, 1),
        ("SYNO.Core.Share", 1, 1),
        ("SYNO.DownloadStation.Task", 1, 3),
    ] {
        entries.insert(
            name.to_owned(),
            json!({"path": "entry.cgi", "minVersion": min, "maxVersion": max}),
        );
    }
    if advertise_malformed {
        entries.insert("SYNO.Broken.Api".to_owned(), json!(42));
    }
    map
}

fn success(data: Value) -> MockResponse {
    MockResponse::Json(json!({"success": true, "data": data}))
}

/// Attach a `Set-Cookie` to the first response to this operation after each login.
fn rotate_session_if_configured(
    state: &mut ServerState,
    api: &str,
    method: &str,
    response: MockResponse,
) -> MockResponse {
    let Some((operation, cookie)) = &state.rotate_session_on else {
        return response;
    };
    if !state.rotation_pending || *operation != format!("{api}.{method}") {
        return response;
    }
    let cookie = cookie.clone();
    state.rotation_pending = false;
    match response {
        MockResponse::Json(value) => MockResponse::JsonWithCookie { value, cookie },
        other => other,
    }
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

/// Whether the request asked for each entry's `real_path`.
fn wants_real_path(fields: &BTreeMap<String, String>) -> bool {
    fields
        .get("additional")
        .is_some_and(|additional| additional.contains("real_path"))
}

/// The path File Station really operates on: every folder link along `path` followed. Bounded, so
/// a link cycle resolves to *something* rather than hanging the mock.
fn resolve_links(state: &ServerState, path: &str) -> String {
    let mut resolved = path.to_owned();
    for _ in 0..32 {
        let Some((link, target)) = state
            .symlinks
            .iter()
            .find(|(link, _)| resolved == **link || resolved.starts_with(&format!("{link}/")))
        else {
            break;
        };
        resolved = format!("{target}{}", &resolved[link.len()..]);
    }
    resolved
}

/// One entry as File Station describes it at the path it was asked about, links followed.
fn listed_node(state: &ServerState, path: &str, real_path: bool) -> Option<Value> {
    let resolved = resolve_links(state, path);
    let mut node = node_value(state, &resolved)?;
    node["path"] = json!(path);
    node["name"] = json!(path.rsplit('/').next().unwrap_or(path));
    if real_path && !state.real_path_withheld.contains(path) {
        node["additional"]["real_path"] = if state.misreport_real_paths {
            let name = resolved.rsplit('/').next().unwrap_or(&resolved);
            json!(format!("/volume1/@misreported/{name}"))
        } else {
            json!(format!("/volume1{resolved}"))
        };
    }
    Some(node)
}

/// A folder's children as listed through `folder`, links followed, each with its listed path.
fn listed_children(state: &ServerState, folder: &str, real_path: bool) -> Vec<Value> {
    let resolved = resolve_links(state, folder);
    let prefix = format!("{}/", resolved.trim_end_matches('/'));
    let mut names = direct_children(state, &resolved)
        .iter()
        .filter_map(|child| child["name"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    names.extend(
        state
            .symlinks
            .keys()
            .filter(|link| link.starts_with(&prefix) && !link[prefix.len()..].contains('/'))
            .map(|link| link[prefix.len()..].to_owned()),
    );
    names
        .into_iter()
        .filter_map(|name| {
            listed_node(
                state,
                &format!("{}/{name}", folder.trim_end_matches('/')),
                real_path,
            )
        })
        .collect()
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

/// Read one request, or `None` when the peer opened a connection and sent nothing on it.
///
/// A transport reachability probe legitimately completes a TCP handshake and closes without
/// writing a byte, and every real HTTP server tolerates that. Treating it as a protocol violation
/// would model something no server does, and would make the mock fail on traffic the client is
/// entitled to send.
fn read_request(stream: &mut TcpStream) -> Option<(String, BTreeMap<String, String>, Vec<u8>)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set mock read timeout");
    let mut received = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 8192];
        let count = stream.read(&mut buffer).expect("read request headers");
        if count == 0 {
            assert!(
                received.is_empty(),
                "client closed part-way through a request header block"
            );
            return None;
        }
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
    Some((
        request_path,
        headers,
        received[header_end..header_end + content_length].to_vec(),
    ))
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
