#![forbid(unsafe_code)]

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::CommandFactory;
use serde_json::{Value, json};
use synology_drive_sync::api::{ApiClient, ClientOptions, UploadObserver, UploadTransferEvent};
use synology_drive_sync::local::{self, IgnoreRules, LocalEntry};
use synology_drive_sync::observability::{
    BearerTokenSource, EventCode, EventLogger, EventMetrics, FileLogConfig, LogEvent,
    LogFormat as EventLogFormat, LogLevel as EventLogLevel, LoggerConfig, RemoteDelivery,
    RemoteLogConfig,
};
use synology_drive_sync::path::RemoteRoot;
use synology_drive_sync::plan::{self, CompareMode, PlanOptions, SyncPlan};
use synology_drive_sync::progress::{
    OperationKind, ProgressFormat, ProgressMode as RendererProgressMode, ProgressRenderer,
    ProgressTotals, ProgressTracker,
};
use synology_drive_sync::sync::{
    self, CancellationToken, ExecuteOptions, ExecutionReport, UploadObserverFactory,
};
use synology_drive_sync::{Error, Result};

mod cli;
mod config;
mod credentials;

const FILE_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const FILE_LOG_BACKUPS: usize = 3;
const REMOTE_LOG_QUEUE_CAPACITY: usize = 1_024;
const REMOTE_LOG_TIMEOUT: Duration = Duration::from_secs(10);
const LOGGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_RENDER_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    let cli = cli::Cli::parse_checked();
    match dispatch(&cli) {
        Ok(code) => code,
        Err(error) => {
            print_error(&error);
            ExitCode::from(error_exit_code(&error))
        }
    }
}

fn dispatch(arguments: &cli::Cli) -> Result<ExitCode> {
    match arguments.invocation() {
        cli::Invocation::Completions(completions) => {
            write_completions(completions.shell)?;
            return Ok(ExitCode::SUCCESS);
        }
        cli::Invocation::Manpage(manpage) => {
            write_manpage(manpage.all.as_deref())?;
            return Ok(ExitCode::SUCCESS);
        }
        cli::Invocation::Config(config) => {
            run_config(arguments, config.action)?;
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    let loaded = load_optional_config(&arguments.global)?;
    let selected = select_optional_profile(loaded.as_ref(), arguments.global.profile.as_deref())?;
    let profile = selected.as_ref().and_then(|selection| selection.values);

    match arguments.invocation() {
        cli::Invocation::Sync {
            arguments: sync,
            legacy,
        } => {
            let resolved = config::resolve_sync(profile, sync, &arguments.global.output)
                .map_err(config_error)?;
            if legacy && resolved.output.verbosity > 0 && !resolved.output.quiet {
                eprintln!(
                    "warning: the positional sync form is retained for compatibility; prefer the `sync` subcommand"
                );
            }
            run_sync(resolved, sync.dry_run, false)
        }
        cli::Invocation::Plan(plan) => {
            let resolved = config::resolve_sync(profile, &plan.sync, &arguments.global.output)
                .map_err(config_error)?;
            run_sync(resolved, true, plan.exit_code)
        }
        cli::Invocation::Doctor(doctor) => {
            let resolved = config::resolve_doctor(profile, doctor, &arguments.global.output)
                .map_err(config_error)?;
            run_doctor(resolved)
        }
        cli::Invocation::Credentials(command) => {
            let resolved = config::resolve_credential_profile(profile, command.profile())
                .map_err(config_error)?;
            let fallback = config::Profile::default();
            let output =
                config::resolve_output(profile.unwrap_or(&fallback), &arguments.global.output)
                    .map_err(config_error)?;
            let result = credentials::run(command, &resolved, profile, output.quiet)?;
            write_credential_output(result, output.output)?;
            Ok(ExitCode::SUCCESS)
        }
        cli::Invocation::Config(_)
        | cli::Invocation::Completions(_)
        | cli::Invocation::Manpage(_) => {
            unreachable!("handled before profile loading")
        }
    }
}

fn run_sync(
    settings: config::ResolvedSync,
    plan_only: bool,
    changes_exit_code: bool,
) -> Result<ExitCode> {
    warn_for_insecure_network(&settings.network, &settings.output);
    let logger = build_logger(&settings.output)?;
    let start_log = log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::RunStarted),
    );
    let started = Instant::now();

    let mut operation = match start_log {
        Ok(()) => prepare_and_run_sync(&settings, plan_only, logger.clone()),
        Err(error) => Err(error),
    };
    let final_log = match &operation {
        Ok((plan, _)) => {
            let metrics = EventMetrics {
                operations: operation_count(plan),
                files: plan.uploads.len() as u64,
                bytes: plan.upload_bytes,
                elapsed_ms: duration_millis(started.elapsed()),
                ..EventMetrics::default()
            };
            log_event(
                logger.as_ref(),
                LogEvent::new(EventLogLevel::Info, EventCode::RunCompleted).metrics(metrics),
            )
        }
        Err(_) => log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Error, EventCode::RunFailed),
        ),
    };
    if operation.is_ok()
        && let Err(error) = final_log
    {
        operation = Err(error);
    }

    let operation = finish_logger(logger.as_ref(), operation, settings.output.quiet)?;
    let (plan, report) = operation;
    let elapsed = started.elapsed();
    write_sync_output(&plan, report.as_ref(), elapsed, &settings.output, plan_only)?;

    if changes_exit_code && !plan.is_empty() {
        Ok(ExitCode::from(cli::PLAN_CHANGES_EXIT_CODE))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn prepare_and_run_sync(
    settings: &config::ResolvedSync,
    plan_only: bool,
    logger: Option<Arc<EventLogger>>,
) -> Result<(SyncPlan, Option<ExecutionReport>)> {
    let root = RemoteRoot::parse(&settings.remote)?;
    let rules = IgnoreRules::build(&settings.source, &settings.behavior.excludes)?;

    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::LocalScanStarted),
    )?;
    let local = local::scan(&settings.source, &rules)?;
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::LocalScanCompleted).metrics(EventMetrics {
            operations: local.entries.len() as u64,
            files: local.files() as u64,
            bytes: local
                .entries
                .values()
                .filter(|entry| entry.kind == local::EntryKind::File)
                .fold(0_u64, |total, entry| total.saturating_add(entry.size)),
            ..EventMetrics::default()
        }),
    )?;

    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::ApiDiscoveryStarted),
    )?;
    let mut client = connect_client(&settings.connection.url, &settings.network)?;
    if settings.safety.delete {
        client.require_delete_api()?;
    }
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::ApiDiscoveryCompleted),
    )?;

    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::AuthenticationStarted),
    )?;
    let mut vault = credentials::VaultSession::new(
        !settings.authentication.no_vault,
        &settings.connection.url,
        &settings.connection.username,
        settings.network.allow_http,
    );
    let password = credentials::read_password_with_file(
        settings.authentication.password_stdin,
        settings.authentication.password_file.as_deref(),
        &mut vault,
    )?;
    credentials::authenticate_with_sources(
        &mut client,
        &settings.connection.username,
        &password,
        &mut vault,
        settings.authentication.totp_secret_file.as_deref(),
    )?;
    drop(password);

    let operation = (|| {
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::AuthenticationCompleted),
        )?;
        client.verify_share_writable(&root)?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanStarted),
        )?;
        let remote = client.remote_inventory(&root)?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanCompleted).metrics(
                EventMetrics {
                    operations: remote.entries.len() as u64,
                    ..EventMetrics::default()
                },
            ),
        )?;

        let plan = plan::build_plan(
            &root,
            &local,
            &remote,
            &rules,
            &PlanOptions {
                delete: settings.safety.delete,
                allow_empty_source: settings.safety.allow_empty_source,
                max_delete: settings.safety.max_delete,
                compare: compare_mode(settings.behavior.compare),
            },
        )?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::PlanReady).metrics(EventMetrics {
                operations: operation_count(&plan),
                files: plan.uploads.len() as u64,
                bytes: plan.upload_bytes,
                ..EventMetrics::default()
            }),
        )?;

        if plan_only || plan.is_empty() {
            return Ok((plan, None));
        }

        let cancellation = CancellationToken::default();
        install_cancellation_handler(cancellation.clone())?;
        let progress = ProgressWiring::new(
            &plan,
            &settings.output,
            logger.clone(),
            cancellation.clone(),
        );
        let execution_log_failure = Arc::new(Mutex::new(None));
        let execution_log_failure_for_report = Arc::clone(&execution_log_failure);
        let execution_logger = logger.clone();
        let cancellation_for_report = cancellation.clone();
        let execution = sync::execute_observed(
            &client,
            &root,
            &plan,
            ExecuteOptions {
                jobs: usize::from(settings.behavior.jobs),
                dry_run: false,
            },
            cancellation,
            progress.observer_factory(),
            |message| {
                let code = if message.starts_with("created directory:") {
                    Some(EventCode::DirectoryCreated)
                } else if message.starts_with("deleted type conflict:")
                    || message.starts_with("deleted remote extra:")
                {
                    Some(EventCode::EntryDeleted)
                } else {
                    None
                };
                if let Some(code) = code
                    && let Some(logger) = &execution_logger
                    && let Err(error) = logger.emit(
                        LogEvent::new(EventLogLevel::Debug, code).metrics(EventMetrics {
                            operations: 1,
                            ..EventMetrics::default()
                        }),
                    )
                {
                    record_progress_failure(&execution_log_failure_for_report, error.to_string());
                    cancellation_for_report.cancel();
                }
                if settings.output.verbosity > 0 && !settings.output.quiet {
                    eprintln!("  {message}");
                }
            },
        );
        let progress_result = progress.finish();
        let execution_log_result = take_recorded_failure(&execution_log_failure)?;
        if let Some(error) = execution_log_result {
            return Err(Error::Message(format!(
                "execution observability failed: {error}"
            )));
        }
        progress_result?;
        if matches!(&execution, Err(Error::Cancelled)) {
            log_event(
                logger.as_ref(),
                LogEvent::new(EventLogLevel::Warn, EventCode::CancellationRequested),
            )?;
        }
        let report = execution?;
        Ok((plan, Some(report)))
    })();

    finish_authenticated_operation(&mut client, operation)
}

fn run_doctor(settings: config::ResolvedDoctor) -> Result<ExitCode> {
    warn_for_insecure_network(&settings.network, &settings.output);
    let logger = build_logger(&settings.output)?;
    let start_log = log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::RunStarted),
    );
    let started = Instant::now();

    let mut operation = match start_log {
        Ok(()) => doctor_checks(&settings, logger.clone()),
        Err(error) => Err(error),
    };
    let final_log = match &operation {
        Ok(_) => log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RunCompleted).metrics(EventMetrics {
                elapsed_ms: duration_millis(started.elapsed()),
                ..EventMetrics::default()
            }),
        ),
        Err(_) => log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Error, EventCode::RunFailed),
        ),
    };
    if operation.is_ok()
        && let Err(error) = final_log
    {
        operation = Err(error);
    }
    let result = finish_logger(logger.as_ref(), operation, settings.output.quiet)?;
    write_doctor_output(&result, &settings.output)?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Copy, Debug)]
struct DoctorResult {
    authenticated: bool,
    remote_checked: bool,
    remote_exists: Option<bool>,
    remote_entries: Option<usize>,
}

fn doctor_checks(
    settings: &config::ResolvedDoctor,
    logger: Option<Arc<EventLogger>>,
) -> Result<DoctorResult> {
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::ApiDiscoveryStarted),
    )?;
    let mut client = connect_client(&settings.url, &settings.network)?;
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::ApiDiscoveryCompleted),
    )?;
    if settings.routing_only {
        return Ok(DoctorResult {
            authenticated: false,
            remote_checked: false,
            remote_exists: None,
            remote_entries: None,
        });
    }

    let username = settings.username.as_deref().ok_or_else(|| {
        Error::Configuration("--username is required for authenticated doctor checks".to_owned())
    })?;
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::AuthenticationStarted),
    )?;
    let mut vault = credentials::VaultSession::new(
        !settings.authentication.no_vault,
        &settings.url,
        username,
        settings.network.allow_http,
    );
    let password = credentials::read_password_with_file(
        settings.authentication.password_stdin,
        settings.authentication.password_file.as_deref(),
        &mut vault,
    )?;
    credentials::authenticate_with_sources(
        &mut client,
        username,
        &password,
        &mut vault,
        settings.authentication.totp_secret_file.as_deref(),
    )?;
    drop(password);

    let operation = (|| {
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::AuthenticationCompleted),
        )?;
        let Some(remote) = settings.remote.as_deref() else {
            return Ok(DoctorResult {
                authenticated: true,
                remote_checked: false,
                remote_exists: None,
                remote_entries: None,
            });
        };
        let root = RemoteRoot::parse(remote)?;
        client.verify_share_writable(&root)?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanStarted),
        )?;
        let inventory = client.remote_inventory(&root)?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanCompleted).metrics(
                EventMetrics {
                    operations: inventory.entries.len() as u64,
                    ..EventMetrics::default()
                },
            ),
        )?;
        Ok(DoctorResult {
            authenticated: true,
            remote_checked: true,
            remote_exists: Some(inventory.root_exists),
            remote_entries: Some(inventory.entries.len()),
        })
    })();
    finish_authenticated_operation(&mut client, operation)
}

fn connect_client(url: &str, network: &config::ResolvedNetwork) -> Result<ApiClient> {
    ApiClient::connect(&ClientOptions {
        base_url: url.to_owned(),
        allow_http: network.allow_http,
        accept_invalid_certs: network.danger_accept_invalid_certs,
        ca_certificate: network.ca_certificate.clone(),
        connect_timeout: Duration::from_secs(network.connect_timeout),
        request_timeout: Duration::from_secs(network.timeout),
        retries: u32::from(network.retries),
    })
}

fn finish_authenticated_operation<T>(client: &mut ApiClient, operation: Result<T>) -> Result<T> {
    let logout = client.logout();
    match (operation, logout) {
        (Err(error), Err(logout_error)) => {
            eprintln!("warning: File Station logout also failed: {logout_error}");
            Err(error)
        }
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

struct ProgressWiring {
    tracker: ProgressTracker,
    renderer: Arc<Mutex<ProgressRenderer<io::Stderr>>>,
    failure: Arc<Mutex<Option<String>>>,
    last_render: Arc<Mutex<Instant>>,
    logger: Option<Arc<EventLogger>>,
    cancellation: CancellationToken,
}

impl ProgressWiring {
    fn new(
        plan: &SyncPlan,
        output: &config::ResolvedOutput,
        logger: Option<Arc<EventLogger>>,
        cancellation: CancellationToken,
    ) -> Self {
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: plan.uploads.len() as u64,
            files: plan.uploads.len() as u64,
            bytes: plan.upload_bytes,
        });
        let mode = renderer_progress_mode(output);
        let format = if output.log_format == cli::LogFormat::Json {
            ProgressFormat::Ndjson
        } else {
            ProgressFormat::Human
        };
        Self {
            tracker,
            renderer: Arc::new(Mutex::new(ProgressRenderer::new(
                io::stderr(),
                mode,
                format,
                io::stderr().is_terminal(),
            ))),
            failure: Arc::new(Mutex::new(None)),
            last_render: Arc::new(Mutex::new(
                Instant::now()
                    .checked_sub(PROGRESS_RENDER_INTERVAL)
                    .unwrap_or_else(Instant::now),
            )),
            logger,
            cancellation,
        }
    }

    fn observer_factory(&self) -> UploadObserverFactory {
        let tracker = self.tracker.clone();
        let renderer = Arc::clone(&self.renderer);
        let failure = Arc::clone(&self.failure);
        let last_render = Arc::clone(&self.last_render);
        let logger = self.logger.clone();
        let cancellation = self.cancellation.clone();
        Arc::new(move |entry: &LocalEntry| {
            let operation = tracker.start(OperationKind::Upload, entry.size);
            if let Some(logger) = &logger
                && let Err(error) = logger.emit(
                    LogEvent::new(EventLogLevel::Debug, EventCode::UploadStarted)
                        .operation(operation.operation_id()),
                )
            {
                record_progress_failure(&failure, error.to_string());
            }
            let tracker = tracker.clone();
            let renderer = Arc::clone(&renderer);
            let failure = Arc::clone(&failure);
            let last_render = Arc::clone(&last_render);
            let logger = logger.clone();
            let cancellation = cancellation.clone();
            let observer: UploadObserver = Arc::new(move |event| {
                let (update, force_render, log_code, attempt) = match event {
                    UploadTransferEvent::AttemptStarted { attempt } => (
                        operation.begin_attempt(),
                        true,
                        Some(EventCode::UploadAttemptStarted),
                        Some(attempt),
                    ),
                    UploadTransferEvent::Advanced { bytes } => {
                        (operation.advance(bytes), false, None, None)
                    }
                    UploadTransferEvent::Completed => (
                        operation.finish_success(),
                        true,
                        Some(EventCode::UploadCompleted),
                        None,
                    ),
                    UploadTransferEvent::Failed => {
                        (operation.fail(), true, Some(EventCode::UploadFailed), None)
                    }
                };
                let Ok(update) = update else {
                    record_progress_failure(
                        &failure,
                        "progress operation state became inconsistent".to_owned(),
                    );
                    cancellation.cancel();
                    return false;
                };
                if let Some(api_attempt) = attempt
                    && update.attempt != api_attempt
                {
                    record_progress_failure(
                        &failure,
                        "upload retry attempt accounting became inconsistent".to_owned(),
                    );
                    cancellation.cancel();
                    return false;
                }
                let snapshot = tracker.snapshot();
                if let Some(code) = log_code
                    && let Some(logger) = &logger
                {
                    let mut event = LogEvent::new(EventLogLevel::Debug, code)
                        .operation(update.operation_id)
                        .metrics(progress_metrics(&snapshot));
                    if let Some(attempt) = attempt {
                        event = event.attempt(attempt);
                    }
                    if let Err(error) = logger.emit(event) {
                        record_progress_failure(&failure, error.to_string());
                    }
                    if attempt.is_some_and(|attempt| attempt > 1)
                        && let Err(error) = logger.emit(
                            LogEvent::new(EventLogLevel::Debug, EventCode::RetryScheduled)
                                .operation(update.operation_id)
                                .attempt(update.attempt)
                                .metrics(progress_metrics(&snapshot)),
                        )
                    {
                        record_progress_failure(&failure, error.to_string());
                    }
                }
                let render_due = force_render || render_is_due(&last_render);
                if render_due {
                    if matches!(event, UploadTransferEvent::Advanced { .. })
                        && let Some(logger) = &logger
                        && let Err(error) = logger.emit(
                            LogEvent::new(EventLogLevel::Trace, EventCode::UploadProgress)
                                .operation(update.operation_id)
                                .attempt(update.attempt)
                                .metrics(progress_metrics(&snapshot)),
                        )
                    {
                        record_progress_failure(&failure, error.to_string());
                    }
                    match renderer.lock() {
                        Ok(mut renderer) => {
                            if let Err(error) = renderer.render(&snapshot, Some(&update)) {
                                record_progress_failure(&failure, error.to_string());
                            }
                        }
                        Err(_) => record_progress_failure(
                            &failure,
                            "progress renderer lock was poisoned".to_owned(),
                        ),
                    }
                }
                continue_after_progress_event(&cancellation, &failure)
            });
            Some(observer)
        })
    }

    fn finish(&self) -> Result<()> {
        match self.renderer.lock() {
            Ok(mut renderer) => renderer.finish().map_err(|error| {
                Error::Message(format!("failed to finish progress output: {error}"))
            })?,
            Err(_) => {
                return Err(Error::Message(
                    "progress renderer lock was poisoned".to_owned(),
                ));
            }
        }
        let failure = self
            .failure
            .lock()
            .map_err(|_| Error::Message("progress failure lock was poisoned".to_owned()))?
            .take();
        if let Some(failure) = failure {
            Err(Error::Message(format!(
                "progress or upload observability failed: {failure}"
            )))
        } else {
            Ok(())
        }
    }
}

fn renderer_progress_mode(output: &config::ResolvedOutput) -> RendererProgressMode {
    if !output.terminal_progress_enabled(io::stderr().is_terminal()) {
        return RendererProgressMode::Never;
    }
    match output.progress {
        cli::ProgressMode::Never => RendererProgressMode::Never,
        cli::ProgressMode::Always => RendererProgressMode::Always,
        cli::ProgressMode::Auto => RendererProgressMode::Auto,
    }
}

fn render_is_due(last_render: &Mutex<Instant>) -> bool {
    let Ok(mut last) = last_render.lock() else {
        return true;
    };
    if last.elapsed() >= PROGRESS_RENDER_INTERVAL {
        *last = Instant::now();
        true
    } else {
        false
    }
}

fn record_progress_failure(failure: &Mutex<Option<String>>, message: String) {
    if let Ok(mut failure) = failure.lock()
        && failure.is_none()
    {
        *failure = Some(message);
    }
}

fn has_progress_failure(failure: &Mutex<Option<String>>) -> bool {
    failure
        .lock()
        .map(|failure| failure.is_some())
        .unwrap_or(true)
}

fn continue_after_progress_event(
    cancellation: &CancellationToken,
    failure: &Mutex<Option<String>>,
) -> bool {
    if has_progress_failure(failure) {
        cancellation.cancel();
        false
    } else {
        !cancellation.is_cancelled()
    }
}

fn take_recorded_failure(failure: &Mutex<Option<String>>) -> Result<Option<String>> {
    failure
        .lock()
        .map_err(|_| Error::Message("observability failure lock was poisoned".to_owned()))
        .map(|mut failure| failure.take())
}

fn progress_metrics(snapshot: &synology_drive_sync::progress::ProgressSnapshot) -> EventMetrics {
    EventMetrics {
        operations: snapshot.completed_operations,
        files: snapshot.completed_files,
        bytes: snapshot.logical_bytes,
        elapsed_ms: duration_millis(snapshot.elapsed),
        throughput_bytes_per_second: snapshot.throughput_bytes_per_second.max(0.0) as u64,
        eta_ms: snapshot.eta.map(duration_millis),
    }
}

fn install_cancellation_handler(cancellation: CancellationToken) -> Result<()> {
    ctrlc::set_handler(move || cancellation.cancel())
        .map_err(|error| Error::Message(format!("failed to install Ctrl-C handler: {error}")))
}

fn build_logger(output: &config::ResolvedOutput) -> Result<Option<Arc<EventLogger>>> {
    if output.log_level == cli::LogLevel::Off {
        return Ok(None);
    }
    let level = match output.log_level {
        cli::LogLevel::Trace => EventLogLevel::Trace,
        cli::LogLevel::Debug => EventLogLevel::Debug,
        cli::LogLevel::Info => EventLogLevel::Info,
        cli::LogLevel::Warn => EventLogLevel::Warn,
        cli::LogLevel::Error => EventLogLevel::Error,
        cli::LogLevel::Off => unreachable!(),
    };
    let format = match output.log_format {
        cli::LogFormat::Human => EventLogFormat::Human,
        cli::LogFormat::Json => EventLogFormat::Json,
    };
    let file = output.log_file.as_ref().map(|path| FileLogConfig {
        path: path.clone(),
        format,
        max_bytes: FILE_LOG_MAX_BYTES,
        backups: FILE_LOG_BACKUPS,
    });
    let remote = output
        .remote_log_url
        .as_ref()
        .map(|endpoint| RemoteLogConfig {
            endpoint: endpoint.clone(),
            bearer_token: output.remote_log_token.as_ref().map(|source| match source {
                config::RemoteTokenSource::File(path) => BearerTokenSource::File(path.clone()),
                config::RemoteTokenSource::Environment(name) => {
                    BearerTokenSource::Environment(name.clone())
                }
            }),
            queue_capacity: REMOTE_LOG_QUEUE_CAPACITY,
            timeout: REMOTE_LOG_TIMEOUT,
            delivery: match output.remote_log_mode {
                cli::RemoteLogMode::BestEffort => RemoteDelivery::BestEffort,
                cli::RemoteLogMode::Required => RemoteDelivery::Required,
            },
        });
    let stderr = (!output.quiet).then_some(format);
    if stderr.is_none() && file.is_none() && remote.is_none() {
        return Ok(None);
    }
    EventLogger::new(LoggerConfig {
        level,
        stderr,
        file,
        remote,
    })
    .map(Arc::new)
    .map(Some)
    .map_err(observability_error)
}

fn log_event(logger: Option<&Arc<EventLogger>>, event: LogEvent) -> Result<()> {
    if let Some(logger) = logger {
        logger.emit(event).map_err(observability_error)?;
    }
    Ok(())
}

fn finish_logger<T>(
    logger: Option<&Arc<EventLogger>>,
    operation: Result<T>,
    quiet: bool,
) -> Result<T> {
    let shutdown = logger
        .map(|logger| logger.shutdown(LOGGER_SHUTDOWN_TIMEOUT))
        .transpose()
        .map_err(observability_error);
    match (operation, shutdown) {
        (Err(error), Err(shutdown_error)) => {
            if !quiet {
                eprintln!("warning: observability shutdown also failed: {shutdown_error}");
            }
            Err(error)
        }
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(Some(report))) => {
            if !quiet && (report.remote_events_dropped > 0 || report.remote_delivery_failures > 0) {
                eprintln!(
                    "warning: remote logging dropped {} events and recorded {} delivery failures",
                    report.remote_events_dropped, report.remote_delivery_failures
                );
            }
            Ok(value)
        }
        (Ok(value), Ok(None)) => Ok(value),
    }
}

fn run_config(arguments: &cli::Cli, action: cli::ConfigAction) -> Result<()> {
    if action == cli::ConfigAction::Path {
        let path = configured_path(&arguments.global).ok_or_else(|| {
            Error::Configuration("no platform configuration directory is available".to_owned())
        })?;
        return write_simple_value(
            arguments
                .global
                .output
                .output
                .unwrap_or(cli::OutputFormat::Human),
            path.display().to_string(),
            json!({"schema": "sdsync.config-path.v1", "path": path}),
        );
    }

    let loaded = load_required_config(&arguments.global)?;
    for profile in loaded.values.profiles.values() {
        config::validate_profile(profile).map_err(config_error)?;
    }
    let selected = loaded
        .select_profile(arguments.global.profile.as_deref())
        .map_err(config_error)?;
    let fallback = config::Profile::default();
    let effective_profile = selected.values.unwrap_or(&fallback);
    let output = config::resolve_output(effective_profile, &arguments.global.output)
        .map_err(config_error)?;

    match action {
        cli::ConfigAction::Path => unreachable!(),
        cli::ConfigAction::Validate => write_simple_value(
            output.output,
            format!(
                "Configuration is valid: {} profile(s); selected {:?}.",
                loaded.values.profiles.len(),
                selected.name
            ),
            json!({
                "schema": "sdsync.config-validation.v1",
                "valid": true,
                "path": loaded.path,
                "profiles": loaded.values.profiles.len(),
                "selected_profile": selected.name,
            }),
        ),
        cli::ConfigAction::Show => {
            let view = selected.non_secret_view();
            match output.output {
                cli::OutputFormat::Human => {
                    let text = toml::to_string_pretty(&view).map_err(|error| {
                        Error::Message(format!(
                            "failed to render non-secret configuration: {error}"
                        ))
                    })?;
                    print!("{text}");
                    io::stdout().flush().map_err(output_error)
                }
                cli::OutputFormat::Json => {
                    write_json(&serde_json::to_value(view).map_err(|error| {
                        Error::Message(format!("failed to render configuration JSON: {error}"))
                    })?)
                }
                cli::OutputFormat::Ndjson => {
                    write_json_line(&serde_json::to_value(view).map_err(|error| {
                        Error::Message(format!("failed to render configuration JSON: {error}"))
                    })?)
                }
            }
        }
    }
}

fn load_optional_config(global: &cli::GlobalArgs) -> Result<Option<config::LoadedConfig>> {
    let Some(path) = configured_path(global) else {
        if global.profile.is_some() {
            return Err(Error::Configuration(
                "--profile requires --config or a platform default configuration path".to_owned(),
            ));
        }
        return Ok(None);
    };
    if global.config.is_some() || path.exists() {
        config::LoadedConfig::load(path)
            .map(Some)
            .map_err(config_error)
    } else if global.profile.is_some() {
        Err(Error::Configuration(
            "--profile was supplied but the platform default configuration file does not exist"
                .to_owned(),
        ))
    } else {
        Ok(None)
    }
}

fn load_required_config(global: &cli::GlobalArgs) -> Result<config::LoadedConfig> {
    let path = configured_path(global).ok_or_else(|| {
        Error::Configuration(
            "no platform configuration directory is available; pass --config".to_owned(),
        )
    })?;
    config::LoadedConfig::load(path).map_err(config_error)
}

fn configured_path(global: &cli::GlobalArgs) -> Option<PathBuf> {
    global.config.clone().or_else(config::default_config_path)
}

fn select_optional_profile<'a>(
    loaded: Option<&'a config::LoadedConfig>,
    requested: Option<&str>,
) -> Result<Option<config::SelectedProfile<'a>>> {
    match loaded {
        Some(loaded) => loaded
            .select_profile(requested)
            .map(Some)
            .map_err(config_error),
        None if requested.is_some() => Err(Error::Configuration(
            "--profile requires an existing configuration file".to_owned(),
        )),
        None => Ok(None),
    }
}

fn write_completions(shell: cli::CompletionShell) -> Result<()> {
    let shell = match shell {
        cli::CompletionShell::Bash => clap_complete::Shell::Bash,
        cli::CompletionShell::Zsh => clap_complete::Shell::Zsh,
        cli::CompletionShell::Fish => clap_complete::Shell::Fish,
        cli::CompletionShell::PowerShell => clap_complete::Shell::PowerShell,
        cli::CompletionShell::Elvish => clap_complete::Shell::Elvish,
    };
    let mut command = cli::Cli::command();
    let name = command.get_name().to_owned();
    clap_complete::generate(shell, &mut command, name, &mut io::stdout());
    Ok(())
}

fn write_manpage(directory: Option<&std::path::Path>) -> Result<()> {
    if let Some(directory) = directory {
        std::fs::create_dir_all(directory).map_err(|error| {
            Error::Message(format!(
                "failed to create manpage output directory {directory:?}: {error}"
            ))
        })?;
        return clap_mangen::generate_to(cli::Cli::command(), directory).map_err(|error| {
            Error::Message(format!(
                "failed to generate manual pages in {directory:?}: {error}"
            ))
        });
    }

    clap_mangen::Man::new(cli::Cli::command())
        .render(&mut io::stdout())
        .map_err(output_error)
}

fn write_sync_output(
    plan: &SyncPlan,
    report: Option<&ExecutionReport>,
    elapsed: Duration,
    output: &config::ResolvedOutput,
    plan_only: bool,
) -> Result<()> {
    match output.output {
        cli::OutputFormat::Human => {
            print_plan_human(plan, plan_only || output.verbosity > 0);
            if let Some(report) = report {
                println!(
                    "Sync complete: {} uploaded ({}), {} directories created, {} remote entries deleted in {} ms.",
                    report.uploaded,
                    format_bytes(plan.upload_bytes),
                    report.created,
                    report.deleted,
                    duration_millis(elapsed),
                );
            } else if !plan_only && plan.is_empty() {
                println!("Already in sync; no remote changes were needed.");
            } else if plan_only {
                println!("Plan only; no remote changes were made.");
            }
            io::stdout().flush().map_err(output_error)
        }
        cli::OutputFormat::Json => {
            write_json(&command_json_value(plan, report, elapsed, plan_only))
        }
        cli::OutputFormat::Ndjson => {
            write_plan_ndjson(plan)?;
            if let Some(report) = report {
                write_json_line(&json!({
                    "schema": "sdsync.output.v1",
                    "kind": "completion",
                    "result": execution_value(report, plan.upload_bytes, elapsed),
                }))?;
            } else if !plan_only {
                write_json_line(&json!({
                    "schema": "sdsync.output.v1",
                    "kind": "completion",
                    "changed": false,
                }))?;
            }
            Ok(())
        }
    }
}

fn print_plan_human(plan: &SyncPlan, detailed: bool) {
    println!(
        "Plan: {} uploads ({}), {} directories, {} deletions, {} unchanged files, {} protected remote entries.",
        plan.uploads.len(),
        format_bytes(plan.upload_bytes),
        plan.creates.len(),
        plan.delete_count(),
        plan.unchanged_files,
        plan.protected_entries
    );
    if !detailed {
        return;
    }
    for action in &plan.pre_deletes {
        println!("  DELETE-CONFLICT {}", action.remote_path);
    }
    for action in &plan.creates {
        println!("  MKDIR  {}", action.remote_path);
    }
    for action in &plan.uploads {
        println!(
            "  UPLOAD {} -> {}",
            action.local.relative, action.remote_path
        );
    }
    for action in &plan.post_deletes {
        println!("  DELETE {}", action.remote_path);
    }
}

fn plan_value(plan: &SyncPlan) -> Value {
    json!({
        "summary": {
            "uploads": plan.uploads.len(),
            "upload_bytes": plan.upload_bytes,
            "directories": plan.creates.len(),
            "deletions": plan.delete_count(),
            "unchanged_files": plan.unchanged_files,
            "protected_entries": plan.protected_entries,
            "changes": !plan.is_empty(),
        },
        "actions": {
            "pre_deletes": plan.pre_deletes.iter().map(|action| json!({
                "relative": action.relative,
                "remote_path": action.remote_path,
                "entry_kind": action.kind.as_str(),
                "type_conflict": action.type_conflict,
            })).collect::<Vec<_>>(),
            "creates": plan.creates.iter().map(|action| json!({
                "relative": action.relative,
                "remote_path": action.remote_path,
            })).collect::<Vec<_>>(),
            "uploads": plan.uploads.iter().map(|action| json!({
                "relative": action.local.relative,
                "remote_path": action.remote_path,
                "bytes": action.local.size,
                "mtime_ms": action.local.mtime_ms,
            })).collect::<Vec<_>>(),
            "post_deletes": plan.post_deletes.iter().map(|action| json!({
                "relative": action.relative,
                "remote_path": action.remote_path,
                "entry_kind": action.kind.as_str(),
                "type_conflict": action.type_conflict,
            })).collect::<Vec<_>>(),
        }
    })
}

fn command_json_value(
    plan: &SyncPlan,
    report: Option<&ExecutionReport>,
    elapsed: Duration,
    plan_only: bool,
) -> Value {
    let mut value = json!({
        "schema": if plan_only { "sdsync.plan.v1" } else { "sdsync.sync.v1" },
        "plan": plan_value(plan),
    });
    if let Some(report) = report {
        value["result"] = execution_value(report, plan.upload_bytes, elapsed);
    } else if !plan_only {
        value["result"] = json!({"changed": false});
    }
    value
}

fn write_plan_ndjson(plan: &SyncPlan) -> Result<()> {
    write_json_line(&plan_summary_record(plan))?;
    for action in &plan.pre_deletes {
        write_json_line(&json!({
            "schema": "sdsync.plan-action.v1", "action": "delete-conflict",
            "relative": action.relative, "remote_path": action.remote_path,
            "entry_kind": action.kind.as_str(),
        }))?;
    }
    for action in &plan.creates {
        write_json_line(&json!({
            "schema": "sdsync.plan-action.v1", "action": "create-directory",
            "relative": action.relative, "remote_path": action.remote_path,
        }))?;
    }
    for action in &plan.uploads {
        write_json_line(&json!({
            "schema": "sdsync.plan-action.v1", "action": "upload",
            "relative": action.local.relative, "remote_path": action.remote_path,
            "bytes": action.local.size, "mtime_ms": action.local.mtime_ms,
        }))?;
    }
    for action in &plan.post_deletes {
        write_json_line(&json!({
            "schema": "sdsync.plan-action.v1", "action": "delete",
            "relative": action.relative, "remote_path": action.remote_path,
            "entry_kind": action.kind.as_str(),
        }))?;
    }
    Ok(())
}

fn plan_summary_record(plan: &SyncPlan) -> Value {
    json!({
        "schema": "sdsync.plan.v1",
        "kind": "summary",
        "uploads": plan.uploads.len(),
        "upload_bytes": plan.upload_bytes,
        "directories": plan.creates.len(),
        "deletions": plan.delete_count(),
        "unchanged_files": plan.unchanged_files,
        "protected_entries": plan.protected_entries,
        "changes": !plan.is_empty(),
    })
}

fn execution_value(report: &ExecutionReport, upload_bytes: u64, elapsed: Duration) -> Value {
    json!({
        "changed": report.uploaded > 0 || report.created > 0 || report.deleted > 0,
        "uploaded": report.uploaded,
        "upload_bytes": upload_bytes,
        "directories_created": report.created,
        "deleted": report.deleted,
        "elapsed_ms": duration_millis(elapsed),
    })
}

fn write_doctor_output(result: &DoctorResult, output: &config::ResolvedOutput) -> Result<()> {
    let value = json!({
        "schema": "sdsync.doctor.v1",
        "routing": true,
        "api_discovery": true,
        "authenticated": result.authenticated,
        "remote_checked": result.remote_checked,
        "remote_exists": result.remote_exists,
        "remote_entries": result.remote_entries,
    });
    match output.output {
        cli::OutputFormat::Human => {
            if result.authenticated {
                if result.remote_checked {
                    println!(
                        "Doctor: routing, API discovery, authentication, and remote access are healthy ({} entries; destination {}).",
                        result.remote_entries.unwrap_or(0),
                        if result.remote_exists == Some(true) {
                            "exists"
                        } else {
                            "will be created"
                        },
                    );
                } else {
                    println!("Doctor: routing, API discovery, and authentication are healthy.");
                }
            } else {
                println!(
                    "Doctor: reverse-proxy routing and File Station API discovery are healthy."
                );
            }
            io::stdout().flush().map_err(output_error)
        }
        cli::OutputFormat::Json => write_json(&value),
        cli::OutputFormat::Ndjson => write_json_line(&value),
    }
}

fn write_credential_output(
    result: credentials::CredentialOutcome,
    format: cli::OutputFormat,
) -> Result<()> {
    let (human, value) = match result {
        credentials::CredentialOutcome::StoredPassword => (
            "Stored the DSM password in the OS credential vault.".to_owned(),
            json!({
                "schema": "sdsync.credentials.v1",
                "kind": "stored",
                "credential": "password",
            }),
        ),
        credentials::CredentialOutcome::StoredTotp => (
            "Stored the DSM TOTP seed in the OS credential vault.".to_owned(),
            json!({
                "schema": "sdsync.credentials.v1",
                "kind": "stored",
                "credential": "totp",
            }),
        ),
        credentials::CredentialOutcome::Status {
            password_stored,
            totp_stored,
        } => (
            format!(
                "Password: {}; TOTP seed: {}.",
                if password_stored {
                    "stored"
                } else {
                    "not stored"
                },
                if totp_stored { "stored" } else { "not stored" },
            ),
            json!({
                "schema": "sdsync.credentials.v1",
                "kind": "status",
                "password_stored": password_stored,
                "totp_stored": totp_stored,
            }),
        ),
        credentials::CredentialOutcome::Removed {
            password_removed,
            totp_removed,
        } => {
            let mut parts = Vec::new();
            if let Some(removed) = password_removed {
                parts.push(format!(
                    "Password: {}",
                    if removed { "removed" } else { "not stored" }
                ));
            }
            if let Some(removed) = totp_removed {
                parts.push(format!(
                    "TOTP seed: {}",
                    if removed { "removed" } else { "not stored" }
                ));
            }
            (
                format!("{}.", parts.join("; ")),
                json!({
                    "schema": "sdsync.credentials.v1",
                    "kind": "removed",
                    "password_removed": password_removed,
                    "totp_removed": totp_removed,
                }),
            )
        }
    };
    write_simple_value(format, human, value)
}

fn write_simple_value(format: cli::OutputFormat, human: String, value: Value) -> Result<()> {
    match format {
        cli::OutputFormat::Human => {
            println!("{human}");
            io::stdout().flush().map_err(output_error)
        }
        cli::OutputFormat::Json => write_json(&value),
        cli::OutputFormat::Ndjson => write_json_line(&value),
    }
}

fn write_json(value: &Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)
        .map_err(|error| Error::Message(format!("failed to write JSON output: {error}")))?;
    writeln!(stdout).map_err(output_error)
}

fn write_json_line(value: &Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)
        .map_err(|error| Error::Message(format!("failed to write JSON output: {error}")))?;
    writeln!(stdout).map_err(output_error)
}

fn warn_for_insecure_network(network: &config::ResolvedNetwork, output: &config::ResolvedOutput) {
    if output.quiet {
        return;
    }
    if network.allow_http {
        eprintln!(
            "warning: HTTP is enabled; DSM credentials, OTP codes, and file data may be exposed in transit"
        );
    }
    if network.danger_accept_invalid_certs {
        eprintln!("warning: TLS certificate verification is disabled");
    }
}

fn compare_mode(value: cli::CompareArg) -> CompareMode {
    match value {
        cli::CompareArg::Metadata => CompareMode::Metadata,
        cli::CompareArg::SizeOnly => CompareMode::SizeOnly,
    }
}

fn operation_count(plan: &SyncPlan) -> u64 {
    (plan.pre_deletes.len() + plan.creates.len() + plan.uploads.len() + plan.post_deletes.len())
        as u64
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn config_error(error: config::ConfigError) -> Error {
    Error::Configuration(error.to_string())
}

fn observability_error(error: synology_drive_sync::observability::ObservabilityError) -> Error {
    use synology_drive_sync::observability::ObservabilityError;

    let configuration = matches!(
        &error,
        ObservabilityError::InvalidLogLevel
            | ObservabilityError::InvalidFileConfiguration
            | ObservabilityError::InvalidRemoteEndpoint
            | ObservabilityError::InvalidRemoteConfiguration
            | ObservabilityError::InvalidBearerToken
    );
    let message = error.to_string();
    if configuration {
        Error::Configuration(message)
    } else {
        Error::Message(message)
    }
}

fn output_error(error: io::Error) -> Error {
    Error::Message(format!("failed to write command output: {error}"))
}

fn print_error(error: &Error) {
    eprintln!("error: {error}");
}

fn error_exit_code(error: &Error) -> u8 {
    match error {
        Error::Cancelled => 130,
        Error::Configuration(_)
        | Error::InvalidUrl(_)
        | Error::HttpsRequired
        | Error::UnsafeRemotePath { .. } => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_plan() -> SyncPlan {
        SyncPlan {
            pre_deletes: Vec::new(),
            creates: Vec::new(),
            uploads: Vec::new(),
            post_deletes: Vec::new(),
            unchanged_files: 2,
            protected_entries: 1,
            upload_bytes: 0,
        }
    }

    #[test]
    fn formats_human_sizes() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn machine_output_disables_progress() {
        let output = config::ResolvedOutput {
            verbosity: 0,
            quiet: false,
            log_level: cli::LogLevel::Info,
            log_format: cli::LogFormat::Human,
            log_file: None,
            remote_log_url: None,
            remote_log_token: None,
            remote_log_mode: cli::RemoteLogMode::BestEffort,
            progress: cli::ProgressMode::Always,
            output: cli::OutputFormat::Json,
        };
        assert_eq!(renderer_progress_mode(&output), RendererProgressMode::Never);
    }

    #[test]
    fn logger_defaults_match_published_contract() {
        assert_eq!(FILE_LOG_MAX_BYTES, 10 * 1024 * 1024);
        assert_eq!(FILE_LOG_BACKUPS, 3);
        assert_eq!(REMOTE_LOG_QUEUE_CAPACITY, 1_024);
        assert_eq!(REMOTE_LOG_TIMEOUT, Duration::from_secs(10));
        assert_eq!(LOGGER_SHUTDOWN_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn stable_exit_codes_distinguish_configuration_and_operations() {
        assert_eq!(
            error_exit_code(&Error::Configuration("invalid profile".to_owned())),
            2
        );
        assert_eq!(
            error_exit_code(&Error::Message("network operation failed".to_owned())),
            1
        );
        assert_eq!(error_exit_code(&Error::Cancelled), 130);
    }

    #[test]
    fn structured_output_contract_has_stable_document_and_stream_schemas() {
        let plan = empty_plan();
        let planned = command_json_value(&plan, None, Duration::ZERO, true);
        assert_eq!(planned["schema"], "sdsync.plan.v1");
        assert_eq!(planned["plan"]["summary"]["changes"], false);
        assert!(planned.get("result").is_none());

        let report = ExecutionReport::default();
        let synced = command_json_value(&plan, Some(&report), Duration::from_millis(12), false);
        assert_eq!(synced["schema"], "sdsync.sync.v1");
        assert_eq!(synced["result"]["changed"], false);
        assert_eq!(synced["result"]["elapsed_ms"], 12);

        let summary = plan_summary_record(&plan);
        assert_eq!(summary["schema"], "sdsync.plan.v1");
        assert_eq!(summary["kind"], "summary");
    }

    #[test]
    fn progress_failure_cancels_before_post_upload_deletions() {
        let cancellation = CancellationToken::default();
        let failure = Mutex::new(Some(
            "completion-event logger rejected the record".to_owned(),
        ));

        assert!(!continue_after_progress_event(&cancellation, &failure));
        assert!(cancellation.is_cancelled());
    }
}
