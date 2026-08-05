#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::CommandFactory;
use serde_json::{Value, json};
use synology_drive_sync::api::{
    ApiClient, ClientOptions, UploadObserver, UploadTransferEvent, WriteProbeReport,
};
use synology_drive_sync::batch::{BatchJob, ValidatedBatch};
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::local::{self, IgnoreRules, LocalEntry};
use synology_drive_sync::observability::{
    BearerTokenSource, EventCode, EventLogger, EventMetrics, FileLogConfig, LogEvent,
    LogFormat as EventLogFormat, LogLevel as EventLogLevel, LoggerConfig, RemoteDelivery,
    RemoteLogConfig,
};
use synology_drive_sync::path::RemoteRoot;
use synology_drive_sync::plan::{self, CompareMode, PlanOptions, RemoteSnapshot, SyncPlan};
use synology_drive_sync::progress::{
    OperationKind, ProgressFormat, ProgressMode as RendererProgressMode, ProgressRenderer,
    ProgressTotals, ProgressTracker,
};
use synology_drive_sync::source_diagnostics::{
    SourceDiagnosticOptions, SourceDiagnosticReport, diagnose_source,
};
use synology_drive_sync::sync::{self, ExecuteOptions, ExecutionReport, UploadObserverFactory};
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

struct NamedProfile<'a> {
    name: String,
    values: Option<&'a config::Profile>,
}

struct NamedSyncSettings {
    name: String,
    settings: config::ResolvedSync,
}

struct NamedDoctorSettings {
    name: String,
    settings: config::ResolvedDoctor,
}

struct NamedSourceSettings {
    name: String,
    settings: config::ResolvedSourceDoctor,
}

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
    match arguments.invocation() {
        cli::Invocation::Sync {
            arguments: sync,
            legacy,
        } => {
            let selected = select_job_profiles(
                loaded.as_ref(),
                arguments.global.profile.as_deref(),
                &sync.batch,
            )?;
            validate_batch_pair_overrides(
                &selected,
                &sync.batch,
                sync.source.is_some(),
                sync.remote.is_some(),
            )?;
            let mut resolved = Vec::with_capacity(selected.len());
            for selection in &selected {
                resolved.push(NamedSyncSettings {
                    name: selection.name.clone(),
                    settings: config::resolve_sync(
                        selection.values,
                        sync,
                        &arguments.global.output,
                    )
                    .map_err(config_error)?,
                });
            }
            if legacy
                && resolved[0].settings.output.verbosity > 0
                && !resolved[0].settings.output.quiet
            {
                eprintln!(
                    "warning: the positional sync form is retained for compatibility; prefer the `sync` subcommand"
                );
            }
            if sync.batch.requested() {
                run_sync_batch(
                    resolved,
                    sync.dry_run,
                    false,
                    sync.batch
                        .max_total_delete
                        .unwrap_or(config::DEFAULT_MAX_TOTAL_DELETE),
                )
            } else {
                run_sync(resolved.remove(0).settings, sync.dry_run, false)
            }
        }
        cli::Invocation::Plan(plan) => {
            let selected = select_job_profiles(
                loaded.as_ref(),
                arguments.global.profile.as_deref(),
                &plan.sync.batch,
            )?;
            validate_batch_pair_overrides(
                &selected,
                &plan.sync.batch,
                plan.sync.source.is_some(),
                plan.sync.remote.is_some(),
            )?;
            let mut resolved = Vec::with_capacity(selected.len());
            for selection in &selected {
                resolved.push(NamedSyncSettings {
                    name: selection.name.clone(),
                    settings: config::resolve_sync(
                        selection.values,
                        &plan.sync,
                        &arguments.global.output,
                    )
                    .map_err(config_error)?,
                });
            }
            if plan.sync.batch.requested() {
                run_sync_batch(
                    resolved,
                    true,
                    plan.exit_code,
                    plan.sync
                        .batch
                        .max_total_delete
                        .unwrap_or(config::DEFAULT_MAX_TOTAL_DELETE),
                )
            } else {
                run_sync(resolved.remove(0).settings, true, plan.exit_code)
            }
        }
        cli::Invocation::Doctor(doctor) => {
            let selected = select_job_profiles(
                loaded.as_ref(),
                arguments.global.profile.as_deref(),
                &doctor.batch,
            )?;
            match doctor.action.as_ref() {
                Some(cli::DoctorAction::Source(source)) => {
                    if doctor.routing_only {
                        return Err(Error::Configuration(
                            "doctor source cannot be combined with --routing-only".to_owned(),
                        ));
                    }
                    validate_batch_source_override(
                        &selected,
                        &doctor.batch,
                        source.source.is_some(),
                    )?;
                    let mut resolved = Vec::with_capacity(selected.len());
                    for selection in &selected {
                        resolved.push(NamedSourceSettings {
                            name: selection.name.clone(),
                            settings: config::resolve_source_doctor(
                                selection.values,
                                source,
                                &arguments.global.output,
                            )
                            .map_err(config_error)?,
                        });
                    }
                    if doctor.batch.requested() {
                        run_source_doctor_batch(resolved)
                    } else {
                        run_source_doctor(resolved.remove(0).settings)
                    }
                }
                _ => {
                    validate_batch_doctor_target_override(doctor)?;
                    let mut resolved = Vec::with_capacity(selected.len());
                    for selection in &selected {
                        resolved.push(NamedDoctorSettings {
                            name: selection.name.clone(),
                            settings: config::resolve_doctor(
                                selection.values,
                                doctor,
                                &arguments.global.output,
                            )
                            .map_err(config_error)?,
                        });
                    }
                    if doctor.batch.requested() {
                        run_doctor_batch(resolved)
                    } else {
                        run_doctor(resolved.remove(0).settings)
                    }
                }
            }
        }
        cli::Invocation::Credentials(command) => {
            let selected =
                select_optional_profile(loaded.as_ref(), arguments.global.profile.as_deref())?;
            let profile = selected.as_ref().and_then(|selection| selection.values);
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

struct TimedSourceDiagnostic {
    report: SourceDiagnosticReport,
    hash_content: bool,
    elapsed: Duration,
}

fn run_source_doctor(settings: config::ResolvedSourceDoctor) -> Result<ExitCode> {
    let cancellation = CancellationToken::default();
    install_cancellation_handler(cancellation.clone())?;
    let result = diagnose_source_job(&settings, &cancellation)?;
    write_source_doctor_output(&result, &settings.output)?;
    if cancellation.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn diagnose_source_job(
    settings: &config::ResolvedSourceDoctor,
    cancellation: &CancellationToken,
) -> Result<TimedSourceDiagnostic> {
    let logger = build_logger(&settings.output)?;
    let started = Instant::now();
    let mut operation = (|| {
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RunStarted),
        )?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::LocalScanStarted),
        )?;
        let report = diagnose_source(
            &settings.source,
            &settings.excludes,
            SourceDiagnosticOptions {
                hash_content: settings.hash_content,
            },
            cancellation,
        )?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::LocalScanCompleted).metrics(
                EventMetrics {
                    operations: report.entries as u64,
                    files: report.files as u64,
                    bytes: report.bytes,
                    elapsed_ms: duration_millis(started.elapsed()),
                    ..EventMetrics::default()
                },
            ),
        )?;
        Ok(report)
    })();
    let final_log = match &operation {
        Ok(report) => log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RunCompleted).metrics(EventMetrics {
                operations: report.entries as u64,
                files: report.files as u64,
                bytes: report.bytes,
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
    let report = finish_logger(logger.as_ref(), operation, settings.output.quiet)?;
    Ok(TimedSourceDiagnostic {
        report,
        hash_content: settings.hash_content,
        elapsed: started.elapsed(),
    })
}

fn source_diagnostic_value(result: &TimedSourceDiagnostic) -> Value {
    json!({
        "canonical_source": result.report.canonical_root,
        "entries": result.report.entries,
        "files": result.report.files,
        "directories": result.report.directories,
        "bytes": result.report.bytes,
        "content_hashed": result.hash_content,
        "hashed_files": result.report.hashed_files,
        "elapsed_ms": duration_millis(result.elapsed),
    })
}

fn write_source_doctor_output(
    result: &TimedSourceDiagnostic,
    output: &config::ResolvedOutput,
) -> Result<()> {
    let value = json!({
        "schema": "sdsync.source-doctor.v1",
        "source": source_diagnostic_value(result),
    });
    match output.output {
        cli::OutputFormat::Human => {
            println!(
                "Source is healthy: {} files, {} directories, {} across {} entries; {} files hashed in {} ms ({}).",
                result.report.files,
                result.report.directories,
                format_bytes(result.report.bytes),
                result.report.entries,
                result.report.hashed_files,
                duration_millis(result.elapsed),
                result.report.canonical_root.display(),
            );
            io::stdout().flush().map_err(output_error)
        }
        cli::OutputFormat::Json => write_json(&value),
        cli::OutputFormat::Ndjson => write_json_line(&value),
    }
}

struct SourceBatchOutcome {
    name: String,
    result: Option<TimedSourceDiagnostic>,
    error: Option<String>,
    not_run: bool,
}

fn run_source_doctor_batch(mut jobs: Vec<NamedSourceSettings>) -> Result<ExitCode> {
    jobs.sort_by(|left, right| left.name.cmp(&right.name));
    let output = common_batch_output(jobs.iter().map(|job| &job.settings.output))?;
    let cancellation = CancellationToken::default();
    install_cancellation_handler(cancellation.clone())?;
    let mut outcomes = Vec::with_capacity(jobs.len());
    let mut cancelled = false;
    for job in jobs {
        if cancellation.is_cancelled() {
            cancelled |= cancellation.is_cancelled();
            outcomes.push(SourceBatchOutcome {
                name: job.name,
                result: None,
                error: None,
                not_run: true,
            });
            continue;
        }
        match diagnose_source_job(&job.settings, &cancellation) {
            Ok(result) => outcomes.push(SourceBatchOutcome {
                name: job.name,
                result: Some(result),
                error: None,
                not_run: false,
            }),
            Err(error) => {
                cancelled |= matches!(error, Error::Cancelled);
                outcomes.push(SourceBatchOutcome {
                    name: job.name,
                    result: None,
                    error: Some(error.to_string()),
                    not_run: false,
                });
            }
        }
    }
    cancelled |= cancellation.is_cancelled();
    write_source_batch_output(&outcomes, &output)?;
    if cancelled || cancellation.is_cancelled() {
        Err(Error::Cancelled)
    } else if outcomes.iter().any(|outcome| outcome.error.is_some()) {
        Err(Error::Message(
            "one or more source diagnostic batch jobs failed; inspect the per-job results"
                .to_owned(),
        ))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn write_source_batch_output(
    outcomes: &[SourceBatchOutcome],
    output: &config::ResolvedOutput,
) -> Result<()> {
    let succeeded = outcomes
        .iter()
        .filter(|outcome| outcome.result.is_some())
        .count();
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.error.is_some())
        .count();
    let not_run = outcomes.iter().filter(|outcome| outcome.not_run).count();
    let status = if failed == 0 && not_run == 0 {
        "success"
    } else if succeeded == 0 {
        "failed"
    } else {
        "partial"
    };
    let job_values = outcomes
        .iter()
        .map(|outcome| {
            json!({
                "schema": "sdsync.source-doctor-job.v1",
                "profile": outcome.name,
                "status": if outcome.result.is_some() { "success" } else if outcome.not_run { "not-run" } else { "failed" },
                "source": outcome.result.as_ref().map(source_diagnostic_value),
                "error": outcome.error,
            })
        })
        .collect::<Vec<_>>();
    let summary = json!({
        "schema": "sdsync.source-doctor-batch.v1",
        "status": status,
        "summary": {
            "jobs": outcomes.len(),
            "succeeded": succeeded,
            "failed": failed,
            "not_run": not_run,
        },
    });
    match output.output {
        cli::OutputFormat::Human => {
            println!(
                "Source diagnostic batch: {succeeded} succeeded, {failed} failed, {not_run} not run."
            );
            for outcome in outcomes {
                if let Some(result) = &outcome.result {
                    println!(
                        "  [{}] healthy: {} files, {} directories, {}, {} hashed ({})",
                        outcome.name,
                        result.report.files,
                        result.report.directories,
                        format_bytes(result.report.bytes),
                        result.report.hashed_files,
                        result.report.canonical_root.display(),
                    );
                } else if outcome.not_run {
                    println!("  [{}] not run", outcome.name);
                } else {
                    println!(
                        "  [{}] failed: {}",
                        outcome.name,
                        outcome.error.as_deref().unwrap_or("unknown failure")
                    );
                }
            }
            io::stdout().flush().map_err(output_error)
        }
        cli::OutputFormat::Json => {
            let mut value = summary;
            value["jobs"] = Value::Array(job_values);
            write_json(&value)
        }
        cli::OutputFormat::Ndjson => {
            for value in job_values {
                write_json_line(&value)?;
            }
            write_json_line(&summary)
        }
    }
}

fn common_batch_output<'a>(
    mut outputs: impl Iterator<Item = &'a config::ResolvedOutput>,
) -> Result<config::ResolvedOutput> {
    let first = outputs.next().ok_or_else(|| {
        Error::Configuration("a batch must contain at least one selected profile".to_owned())
    })?;
    for output in outputs {
        if output.output != first.output {
            return Err(Error::Configuration(
                "selected profiles resolve different output formats; pass --output explicitly for the batch"
                    .to_owned(),
            ));
        }
    }
    Ok(first.clone())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncBatchStatus {
    Preflighted,
    Success,
    Partial,
    Failed,
    NotRun,
}

impl SyncBatchStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preflighted => "preflighted",
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::NotRun => "not-run",
        }
    }
}

struct SyncBatchOutcome {
    name: String,
    status: SyncBatchStatus,
    preflight_plan: Option<SyncPlan>,
    execution_plan: Option<SyncPlan>,
    mutation_authorized: bool,
    report: Option<ExecutionReport>,
    elapsed: Option<Duration>,
    error: Option<String>,
}

fn run_sync_batch(
    mut jobs: Vec<NamedSyncSettings>,
    plan_only: bool,
    changes_exit_code: bool,
    max_total_delete: usize,
) -> Result<ExitCode> {
    jobs.sort_by(|left, right| left.name.cmp(&right.name));
    let output = common_batch_output(jobs.iter().map(|job| &job.settings.output))?;
    let validated = validate_sync_batch(&jobs)?;
    let cancellation = CancellationToken::default();
    install_cancellation_handler(cancellation.clone())?;

    // Every target must produce a complete, non-mutating plan before any target is allowed to
    // mutate. Ordinary failures do not prevent the remaining preflights; cancellation does.
    let mut outcomes = Vec::with_capacity(jobs.len());
    let mut cancelled = false;
    for job in &jobs {
        if cancellation.is_cancelled() {
            cancelled = true;
            outcomes.push(SyncBatchOutcome {
                name: job.name.clone(),
                status: SyncBatchStatus::NotRun,
                preflight_plan: None,
                execution_plan: None,
                mutation_authorized: false,
                report: None,
                elapsed: None,
                error: None,
            });
            continue;
        }
        match run_sync_job(&job.settings, true, &cancellation, |_| Ok(())) {
            Ok(result) => outcomes.push(SyncBatchOutcome {
                name: job.name.clone(),
                status: SyncBatchStatus::Preflighted,
                preflight_plan: Some(result.plan),
                execution_plan: None,
                mutation_authorized: false,
                report: None,
                elapsed: Some(result.elapsed),
                error: None,
            }),
            Err(error) => {
                cancelled |= matches!(error, Error::Cancelled);
                outcomes.push(SyncBatchOutcome {
                    name: job.name.clone(),
                    status: SyncBatchStatus::Failed,
                    preflight_plan: None,
                    execution_plan: None,
                    mutation_authorized: false,
                    report: None,
                    elapsed: None,
                    error: Some(error.to_string()),
                });
            }
        }
    }
    cancelled |= cancellation.is_cancelled();

    if outcomes
        .iter()
        .any(|outcome| outcome.status == SyncBatchStatus::Failed)
        || cancelled
    {
        write_sync_batch_output(
            &outcomes,
            &output,
            plan_only,
            max_total_delete,
            None,
            None,
            None,
        )?;
        return if cancelled || cancellation.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Err(Error::Message(
                "one or more batch preflights failed; no remote mutations were attempted"
                    .to_owned(),
            ))
        };
    }

    let deletion_preflight = match validated.preflight_deletions(
        outcomes.iter().map(|outcome| {
            (
                outcome.name.as_str(),
                outcome
                    .preflight_plan
                    .as_ref()
                    .map_or(0, SyncPlan::delete_count),
            )
        }),
        Some(max_total_delete),
    ) {
        Ok(preflight) => preflight,
        Err(error) => {
            let message = error.to_string();
            write_sync_batch_output(
                &outcomes,
                &output,
                plan_only,
                max_total_delete,
                None,
                None,
                Some(&message),
            )?;
            return Err(Error::Message(message));
        }
    };

    if plan_only {
        let changes = outcomes.iter().any(|outcome| {
            outcome
                .preflight_plan
                .as_ref()
                .is_some_and(|plan| !plan.is_empty())
        });
        write_sync_batch_output(
            &outcomes,
            &output,
            true,
            max_total_delete,
            Some(deletion_preflight.total_planned),
            None,
            None,
        )?;
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        return if changes_exit_code && changes {
            Ok(ExitCode::from(cli::PLAN_CHANGES_EXIT_CODE))
        } else {
            Ok(ExitCode::SUCCESS)
        };
    }

    // Execute deterministically and serially. Each job is replanned immediately before it can
    // mutate; the guard reserves its full fresh deletion count against the aggregate cap.
    let mut reserved_deletions = 0_usize;
    let mut stopped = false;
    for (job, outcome) in jobs.iter().zip(&mut outcomes) {
        if stopped || cancellation.is_cancelled() {
            cancelled |= cancellation.is_cancelled();
            outcome.status = SyncBatchStatus::NotRun;
            outcome.report = None;
            outcome.elapsed = None;
            continue;
        }
        let mut fresh_plan = None;
        let mut mutation_authorized = false;
        let execution = run_sync_job(&job.settings, false, &cancellation, |plan| {
            fresh_plan = Some(plan.clone());
            reserve_batch_deletions(
                &mut reserved_deletions,
                plan.delete_count(),
                max_total_delete,
                &job.name,
            )?;
            mutation_authorized = !plan.is_empty();
            Ok(())
        });
        match execution {
            Ok(result) => {
                outcome.status = SyncBatchStatus::Success;
                outcome.execution_plan = Some(result.plan);
                outcome.mutation_authorized = mutation_authorized;
                outcome.report = result.report;
                outcome.elapsed = Some(result.elapsed);
                outcome.error = None;
            }
            Err(error) => {
                cancelled |= matches!(error, Error::Cancelled);
                outcome.status = if mutation_authorized {
                    SyncBatchStatus::Partial
                } else {
                    SyncBatchStatus::Failed
                };
                outcome.execution_plan = fresh_plan;
                outcome.mutation_authorized = mutation_authorized;
                outcome.report = None;
                outcome.elapsed = None;
                outcome.error = Some(error.to_string());
                stopped = true;
            }
        }
    }
    cancelled |= cancellation.is_cancelled();

    write_sync_batch_output(
        &outcomes,
        &output,
        false,
        max_total_delete,
        Some(deletion_preflight.total_planned),
        Some(reserved_deletions),
        None,
    )?;
    if cancelled || cancellation.is_cancelled() {
        Err(Error::Cancelled)
    } else if outcomes.iter().any(|outcome| {
        matches!(
            outcome.status,
            SyncBatchStatus::Failed | SyncBatchStatus::Partial
        )
    }) {
        Err(Error::Message(
            "batch sync stopped after a target failed; completed targets were retained and later targets were not run"
                .to_owned(),
        ))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn reserve_batch_deletions(
    reserved: &mut usize,
    planned: usize,
    maximum: usize,
    profile: &str,
) -> Result<()> {
    let next = reserved.checked_add(planned).ok_or_else(|| {
        Error::Message(
            "aggregate batch deletion counts exceed this platform's numeric range".to_owned(),
        )
    })?;
    if next > maximum {
        return Err(Error::Message(format!(
            "fresh execution plans would delete {next} entries across the batch, exceeding --max-total-delete {maximum}; no mutations for profile {profile:?} were attempted"
        )));
    }
    *reserved = next;
    Ok(())
}

fn validate_sync_batch(jobs: &[NamedSyncSettings]) -> Result<ValidatedBatch> {
    if jobs
        .iter()
        .any(|job| job.settings.authentication.password_stdin)
    {
        return Err(Error::Configuration(
            "batch sync cannot use --password-stdin because every target is authenticated during preflight and execution; use the OS vault or per-profile password files"
                .to_owned(),
        ));
    }
    let batch_jobs = jobs
        .iter()
        .map(|job| {
            BatchJob::parse(
                job.name.clone(),
                &job.settings.connection.url,
                job.settings.connection.username.clone(),
                &job.settings.remote,
                job.settings.safety.delete,
                job.settings.safety.max_delete,
            )
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(batch_configuration_error)?;
    ValidatedBatch::new(batch_jobs).map_err(batch_configuration_error)
}

fn batch_configuration_error(error: impl std::fmt::Display) -> Error {
    Error::Configuration(format!("invalid batch configuration: {error}"))
}

fn sync_batch_status(outcomes: &[SyncBatchOutcome], batch_error: Option<&str>) -> &'static str {
    if batch_error.is_some() {
        return "failed";
    }
    let failed = outcomes
        .iter()
        .any(|outcome| outcome.status == SyncBatchStatus::Failed);
    let not_run = outcomes
        .iter()
        .any(|outcome| outcome.status == SyncBatchStatus::NotRun);
    if outcomes
        .iter()
        .any(|outcome| outcome.status == SyncBatchStatus::Partial)
    {
        return "partial";
    }
    if !failed && !not_run {
        "success"
    } else if outcomes
        .iter()
        .any(|outcome| outcome.status == SyncBatchStatus::Success)
    {
        "partial"
    } else {
        "failed"
    }
}

fn sync_batch_job_value(outcome: &SyncBatchOutcome) -> Value {
    json!({
        "schema": "sdsync.batch-job.v1",
        "profile": outcome.name,
        "status": outcome.status.as_str(),
        "preflight_plan": outcome.preflight_plan.as_ref().map(plan_value),
        "execution_plan": outcome.execution_plan.as_ref().map(plan_value),
        "mutation_authorized": outcome.mutation_authorized,
        "result": outcome.report.as_ref().map(|report| {
            execution_value(report, outcome.elapsed.unwrap_or_default())
        }),
        "elapsed_ms": outcome.elapsed.map(duration_millis),
        "error": outcome.error,
    })
}

fn sync_batch_summary_value(
    outcomes: &[SyncBatchOutcome],
    plan_only: bool,
    max_total_delete: usize,
    preflight_deletions: Option<usize>,
    execution_reserved_deletions: Option<usize>,
    batch_error: Option<&str>,
) -> Value {
    let succeeded = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::Success)
        .count();
    let preflighted = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::Preflighted)
        .count();
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::Failed)
        .count();
    let partial = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::Partial)
        .count();
    let not_run = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::NotRun)
        .count();
    json!({
        "schema": "sdsync.batch.v1",
        "kind": "summary",
        "mode": if plan_only { "plan" } else { "sync" },
        "status": sync_batch_status(outcomes, batch_error),
        "execution": "sequential",
        "all_targets_preflighted_before_mutation": outcomes
            .iter()
            .all(|outcome| outcome.preflight_plan.is_some()),
        "max_total_delete": max_total_delete,
        "preflight_deletions": preflight_deletions,
        "execution_reserved_deletions": execution_reserved_deletions,
        "summary": {
            "jobs": outcomes.len(),
            "succeeded": succeeded,
            "preflighted": preflighted,
            "partial": partial,
            "failed": failed,
            "not_run": not_run,
        },
        "error": batch_error,
    })
}

fn write_sync_batch_output(
    outcomes: &[SyncBatchOutcome],
    output: &config::ResolvedOutput,
    plan_only: bool,
    max_total_delete: usize,
    preflight_deletions: Option<usize>,
    execution_reserved_deletions: Option<usize>,
    batch_error: Option<&str>,
) -> Result<()> {
    let job_values = outcomes
        .iter()
        .map(sync_batch_job_value)
        .collect::<Vec<_>>();
    let succeeded = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::Success)
        .count();
    let preflighted = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::Preflighted)
        .count();
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::Failed)
        .count();
    let partial = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::Partial)
        .count();
    let not_run = outcomes
        .iter()
        .filter(|outcome| outcome.status == SyncBatchStatus::NotRun)
        .count();
    let status = sync_batch_status(outcomes, batch_error);
    let mut summary = sync_batch_summary_value(
        outcomes,
        plan_only,
        max_total_delete,
        preflight_deletions,
        execution_reserved_deletions,
        batch_error,
    );
    match output.output {
        cli::OutputFormat::Human => {
            println!(
                "Batch {}: status {status}; {succeeded} succeeded, {preflighted} preflighted, {partial} potentially partial, {failed} failed before mutation authorization, {not_run} not run; aggregate deletion cap {max_total_delete}.",
                if plan_only { "plan" } else { "sync" },
            );
            if let Some(error) = batch_error {
                println!("Batch safety check failed: {error}");
            }
            for outcome in outcomes {
                println!("\n[{}] {}", outcome.name, outcome.status.as_str());
                let display_plan = outcome
                    .execution_plan
                    .as_ref()
                    .or(outcome.preflight_plan.as_ref());
                if let Some(plan) = display_plan {
                    print_plan_human(plan, plan_only || output.verbosity > 0);
                }
                if let (Some(preflight), Some(execution)) =
                    (&outcome.preflight_plan, &outcome.execution_plan)
                    && preflight.delete_count() != execution.delete_count()
                {
                    println!(
                        "Deletion-plan drift: preflight {}, fresh execution {}.",
                        preflight.delete_count(),
                        execution.delete_count()
                    );
                }
                if let Some(report) = &outcome.report {
                    println!(
                        "Result: {} uploaded ({}), {} copied on NAS, {} directories created, {} deleted in {} ms.",
                        report.uploaded,
                        format_bytes(report.uploaded_bytes),
                        report.copied,
                        report.created,
                        report.deleted,
                        duration_millis(outcome.elapsed.unwrap_or_default()),
                    );
                }
                if let Some(error) = &outcome.error {
                    println!("Error: {error}");
                }
            }
            io::stdout().flush().map_err(output_error)
        }
        cli::OutputFormat::Json => {
            summary["jobs"] = Value::Array(job_values);
            write_json(&summary)
        }
        cli::OutputFormat::Ndjson => {
            for value in job_values {
                write_json_line(&value)?;
            }
            write_json_line(&summary)
        }
    }
}

fn run_sync(
    settings: config::ResolvedSync,
    plan_only: bool,
    changes_exit_code: bool,
) -> Result<ExitCode> {
    let cancellation = CancellationToken::default();
    install_cancellation_handler(cancellation.clone())?;
    let result = run_sync_job(&settings, plan_only, &cancellation, |_| Ok(()))?;
    write_sync_output(
        &result.plan,
        result.report.as_ref(),
        result.elapsed,
        &settings.output,
        plan_only,
    )?;

    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }

    if changes_exit_code && !result.plan.is_empty() {
        Ok(ExitCode::from(cli::PLAN_CHANGES_EXIT_CODE))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

struct TimedSyncResult {
    plan: SyncPlan,
    report: Option<ExecutionReport>,
    elapsed: Duration,
}

fn run_sync_job(
    settings: &config::ResolvedSync,
    plan_only: bool,
    cancellation: &CancellationToken,
    plan_guard: impl FnOnce(&SyncPlan) -> Result<()>,
) -> Result<TimedSyncResult> {
    warn_for_insecure_network(&settings.network, &settings.output);
    let logger = build_logger(&settings.output)?;
    let start_log = log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::RunStarted),
    );
    let started = Instant::now();

    let mut operation = match start_log {
        Ok(()) => prepare_and_run_sync(
            settings,
            plan_only,
            logger.clone(),
            cancellation,
            plan_guard,
        ),
        Err(error) => Err(error),
    };
    let final_log = match &operation {
        Ok((plan, _)) => {
            let metrics = EventMetrics {
                operations: operation_count(plan),
                files: (plan.uploads.len() + plan.copies.len()) as u64,
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
    Ok(TimedSyncResult {
        plan,
        report,
        elapsed: started.elapsed(),
    })
}

fn prepare_and_run_sync(
    settings: &config::ResolvedSync,
    plan_only: bool,
    logger: Option<Arc<EventLogger>>,
    cancellation: &CancellationToken,
    plan_guard: impl FnOnce(&SyncPlan) -> Result<()>,
) -> Result<(SyncPlan, Option<ExecutionReport>)> {
    cancellation.check()?;
    let root = RemoteRoot::parse(&settings.remote)?;
    let rules = IgnoreRules::build(&settings.source, &settings.behavior.excludes)?;

    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::LocalScanStarted),
    )?;
    let mut local = local::scan(&settings.source, &rules)?;
    cancellation.check()?;
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
    let server_copy = client.supports_server_copy();
    if settings.safety.delete {
        client.require_delete_api()?;
    }
    cancellation.check()?;
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
        cancellation.check()?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::AuthenticationCompleted),
        )?;
        client.verify_destination_writable(&root)?;
        cancellation.check()?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanStarted),
        )?;
        let mut remote = client.remote_inventory(&root)?;
        cancellation.check()?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanCompleted).metrics(
                EventMetrics {
                    operations: remote.entries.len() as u64,
                    ..EventMetrics::default()
                },
            ),
        )?;

        if compare_mode(settings.behavior.compare) == CompareMode::Content {
            client.require_content_api()?;
            local::populate_content_md5(&mut local, cancellation)?;
            let selected = plan::select_remote_content_hashes_for_plan(
                &local,
                &remote,
                &rules,
                server_copy,
                settings.safety.delete,
            );
            client.populate_remote_content_md5(&mut remote, &selected, cancellation)?;
        }

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
                server_copy,
            },
        )?;
        cancellation.check()?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::PlanReady).metrics(EventMetrics {
                operations: operation_count(&plan),
                files: (plan.uploads.len() + plan.copies.len()) as u64,
                bytes: plan.upload_bytes,
                ..EventMetrics::default()
            }),
        )?;
        plan_guard(&plan)?;
        cancellation.check()?;

        if plan_only {
            return Ok((plan, None));
        }

        if plan.is_empty() {
            let reconciliation = build_reconciliation_plan(
                &client,
                settings,
                &root,
                &rules,
                server_copy,
                cancellation,
            )?;
            ensure_reconciled(&reconciliation)?;
            cancellation.check()?;
            return Ok((plan, None));
        }

        cancellation.check()?;
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
            cancellation.clone(),
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
        cancellation.check()?;
        let reconciliation =
            build_reconciliation_plan(&client, settings, &root, &rules, server_copy, cancellation)?;
        ensure_reconciled(&reconciliation)?;
        cancellation.check()?;
        Ok((plan, Some(report)))
    })();

    finish_authenticated_operation(&mut client, operation)
}

fn build_reconciliation_plan(
    client: &ApiClient,
    settings: &config::ResolvedSync,
    root: &RemoteRoot,
    rules: &IgnoreRules,
    server_copy: bool,
    cancellation: &CancellationToken,
) -> Result<SyncPlan> {
    cancellation.check()?;
    let mut local = local::scan(&settings.source, rules)?;
    cancellation.check()?;
    let mut remote = client.remote_inventory(root)?;
    cancellation.check()?;
    if compare_mode(settings.behavior.compare) == CompareMode::Content {
        client.require_content_api()?;
        local::populate_content_md5(&mut local, cancellation)?;
        let selected = plan::select_remote_content_hashes_for_plan(
            &local,
            &remote,
            rules,
            server_copy,
            settings.safety.delete,
        );
        client.populate_remote_content_md5(&mut remote, &selected, cancellation)?;
    }
    let plan = plan::build_plan(
        root,
        &local,
        &remote,
        rules,
        &PlanOptions {
            delete: settings.safety.delete,
            allow_empty_source: settings.safety.allow_empty_source,
            max_delete: settings.safety.max_delete,
            compare: compare_mode(settings.behavior.compare),
            server_copy,
        },
    )?;
    cancellation.check()?;
    Ok(plan)
}

fn ensure_reconciled(plan: &SyncPlan) -> Result<()> {
    if plan.is_empty() {
        Ok(())
    } else {
        Err(Error::ReconciliationPending {
            operations: usize::try_from(operation_count(plan)).unwrap_or(usize::MAX),
        })
    }
}

fn run_doctor(settings: config::ResolvedDoctor) -> Result<ExitCode> {
    let cancellation = CancellationToken::default();
    install_cancellation_handler(cancellation.clone())?;
    let timed = run_doctor_job(&settings, &cancellation, true)?;
    write_doctor_output(&timed.result, timed.elapsed, &settings.output)?;
    if cancellation.is_cancelled() || timed.result.write_probe_cancelled {
        Err(Error::Cancelled)
    } else if let Some(error) = timed.result.write_probe_error {
        Err(Error::Message(error))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

struct TimedDoctorResult {
    result: DoctorResult,
    elapsed: Duration,
}

fn run_doctor_job(
    settings: &config::ResolvedDoctor,
    cancellation: &CancellationToken,
    perform_write_probe: bool,
) -> Result<TimedDoctorResult> {
    warn_for_insecure_network(&settings.network, &settings.output);
    let logger = build_logger(&settings.output)?;
    let start_log = log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::RunStarted),
    );
    let started = Instant::now();

    let mut operation = match start_log {
        Ok(()) => doctor_checks(settings, logger.clone(), cancellation, perform_write_probe),
        Err(error) => Err(error),
    };
    let final_log = match &operation {
        Ok(result) if result.write_probe_error.is_none() => log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RunCompleted).metrics(EventMetrics {
                elapsed_ms: duration_millis(started.elapsed()),
                ..EventMetrics::default()
            }),
        ),
        Ok(_) | Err(_) => log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Error, EventCode::RunFailed),
        ),
    };
    if let Err(error) = final_log {
        match &mut operation {
            Ok(result) if result.write_probe_performed => {
                append_doctor_failure_context(
                    result,
                    "writing the final diagnostic log also failed",
                    &error.to_string(),
                );
            }
            Ok(_) => operation = Err(error),
            Err(_) => {}
        }
    }
    let result = finish_doctor_logger(logger.as_ref(), operation, settings.output.quiet)?;
    Ok(TimedDoctorResult {
        result,
        elapsed: started.elapsed(),
    })
}

#[derive(Clone, Debug)]
struct DoctorResult {
    authenticated: bool,
    remote_checked: bool,
    remote_exists: Option<bool>,
    remote_entries: Option<usize>,
    write_permission_scope: Option<&'static str>,
    write_permission_path: Option<String>,
    write_probe_requested: bool,
    write_probe_performed: bool,
    write_probe: Option<WriteProbeReport>,
    write_probe_error: Option<String>,
    write_probe_cancelled: bool,
}

fn doctor_checks(
    settings: &config::ResolvedDoctor,
    logger: Option<Arc<EventLogger>>,
    cancellation: &CancellationToken,
    perform_write_probe: bool,
) -> Result<DoctorResult> {
    cancellation.check()?;
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::ApiDiscoveryStarted),
    )?;
    let mut client = connect_client(&settings.url, &settings.network)?;
    if settings.compare == cli::CompareArg::Content {
        client.require_content_api()?;
    }
    if settings.delete {
        client.require_delete_api()?;
    }
    if settings.write_test {
        client.require_content_api()?;
        client.require_delete_api()?;
    }
    cancellation.check()?;
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
            write_permission_scope: None,
            write_permission_path: None,
            write_probe_requested: false,
            write_probe_performed: false,
            write_probe: None,
            write_probe_error: None,
            write_probe_cancelled: false,
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
        cancellation.check()?;
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
                write_permission_scope: None,
                write_permission_path: None,
                write_probe_requested: false,
                write_probe_performed: false,
                write_probe: None,
                write_probe_error: None,
                write_probe_cancelled: false,
            });
        };
        let root = RemoteRoot::parse(remote)?;
        cancellation.check()?;
        let write_check = client.verify_destination_writable(&root)?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanStarted),
        )?;
        let inventory = client.remote_inventory(&root)?;
        cancellation.check()?;
        log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanCompleted).metrics(
                EventMetrics {
                    operations: inventory.entries.len() as u64,
                    ..EventMetrics::default()
                },
            ),
        )?;
        if settings.write_test && !inventory.root_exists {
            return Err(Error::Message(format!(
                "disposable write probe requires the configured target {:?} to already exist",
                root.as_str()
            )));
        }
        let mut result = DoctorResult {
            authenticated: true,
            remote_checked: true,
            remote_exists: Some(inventory.root_exists),
            remote_entries: Some(inventory.entries.len()),
            write_permission_scope: Some(if write_check.destination_exists {
                "exact_destination"
            } else {
                "nearest_existing_ancestor"
            }),
            write_permission_path: Some(write_check.checked_directory),
            write_probe_requested: settings.write_test,
            write_probe_performed: false,
            write_probe: None,
            write_probe_error: None,
            write_probe_cancelled: false,
        };
        if settings.write_test && perform_write_probe {
            result.write_probe_performed = true;
            match client.run_write_probe(&root, cancellation) {
                Ok(report) => result.write_probe = Some(report),
                Err(failure) => {
                    result.write_probe_cancelled = matches!(&failure.cause, Error::Cancelled);
                    result.write_probe_error = Some(failure.to_string());
                    result.write_probe = Some(failure.report);
                }
            }
        }
        Ok(result)
    })();
    finish_doctor_authenticated_operation(&mut client, operation)
}

fn append_doctor_failure_context(result: &mut DoctorResult, context: &str, error: &str) {
    let detail = format!("{context}: {error}");
    match &mut result.write_probe_error {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&detail);
        }
        None => result.write_probe_error = Some(detail),
    }
}

fn finish_doctor_authenticated_operation(
    client: &mut ApiClient,
    operation: Result<DoctorResult>,
) -> Result<DoctorResult> {
    let logout = client.logout();
    match (operation, logout) {
        (Err(error), Err(logout_error)) => {
            eprintln!("warning: File Station logout also failed: {logout_error}");
            Err(error)
        }
        (Err(error), _) => Err(error),
        (Ok(mut result), Err(error)) if result.write_probe_performed => {
            append_doctor_failure_context(
                &mut result,
                "File Station logout also failed",
                &error.to_string(),
            );
            Ok(result)
        }
        (Ok(_), Err(error)) => Err(error),
        (Ok(result), Ok(())) => Ok(result),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DoctorBatchStatus {
    Preflighted,
    Success,
    Partial,
    Failed,
    NotRun,
}

impl DoctorBatchStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preflighted => "preflighted",
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::NotRun => "not-run",
        }
    }
}

struct DoctorBatchOutcome {
    name: String,
    status: DoctorBatchStatus,
    result: Option<TimedDoctorResult>,
    error: Option<String>,
}

fn run_doctor_batch(mut jobs: Vec<NamedDoctorSettings>) -> Result<ExitCode> {
    jobs.sort_by(|left, right| left.name.cmp(&right.name));
    let output = common_batch_output(jobs.iter().map(|job| &job.settings.output))?;
    validate_doctor_batch(&jobs)?;
    let cancellation = CancellationToken::default();
    install_cancellation_handler(cancellation.clone())?;
    let write_tests = jobs.iter().any(|job| job.settings.write_test);
    let mut cancelled = false;

    let mut outcomes = if write_tests {
        // A write-test batch first runs every selected target with mutation disabled. A failure in
        // this phase prevents every disposable probe, including probes for healthy targets.
        let mut outcomes = Vec::with_capacity(jobs.len());
        for job in &jobs {
            if cancellation.is_cancelled() {
                cancelled = true;
                outcomes.push(DoctorBatchOutcome {
                    name: job.name.clone(),
                    status: DoctorBatchStatus::NotRun,
                    result: None,
                    error: None,
                });
                continue;
            }
            match run_doctor_job(&job.settings, &cancellation, false) {
                Ok(result) => outcomes.push(DoctorBatchOutcome {
                    name: job.name.clone(),
                    status: DoctorBatchStatus::Preflighted,
                    result: Some(result),
                    error: None,
                }),
                Err(error) => {
                    cancelled |= matches!(error, Error::Cancelled);
                    outcomes.push(DoctorBatchOutcome {
                        name: job.name.clone(),
                        status: DoctorBatchStatus::Failed,
                        result: None,
                        error: Some(error.to_string()),
                    });
                }
            }
        }
        outcomes
    } else {
        Vec::with_capacity(jobs.len())
    };
    cancelled |= cancellation.is_cancelled();

    if write_tests
        && (cancelled
            || outcomes
                .iter()
                .any(|outcome| outcome.status == DoctorBatchStatus::Failed))
    {
        write_doctor_batch_output(&outcomes, &output, true)?;
        return if cancelled || cancellation.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Err(Error::Message(
                "one or more target diagnostic preflights failed; no disposable write probes were attempted"
                    .to_owned(),
            ))
        };
    }

    if write_tests {
        let mut stopped = false;
        for (job, outcome) in jobs.iter().zip(&mut outcomes) {
            if !job.settings.write_test {
                outcome.status = DoctorBatchStatus::Success;
                continue;
            }
            if stopped || cancellation.is_cancelled() {
                cancelled |= cancellation.is_cancelled();
                outcome.status = DoctorBatchStatus::NotRun;
                continue;
            }
            match run_doctor_job(&job.settings, &cancellation, true) {
                Ok(result) => {
                    let probe_error = result.result.write_probe_error.clone();
                    let probe_cancelled = result.result.write_probe_cancelled;
                    let may_have_mutated = doctor_result_may_have_mutated(&result.result);
                    outcome.result = Some(result);
                    if let Some(error) = probe_error {
                        outcome.status = if may_have_mutated {
                            DoctorBatchStatus::Partial
                        } else {
                            DoctorBatchStatus::Failed
                        };
                        outcome.error = Some(error);
                        cancelled |= probe_cancelled;
                        stopped = true;
                    } else {
                        outcome.status = DoctorBatchStatus::Success;
                        outcome.error = None;
                    }
                }
                Err(error) => {
                    cancelled |= matches!(error, Error::Cancelled);
                    outcome.status = DoctorBatchStatus::Failed;
                    outcome.error = Some(error.to_string());
                    stopped = true;
                }
            }
        }
    } else {
        // Non-mutating diagnostics remain useful independently, so collect every target failure.
        for job in &jobs {
            if cancellation.is_cancelled() {
                cancelled = true;
                outcomes.push(DoctorBatchOutcome {
                    name: job.name.clone(),
                    status: DoctorBatchStatus::NotRun,
                    result: None,
                    error: None,
                });
                continue;
            }
            match run_doctor_job(&job.settings, &cancellation, true) {
                Ok(result) => outcomes.push(DoctorBatchOutcome {
                    name: job.name.clone(),
                    status: DoctorBatchStatus::Success,
                    result: Some(result),
                    error: None,
                }),
                Err(error) => {
                    cancelled |= matches!(error, Error::Cancelled);
                    outcomes.push(DoctorBatchOutcome {
                        name: job.name.clone(),
                        status: DoctorBatchStatus::Failed,
                        result: None,
                        error: Some(error.to_string()),
                    });
                }
            }
        }
    }
    cancelled |= cancellation.is_cancelled();

    write_doctor_batch_output(&outcomes, &output, write_tests)?;
    if cancelled || cancellation.is_cancelled() {
        Err(Error::Cancelled)
    } else if outcomes.iter().any(|outcome| {
        matches!(
            outcome.status,
            DoctorBatchStatus::Failed | DoctorBatchStatus::Partial
        )
    }) {
        Err(Error::Message(
            "one or more target diagnostic jobs failed; inspect the per-job results".to_owned(),
        ))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn doctor_result_may_have_mutated(result: &DoctorResult) -> bool {
    result.write_probe_performed
        && result.write_probe.as_ref().is_some_and(|report| {
            report.target_verified
                || report.directory_created
                || report.upload_attempted
                || report.server_copy_attempted
                || report.leftover_remote_probe_path.is_some()
        })
}

fn validate_doctor_batch(jobs: &[NamedDoctorSettings]) -> Result<()> {
    if jobs
        .iter()
        .any(|job| job.settings.authentication.password_stdin)
    {
        return Err(Error::Configuration(
            "batch target diagnostics cannot use --password-stdin; use the OS vault or per-profile password files"
                .to_owned(),
        ));
    }
    let batch_jobs = jobs
        .iter()
        .filter_map(|job| {
            let remote = job.settings.remote.as_deref()?;
            let username = job.settings.username.as_deref()?;
            Some(BatchJob::parse(
                job.name.clone(),
                &job.settings.url,
                username.to_owned(),
                remote,
                false,
                0,
            ))
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(batch_configuration_error)?;
    if !batch_jobs.is_empty() {
        ValidatedBatch::new(batch_jobs).map_err(batch_configuration_error)?;
    }
    Ok(())
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

fn finish_doctor_logger(
    logger: Option<&Arc<EventLogger>>,
    operation: Result<DoctorResult>,
    quiet: bool,
) -> Result<DoctorResult> {
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
        (Ok(mut result), Err(error)) if result.write_probe_performed => {
            append_doctor_failure_context(
                &mut result,
                "observability shutdown also failed",
                &error.to_string(),
            );
            Ok(result)
        }
        (Ok(_), Err(error)) => Err(error),
        (Ok(result), Ok(Some(report))) => {
            if !quiet && (report.remote_events_dropped > 0 || report.remote_delivery_failures > 0) {
                eprintln!(
                    "warning: remote logging dropped {} events and recorded {} delivery failures",
                    report.remote_events_dropped, report.remote_delivery_failures
                );
            }
            Ok(result)
        }
        (Ok(result), Ok(None)) => Ok(result),
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

fn select_job_profiles<'a>(
    loaded: Option<&'a config::LoadedConfig>,
    requested_single: Option<&str>,
    batch: &cli::BatchArgs,
) -> Result<Vec<NamedProfile<'a>>> {
    if !batch.requested() {
        let selected = select_optional_profile(loaded, requested_single)?;
        return Ok(vec![match selected {
            Some(selected) => NamedProfile {
                name: selected.name,
                values: selected.values,
            },
            None => NamedProfile {
                name: "command-line".to_owned(),
                values: None,
            },
        }]);
    }
    if requested_single.is_some() {
        return Err(Error::Configuration(
            "--profile cannot be combined with --profiles or --all-profiles".to_owned(),
        ));
    }
    let loaded = loaded.ok_or_else(|| {
        Error::Configuration(
            "--profiles and --all-profiles require an existing configuration file".to_owned(),
        )
    })?;
    if loaded.values.profiles.is_empty() {
        return Err(Error::Configuration(
            "the configuration contains no named profiles to select".to_owned(),
        ));
    }

    let names = if batch.all_profiles {
        loaded.values.profiles.keys().cloned().collect::<Vec<_>>()
    } else {
        let mut unique = BTreeSet::new();
        for name in &batch.profiles {
            if name.is_empty() || name.trim() != name {
                return Err(Error::Configuration(format!(
                    "invalid batch profile name {name:?}"
                )));
            }
            if !unique.insert(name.clone()) {
                return Err(Error::Configuration(format!(
                    "batch profile {name:?} was selected more than once"
                )));
            }
        }
        unique.into_iter().collect::<Vec<_>>()
    };

    let mut selected = Vec::with_capacity(names.len());
    for name in names {
        if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
            return Err(Error::Configuration(format!(
                "invalid batch profile name {name:?}: names must be non-empty, have no surrounding whitespace, and contain no control characters"
            )));
        }
        let values = loaded.values.profiles.get(&name).ok_or_else(|| {
            Error::Configuration(format!("configuration profile {name:?} does not exist"))
        })?;
        config::validate_profile(values).map_err(config_error)?;
        selected.push(NamedProfile {
            name,
            values: Some(values),
        });
    }
    Ok(selected)
}

fn validate_batch_pair_overrides(
    _selected: &[NamedProfile<'_>],
    batch: &cli::BatchArgs,
    source_override: bool,
    remote_override: bool,
) -> Result<()> {
    if !batch.requested() {
        if batch.max_total_delete.is_some() {
            return Err(Error::Configuration(
                "--max-total-delete requires --profiles or --all-profiles".to_owned(),
            ));
        }
        return Ok(());
    }
    if source_override || remote_override {
        return Err(Error::Configuration(
            "batch jobs must take SOURCE and REMOTE from each selected profile; positional overrides are not allowed"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_batch_source_override(
    _selected: &[NamedProfile<'_>],
    batch: &cli::BatchArgs,
    source_override: bool,
) -> Result<()> {
    if batch.max_total_delete.is_some() {
        return Err(Error::Configuration(
            "--max-total-delete applies only to batch plan and sync".to_owned(),
        ));
    }
    if batch.requested() && source_override {
        return Err(Error::Configuration(
            "doctor source batch jobs must take SOURCE from each selected profile".to_owned(),
        ));
    }
    Ok(())
}

fn validate_batch_doctor_target_override(arguments: &cli::DoctorArgs) -> Result<()> {
    if arguments.batch.max_total_delete.is_some() {
        return Err(Error::Configuration(
            "--max-total-delete applies only to batch plan and sync".to_owned(),
        ));
    }
    if !arguments.batch.requested() {
        return Ok(());
    }
    let action_remote = match arguments.action.as_ref() {
        Some(cli::DoctorAction::Target(target)) => target.remote.is_some(),
        _ => false,
    };
    if arguments.remote.is_some() || action_remote {
        return Err(Error::Configuration(
            "doctor target batch jobs must take REMOTE from each selected profile".to_owned(),
        ));
    }
    Ok(())
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
                    "Sync complete: {} uploaded ({}), {} copied on NAS, {} directories created, {} remote entries deleted in {} ms.",
                    report.uploaded,
                    format_bytes(report.uploaded_bytes),
                    report.copied,
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
                    "result": execution_value(report, elapsed),
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
        "Plan: {} uploads ({}), {} server copies (verified upload fallback up to {}), {} directories, {} deletions, {} unchanged files, {} protected remote entries.",
        plan.uploads.len(),
        format_bytes(plan.upload_bytes),
        plan.copies.len(),
        format_bytes(copy_fallback_bytes(plan)),
        plan.creates.len(),
        plan.delete_count(),
        plan.unchanged_files,
        plan.protected_entries
    );
    if !detailed {
        return;
    }
    for action in &plan.pre_deletes {
        println!(
            "  DELETE-CONFLICT {} (remote snapshot guarded)",
            action.remote_path
        );
    }
    for action in &plan.creates {
        println!("  MKDIR  {}", action.remote_path);
    }
    for action in &plan.copies {
        println!(
            "  COPY   {} -> {} ({}; verified upload fallback allowed before task start)",
            action.from_remote_path,
            action.to_remote_path,
            format_bytes(action.expected_size)
        );
    }
    for action in &plan.uploads {
        println!(
            "  UPLOAD {} -> {}",
            action.local.relative, action.remote_path
        );
    }
    for action in &plan.post_deletes {
        if let Some(guard) = &action.destination_guard {
            println!(
                "  DELETE {} (remote snapshot guarded; destination guarded by {} bytes+mtime+MD5 at {})",
                action.remote_path, guard.expected_size, guard.remote_path
            );
        } else {
            println!("  DELETE {} (remote snapshot guarded)", action.remote_path);
        }
    }
}

fn remote_snapshot_value(snapshot: &RemoteSnapshot) -> Value {
    json!({
        "entry_kind": snapshot.kind.as_str(),
        "size": snapshot.size,
        "mtime_seconds": snapshot.mtime_seconds,
        "content_md5": snapshot.content_md5.map(|digest| digest.to_string()),
        "require_mtime": snapshot.require_mtime,
    })
}

fn plan_value(plan: &SyncPlan) -> Value {
    json!({
        "summary": {
            "uploads": plan.uploads.len(),
            "upload_bytes": plan.upload_bytes,
            "server_copy_fallback_bytes": copy_fallback_bytes(plan),
            "server_copies": plan.copies.len(),
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
                "snapshot_guard": remote_snapshot_value(&action.snapshot),
            })).collect::<Vec<_>>(),
            "creates": plan.creates.iter().map(|action| json!({
                "relative": action.relative,
                "remote_path": action.remote_path,
            })).collect::<Vec<_>>(),
            "copies": plan.copies.iter().map(|action| json!({
                "from_relative": action.from_relative,
                "from_remote_path": action.from_remote_path,
                "to_relative": action.to_relative,
                "to_remote_path": action.to_remote_path,
                "expected_size": action.expected_size,
                "expected_mtime_seconds": action.local.mtime_ms.div_euclid(1000),
                "content_md5": action.content_md5.to_string(),
                "source_snapshot_guard": remote_snapshot_value(&action.source_snapshot),
                "verified_upload_fallback": "only-before-copy-task-start",
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
                "snapshot_guard": remote_snapshot_value(&action.snapshot),
                "destination_guard": action.destination_guard.as_ref().map(|guard| json!({
                    "remote_path": guard.remote_path,
                    "local_relative": guard.local.relative,
                    "expected_size": guard.expected_size,
                    "expected_mtime_seconds": guard.expected_mtime_seconds,
                    "content_md5": guard.content_md5.to_string(),
                })),
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
        value["result"] = execution_value(report, elapsed);
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
            "snapshot_guard": remote_snapshot_value(&action.snapshot),
        }))?;
    }
    for action in &plan.creates {
        write_json_line(&json!({
            "schema": "sdsync.plan-action.v1", "action": "create-directory",
            "relative": action.relative, "remote_path": action.remote_path,
        }))?;
    }
    for action in &plan.copies {
        write_json_line(&json!({
            "schema": "sdsync.plan-action.v1", "action": "copy-remote-content",
            "from_relative": action.from_relative,
            "from_remote_path": action.from_remote_path,
            "to_relative": action.to_relative,
            "to_remote_path": action.to_remote_path,
            "expected_size": action.expected_size,
            "expected_mtime_seconds": action.local.mtime_ms.div_euclid(1000),
            "content_md5": action.content_md5.to_string(),
            "source_snapshot_guard": remote_snapshot_value(&action.source_snapshot),
            "verified_upload_fallback": "only-before-copy-task-start",
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
            "snapshot_guard": remote_snapshot_value(&action.snapshot),
            "destination_guard": action.destination_guard.as_ref().map(|guard| json!({
                "remote_path": guard.remote_path,
                "local_relative": guard.local.relative,
                "expected_size": guard.expected_size,
                "expected_mtime_seconds": guard.expected_mtime_seconds,
                "content_md5": guard.content_md5.to_string(),
            })),
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
        "server_copy_fallback_bytes": copy_fallback_bytes(plan),
        "server_copies": plan.copies.len(),
        "directories": plan.creates.len(),
        "deletions": plan.delete_count(),
        "unchanged_files": plan.unchanged_files,
        "protected_entries": plan.protected_entries,
        "changes": !plan.is_empty(),
    })
}

fn execution_value(report: &ExecutionReport, elapsed: Duration) -> Value {
    json!({
        "changed": report.uploaded > 0 || report.copied > 0 || report.created > 0 || report.deleted > 0,
        "uploaded": report.uploaded,
        "server_copied": report.copied,
        "upload_bytes": report.uploaded_bytes,
        "directories_created": report.created,
        "deleted": report.deleted,
        "elapsed_ms": duration_millis(elapsed),
    })
}

fn write_probe_value(report: &WriteProbeReport) -> Value {
    json!({
        "target_path": report.target_path,
        "probe_path": report.probe_path,
        "target_verified": report.target_verified,
        "directory_created": report.directory_created,
        "upload_attempted": report.upload_attempted,
        "upload_verified": report.upload_verified,
        "uploaded_size": report.uploaded_size,
        "uploaded_md5": report.uploaded_md5.to_string(),
        "uploaded_mtime_seconds": report.uploaded_mtime_seconds,
        "server_copy_supported": report.server_copy_supported,
        "server_copy_attempted": report.server_copy_attempted,
        "server_copy_verified": report.server_copy_verified,
        "cleanup_completed": report.cleanup_completed,
        "leftover_remote_probe_path": report.leftover_remote_probe_path,
    })
}

fn doctor_value(result: &DoctorResult, elapsed: Duration) -> Value {
    json!({
        "schema": "sdsync.doctor.v1",
        "routing": true,
        "api_discovery": true,
        "authenticated": result.authenticated,
        "remote_checked": result.remote_checked,
        "remote_exists": result.remote_exists,
        "remote_entries": result.remote_entries,
        "write_permission_scope": result.write_permission_scope,
        "write_permission_path": result.write_permission_path,
        "write_test": {
            "requested": result.write_probe_requested,
            "status": if !result.write_probe_requested {
                "not-requested"
            } else if !result.write_probe_performed {
                "preflighted"
            } else if result.write_probe_error.is_some() {
                "failed"
            } else {
                "success"
            },
            "report": result.write_probe.as_ref().map(write_probe_value),
            "error": result.write_probe_error,
        },
        "elapsed_ms": duration_millis(elapsed),
    })
}

fn print_doctor_human(result: &DoctorResult) {
    if result.authenticated {
        if result.remote_checked {
            println!(
                "Doctor: routing, API discovery, authentication, and remote access are healthy ({} entries; destination {}; write permission checked at {} {}).",
                result.remote_entries.unwrap_or(0),
                if result.remote_exists == Some(true) {
                    "exists"
                } else {
                    "will be created"
                },
                if result.write_permission_scope == Some("exact_destination") {
                    "the exact destination"
                } else {
                    "the nearest existing ancestor"
                },
                result
                    .write_permission_path
                    .as_deref()
                    .unwrap_or("<unknown>"),
            );
        } else {
            println!("Doctor: routing, API discovery, and authentication are healthy.");
        }
    } else {
        println!("Doctor: reverse-proxy routing and File Station API discovery are healthy.");
    }
    if result.write_probe_requested {
        if !result.write_probe_performed {
            println!(
                "Disposable write probe prerequisites passed; no remote probe mutation was attempted."
            );
        } else if let Some(error) = &result.write_probe_error {
            println!("Disposable write probe failed: {error}");
        } else if let Some(report) = &result.write_probe {
            println!(
                "Disposable write probe passed: directory creation, {}-byte upload with size/MD5/mtime verification{}; cleanup completed{}.",
                report.uploaded_size,
                if report.server_copy_supported {
                    ", and server-side copy verification"
                } else {
                    ""
                },
                report
                    .leftover_remote_probe_path
                    .as_ref()
                    .map(|path| format!(" (unexpected leftover: {path})"))
                    .unwrap_or_default(),
            );
        }
    }
}

fn write_doctor_output(
    result: &DoctorResult,
    elapsed: Duration,
    output: &config::ResolvedOutput,
) -> Result<()> {
    let value = doctor_value(result, elapsed);
    match output.output {
        cli::OutputFormat::Human => {
            print_doctor_human(result);
            io::stdout().flush().map_err(output_error)
        }
        cli::OutputFormat::Json => write_json(&value),
        cli::OutputFormat::Ndjson => write_json_line(&value),
    }
}

fn doctor_batch_job_value(outcome: &DoctorBatchOutcome) -> Value {
    json!({
        "schema": "sdsync.doctor-job.v1",
        "profile": outcome.name,
        "status": outcome.status.as_str(),
        "doctor": outcome
            .result
            .as_ref()
            .map(|result| doctor_value(&result.result, result.elapsed)),
        "error": outcome.error,
    })
}

fn doctor_batch_status(outcomes: &[DoctorBatchOutcome]) -> &'static str {
    let failed = outcomes
        .iter()
        .any(|outcome| outcome.status == DoctorBatchStatus::Failed);
    let not_run = outcomes
        .iter()
        .any(|outcome| outcome.status == DoctorBatchStatus::NotRun);
    if outcomes
        .iter()
        .any(|outcome| outcome.status == DoctorBatchStatus::Partial)
    {
        return "partial";
    }
    if !failed && !not_run {
        "success"
    } else if outcomes
        .iter()
        .any(|outcome| outcome.status == DoctorBatchStatus::Success)
    {
        "partial"
    } else {
        "failed"
    }
}

fn doctor_batch_summary_value(outcomes: &[DoctorBatchOutcome], write_tests: bool) -> Value {
    let succeeded = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::Success)
        .count();
    let preflighted = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::Preflighted)
        .count();
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::Failed)
        .count();
    let partial = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::Partial)
        .count();
    let not_run = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::NotRun)
        .count();
    json!({
        "schema": "sdsync.doctor-batch.v1",
        "kind": "summary",
        "status": doctor_batch_status(outcomes),
        "execution": "sequential",
        "write_tests_requested": write_tests,
        "all_targets_preflighted_before_mutation": write_tests
            && outcomes.iter().all(|outcome| outcome.result.is_some()),
        "summary": {
            "jobs": outcomes.len(),
            "succeeded": succeeded,
            "preflighted": preflighted,
            "partial": partial,
            "failed": failed,
            "not_run": not_run,
        },
    })
}

fn write_doctor_batch_output(
    outcomes: &[DoctorBatchOutcome],
    output: &config::ResolvedOutput,
    write_tests: bool,
) -> Result<()> {
    let succeeded = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::Success)
        .count();
    let preflighted = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::Preflighted)
        .count();
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::Failed)
        .count();
    let partial = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::Partial)
        .count();
    let not_run = outcomes
        .iter()
        .filter(|outcome| outcome.status == DoctorBatchStatus::NotRun)
        .count();
    let status = doctor_batch_status(outcomes);
    let job_values = outcomes
        .iter()
        .map(doctor_batch_job_value)
        .collect::<Vec<_>>();
    let mut summary = doctor_batch_summary_value(outcomes, write_tests);
    match output.output {
        cli::OutputFormat::Human => {
            println!(
                "Target diagnostic batch: status {status}; {succeeded} succeeded, {preflighted} preflighted, {partial} potentially partial, {failed} failed before a probe could mutate, {not_run} not run."
            );
            for outcome in outcomes {
                println!("\n[{}] {}", outcome.name, outcome.status.as_str());
                if let Some(result) = &outcome.result {
                    print_doctor_human(&result.result);
                }
                if let Some(error) = &outcome.error {
                    println!("Error: {error}");
                }
            }
            io::stdout().flush().map_err(output_error)
        }
        cli::OutputFormat::Json => {
            summary["jobs"] = Value::Array(job_values);
            write_json(&summary)
        }
        cli::OutputFormat::Ndjson => {
            for value in job_values {
                write_json_line(&value)?;
            }
            write_json_line(&summary)
        }
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
        cli::CompareArg::Content => CompareMode::Content,
        cli::CompareArg::Metadata => CompareMode::Metadata,
        cli::CompareArg::SizeOnly => CompareMode::SizeOnly,
    }
}

fn operation_count(plan: &SyncPlan) -> u64 {
    (plan.pre_deletes.len()
        + plan.creates.len()
        + plan.copies.len()
        + plan.uploads.len()
        + plan.post_deletes.len()) as u64
}

fn copy_fallback_bytes(plan: &SyncPlan) -> u64 {
    plan.copies.iter().fold(0_u64, |total, action| {
        total.saturating_add(action.expected_size)
    })
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

    fn json_object_keys(value: &Value) -> BTreeSet<&str> {
        value
            .as_object()
            .expect("structured output value must be an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    fn empty_plan() -> SyncPlan {
        SyncPlan {
            pre_deletes: Vec::new(),
            creates: Vec::new(),
            copies: Vec::new(),
            uploads: Vec::new(),
            post_deletes: Vec::new(),
            unchanged_files: 2,
            protected_entries: 1,
            upload_bytes: 0,
        }
    }

    fn plan_with_deletions(count: usize) -> SyncPlan {
        let mut plan = empty_plan();
        plan.post_deletes = (0..count)
            .map(|index| plan::DeleteAction {
                relative: format!("stale-{index}"),
                remote_path: format!("/share/root/stale-{index}"),
                kind: local::EntryKind::File,
                type_conflict: false,
                snapshot: RemoteSnapshot {
                    kind: local::EntryKind::File,
                    size: 1,
                    mtime_seconds: 1_700_000_000,
                    content_md5: None,
                    require_mtime: true,
                },
                destination_guard: None,
            })
            .collect();
        plan
    }

    fn sync_outcome(
        name: &str,
        status: SyncBatchStatus,
        preflight_plan: Option<SyncPlan>,
        execution_plan: Option<SyncPlan>,
        mutation_authorized: bool,
    ) -> SyncBatchOutcome {
        SyncBatchOutcome {
            name: name.to_owned(),
            status,
            preflight_plan,
            execution_plan,
            mutation_authorized,
            report: None,
            elapsed: None,
            error: None,
        }
    }

    fn write_probe_report() -> WriteProbeReport {
        WriteProbeReport {
            target_path: "/share/acceptance".to_owned(),
            probe_path: "/share/acceptance/.sdsync-write-probe-test".to_owned(),
            target_verified: false,
            directory_created: false,
            upload_attempted: false,
            upload_verified: false,
            uploaded_size: 23,
            uploaded_md5: synology_drive_sync::integrity::ContentMd5::from_bytes([0x2a; 16]),
            uploaded_mtime_seconds: 1_700_000_000,
            server_copy_supported: true,
            server_copy_attempted: false,
            server_copy_verified: false,
            cleanup_completed: false,
            leftover_remote_probe_path: None,
        }
    }

    fn doctor_result(
        write_probe_performed: bool,
        write_probe: Option<WriteProbeReport>,
        write_probe_error: Option<&str>,
    ) -> DoctorResult {
        DoctorResult {
            authenticated: true,
            remote_checked: true,
            remote_exists: Some(true),
            remote_entries: Some(4),
            write_permission_scope: Some("exact_destination"),
            write_permission_path: Some("/share/acceptance".to_owned()),
            write_probe_requested: true,
            write_probe_performed,
            write_probe,
            write_probe_error: write_probe_error.map(str::to_owned),
            write_probe_cancelled: false,
        }
    }

    fn doctor_outcome(
        name: &str,
        status: DoctorBatchStatus,
        result: Option<DoctorResult>,
    ) -> DoctorBatchOutcome {
        DoctorBatchOutcome {
            name: name.to_owned(),
            status,
            result: result.map(|result| TimedDoctorResult {
                result,
                elapsed: Duration::from_millis(7),
            }),
            error: None,
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
        assert_eq!(
            json_object_keys(&planned["plan"]["summary"]),
            BTreeSet::from([
                "changes",
                "deletions",
                "directories",
                "protected_entries",
                "server_copies",
                "server_copy_fallback_bytes",
                "unchanged_files",
                "upload_bytes",
                "uploads",
            ])
        );
        assert_eq!(
            json_object_keys(&planned["plan"]["actions"]),
            BTreeSet::from([
                "copies",
                "creates",
                "post_deletes",
                "pre_deletes",
                "uploads",
            ])
        );
        assert!(planned.get("result").is_none());

        let report = ExecutionReport::default();
        let synced = command_json_value(&plan, Some(&report), Duration::from_millis(12), false);
        assert_eq!(synced["schema"], "sdsync.sync.v1");
        assert_eq!(synced["result"]["changed"], false);
        assert_eq!(synced["result"]["elapsed_ms"], 12);
        assert_eq!(
            json_object_keys(&synced["result"]),
            BTreeSet::from([
                "changed",
                "deleted",
                "directories_created",
                "elapsed_ms",
                "server_copied",
                "upload_bytes",
                "uploaded",
            ])
        );

        let summary = plan_summary_record(&plan);
        assert_eq!(summary["schema"], "sdsync.plan.v1");
        assert_eq!(summary["kind"], "summary");
        assert_eq!(
            json_object_keys(&summary),
            BTreeSet::from([
                "changes",
                "deletions",
                "directories",
                "kind",
                "protected_entries",
                "schema",
                "server_copies",
                "server_copy_fallback_bytes",
                "unchanged_files",
                "upload_bytes",
                "uploads",
            ])
        );

        let mut diagnostic = doctor_result(false, None, None);
        diagnostic.write_probe_requested = false;
        let diagnosed = doctor_value(&diagnostic, Duration::from_millis(9));
        assert_eq!(diagnosed["schema"], "sdsync.doctor.v1");
        assert_eq!(diagnosed["write_test"]["status"], "not-requested");
        assert_eq!(diagnosed["elapsed_ms"], 9);
    }

    #[test]
    fn deletion_reservation_is_atomic_on_cap_failure_and_overflow() {
        let mut reserved = 3;
        reserve_batch_deletions(&mut reserved, 2, 5, "alpha").unwrap();
        assert_eq!(reserved, 5);

        let cap_error = reserve_batch_deletions(&mut reserved, 1, 5, "beta").unwrap_err();
        assert_eq!(reserved, 5, "a rejected job must not consume the budget");
        assert!(
            cap_error
                .to_string()
                .contains("no mutations for profile \"beta\"")
        );
        assert_eq!(error_exit_code(&cap_error), 1);

        let mut overflowed = usize::MAX;
        let overflow_error =
            reserve_batch_deletions(&mut overflowed, 1, usize::MAX, "gamma").unwrap_err();
        assert_eq!(overflowed, usize::MAX);
        assert!(
            overflow_error
                .to_string()
                .contains("exceed this platform's numeric range")
        );
        assert_eq!(error_exit_code(&overflow_error), 1);
    }

    #[test]
    fn sync_batch_summary_uses_observed_preflights_and_partial_not_run_counts() {
        let plan_outcomes = vec![
            sync_outcome(
                "alpha",
                SyncBatchStatus::Preflighted,
                Some(plan_with_deletions(1)),
                None,
                false,
            ),
            sync_outcome(
                "beta",
                SyncBatchStatus::Preflighted,
                Some(empty_plan()),
                None,
                false,
            ),
        ];
        let plan_summary = sync_batch_summary_value(&plan_outcomes, true, 10, Some(1), None, None);
        assert_eq!(plan_summary["mode"], "plan");
        assert_eq!(plan_summary["status"], "success");
        assert_eq!(plan_summary["summary"]["preflighted"], 2);
        assert_eq!(
            plan_summary["all_targets_preflighted_before_mutation"],
            true
        );
        assert!(plan_summary["execution_reserved_deletions"].is_null());

        let outcomes = vec![
            sync_outcome(
                "alpha",
                SyncBatchStatus::Success,
                Some(plan_with_deletions(1)),
                Some(plan_with_deletions(1)),
                true,
            ),
            sync_outcome(
                "beta",
                SyncBatchStatus::Partial,
                Some(plan_with_deletions(1)),
                Some(plan_with_deletions(2)),
                true,
            ),
            sync_outcome(
                "gamma",
                SyncBatchStatus::NotRun,
                Some(empty_plan()),
                None,
                false,
            ),
        ];

        let summary = sync_batch_summary_value(&outcomes, false, 10, Some(2), Some(3), None);
        assert_eq!(summary["schema"], "sdsync.batch.v1");
        assert_eq!(summary["mode"], "sync");
        assert_eq!(summary["status"], "partial");
        assert_eq!(summary["execution"], "sequential");
        assert_eq!(summary["all_targets_preflighted_before_mutation"], true);
        assert_eq!(summary["preflight_deletions"], 2);
        assert_eq!(summary["execution_reserved_deletions"], 3);
        assert_eq!(summary["summary"]["jobs"], 3);
        assert_eq!(summary["summary"]["succeeded"], 1);
        assert_eq!(summary["summary"]["partial"], 1);
        assert_eq!(summary["summary"]["not_run"], 1);

        let missing_preflight = vec![sync_outcome(
            "delta",
            SyncBatchStatus::Failed,
            None,
            None,
            false,
        )];
        let summary = sync_batch_summary_value(
            &missing_preflight,
            false,
            10,
            None,
            None,
            Some("preflight failed"),
        );
        assert_eq!(summary["status"], "failed");
        assert_eq!(summary["all_targets_preflighted_before_mutation"], false);
        assert_eq!(summary["error"], "preflight failed");
    }

    #[test]
    fn sync_batch_job_schema_preserves_preflight_and_fresh_plan_drift() {
        let outcome = sync_outcome(
            "photos",
            SyncBatchStatus::Partial,
            Some(plan_with_deletions(1)),
            Some(plan_with_deletions(2)),
            true,
        );

        let value = sync_batch_job_value(&outcome);
        assert_eq!(value["schema"], "sdsync.batch-job.v1");
        assert_eq!(value["profile"], "photos");
        assert_eq!(value["status"], "partial");
        assert_eq!(value["mutation_authorized"], true);
        assert_eq!(value["preflight_plan"]["summary"]["deletions"], 1);
        assert_eq!(value["execution_plan"]["summary"]["deletions"], 2);
        assert_eq!(
            value["preflight_plan"]["actions"]["post_deletes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            value["execution_plan"]["actions"]["post_deletes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn sync_batch_status_distinguishes_preflight_failure_from_partial_execution() {
        let preflight_failure = vec![
            sync_outcome("alpha", SyncBatchStatus::Failed, None, None, false),
            sync_outcome("beta", SyncBatchStatus::NotRun, None, None, false),
        ];
        assert_eq!(sync_batch_status(&preflight_failure, None), "failed");

        let partial_execution = vec![
            sync_outcome(
                "alpha",
                SyncBatchStatus::Success,
                Some(empty_plan()),
                Some(empty_plan()),
                false,
            ),
            sync_outcome(
                "beta",
                SyncBatchStatus::Partial,
                Some(empty_plan()),
                Some(plan_with_deletions(1)),
                true,
            ),
            sync_outcome(
                "gamma",
                SyncBatchStatus::NotRun,
                Some(empty_plan()),
                None,
                false,
            ),
        ];
        assert_eq!(sync_batch_status(&partial_execution, None), "partial");
        assert_eq!(
            sync_batch_status(&partial_execution, Some("aggregate guard failed")),
            "failed"
        );
    }

    #[test]
    fn doctor_batch_summary_tracks_observed_preflights_and_partial_probe_state() {
        let mut partial_probe = write_probe_report();
        partial_probe.upload_attempted = true;
        partial_probe.leftover_remote_probe_path =
            Some("/share/acceptance/.sdsync-write-probe-test".to_owned());
        let outcomes = vec![
            doctor_outcome(
                "alpha",
                DoctorBatchStatus::Success,
                Some(doctor_result(true, Some(write_probe_report()), None)),
            ),
            doctor_outcome(
                "beta",
                DoctorBatchStatus::Partial,
                Some(doctor_result(
                    true,
                    Some(partial_probe),
                    Some("probe cleanup failed"),
                )),
            ),
            doctor_outcome(
                "gamma",
                DoctorBatchStatus::NotRun,
                Some(doctor_result(false, None, None)),
            ),
        ];

        let summary = doctor_batch_summary_value(&outcomes, true);
        assert_eq!(summary["schema"], "sdsync.doctor-batch.v1");
        assert_eq!(summary["status"], "partial");
        assert_eq!(summary["execution"], "sequential");
        assert_eq!(summary["write_tests_requested"], true);
        assert_eq!(summary["all_targets_preflighted_before_mutation"], true);
        assert_eq!(summary["summary"]["succeeded"], 1);
        assert_eq!(summary["summary"]["partial"], 1);
        assert_eq!(summary["summary"]["not_run"], 1);

        let failed_preflight = vec![doctor_outcome("delta", DoctorBatchStatus::Failed, None)];
        let summary = doctor_batch_summary_value(&failed_preflight, true);
        assert_eq!(summary["status"], "failed");
        assert_eq!(summary["all_targets_preflighted_before_mutation"], false);

        let non_mutating = doctor_batch_summary_value(&outcomes, false);
        assert_eq!(
            non_mutating["all_targets_preflighted_before_mutation"],
            false
        );
    }

    #[test]
    fn doctor_probe_failure_keeps_report_cleanup_evidence_and_appended_context() {
        let mut report = write_probe_report();
        report.directory_created = true;
        report.upload_attempted = true;
        report.leftover_remote_probe_path =
            Some("/share/acceptance/.sdsync-write-probe-test".to_owned());
        let mut result = doctor_result(
            true,
            Some(report.clone()),
            Some("upload verification failed"),
        );

        assert!(doctor_result_may_have_mutated(&result));
        append_doctor_failure_context(&mut result, "File Station logout also failed", "timeout");
        append_doctor_failure_context(
            &mut result,
            "observability shutdown also failed",
            "collector unavailable",
        );
        let result = finish_doctor_logger(None, Ok(result), true).unwrap();
        assert_eq!(result.write_probe, Some(report));

        let error = result.write_probe_error.clone().unwrap();
        assert!(error.contains("upload verification failed"));
        assert!(error.contains("File Station logout also failed: timeout"));
        assert!(error.contains("observability shutdown also failed: collector unavailable"));

        let mut outcome = doctor_outcome("acceptance", DoctorBatchStatus::Partial, Some(result));
        outcome.error = Some(error);
        let value = doctor_batch_job_value(&outcome);
        assert_eq!(value["schema"], "sdsync.doctor-job.v1");
        assert_eq!(value["status"], "partial");
        assert_eq!(value["doctor"]["write_test"]["status"], "failed");
        assert_eq!(
            value["doctor"]["write_test"]["report"]["leftover_remote_probe_path"],
            "/share/acceptance/.sdsync-write-probe-test"
        );
        assert_eq!(
            value["doctor"]["write_test"]["report"]["cleanup_completed"],
            false
        );
    }

    #[test]
    fn doctor_mutation_evidence_is_conservative_but_not_assumed_from_preflight() {
        let preflight = doctor_result(false, None, None);
        assert!(!doctor_result_may_have_mutated(&preflight));

        let untouched_probe =
            doctor_result(true, Some(write_probe_report()), Some("create failed"));
        assert!(!doctor_result_may_have_mutated(&untouched_probe));

        let mut attempted_report = write_probe_report();
        attempted_report.server_copy_attempted = true;
        let attempted = doctor_result(true, Some(attempted_report), Some("copy failed"));
        assert!(doctor_result_may_have_mutated(&attempted));
    }

    #[test]
    fn cancellation_token_failure_maps_to_exit_130() {
        let cancellation = CancellationToken::default();
        assert!(cancellation.check().is_ok());
        cancellation.cancel();

        let error = cancellation.check().unwrap_err();
        assert!(matches!(error, Error::Cancelled));
        assert_eq!(error_exit_code(&error), 130);
    }

    #[test]
    fn every_batch_selection_mode_rejects_control_characters_in_profile_names() {
        let unsafe_name = "source\nforged-output";
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert(unsafe_name.to_owned(), config::Profile::default());
        let loaded = config::LoadedConfig {
            path: PathBuf::from("config.toml"),
            values: config::ConfigFile {
                default_profile: None,
                profiles,
            },
        };

        for batch in [
            cli::BatchArgs {
                profiles: Vec::new(),
                all_profiles: true,
                max_total_delete: None,
            },
            cli::BatchArgs {
                profiles: vec![unsafe_name.to_owned()],
                all_profiles: false,
                max_total_delete: None,
            },
        ] {
            let error = match select_job_profiles(Some(&loaded), None, &batch) {
                Ok(_) => panic!("control-character profile name was accepted"),
                Err(error) => error,
            };
            assert!(matches!(error, Error::Configuration(_)));
            assert!(error.to_string().contains("contain no control characters"));
        }
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

    #[test]
    fn final_reconciliation_rejects_any_remaining_in_scope_action() {
        assert!(ensure_reconciled(&empty_plan()).is_ok());
        let mut pending = empty_plan();
        pending.creates.push(plan::CreateAction {
            relative: "late".to_owned(),
            remote_path: "/share/root/late".to_owned(),
        });
        assert!(matches!(
            ensure_reconciled(&pending),
            Err(Error::ReconciliationPending { operations: 1 })
        ));
    }
}
