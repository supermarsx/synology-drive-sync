//! Focused, secret-free runtime observability.
//!
//! Log records are closed enums plus numeric counters. There is intentionally no public
//! free-form message, header, token, URL, or key/value field. Bearer credentials can only be
//! loaded from an environment variable or file and are held in zeroizing memory.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use reqwest::redirect::Policy;
use zeroize::{Zeroize, Zeroizing};

const MAX_BEARER_TOKEN_BYTES: u64 = 16 * 1024;
const MAX_REMOTE_QUEUE: usize = 65_536;
const MAX_FILE_BACKUPS: usize = 32;

/// Severity threshold. A configured level includes that level and every more-severe level.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    #[default]
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl LogLevel {
    pub fn parse(value: &str) -> ObservabilityResult<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "warn" | "warning" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(ObservabilityError::InvalidLogLevel),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    fn permits(self, event: Self) -> bool {
        event <= self
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// User-facing verbosity when no explicit log level is selected.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
    VeryVerbose,
}

impl Verbosity {
    fn log_level(self) -> LogLevel {
        match self {
            Self::Quiet => LogLevel::Warn,
            Self::Normal => LogLevel::Info,
            Self::Verbose => LogLevel::Debug,
            Self::VeryVerbose => LogLevel::Trace,
        }
    }
}

/// Resolve log level with unambiguous precedence: explicit CLI value, then environment,
/// then the verbosity-derived default. Lower-precedence invalid values are not inspected.
pub fn resolve_log_level(
    explicit: Option<&str>,
    environment: Option<&str>,
    verbosity: Verbosity,
) -> ObservabilityResult<LogLevel> {
    if let Some(level) = explicit {
        LogLevel::parse(level)
    } else if let Some(level) = environment {
        LogLevel::parse(level)
    } else {
        Ok(verbosity.log_level())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Human,
    /// One complete JSON object per line.
    Json,
}

/// Stable machine-readable event codes. Human messages are derived from these codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCode {
    RunStarted,
    RunCompleted,
    RunFailed,
    LocalScanStarted,
    LocalScanCompleted,
    ApiDiscoveryStarted,
    ApiDiscoveryCompleted,
    AuthenticationStarted,
    AuthenticationCompleted,
    RemoteScanStarted,
    RemoteScanCompleted,
    PlanReady,
    UploadStarted,
    UploadAttemptStarted,
    UploadProgress,
    UploadCompleted,
    UploadFailed,
    DirectoryCreated,
    EntryDeleted,
    RetryScheduled,
    CancellationRequested,
}

impl EventCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run.started",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
            Self::LocalScanStarted => "local_scan.started",
            Self::LocalScanCompleted => "local_scan.completed",
            Self::ApiDiscoveryStarted => "api_discovery.started",
            Self::ApiDiscoveryCompleted => "api_discovery.completed",
            Self::AuthenticationStarted => "authentication.started",
            Self::AuthenticationCompleted => "authentication.completed",
            Self::RemoteScanStarted => "remote_scan.started",
            Self::RemoteScanCompleted => "remote_scan.completed",
            Self::PlanReady => "plan.ready",
            Self::UploadStarted => "upload.started",
            Self::UploadAttemptStarted => "upload.attempt_started",
            Self::UploadProgress => "upload.progress",
            Self::UploadCompleted => "upload.completed",
            Self::UploadFailed => "upload.failed",
            Self::DirectoryCreated => "directory.created",
            Self::EntryDeleted => "entry.deleted",
            Self::RetryScheduled => "retry.scheduled",
            Self::CancellationRequested => "cancellation.requested",
        }
    }

    fn human(self) -> &'static str {
        match self {
            Self::RunStarted => "sync run started",
            Self::RunCompleted => "sync run completed",
            Self::RunFailed => "sync run failed",
            Self::LocalScanStarted => "local scan started",
            Self::LocalScanCompleted => "local scan completed",
            Self::ApiDiscoveryStarted => "API discovery started",
            Self::ApiDiscoveryCompleted => "API discovery completed",
            Self::AuthenticationStarted => "authentication started",
            Self::AuthenticationCompleted => "authentication completed",
            Self::RemoteScanStarted => "remote scan started",
            Self::RemoteScanCompleted => "remote scan completed",
            Self::PlanReady => "sync plan ready",
            Self::UploadStarted => "upload started",
            Self::UploadAttemptStarted => "upload attempt started",
            Self::UploadProgress => "upload progress",
            Self::UploadCompleted => "upload completed",
            Self::UploadFailed => "upload failed",
            Self::DirectoryCreated => "directory created",
            Self::EntryDeleted => "entry deleted",
            Self::RetryScheduled => "retry scheduled",
            Self::CancellationRequested => "cancellation requested",
        }
    }
}

/// Optional aggregate counters attached to a log record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventMetrics {
    pub operations: u64,
    pub files: u64,
    pub bytes: u64,
    pub elapsed_ms: u64,
    pub throughput_bytes_per_second: u64,
    pub eta_ms: Option<u64>,
}

/// A log event with no free-form or secret-bearing fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogEvent {
    pub timestamp_ms: u64,
    pub level: LogLevel,
    pub code: EventCode,
    pub operation_id: Option<u64>,
    pub attempt: Option<u32>,
    pub metrics: EventMetrics,
}

impl LogEvent {
    pub fn new(level: LogLevel, code: EventCode) -> Self {
        Self {
            timestamp_ms: unix_timestamp_ms(),
            level,
            code,
            operation_id: None,
            attempt: None,
            metrics: EventMetrics::default(),
        }
    }

    pub fn operation(mut self, operation_id: u64) -> Self {
        self.operation_id = Some(operation_id);
        self
    }

    pub fn attempt(mut self, attempt: u32) -> Self {
        self.attempt = Some(attempt);
        self
    }

    pub fn metrics(mut self, metrics: EventMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    fn json_value(self) -> serde_json::Value {
        serde_json::json!({
            "schema": "sdsync.log.v1",
            "timestamp_ms": self.timestamp_ms,
            "level": self.level.as_str(),
            "event": self.code.as_str(),
            "operation_id": self.operation_id,
            "attempt": self.attempt,
            "metrics": {
                "operations": self.metrics.operations,
                "files": self.metrics.files,
                "bytes": self.metrics.bytes,
                "elapsed_ms": self.metrics.elapsed_ms,
                "throughput_bytes_per_second": self.metrics.throughput_bytes_per_second,
                "eta_ms": self.metrics.eta_ms,
            }
        })
    }

    fn human_line(self) -> String {
        let mut line = format!(
            "{} {:<5} {}",
            self.timestamp_ms,
            self.level.as_str().to_ascii_uppercase(),
            self.code.human()
        );
        if let Some(operation_id) = self.operation_id {
            use std::fmt::Write as _;
            let _ = write!(line, " operation_id={operation_id}");
        }
        if let Some(attempt) = self.attempt {
            use std::fmt::Write as _;
            let _ = write!(line, " attempt={attempt}");
        }
        if self.metrics.operations > 0 {
            use std::fmt::Write as _;
            let _ = write!(line, " operations={}", self.metrics.operations);
        }
        if self.metrics.files > 0 {
            use std::fmt::Write as _;
            let _ = write!(line, " files={}", self.metrics.files);
        }
        if self.metrics.bytes > 0 {
            use std::fmt::Write as _;
            let _ = write!(line, " bytes={}", self.metrics.bytes);
        }
        if self.metrics.elapsed_ms > 0 {
            use std::fmt::Write as _;
            let _ = write!(line, " elapsed_ms={}", self.metrics.elapsed_ms);
        }
        line
    }
}

#[derive(Clone, Debug)]
pub struct FileLogConfig {
    pub path: PathBuf,
    pub format: LogFormat,
    pub max_bytes: u64,
    pub backups: usize,
}

/// No raw-token variant is provided intentionally.
#[derive(Clone, Debug)]
pub enum BearerTokenSource {
    Environment(String),
    File(PathBuf),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RemoteDelivery {
    #[default]
    BestEffort,
    Required,
}

#[derive(Clone)]
pub struct RemoteLogConfig {
    pub endpoint: String,
    pub bearer_token: Option<BearerTokenSource>,
    pub queue_capacity: usize,
    pub timeout: Duration,
    pub delivery: RemoteDelivery,
}

pub struct LoggerConfig {
    pub level: LogLevel,
    /// `None` disables the stderr log sink.
    pub stderr: Option<LogFormat>,
    pub file: Option<FileLogConfig>,
    pub remote: Option<RemoteLogConfig>,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            stderr: Some(LogFormat::Human),
            file: None,
            remote: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub remote_events_dropped: u64,
    pub remote_delivery_failures: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteFailure {
    Transport,
    Rejected,
    WorkerStopped,
}

impl fmt::Display for RemoteFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Transport => "remote log transport failed",
            Self::Rejected => "remote log endpoint rejected an event",
            Self::WorkerStopped => "remote log worker stopped",
        })
    }
}

#[derive(Debug)]
pub enum ObservabilityError {
    InvalidLogLevel,
    InvalidFileConfiguration,
    InvalidRemoteEndpoint,
    InvalidRemoteConfiguration,
    InvalidBearerToken,
    RemoteQueueFull,
    RemoteFailure(RemoteFailure),
    FlushTimeout,
    AlreadyShutdown,
    Io {
        operation: &'static str,
        source: io::Error,
    },
    HttpClient(reqwest::Error),
}

impl fmt::Display for ObservabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLogLevel => formatter.write_str("invalid log level"),
            Self::InvalidFileConfiguration => formatter.write_str("invalid file log configuration"),
            Self::InvalidRemoteEndpoint => formatter.write_str(
                "remote log endpoint must be an HTTPS URL without credentials, query, or fragment",
            ),
            Self::InvalidRemoteConfiguration => {
                formatter.write_str("invalid remote log configuration")
            }
            Self::InvalidBearerToken => formatter.write_str("invalid remote log bearer token"),
            Self::RemoteQueueFull => formatter.write_str("remote log queue is full"),
            Self::RemoteFailure(error) => error.fmt(formatter),
            Self::FlushTimeout => formatter.write_str("timed out flushing observability output"),
            Self::AlreadyShutdown => formatter.write_str("observability logger is shut down"),
            Self::Io { operation, .. } => {
                write!(formatter, "observability I/O failed during {operation}")
            }
            Self::HttpClient(_) => formatter.write_str("failed to initialize remote log transport"),
        }
    }
}

impl std::error::Error for ObservabilityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::HttpClient(source) => Some(source),
            _ => None,
        }
    }
}

pub type ObservabilityResult<T> = std::result::Result<T, ObservabilityError>;

/// A synchronous local logger with an optional bounded asynchronous HTTPS sink.
/// Share it across workers with `Arc<EventLogger>`.
pub struct EventLogger {
    level: LogLevel,
    stderr: Option<(LogFormat, Mutex<Box<dyn Write + Send>>)>,
    file: Option<(LogFormat, Mutex<RotatingFile>)>,
    remote: Option<RemoteSink>,
}

impl EventLogger {
    pub fn new(config: LoggerConfig) -> ObservabilityResult<Self> {
        Self::with_stderr_writer(config, io::stderr())
    }

    pub fn with_stderr_writer<W>(config: LoggerConfig, writer: W) -> ObservabilityResult<Self>
    where
        W: Write + Send + 'static,
    {
        let file = config
            .file
            .map(|file| {
                let format = file.format;
                RotatingFile::open(file).map(|sink| (format, Mutex::new(sink)))
            })
            .transpose()?;
        let remote = config.remote.map(RemoteSink::from_config).transpose()?;
        Ok(Self {
            level: config.level,
            stderr: config.stderr.map(|format| {
                (
                    format,
                    Mutex::new(Box::new(writer) as Box<dyn Write + Send>),
                )
            }),
            file,
            remote,
        })
    }

    pub fn emit(&self, event: LogEvent) -> ObservabilityResult<()> {
        if !self.level.permits(event.level) {
            return Ok(());
        }
        if let Some((format, sink)) = &self.stderr {
            write_event(&mut *lock(sink)?, *format, event, "stderr log write")?;
        }
        if let Some((format, sink)) = &self.file {
            let line = format_event(*format, event);
            lock(sink)?.write_line(&line)?;
        }
        if let Some(remote) = &self.remote {
            remote.enqueue(event.json_value().to_string())?;
        }
        Ok(())
    }

    /// Flush local writers and wait until every earlier remote event has been attempted.
    pub fn flush(&self, wait: Duration) -> ObservabilityResult<()> {
        self.flush_local()?;
        if let Some(remote) = &self.remote {
            remote.flush(wait)?;
        }
        Ok(())
    }

    fn flush_local(&self) -> ObservabilityResult<()> {
        if let Some((_, sink)) = &self.stderr {
            lock(sink)?
                .flush()
                .map_err(|source| ObservabilityError::Io {
                    operation: "stderr log flush",
                    source,
                })?;
        }
        if let Some((_, sink)) = &self.file {
            lock(sink)?.flush()?;
        }
        Ok(())
    }

    /// Flush, stop, and join the remote worker. In required mode, any rejected or failed
    /// delivery is returned. Best-effort mode reports failures without failing shutdown.
    pub fn shutdown(&self, wait: Duration) -> ObservabilityResult<ShutdownReport> {
        // A Shutdown command is itself a FIFO barrier for every earlier remote event. Flush only
        // local writers here so the remote side gets one overall caller-supplied deadline.
        let flush_result = self.flush_local();
        let shutdown_result = match &self.remote {
            Some(remote) => remote.shutdown(wait),
            None => Ok(()),
        };
        let report = self
            .remote
            .as_ref()
            .map(RemoteSink::report)
            .unwrap_or_default();
        flush_result.and(shutdown_result).map(|()| report)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> ObservabilityResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| ObservabilityError::Io {
        operation: "log sink lock",
        source: io::Error::other("log sink lock was poisoned"),
    })
}

fn write_event(
    writer: &mut dyn Write,
    format: LogFormat,
    event: LogEvent,
    operation: &'static str,
) -> ObservabilityResult<()> {
    writeln!(writer, "{}", format_event(format, event))
        .map_err(|source| ObservabilityError::Io { operation, source })
}

fn format_event(format: LogFormat, event: LogEvent) -> String {
    match format {
        LogFormat::Human => event.human_line(),
        LogFormat::Json => event.json_value().to_string(),
    }
}

struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    length: u64,
    max_bytes: u64,
    backups: usize,
}

impl RotatingFile {
    fn open(config: FileLogConfig) -> ObservabilityResult<Self> {
        if config.path.as_os_str().is_empty()
            || config.max_bytes == 0
            || config.backups > MAX_FILE_BACKUPS
        {
            return Err(ObservabilityError::InvalidFileConfiguration);
        }
        let file = open_private_append(&config.path)?;
        let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Ok(Self {
            path: config.path,
            file: Some(file),
            length,
            max_bytes: config.max_bytes,
            backups: config.backups,
        })
    }

    fn write_line(&mut self, line: &str) -> ObservabilityResult<()> {
        let added = u64::try_from(line.len().saturating_add(1)).unwrap_or(u64::MAX);
        if self.length > 0 && self.length.saturating_add(added) > self.max_bytes {
            self.rotate()?;
        }
        let file = self.file.as_mut().ok_or_else(|| ObservabilityError::Io {
            operation: "file log write",
            source: io::Error::other("file log is closed"),
        })?;
        writeln!(file, "{line}").map_err(|source| ObservabilityError::Io {
            operation: "file log write",
            source,
        })?;
        self.length = self.length.saturating_add(added);
        Ok(())
    }

    fn flush(&mut self) -> ObservabilityResult<()> {
        self.file
            .as_mut()
            .ok_or_else(|| ObservabilityError::Io {
                operation: "file log flush",
                source: io::Error::other("file log is closed"),
            })?
            .flush()
            .map_err(|source| ObservabilityError::Io {
                operation: "file log flush",
                source,
            })
    }

    fn rotate(&mut self) -> ObservabilityResult<()> {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }
        if self.backups == 0 {
            let file = open_private_truncate(&self.path)?;
            self.file = Some(file);
            self.length = 0;
            return Ok(());
        }
        let oldest = rotated_path(&self.path, self.backups);
        if oldest.exists() {
            fs::remove_file(&oldest).map_err(|source| ObservabilityError::Io {
                operation: "old file log removal",
                source,
            })?;
        }
        for index in (1..self.backups).rev() {
            let source_path = rotated_path(&self.path, index);
            if source_path.exists() {
                fs::rename(&source_path, rotated_path(&self.path, index + 1)).map_err(
                    |source| ObservabilityError::Io {
                        operation: "file log rotation",
                        source,
                    },
                )?;
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, rotated_path(&self.path, 1)).map_err(|source| {
                ObservabilityError::Io {
                    operation: "file log rotation",
                    source,
                }
            })?;
        }
        self.file = Some(open_private_truncate(&self.path)?);
        self.length = 0;
        Ok(())
    }
}

fn open_private_append(path: &Path) -> ObservabilityResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    set_private_mode(&mut options);
    options.open(path).map_err(|source| ObservabilityError::Io {
        operation: "file log open",
        source,
    })
}

fn open_private_truncate(path: &Path) -> ObservabilityResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    set_private_mode(&mut options);
    options.open(path).map_err(|source| ObservabilityError::Io {
        operation: "file log open",
        source,
    })
}

#[cfg(unix)]
fn set_private_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_mode(_options: &mut OpenOptions) {}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

trait RemoteTransport: Send + 'static {
    fn send(&mut self, body: &str) -> std::result::Result<(), RemoteFailure>;
}

struct HttpRemoteTransport {
    client: Client,
    endpoint: Url,
    bearer_token: Option<Zeroizing<String>>,
}

impl RemoteTransport for HttpRemoteTransport {
    fn send(&mut self, body: &str) -> std::result::Result<(), RemoteFailure> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_owned());
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token.as_str());
        }
        let response = request.send().map_err(|_| RemoteFailure::Transport)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(RemoteFailure::Rejected)
        }
    }
}

enum RemoteCommand {
    Event(String),
    Flush(mpsc::Sender<std::result::Result<(), RemoteFailure>>),
    Shutdown(mpsc::Sender<std::result::Result<(), RemoteFailure>>),
}

#[derive(Default)]
struct RemoteStatus {
    first_failure: Mutex<Option<RemoteFailure>>,
    dropped: AtomicU64,
    failures: AtomicU64,
}

struct RemoteSink {
    sender: Mutex<Option<SyncSender<RemoteCommand>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    status: std::sync::Arc<RemoteStatus>,
    delivery: RemoteDelivery,
    closed: AtomicBool,
}

impl RemoteSink {
    fn from_config(config: RemoteLogConfig) -> ObservabilityResult<Self> {
        if config.queue_capacity == 0
            || config.queue_capacity > MAX_REMOTE_QUEUE
            || config.timeout.is_zero()
        {
            return Err(ObservabilityError::InvalidRemoteConfiguration);
        }
        let endpoint = validate_remote_endpoint(&config.endpoint)?;
        let bearer_token = config
            .bearer_token
            .as_ref()
            .map(load_bearer_token)
            .transpose()?;
        let client = Client::builder()
            .connect_timeout(config.timeout)
            .timeout(config.timeout)
            // Never forward an observability payload or bearer credential to a redirect
            // target. Operators must configure the final HTTPS endpoint explicitly.
            .redirect(Policy::none())
            .build()
            .map_err(ObservabilityError::HttpClient)?;
        let transport = HttpRemoteTransport {
            client,
            endpoint,
            bearer_token,
        };
        Self::spawn(Box::new(transport), config.queue_capacity, config.delivery)
    }

    fn spawn(
        transport: Box<dyn RemoteTransport>,
        capacity: usize,
        delivery: RemoteDelivery,
    ) -> ObservabilityResult<Self> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let status = std::sync::Arc::new(RemoteStatus::default());
        let worker_status = std::sync::Arc::clone(&status);
        let join = thread::Builder::new()
            .name("sdsync-log-delivery".to_owned())
            .spawn(move || remote_worker(receiver, transport, worker_status))
            .map_err(|source| ObservabilityError::Io {
                operation: "remote log worker start",
                source,
            })?;
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            join: Mutex::new(Some(join)),
            status,
            delivery,
            closed: AtomicBool::new(false),
        })
    }

    fn enqueue(&self, body: String) -> ObservabilityResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return self.delivery_result(Err(RemoteFailure::WorkerStopped));
        }
        if self.delivery == RemoteDelivery::Required
            && let Some(failure) = self.first_failure()
        {
            return Err(ObservabilityError::RemoteFailure(failure));
        }
        let sender = self.sender()?;
        match sender.try_send(RemoteCommand::Event(body)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.status.dropped.fetch_add(1, Ordering::Relaxed);
                if self.delivery == RemoteDelivery::Required {
                    Err(ObservabilityError::RemoteQueueFull)
                } else {
                    Ok(())
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.delivery_result(Err(RemoteFailure::WorkerStopped))
            }
        }
    }

    fn flush(&self, wait: Duration) -> ObservabilityResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ObservabilityError::AlreadyShutdown);
        }
        self.control(wait, false)
    }

    fn shutdown(&self, wait: Duration) -> ObservabilityResult<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let started = Instant::now();
        let result = self.control(wait, true);
        lock(&self.sender)?.take();
        let remaining = wait.saturating_sub(started.elapsed());
        let join_result = self.finish_worker_within(remaining);
        result.and(join_result)
    }

    fn control(&self, wait: Duration, shutdown: bool) -> ObservabilityResult<()> {
        let started = Instant::now();
        let (acknowledge, result) = mpsc::channel();
        let mut command = Some(if shutdown {
            RemoteCommand::Shutdown(acknowledge)
        } else {
            RemoteCommand::Flush(acknowledge)
        });
        loop {
            let sender = self.sender()?;
            match sender.try_send(command.take().expect("control command is present")) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    command = Some(returned);
                    if started.elapsed() >= wait {
                        return Err(ObservabilityError::FlushTimeout);
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return self.delivery_result(Err(RemoteFailure::WorkerStopped));
                }
            }
        }
        let remaining = wait.saturating_sub(started.elapsed());
        let delivered = result
            .recv_timeout(remaining)
            .map_err(|_| ObservabilityError::FlushTimeout)?;
        self.delivery_result(delivered)
    }

    fn sender(&self) -> ObservabilityResult<SyncSender<RemoteCommand>> {
        lock(&self.sender)?
            .as_ref()
            .cloned()
            .ok_or(ObservabilityError::AlreadyShutdown)
    }

    fn first_failure(&self) -> Option<RemoteFailure> {
        self.status
            .first_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .copied()
    }

    fn delivery_result(
        &self,
        result: std::result::Result<(), RemoteFailure>,
    ) -> ObservabilityResult<()> {
        match (self.delivery, result) {
            (_, Ok(())) | (RemoteDelivery::BestEffort, Err(_)) => Ok(()),
            (RemoteDelivery::Required, Err(error)) => Err(ObservabilityError::RemoteFailure(error)),
        }
    }

    fn report(&self) -> ShutdownReport {
        ShutdownReport {
            remote_events_dropped: self.status.dropped.load(Ordering::Relaxed),
            remote_delivery_failures: self.status.failures.load(Ordering::Relaxed),
        }
    }

    /// Join a completed worker, waiting at most `wait`. Taking and dropping an unfinished
    /// handle detaches it; this preserves the shutdown deadline even if a transport is stuck.
    fn finish_worker_within(&self, wait: Duration) -> ObservabilityResult<()> {
        let started = Instant::now();
        let join = lock(&self.join)?.take();
        let Some(join) = join else {
            return Ok(());
        };
        while !join.is_finished() {
            if started.elapsed() >= wait {
                drop(join);
                return Err(ObservabilityError::FlushTimeout);
            }
            thread::sleep(Duration::from_millis(1));
        }
        join.join()
            .map_err(|_| ObservabilityError::RemoteFailure(RemoteFailure::WorkerStopped))
    }
}

impl Drop for RemoteSink {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let sender = self
            .sender
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(sender);
        if let Some(join) = self
            .join
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            && join.is_finished()
        {
            let _ = join.join();
        }
        // Dropping an unfinished JoinHandle detaches it. Normal destruction must not turn a
        // best-effort network sink into an unbounded application shutdown delay.
    }
}

fn remote_worker(
    receiver: Receiver<RemoteCommand>,
    mut transport: Box<dyn RemoteTransport>,
    status: std::sync::Arc<RemoteStatus>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            RemoteCommand::Event(body) => {
                if let Err(failure) = transport.send(&body) {
                    status.failures.fetch_add(1, Ordering::Relaxed);
                    let mut first = status
                        .first_failure
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if first.is_none() {
                        *first = Some(failure);
                    }
                }
            }
            RemoteCommand::Flush(acknowledge) => {
                let result = status
                    .first_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .map_or(Ok(()), Err);
                let _ = acknowledge.send(result);
            }
            RemoteCommand::Shutdown(acknowledge) => {
                let result = status
                    .first_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .map_or(Ok(()), Err);
                let _ = acknowledge.send(result);
                break;
            }
        }
    }
}

fn validate_remote_endpoint(input: &str) -> ObservabilityResult<Url> {
    let url = Url::parse(input).map_err(|_| ObservabilityError::InvalidRemoteEndpoint)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ObservabilityError::InvalidRemoteEndpoint);
    }
    Ok(url)
}

fn load_bearer_token(source: &BearerTokenSource) -> ObservabilityResult<Zeroizing<String>> {
    let token = match source {
        BearerTokenSource::Environment(name) => {
            if name.is_empty() {
                return Err(ObservabilityError::InvalidBearerToken);
            }
            env::var(name)
                .map(Zeroizing::new)
                .map_err(|_| ObservabilityError::InvalidBearerToken)?
        }
        BearerTokenSource::File(path) => {
            let mut file = File::open(path).map_err(|source| ObservabilityError::Io {
                operation: "bearer token file open",
                source,
            })?;
            let mut bytes = Zeroizing::new(Vec::new());
            Read::by_ref(&mut file)
                .take(MAX_BEARER_TOKEN_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|source| ObservabilityError::Io {
                    operation: "bearer token file read",
                    source,
                })?;
            if bytes.len() as u64 > MAX_BEARER_TOKEN_BYTES {
                return Err(ObservabilityError::InvalidBearerToken);
            }
            let decoded = String::from_utf8(std::mem::take(&mut *bytes)).map_err(|error| {
                let mut bytes = error.into_bytes();
                bytes.zeroize();
                ObservabilityError::InvalidBearerToken
            })?;
            Zeroizing::new(decoded)
        }
    };
    normalize_bearer_token(token)
}

fn normalize_bearer_token(mut token: Zeroizing<String>) -> ObservabilityResult<Zeroizing<String>> {
    while token.ends_with(['\r', '\n']) {
        token.pop();
    }
    if token.is_empty()
        || token.len() as u64 > MAX_BEARER_TOKEN_BYTES
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"')
    {
        return Err(ObservabilityError::InvalidBearerToken);
    }
    Ok(token)
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn explicit_log_level_has_strict_precedence() {
        assert_eq!(
            resolve_log_level(Some("debug"), Some("not-a-level"), Verbosity::Quiet).unwrap(),
            LogLevel::Debug
        );
        assert_eq!(
            resolve_log_level(None, Some("trace"), Verbosity::Quiet).unwrap(),
            LogLevel::Trace
        );
        assert_eq!(
            resolve_log_level(None, None, Verbosity::Verbose).unwrap(),
            LogLevel::Debug
        );
    }

    #[test]
    fn structured_events_have_no_free_form_secret_field() {
        let marker = "LEAK-ME-NOT";
        let event = LogEvent::new(LogLevel::Info, EventCode::UploadCompleted)
            .operation(7)
            .metrics(EventMetrics {
                files: 1,
                bytes: 42,
                ..EventMetrics::default()
            });
        let json = event.json_value().to_string();
        let human = event.human_line();
        assert!(!json.contains(marker));
        assert!(!human.contains(marker));
        assert!(!json.contains("token"));
        assert!(!json.contains("password"));
        assert!(!json.contains("url"));
    }

    #[test]
    fn upload_progress_has_stable_machine_and_human_names() {
        let event = LogEvent::new(LogLevel::Trace, EventCode::UploadProgress).operation(9);
        assert_eq!(event.json_value()["event"], "upload.progress");
        assert!(event.human_line().contains("upload progress"));
    }

    #[test]
    fn every_event_code_has_a_stable_machine_and_human_name() {
        for (code, machine, human) in [
            (EventCode::RunStarted, "run.started", "sync run started"),
            (
                EventCode::RunCompleted,
                "run.completed",
                "sync run completed",
            ),
            (EventCode::RunFailed, "run.failed", "sync run failed"),
            (
                EventCode::LocalScanStarted,
                "local_scan.started",
                "local scan started",
            ),
            (
                EventCode::LocalScanCompleted,
                "local_scan.completed",
                "local scan completed",
            ),
            (
                EventCode::ApiDiscoveryStarted,
                "api_discovery.started",
                "API discovery started",
            ),
            (
                EventCode::ApiDiscoveryCompleted,
                "api_discovery.completed",
                "API discovery completed",
            ),
            (
                EventCode::AuthenticationStarted,
                "authentication.started",
                "authentication started",
            ),
            (
                EventCode::AuthenticationCompleted,
                "authentication.completed",
                "authentication completed",
            ),
            (
                EventCode::RemoteScanStarted,
                "remote_scan.started",
                "remote scan started",
            ),
            (
                EventCode::RemoteScanCompleted,
                "remote_scan.completed",
                "remote scan completed",
            ),
            (EventCode::PlanReady, "plan.ready", "sync plan ready"),
            (EventCode::UploadStarted, "upload.started", "upload started"),
            (
                EventCode::UploadAttemptStarted,
                "upload.attempt_started",
                "upload attempt started",
            ),
            (
                EventCode::UploadProgress,
                "upload.progress",
                "upload progress",
            ),
            (
                EventCode::UploadCompleted,
                "upload.completed",
                "upload completed",
            ),
            (EventCode::UploadFailed, "upload.failed", "upload failed"),
            (
                EventCode::DirectoryCreated,
                "directory.created",
                "directory created",
            ),
            (EventCode::EntryDeleted, "entry.deleted", "entry deleted"),
            (
                EventCode::RetryScheduled,
                "retry.scheduled",
                "retry scheduled",
            ),
            (
                EventCode::CancellationRequested,
                "cancellation.requested",
                "cancellation requested",
            ),
        ] {
            let event = LogEvent::new(LogLevel::Info, code);
            assert_eq!(event.json_value()["event"], machine);
            assert!(event.human_line().contains(human), "{machine}");
        }
    }

    #[test]
    fn log_levels_parse_case_insensitively_and_reject_unknown_values() {
        for (input, expected, rendered) in [
            (" ERROR ", LogLevel::Error, "error"),
            ("warning", LogLevel::Warn, "warn"),
            ("INFO", LogLevel::Info, "info"),
            ("Debug", LogLevel::Debug, "debug"),
            ("trace", LogLevel::Trace, "trace"),
        ] {
            let parsed = LogLevel::parse(input).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), rendered);
        }
        assert!(matches!(
            LogLevel::parse("verbose"),
            Err(ObservabilityError::InvalidLogLevel)
        ));
    }

    #[test]
    fn structured_and_human_events_include_only_bounded_metrics() {
        let event = LogEvent::new(LogLevel::Warn, EventCode::RetryScheduled)
            .operation(17)
            .attempt(3)
            .metrics(EventMetrics {
                operations: 2,
                files: 1,
                bytes: 4096,
                elapsed_ms: 250,
                throughput_bytes_per_second: 1024,
                eta_ms: Some(750),
            });
        let json = event.json_value();
        assert_eq!(json["operation_id"], 17);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["metrics"]["throughput_bytes_per_second"], 1024);
        assert_eq!(json["metrics"]["eta_ms"], 750);
        let human = event.human_line();
        for field in [
            "operation_id=17",
            "attempt=3",
            "operations=2",
            "files=1",
            "bytes=4096",
            "elapsed_ms=250",
        ] {
            assert!(human.contains(field), "{field}");
        }
    }

    #[test]
    fn logger_filters_and_writes_json_lines() {
        let buffer = SharedWriter::default();
        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Info,
                stderr: Some(LogFormat::Json),
                file: None,
                remote: None,
            },
            buffer.clone(),
        )
        .unwrap();
        logger
            .emit(LogEvent::new(LogLevel::Debug, EventCode::UploadStarted))
            .unwrap();
        logger
            .emit(LogEvent::new(LogLevel::Info, EventCode::UploadCompleted))
            .unwrap();
        logger.flush(Duration::from_secs(1)).unwrap();
        let output = buffer.text();
        assert_eq!(output.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(value["event"], "upload.completed");
    }

    #[test]
    fn file_sink_rotates_locally() {
        let directory = unique_test_directory("rotation");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.log");
        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Trace,
                stderr: None,
                file: Some(FileLogConfig {
                    path: path.clone(),
                    format: LogFormat::Json,
                    max_bytes: 180,
                    backups: 2,
                }),
                remote: None,
            },
            Vec::new(),
        )
        .unwrap();
        for id in 0..5 {
            logger
                .emit(LogEvent::new(LogLevel::Info, EventCode::UploadCompleted).operation(id))
                .unwrap();
        }
        logger.flush(Duration::from_secs(1)).unwrap();
        assert!(path.exists());
        assert!(rotated_path(&path, 1).exists());
        drop(logger);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_file_sink_configuration_is_rejected_before_opening_a_file() {
        let directory = unique_test_directory("invalid-file-config");
        fs::create_dir_all(&directory).unwrap();
        for config in [
            FileLogConfig {
                path: PathBuf::new(),
                format: LogFormat::Json,
                max_bytes: 1,
                backups: 1,
            },
            FileLogConfig {
                path: directory.join("zero.log"),
                format: LogFormat::Json,
                max_bytes: 0,
                backups: 1,
            },
            FileLogConfig {
                path: directory.join("too-many.log"),
                format: LogFormat::Json,
                max_bytes: 1,
                backups: MAX_FILE_BACKUPS + 1,
            },
        ] {
            assert!(matches!(
                RotatingFile::open(config),
                Err(ObservabilityError::InvalidFileConfiguration)
            ));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn zero_backup_rotation_truncates_in_place() {
        let directory = unique_test_directory("zero-backup");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.log");
        let mut sink = RotatingFile::open(FileLogConfig {
            path: path.clone(),
            format: LogFormat::Human,
            max_bytes: 6,
            backups: 0,
        })
        .unwrap();

        sink.write_line("first").unwrap();
        sink.write_line("next").unwrap();
        sink.flush().unwrap();
        drop(sink);

        assert_eq!(fs::read_to_string(&path).unwrap(), "next\n");
        assert!(!rotated_path(&path, 1).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bearer_token_files_are_trimmed_bounded_and_ascii_safe() {
        let directory = unique_test_directory("bearer-token");
        fs::create_dir_all(&directory).unwrap();

        let valid = directory.join("valid.token");
        fs::write(&valid, b"header.payload.signature\r\n").unwrap();
        let token = load_bearer_token(&BearerTokenSource::File(valid)).unwrap();
        assert_eq!(token.as_str(), "header.payload.signature");

        for (name, bytes) in [
            ("empty", Vec::new()),
            ("space", b"token with spaces".to_vec()),
            ("quote", b"token\"value".to_vec()),
            ("utf8", vec![0xff, 0xfe]),
            (
                "oversized",
                vec![b'x'; usize::try_from(MAX_BEARER_TOKEN_BYTES + 1).unwrap()],
            ),
        ] {
            let path = directory.join(name);
            fs::write(&path, bytes).unwrap();
            assert!(matches!(
                load_bearer_token(&BearerTokenSource::File(path)),
                Err(ObservabilityError::InvalidBearerToken)
            ));
        }
        let missing = directory.join("missing");
        assert!(matches!(
            load_bearer_token(&BearerTokenSource::File(missing)),
            Err(ObservabilityError::Io {
                operation: "bearer token file open",
                ..
            })
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn remote_endpoint_rejects_credential_bearing_forms() {
        assert!(validate_remote_endpoint("https://logs.example.test/events").is_ok());
        for endpoint in [
            "http://logs.example.test/events",
            "https://user:pass@logs.example.test/events",
            "https://logs.example.test/events?token=secret",
            "https://logs.example.test/events#secret",
        ] {
            assert!(validate_remote_endpoint(endpoint).is_err());
        }
    }

    #[test]
    fn invalid_remote_configuration_is_rejected_without_starting_a_worker() {
        for (queue_capacity, timeout) in [
            (0, Duration::from_secs(1)),
            (MAX_REMOTE_QUEUE + 1, Duration::from_secs(1)),
            (1, Duration::ZERO),
        ] {
            assert!(matches!(
                RemoteSink::from_config(RemoteLogConfig {
                    endpoint: "https://logs.example.test/events".to_owned(),
                    bearer_token: None,
                    queue_capacity,
                    timeout,
                    delivery: RemoteDelivery::Required,
                }),
                Err(ObservabilityError::InvalidRemoteConfiguration)
            ));
        }
    }

    #[test]
    fn required_remote_queue_overflow_is_reported_and_counted() {
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let remote = RemoteSink::spawn(
            Box::new(BlockingTransport {
                started: started_send,
                release: release_receive,
            }),
            1,
            RemoteDelivery::Required,
        )
        .unwrap();
        remote.enqueue("first".to_owned()).unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        remote.enqueue("queued".to_owned()).unwrap();

        assert!(matches!(
            remote.enqueue("overflow".to_owned()),
            Err(ObservabilityError::RemoteQueueFull)
        ));
        assert_eq!(remote.report().remote_events_dropped, 1);

        release_send.send(()).unwrap();
        release_send.send(()).unwrap();
        remote.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn successful_remote_shutdown_is_idempotent_and_closes_the_sink() {
        let calls = Arc::new(AtomicU64::new(0));
        let remote = RemoteSink::spawn(
            Box::new(FakeTransport {
                calls: Arc::clone(&calls),
                fail: false,
            }),
            2,
            RemoteDelivery::Required,
        )
        .unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        remote.flush(Duration::from_secs(1)).unwrap();
        remote.shutdown(Duration::from_secs(1)).unwrap();

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(matches!(
            remote.flush(Duration::from_secs(1)),
            Err(ObservabilityError::AlreadyShutdown)
        ));
        assert!(matches!(
            remote.enqueue("{}".to_owned()),
            Err(ObservabilityError::RemoteFailure(
                RemoteFailure::WorkerStopped
            ))
        ));
        remote.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn required_remote_failure_surfaces_on_flush() {
        let transport = Box::new(FakeTransport {
            calls: Arc::new(AtomicU64::new(0)),
            fail: true,
        });
        let remote = RemoteSink::spawn(transport, 4, RemoteDelivery::Required).unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        assert!(matches!(
            remote.flush(Duration::from_secs(1)),
            Err(ObservabilityError::RemoteFailure(RemoteFailure::Transport))
        ));
        assert_eq!(remote.report().remote_delivery_failures, 1);
        let _ = remote.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn best_effort_remote_failure_is_reported_without_failing() {
        let transport = Box::new(FakeTransport {
            calls: Arc::new(AtomicU64::new(0)),
            fail: true,
        });
        let remote = RemoteSink::spawn(transport, 4, RemoteDelivery::BestEffort).unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        remote.flush(Duration::from_secs(1)).unwrap();
        assert_eq!(remote.report().remote_delivery_failures, 1);
        remote.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn dropping_remote_sink_does_not_wait_for_blocked_transport() {
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let remote = RemoteSink::spawn(
            Box::new(BlockingTransport {
                started: started_send,
                release: release_receive,
            }),
            1,
            RemoteDelivery::BestEffort,
        )
        .unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let started = Instant::now();
        drop(remote);
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = release_send.send(());
    }

    #[test]
    fn explicit_shutdown_observes_its_deadline() {
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let remote = RemoteSink::spawn(
            Box::new(BlockingTransport {
                started: started_send,
                release: release_receive,
            }),
            1,
            RemoteDelivery::BestEffort,
        )
        .unwrap();
        remote.enqueue("{}".to_owned()).unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let started = Instant::now();
        assert!(matches!(
            remote.shutdown(Duration::from_millis(20)),
            Err(ObservabilityError::FlushTimeout)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = release_send.send(());
    }

    #[test]
    fn defaults_public_errors_and_local_shutdown_have_stable_contracts() {
        let config = LoggerConfig::default();
        assert_eq!(config.level, LogLevel::Info);
        assert_eq!(config.stderr, Some(LogFormat::Human));
        assert!(config.file.is_none() && config.remote.is_none());
        let logger = EventLogger::new(config).unwrap();
        assert_eq!(
            logger.shutdown(Duration::from_secs(1)).unwrap(),
            ShutdownReport::default()
        );

        let cases = vec![
            (ObservabilityError::InvalidLogLevel, "invalid log level"),
            (
                ObservabilityError::InvalidFileConfiguration,
                "invalid file log configuration",
            ),
            (
                ObservabilityError::InvalidRemoteEndpoint,
                "must be an HTTPS URL",
            ),
            (
                ObservabilityError::InvalidRemoteConfiguration,
                "invalid remote log configuration",
            ),
            (
                ObservabilityError::InvalidBearerToken,
                "invalid remote log bearer token",
            ),
            (
                ObservabilityError::RemoteQueueFull,
                "remote log queue is full",
            ),
            (
                ObservabilityError::RemoteFailure(RemoteFailure::Transport),
                "transport failed",
            ),
            (
                ObservabilityError::RemoteFailure(RemoteFailure::Rejected),
                "rejected an event",
            ),
            (
                ObservabilityError::RemoteFailure(RemoteFailure::WorkerStopped),
                "worker stopped",
            ),
            (ObservabilityError::FlushTimeout, "timed out flushing"),
            (ObservabilityError::AlreadyShutdown, "logger is shut down"),
        ];
        for (error, expected) in cases {
            assert!(error.to_string().contains(expected));
            assert!(std::error::Error::source(&error).is_none());
        }
        let io_error = ObservabilityError::Io {
            operation: "test write",
            source: io::Error::other("sensitive detail"),
        };
        assert_eq!(
            io_error.to_string(),
            "observability I/O failed during test write"
        );
        assert_eq!(
            std::error::Error::source(&io_error).unwrap().to_string(),
            "sensitive detail"
        );
    }

    #[test]
    fn writer_and_closed_file_failures_preserve_sink_operation_context() {
        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Trace,
                stderr: Some(LogFormat::Human),
                file: None,
                remote: None,
            },
            FailingWriter { fail_write: true },
        )
        .unwrap();
        assert!(matches!(
            logger.emit(LogEvent::new(LogLevel::Info, EventCode::RunStarted)),
            Err(ObservabilityError::Io {
                operation: "stderr log write",
                ..
            })
        ));

        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Trace,
                stderr: Some(LogFormat::Json),
                file: None,
                remote: None,
            },
            FailingWriter { fail_write: false },
        )
        .unwrap();
        assert!(matches!(
            logger.flush(Duration::ZERO),
            Err(ObservabilityError::Io {
                operation: "stderr log flush",
                ..
            })
        ));

        let directory = unique_test_directory("closed-file");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.log");
        let mut sink = RotatingFile::open(FileLogConfig {
            path: path.clone(),
            format: LogFormat::Human,
            max_bytes: 64,
            backups: 1,
        })
        .unwrap();
        sink.file = None;
        assert!(matches!(
            sink.write_line("event"),
            Err(ObservabilityError::Io {
                operation: "file log write",
                ..
            })
        ));
        assert!(matches!(
            sink.flush(),
            Err(ObservabilityError::Io {
                operation: "file log flush",
                ..
            })
        ));
        drop(sink);

        assert!(matches!(
            RotatingFile::open(FileLogConfig {
                path: directory.clone(),
                format: LogFormat::Json,
                max_bytes: 64,
                backups: 1,
            }),
            Err(ObservabilityError::Io {
                operation: "file log open",
                ..
            })
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn configured_https_remote_sink_reports_transport_failure_on_shutdown() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("https://{}/events", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let acceptor = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        drop(stream);
                        return true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("ephemeral TLS listener failed: {error}"),
                }
            }
        });
        let logger = EventLogger::with_stderr_writer(
            LoggerConfig {
                level: LogLevel::Trace,
                stderr: None,
                file: None,
                remote: Some(RemoteLogConfig {
                    endpoint,
                    bearer_token: None,
                    queue_capacity: 2,
                    timeout: Duration::from_millis(100),
                    delivery: RemoteDelivery::Required,
                }),
            },
            Vec::new(),
        )
        .unwrap();
        logger
            .emit(LogEvent::new(LogLevel::Info, EventCode::RunFailed))
            .unwrap();
        assert!(matches!(
            logger.shutdown(Duration::from_secs(2)),
            Err(ObservabilityError::RemoteFailure(RemoteFailure::Transport))
        ));
        assert!(acceptor.join().unwrap(), "HTTPS transport never connected");
    }

    #[test]
    fn best_effort_queue_overflow_is_counted_without_failing_the_caller() {
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let remote = RemoteSink::spawn(
            Box::new(BlockingTransport {
                started: started_send,
                release: release_receive,
            }),
            1,
            RemoteDelivery::BestEffort,
        )
        .unwrap();
        remote.enqueue("first".to_owned()).unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        remote.enqueue("queued".to_owned()).unwrap();
        remote.enqueue("dropped".to_owned()).unwrap();
        assert_eq!(remote.report().remote_events_dropped, 1);
        release_send.send(()).unwrap();
        release_send.send(()).unwrap();
        remote.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeTransport {
        calls: Arc<AtomicU64>,
        fail: bool,
    }

    struct FailingWriter {
        fail_write: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                Err(io::Error::other("write failed"))
            } else {
                Ok(buffer.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush failed"))
        }
    }

    impl RemoteTransport for FakeTransport {
        fn send(&mut self, _body: &str) -> std::result::Result<(), RemoteFailure> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                Err(RemoteFailure::Transport)
            } else {
                Ok(())
            }
        }
    }

    struct BlockingTransport {
        started: mpsc::Sender<()>,
        release: Receiver<()>,
    }

    impl RemoteTransport for BlockingTransport {
        fn send(&mut self, _body: &str) -> std::result::Result<(), RemoteFailure> {
            let _ = self.started.send(());
            let _ = self.release.recv();
            Ok(())
        }
    }

    fn unique_test_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "sdsync-observability-{label}-{}-{nonce}",
            std::process::id()
        ))
    }
}
