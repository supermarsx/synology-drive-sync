#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as FmtWrite;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use clap::CommandFactory;
use serde_json::{Value, json};
use synology_drive_sync::DOCTOR_SECTION_SPECS;
use synology_drive_sync::api::{
    self, API_REQUIREMENTS, ApiCatalogue, ApiClient, ApiObservation, ApiRequirement,
    CapabilityProbe, CapabilityProbeSpec, ChannelProbe, ClientOptions, DestinationPathResolution,
    DiagnosticRemoteInventory, FileStationInfo, RequestObserver, SESSION_CHANNEL_VARIANTS,
    UploadObserver, UploadTransferEvent, WriteProbeReport, is_discovery_response_failure,
};
use synology_drive_sync::batch::{BatchJob, ValidatedBatch};
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::local::{self, IgnoreRules, LocalEntry};
use synology_drive_sync::observability::{
    BUILD, BearerTokenSource, BoundedText, DecodeFault, EventCode, EventLogger, EventMetrics,
    FileLogConfig, LogEvent, LogFormat as EventLogFormat, LogLevel as EventLogLevel, LoggerConfig,
    RemoteDelivery, RemoteLogConfig, RequestOutcome, SessionTransport,
};
use synology_drive_sync::path::RemoteRoot;
use synology_drive_sync::plan::{self, CompareMode, PlanOptions, RemoteSnapshot, SyncPlan};
use synology_drive_sync::progress::{
    OperationKind, ProgressFormat, ProgressMode as RendererProgressMode, ProgressRenderer,
    ProgressTotals, ProgressTracker,
};
use synology_drive_sync::progress_record::ProgressRecorder;
use synology_drive_sync::source_diagnostics::{
    SourceDiagnosticOptions, SourceDiagnosticReport, diagnose_source,
};
use synology_drive_sync::status_cache;
use synology_drive_sync::sync::{
    self, ExecuteOptions, ExecutionEvent, ExecutionReport, UploadObserverFactory,
};
use synology_drive_sync::transport_diagnostics::{
    CookieLedger, IntermediarySummary, ProbeObservation, ReachabilityBudget, ReachabilityReport,
    TransportTranscript, classify_endpoint, measure_reachability,
};
use synology_drive_sync::{Error, Result};
use zeroize::Zeroizing;

mod cli;
mod config;
mod credentials;

const FILE_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const FILE_LOG_BACKUPS: usize = 3;
const REMOTE_LOG_QUEUE_CAPACITY: usize = 1_024;
const REMOTE_LOG_TIMEOUT: Duration = Duration::from_secs(10);
const LOGGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Flush window for a run that is already stopping because the operator asked it to.
///
/// The full [`LOGGER_SHUTDOWN_TIMEOUT`] is a delivery guarantee owed to a run that reached its
/// own end. A cancelled run owes no such guarantee, and five more seconds of waiting after the
/// cancellation has already been observed is exactly what Ctrl-C asked to avoid. Timing out here
/// only adds a warning: the cancellation is what the caller receives either way, so the exit code
/// stays 130.
const CANCELLED_LOGGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const PROGRESS_RENDER_INTERVAL: Duration = Duration::from_millis(100);
/// The documented exit code for cooperative Ctrl+C/SIGINT/SIGTERM cancellation, used both by the
/// ordinary error mapping and by the second-signal force exit.
const CANCELLED_EXIT_CODE: u8 = 130;

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
    // Before anything else: this build selects its own TLS provider, and a
    // missing one would otherwise surface much later, inside client
    // construction, where no deadline covers it.
    if let Err(error) = synology_drive_sync::install_crypto_provider() {
        print_error(&error);
        return ExitCode::from(error_exit_code(&error));
    }
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
                run_sync(
                    resolved.remove(0).settings,
                    sync.dry_run,
                    false,
                    arguments.global.output.progress_record.as_deref(),
                )
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
                run_sync(
                    resolved.remove(0).settings,
                    true,
                    plan.exit_code,
                    arguments.global.output.progress_record.as_deref(),
                )
            }
        }
        cli::Invocation::Status(status) => {
            if status.sync.batch.requested() {
                return Err(Error::Configuration(
                    "status inspects one source and destination pair; select a single profile"
                        .to_owned(),
                ));
            }
            let selected = select_job_profiles(
                loaded.as_ref(),
                arguments.global.profile.as_deref(),
                &status.sync.batch,
            )?;
            let profile = selected[0].name.clone();
            let settings =
                config::resolve_sync(selected[0].values, &status.sync, &arguments.global.output)
                    .map_err(config_error)?;
            run_status(
                status,
                &profile,
                settings,
                arguments.global.output.progress_record.as_deref(),
            )
        }
        cli::Invocation::StatusRollup(rollup) => {
            run_status_rollup(rollup, &arguments.global.output)
        }
        cli::Invocation::Resync(resync) => {
            // Checked before anything is resolved: a caller who asked a destructive-adjacent
            // command to also delete should hear about that first, not after a profile error.
            if resync.sync.safety.delete {
                return Err(Error::Configuration(
                    "resync never deletes; it only re-uploads. Remove --delete, and use `sync --delete` if a mirror deletion is what you want"
                        .to_owned(),
                ));
            }
            if resync.sync.batch.requested() {
                return Err(Error::Configuration(
                    "resync overwrites one destination at a time; select a single profile"
                        .to_owned(),
                ));
            }
            let selected = select_job_profiles(
                loaded.as_ref(),
                arguments.global.profile.as_deref(),
                &resync.sync.batch,
            )?;
            let settings =
                config::resolve_sync(selected[0].values, &resync.sync, &arguments.global.output)
                    .map_err(config_error)?;
            run_resync(
                resync,
                settings,
                arguments.global.output.progress_record.as_deref(),
            )
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
                    if doctor.level.is_some() {
                        return Err(Error::Configuration(
                            "doctor source cannot be combined with --level; target diagnostic levels do not alter local source checks"
                                .to_owned(),
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
                        run_doctor(
                            resolved.remove(0).settings,
                            arguments.global.output.progress_record.as_deref(),
                        )
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
    let cancellation = install_cancellation_handler()?;
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
    write_rendered_output(source_doctor_output(result, output.output))
}

fn source_doctor_output(
    result: &TimedSourceDiagnostic,
    format: cli::OutputFormat,
) -> RenderedOutput {
    let value = json!({
        "schema": "sdsync.source-doctor.v1",
        "source": source_diagnostic_value(result),
    });
    match format {
        cli::OutputFormat::Human => RenderedOutput::Human(
            format!(
                "Source is healthy: {} files, {} directories, {} across {} entries; {} files hashed in {} ms ({}).",
                result.report.files,
                result.report.directories,
                format_bytes(result.report.bytes),
                result.report.entries,
                result.report.hashed_files,
                duration_millis(result.elapsed),
                result.report.canonical_root.display(),
            ) + "\n",
        ),
        cli::OutputFormat::Json => RenderedOutput::Json(value),
        cli::OutputFormat::Ndjson => RenderedOutput::Ndjson(vec![value]),
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
    let cancellation = install_cancellation_handler()?;
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
    source_batch_completion(&outcomes, cancelled || cancellation.is_cancelled())
}

fn source_batch_completion(outcomes: &[SourceBatchOutcome], cancelled: bool) -> Result<ExitCode> {
    if cancelled {
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
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_source_batch_output_to(&mut stdout, outcomes, output.output)
}

fn source_batch_job_value(outcome: &SourceBatchOutcome) -> Value {
    json!({
        "schema": "sdsync.source-doctor-job.v1",
        "profile": outcome.name,
        "status": if outcome.result.is_some() { "success" } else if outcome.not_run { "not-run" } else { "failed" },
        "source": outcome.result.as_ref().map(source_diagnostic_value),
        "error": outcome.error,
    })
}

fn write_source_batch_output_to<W: Write>(
    writer: &mut W,
    outcomes: &[SourceBatchOutcome],
    format: cli::OutputFormat,
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
    match format {
        cli::OutputFormat::Human => {
            writeln!(
                writer,
                "Source diagnostic batch: {succeeded} succeeded, {failed} failed, {not_run} not run."
            )
            .map_err(output_error)?;
            for outcome in outcomes {
                if let Some(result) = &outcome.result {
                    writeln!(
                        writer,
                        "  [{}] healthy: {} files, {} directories, {}, {} hashed ({})",
                        outcome.name,
                        result.report.files,
                        result.report.directories,
                        format_bytes(result.report.bytes),
                        result.report.hashed_files,
                        result.report.canonical_root.display(),
                    )
                    .map_err(output_error)?;
                } else if outcome.not_run {
                    writeln!(writer, "  [{}] not run", outcome.name).map_err(output_error)?;
                } else {
                    writeln!(
                        writer,
                        "  [{}] failed: {}",
                        outcome.name,
                        outcome.error.as_deref().unwrap_or("unknown failure")
                    )
                    .map_err(output_error)?;
                }
            }
            writer.flush().map_err(output_error)
        }
        cli::OutputFormat::Json => {
            let job_values = outcomes
                .iter()
                .map(source_batch_job_value)
                .collect::<Vec<_>>();
            let mut value = summary;
            value["jobs"] = Value::Array(job_values);
            write_json_to(writer, &value)
        }
        cli::OutputFormat::Ndjson => {
            for outcome in outcomes {
                write_json_line_to(writer, &source_batch_job_value(outcome))?;
            }
            write_json_line_to(writer, &summary)
        }
    }
}

#[cfg(test)]
fn source_batch_output(
    outcomes: &[SourceBatchOutcome],
    format: cli::OutputFormat,
) -> RenderedOutput {
    let mut buffer = Vec::new();
    write_source_batch_output_to(&mut buffer, outcomes, format)
        .expect("writing rendered source batch output to a Vec cannot fail");
    captured_rendered_output(format, buffer)
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
    let cancellation = install_cancellation_handler()?;

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
        return changes_completion(changes, cancellation.is_cancelled(), changes_exit_code);
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
    sync_batch_completion(&outcomes, cancelled || cancellation.is_cancelled())
}

fn sync_batch_completion(outcomes: &[SyncBatchOutcome], cancelled: bool) -> Result<ExitCode> {
    if cancelled {
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
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_sync_batch_output_to(
        &mut stdout,
        SyncBatchRender {
            outcomes,
            output,
            plan_only,
            max_total_delete,
            preflight_deletions,
            execution_reserved_deletions,
            batch_error,
        },
    )
}

#[derive(Clone, Copy)]
struct SyncBatchRender<'a> {
    outcomes: &'a [SyncBatchOutcome],
    output: &'a config::ResolvedOutput,
    plan_only: bool,
    max_total_delete: usize,
    preflight_deletions: Option<usize>,
    execution_reserved_deletions: Option<usize>,
    batch_error: Option<&'a str>,
}

fn write_sync_batch_output_to<W: Write>(writer: &mut W, render: SyncBatchRender<'_>) -> Result<()> {
    let SyncBatchRender {
        outcomes,
        output,
        plan_only,
        max_total_delete,
        preflight_deletions,
        execution_reserved_deletions,
        batch_error,
    } = render;
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
    let summary = sync_batch_summary_value(
        outcomes,
        plan_only,
        max_total_delete,
        preflight_deletions,
        execution_reserved_deletions,
        batch_error,
    );
    match output.output {
        cli::OutputFormat::Human => {
            writeln!(
                writer,
                "Batch {}: status {status}; {succeeded} succeeded, {preflighted} preflighted, {partial} potentially partial, {failed} failed before mutation authorization, {not_run} not run; aggregate deletion cap {max_total_delete}.",
                if plan_only { "plan" } else { "sync" },
            )
            .map_err(output_error)?;
            if let Some(error) = batch_error {
                writeln!(writer, "Batch safety check failed: {error}").map_err(output_error)?;
            }
            for outcome in outcomes {
                writeln!(writer, "\n[{}] {}", outcome.name, outcome.status.as_str())
                    .map_err(output_error)?;
                let display_plan = outcome
                    .execution_plan
                    .as_ref()
                    .or(outcome.preflight_plan.as_ref());
                if let Some(plan) = display_plan {
                    write_plan_human_to(
                        writer,
                        plan,
                        plan_only || output.verbosity > 0,
                        &plan::Scope::root(),
                    )?;
                }
                if let (Some(preflight), Some(execution)) =
                    (&outcome.preflight_plan, &outcome.execution_plan)
                    && preflight.delete_count() != execution.delete_count()
                {
                    writeln!(
                        writer,
                        "Deletion-plan drift: preflight {}, fresh execution {}.",
                        preflight.delete_count(),
                        execution.delete_count()
                    )
                    .map_err(output_error)?;
                }
                if let Some(report) = &outcome.report {
                    writeln!(
                        writer,
                        "Result: {} uploaded ({}), {} copied on NAS, {} directories created, {} deleted in {} ms.",
                        report.uploaded,
                        format_bytes(report.uploaded_bytes),
                        report.copied,
                        report.created,
                        report.deleted,
                        duration_millis(outcome.elapsed.unwrap_or_default()),
                    )
                    .map_err(output_error)?;
                }
                if let Some(error) = &outcome.error {
                    writeln!(writer, "Error: {error}").map_err(output_error)?;
                }
            }
            writer.flush().map_err(output_error)
        }
        cli::OutputFormat::Json => {
            let job_values = outcomes
                .iter()
                .map(sync_batch_job_value)
                .collect::<Vec<_>>();
            let mut summary = summary;
            summary["jobs"] = Value::Array(job_values);
            write_json_to(writer, &summary)
        }
        cli::OutputFormat::Ndjson => {
            for outcome in outcomes {
                write_json_line_to(writer, &sync_batch_job_value(outcome))?;
            }
            write_json_line_to(writer, &summary)
        }
    }
}

#[cfg(test)]
fn sync_batch_output(
    outcomes: &[SyncBatchOutcome],
    output: &config::ResolvedOutput,
    plan_only: bool,
    max_total_delete: usize,
    preflight_deletions: Option<usize>,
    execution_reserved_deletions: Option<usize>,
    batch_error: Option<&str>,
) -> RenderedOutput {
    let mut buffer = Vec::new();
    write_sync_batch_output_to(
        &mut buffer,
        SyncBatchRender {
            outcomes,
            output,
            plan_only,
            max_total_delete,
            preflight_deletions,
            execution_reserved_deletions,
            batch_error,
        },
    )
    .expect("writing rendered batch output to a Vec cannot fail");
    captured_rendered_output(output.output, buffer)
}

/// Attach the progress record a queued dashboard request asked for, if it asked for one.
///
/// Every rejection -- no flag, an unparseable file name, an operation with no phase catalogue --
/// yields the token unchanged, so the run proceeds exactly as it did before this option existed.
/// That is the whole contract: the record is something a supervisor may read, never something the
/// operation depends on.
///
/// Batch invocations deliberately publish nothing. Phases are reported as "step N of M" and that
/// claim has to stay true; N profiles run in sequence through the same phase list, so a single
/// record would count 1..M once per profile with no way to say which. Naming the profile would
/// need a seventh field, and the dashboard validates the document by exact key count -- it would
/// drop every record rather than render the extra one. The honest answer is to publish none.
fn attach_progress_record(
    cancellation: CancellationToken,
    record: Option<&Path>,
    operation: &str,
) -> CancellationToken {
    match record.and_then(|path| ProgressRecorder::new(path, operation)) {
        Some(recorder) => cancellation.with_progress(Arc::new(recorder)),
        None => cancellation,
    }
}

fn run_sync(
    settings: config::ResolvedSync,
    plan_only: bool,
    changes_exit_code: bool,
    record: Option<&Path>,
) -> Result<ExitCode> {
    let cancellation = attach_progress_record(
        install_cancellation_handler()?,
        record,
        if plan_only { "plan" } else { "run" },
    );
    let result = run_sync_job(&settings, plan_only, &cancellation, |_| Ok(()))?;
    write_sync_output(
        &result.plan,
        result.report.as_ref(),
        result.elapsed,
        &settings.output,
        plan_only,
        &resolved_scope(&settings)?,
    )?;
    changes_completion(
        !result.plan.is_empty(),
        cancellation.is_cancelled(),
        changes_exit_code,
    )
}

fn changes_completion(changes: bool, cancelled: bool, changes_exit_code: bool) -> Result<ExitCode> {
    if cancelled {
        Err(Error::Cancelled)
    } else if changes_exit_code && changes {
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

/// Plan, and only on a matching confirmation perform, an unconditional re-upload.
///
/// Two steps by construction. Without a ticket this plans and returns; there is no argument
/// combination that overwrites anything on a first invocation. With a ticket, the plan is rebuilt
/// from live state and the guard runs *before* any mutation, so a destination that changed since
/// the caller looked cannot be overwritten on the strength of a stale confirmation.
fn run_resync(
    arguments: &cli::ResyncArgs,
    mut settings: config::ResolvedSync,
    record: Option<&Path>,
) -> Result<ExitCode> {
    // The `--delete` argument is refused before resolution; this pins the setting off regardless
    // of where else it could have come from, including a profile.
    settings.safety.delete = false;
    settings.behavior.force_resync = true;

    let scope = resolved_scope(&settings)?;
    let cancellation = attach_progress_record(install_cancellation_handler()?, record, "resync");
    let plan_only = arguments.confirm.is_none();

    // Captured from inside the guard so a refused confirmation can still report the plan that
    // caused the refusal, rather than making the caller run the planning step again themselves.
    let planned: RefCell<Option<SyncPlan>> = RefCell::new(None);
    let expected = arguments.confirm.as_deref();
    let outcome = run_sync_job(&settings, plan_only, &cancellation, |plan| {
        planned.replace(Some(plan.clone()));
        let Some(expected) = expected else {
            return Ok(());
        };
        let current = plan::resync_ticket(&scope, CompareMode::Force, plan);
        if current == expected {
            Ok(())
        } else {
            Err(Error::ResyncTicketStale {
                presented: expected.to_owned(),
                current,
            })
        }
    });

    match outcome {
        Ok(result) => {
            let ticket = plan::resync_ticket(&scope, CompareMode::Force, &result.plan);
            write_resync_output(
                &result.plan,
                result.report.as_ref(),
                result.elapsed,
                &settings.output,
                &scope,
                plan_only.then_some(ticket.as_str()),
                None,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Err(Error::ResyncTicketStale { presented, current }) => {
            // A live destination changes; being told only "no" would leave the caller looping
            // between plan and confirm. The refreshed plan and its ticket are printed here, so the
            // next attempt describes what would actually happen.
            let plan = planned
                .into_inner()
                .expect("the guard captured the plan before rejecting it");
            write_resync_output(
                &plan,
                None,
                Duration::default(),
                &settings.output,
                &scope,
                Some(current.as_str()),
                Some(presented.as_str()),
            )?;
            Ok(ExitCode::from(cli::PLAN_CHANGES_EXIT_CODE))
        }
        Err(error) => Err(error),
    }
}

fn write_resync_output(
    plan: &SyncPlan,
    report: Option<&ExecutionReport>,
    elapsed: Duration,
    output: &config::ResolvedOutput,
    scope: &plan::Scope,
    ticket: Option<&str>,
    stale: Option<&str>,
) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_resync_output_to(
        &mut stdout,
        plan,
        report,
        elapsed,
        output,
        scope,
        ticket,
        stale,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_resync_output_to<W: Write>(
    writer: &mut W,
    plan: &SyncPlan,
    report: Option<&ExecutionReport>,
    elapsed: Duration,
    output: &config::ResolvedOutput,
    scope: &plan::Scope,
    ticket: Option<&str>,
    stale: Option<&str>,
) -> Result<()> {
    match output.output {
        cli::OutputFormat::Human => {
            if let Some(stale) = stale {
                writeln!(
                    writer,
                    "Ticket {stale} no longer matches what would be overwritten; nothing was changed. The refreshed plan follows."
                )
                .map_err(output_error)?;
            }
            write_plan_human_to(writer, plan, true, scope)?;
            if let Some(report) = report {
                writeln!(
                    writer,
                    "Resync complete: {} re-uploaded ({}) in {} ms.",
                    report.uploaded,
                    format_bytes(report.uploaded_bytes),
                    duration_millis(elapsed),
                )
                .map_err(output_error)?;
            }
            if let Some(ticket) = ticket {
                writeln!(
                    writer,
                    "Nothing has been changed. To perform this re-upload, confirm this exact plan:\n  --confirm {ticket}"
                )
                .map_err(output_error)?;
            }
            writer.flush().map_err(output_error)
        }
        cli::OutputFormat::Json => write_json_to(
            writer,
            &resync_value(plan, report, elapsed, scope, ticket, stale),
        ),
        cli::OutputFormat::Ndjson => {
            write_json_line_to(
                writer,
                &resync_value(plan, report, elapsed, scope, ticket, stale),
            )?;
            write_plan_ndjson_to(writer, plan)
        }
    }
}

fn resync_value(
    plan: &SyncPlan,
    report: Option<&ExecutionReport>,
    elapsed: Duration,
    scope: &plan::Scope,
    ticket: Option<&str>,
    stale: Option<&str>,
) -> Value {
    json!({
        "schema": "sdsync.resync.v1",
        "kind": if ticket.is_some() { "plan" } else { "completion" },
        "scope": scope.as_str(),
        "confirmed": report.is_some(),
        "stale_ticket": stale,
        "ticket": ticket,
        "overwrites": plan.uploads.len(),
        "overwrite_bytes": plan.upload_bytes,
        "deletes": plan.delete_count(),
        "paths": plan
            .uploads
            .iter()
            .map(|action| json!({
                "relative": action.local.relative,
                "remote_path": action.remote_path,
                "bytes": action.local.size,
            }))
            .collect::<Vec<_>>(),
        "result": report.map(|report| execution_value(report, elapsed)),
    })
}

/// Answer one scoped status query and render it.
///
/// Read-only throughout: it authenticates, walks both sides under the scope, compares, and prints.
/// It never writes to the NAS and cannot reach [`CompareMode::Force`].
fn run_status(
    arguments: &cli::StatusArgs,
    profile: &str,
    settings: config::ResolvedSync,
    record: Option<&Path>,
) -> Result<ExitCode> {
    // The seven phases below are announced through this token. It is already threaded into the
    // scan, the inventory, the digest pass and the cache, so every boundary a dashboard cares
    // about is a call site that already exists -- no phase here was invented to have one.
    let cancellation =
        attach_progress_record(install_cancellation_handler()?, record, "sync-status");
    warn_for_insecure_network(&settings.network, &settings.output);

    let scope = resolved_scope(&settings)?;
    let root = RemoteRoot::parse(&settings.remote)?;
    let rules = IgnoreRules::build(&settings.source, &settings.behavior.excludes)?;
    let compare = compare_mode(settings.behavior.compare);
    let query_states = status_state_filter(&arguments.states, arguments.all);
    let cache = open_status_cache(arguments, profile, &settings, compare);

    cancellation.check()?;
    cancellation.phase("scan_local");
    let scan = local::scan_scoped(
        &settings.source,
        &rules,
        &scope,
        arguments.include_excluded,
        local::SCAN_BUDGET_DEFAULT,
        &cancellation,
    )?;
    let mut local = scan.inventory;

    cancellation.phase("connect");
    let mut client = connect_client(
        &settings.connection.url,
        &settings.network,
        &cancellation,
        None,
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
    cancellation.phase("authenticate");
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
        cancellation.phase("list_remote");
        let scoped = client.remote_inventory_scoped(
            &root,
            &scope,
            local::SCAN_BUDGET_DEFAULT,
            &cancellation,
        )?;
        let mut remote = scoped.inventory;
        cancellation.check()?;

        // A scope that exists on neither side is a distinct answer, not an empty listing: a
        // caller asking about one folder needs to tell "not there" from "nothing to report".
        if !scope.is_root()
            && !local.entries.contains_key(scope.as_str())
            && !remote.entries.contains_key(scope.as_str())
        {
            return Err(Error::ScopeNotFound(scope.as_str().to_owned()));
        }

        let mut cache_report = status_cache::CacheReport::off();
        let mut comparison = BTreeSet::new();
        if compare == CompareMode::Content {
            cancellation.phase("compare");
            client.require_content_fingerprint_api()?;
            // Status is read-only: it never deletes, uploads or server-copies, so every digest
            // it needs is a comparison digest, none of them guards a mutation, and nothing here
            // is ever promoted -- a status run reports a difference rather than acting on one.
            //
            // That is also precisely why this is the one path allowed to reuse stored digests.
            let (selected, report) = populate_status_digests(
                &client,
                &mut local,
                &mut remote,
                &rules,
                cache.as_ref(),
                &settings.output,
                &cancellation,
            )?;
            comparison = selected;
            cache_report = report;
        }
        cancellation.check()?;

        cancellation.phase("build_report");
        let mut page = plan::build_status_page(
            &root,
            &local,
            &remote,
            &scan.excluded,
            &rules,
            &plan::StatusQuery {
                scope: &scope,
                compare,
                filter: arguments.filter.as_deref(),
                states: query_states,
                include_excluded: arguments.include_excluded,
                limit: arguments
                    .limit
                    .map_or(plan::STATUS_PAGE_SIZE_DEFAULT, usize::from),
                cursor: arguments
                    .cursor
                    .as_deref()
                    .map(plan::StatusCursor::new)
                    .as_ref(),
            },
        )?;
        // Either side hitting its budget makes every total a floor rather than a count.
        page.stats.complete = page.stats.complete && scan.complete && scoped.complete;

        // Persist last, and only what this pass actually observed. A failure to write is not a
        // failure to answer: the page in hand was derived from live evidence either way, so a
        // full disk degrades the next run to a cold one rather than this one to an error.
        if let Some(cache) = cache.as_ref()
            && compare == CompareMode::Content
        {
            cancellation.phase("store_results");
            let scoped_query = !scope.is_root();
            if let Err(error) = cache.store(&local, &remote, &comparison, scoped_query) {
                warn_cache(
                    &settings.output,
                    &format!("digests were not stored: {error}"),
                );
            }
            if let Err(error) = cache.store_rollup(&page.stats, &cache_report, scoped_query) {
                warn_cache(&settings.output, &format!("rollup was not stored: {error}"));
            }
        }
        Ok((page, cache_report))
    })();

    let (page, cache_report) = finish_authenticated_operation(&mut client, operation)?;
    write_status_output(&page, &scope, compare, &cache_report, &settings.output)?;
    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(ExitCode::SUCCESS)
}

/// Print the stored totals for every profile, and their combination.
///
/// The read path the dashboard and the desktop widget sit on, and the reason the whole feature can
/// answer "what needs my attention" without a scan. It opens the rollup documents and **nothing
/// else**: no digest cache, no source file, no File Station call, no authentication. That property
/// is what makes it affordable on a short polling interval, and it is pinned by a test rather than
/// left to be eroded by a later "small" lookup.
fn run_status_rollup(
    arguments: &cli::StatusRollupArgs,
    output: &cli::OutputArgs,
) -> Result<ExitCode> {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Message("the system clock is before the Unix epoch".to_owned()))?
            .as_secs(),
    )
    .map_err(|_| Error::Message("the system clock is outside the supported range".to_owned()))?;

    let expected = (!arguments.profiles.is_empty()).then_some(arguments.profiles.as_slice());
    let aggregate = status_cache::compose_rollups(&arguments.status_cache, expected, now);

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    // Read straight from the arguments rather than through `resolve_output`: that needs a
    // configuration profile, and this command deliberately loads none.
    match output.output.unwrap_or(cli::OutputFormat::Human) {
        cli::OutputFormat::Json | cli::OutputFormat::Ndjson => {
            let value = serde_json::to_value(&aggregate).map_err(|error| {
                Error::Message(format!("could not render the stored totals: {error}"))
            })?;
            write_json_line_to(&mut stdout, &value)?;
        }
        cli::OutputFormat::Human => write_status_rollup_human_to(&mut stdout, &aggregate)?,
    }
    stdout.flush().map_err(output_error)?;
    Ok(ExitCode::SUCCESS)
}

fn write_status_rollup_human_to<W: Write>(
    writer: &mut W,
    aggregate: &status_cache::RollupAggregate,
) -> Result<()> {
    if aggregate.profiles.is_empty() {
        writeln!(
            writer,
            "No profile has been observed yet; run status once to record totals."
        )
        .map_err(output_error)?;
        return Ok(());
    }
    // A truncated walk makes every count a floor, so it is marked wherever it is printed rather
    // than explained once at the bottom where a reader may not reach it.
    for profile in &aggregate.profiles {
        let more = if profile.observation.complete {
            ""
        } else {
            "+"
        };
        writeln!(
            writer,
            "{}: {}{more} in sync, {}{more} pending upload ({}{more}), {}{more} needing attention, observed {}.",
            profile.profile,
            profile.state.in_sync.files,
            profile.state.would_transfer.files,
            format_bytes(profile.state.would_transfer.bytes),
            profile.state.attention_entries,
            describe_evidence_age(profile.observed_at_epoch),
        )
        .map_err(output_error)?;
    }
    for missing in &aggregate.profiles_never_observed {
        writeln!(writer, "{missing}: never observed.").map_err(output_error)?;
    }

    match (&aggregate.total, &aggregate.total_unavailable_reason) {
        (Some(total), _) => {
            let more = if aggregate.complete { "" } else { "+" };
            writeln!(
                writer,
                "All profiles: {}{more} in sync, {}{more} pending upload ({}{more}), {}{more} needing attention.",
                total.in_sync.files,
                total.would_transfer.files,
                format_bytes(total.would_transfer.bytes),
                total.attention_entries,
            )
            .map_err(output_error)?;
        }
        // Printed, never omitted. A missing row invites the reader to add the per-profile figures
        // themselves and reach by hand the same wrong answer this withholding exists to prevent.
        (None, Some(reason)) => {
            writeln!(writer, "Combined total unavailable: {reason}.").map_err(output_error)?;
        }
        (None, None) => {}
    }
    if !aggregate.complete && aggregate.total.is_some() {
        writeln!(
            writer,
            "Marked totals are lower bounds: a profile was never observed, or a scan budget stopped its walk."
        )
        .map_err(output_error)?;
    }
    Ok(())
}

/// Bind this query to a digest cache, or decline to use one.
///
/// Declines silently in every unusable case -- no directory configured, an unsafe directory, a
/// comparison mode that computes no digests, a clock that cannot be read. A cache is an
/// accelerator, so failing to obtain one is never an error a caller should see: the query simply
/// costs what it always cost.
///
/// Only [`CompareMode::Content`] is eligible. The other modes decide from size and modification
/// time alone, so there is nothing to accelerate and nothing to get wrong.
fn open_status_cache(
    arguments: &cli::StatusArgs,
    profile: &str,
    settings: &config::ResolvedSync,
    compare: CompareMode,
) -> Option<status_cache::StatusCache> {
    if compare != CompareMode::Content {
        return None;
    }
    let directory = arguments.status_cache.clone()?;
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
    .ok()?;
    status_cache::StatusCache::open(
        status_cache::CacheOptions {
            directory,
            profile: profile.to_owned(),
            // The source and destination are part of the key. A profile repointed at another
            // folder must not be answered from evidence gathered about the previous one.
            source: settings.source.to_string_lossy().into_owned(),
            remote: settings.remote.clone(),
            compare: compare_label(compare),
            refresh: arguments.status_cache_refresh,
            max_age_seconds: arguments
                .status_cache_max_age
                .map_or(status_cache::DEFAULT_MAX_AGE_SECONDS, |days| {
                    i64::from(days).saturating_mul(24 * 60 * 60)
                }),
            canary: arguments
                .status_cache_canary
                .map_or(status_cache::DEFAULT_CANARY, usize::from),
        },
        now,
    )
}

/// Fill both inventories' content digests, reusing stored evidence where it is still valid.
///
/// The cache is a third digest populator, running before the two that already exist and narrowing
/// what they are asked for. It never decides anything: the verdict is still derived from digests by
/// [`plan::build_status_page`], so there is exactly one implementation of the comparison and a
/// cached answer cannot diverge from a computed one.
fn populate_status_digests(
    client: &ApiClient,
    local: &mut local::LocalInventory,
    remote: &mut api::RemoteInventory,
    rules: &IgnoreRules,
    cache: Option<&status_cache::StatusCache>,
    output: &config::ResolvedOutput,
    cancellation: &CancellationToken,
) -> Result<(BTreeSet<String>, status_cache::CacheReport)> {
    let comparison = plan::select_comparison_remote_digests(local, remote, rules);

    // Both populators skip whatever already carries a digest, so a warm cache narrows their work
    // without either of them being told about it -- and, deliberately, without a set naming the
    // served paths, which would hold every path a third time and breach the memory ceiling the
    // scan budget itself is derived from.
    let compute_live = |local: &mut local::LocalInventory,
                        remote: &mut api::RemoteInventory|
     -> Result<()> {
        local::populate_content_md5_selective(local, &comparison, cancellation)?;
        let outstanding: BTreeSet<String> = comparison
            .iter()
            .filter(|relative| {
                remote
                    .entries
                    .get(relative.as_str())
                    .is_none_or(|entry| entry.content_md5.is_none())
            })
            .cloned()
            .collect();
        client.populate_remote_content_digests(remote, &outstanding, &BTreeSet::new(), cancellation)
    };

    let Some(cache) = cache else {
        compute_live(local, remote)?;
        return Ok((comparison, status_cache::CacheReport::off()));
    };

    let served = cache.serve(local, remote, &comparison);
    compute_live(local, remote)?;

    // The canary is checked against digests that were just computed live, so a disagreement is
    // evidence about the stored file rather than about the sample. Discarding all of it and
    // recomputing is the response: repairing the one entry would conceal the systematic case,
    // which is the only case worth detecting.
    if let Some(disagreeing) =
        status_cache::StatusCache::verify_canary(&served.probes, local, remote)
    {
        warn_cache(
            output,
            &format!(
                "stored digests disagreed with live evidence at {disagreeing:?}; the cache was discarded and this answer recomputed in full"
            ),
        );
        cache.discard();
        for entry in local.entries.values_mut() {
            entry.content_md5 = None;
        }
        for entry in remote.entries.values_mut() {
            entry.content_md5 = None;
        }
        compute_live(local, remote)?;
        let report = status_cache::CacheReport {
            state: status_cache::CacheState::Unusable,
            digests_reused: 0,
            digests_computed: comparison.len(),
            oldest_evidence_epoch: None,
            canary_checked: served.probes.len(),
        };
        return Ok((comparison, report));
    }

    let reused = served.supplied;
    let state = if cache.is_refresh() {
        status_cache::CacheState::Refreshed
    } else if reused == 0 {
        status_cache::CacheState::Cold
    } else {
        status_cache::CacheState::Warm
    };
    let report = status_cache::CacheReport {
        state,
        digests_reused: reused,
        digests_computed: comparison.len().saturating_sub(reused),
        oldest_evidence_epoch: served.oldest_evidence_epoch,
        canary_checked: served.probes.len(),
    };
    Ok((comparison, report))
}

/// Report a cache problem without failing the query.
///
/// Written to standard error, never to standard output: the DSM manager captures the two
/// separately precisely so a diagnostic cannot corrupt the JSON document a caller is parsing.
fn warn_cache(output: &config::ResolvedOutput, message: &str) {
    if output.quiet {
        return;
    }
    eprintln!("warning: status cache: {message}");
}

/// Translate the requested states, expanding the `attention` shorthand.
///
/// The expansion comes from [`plan::StateKind::ATTENTION`] rather than being spelled out here, so
/// the command line cannot drift from the set the engine and any other caller use.
fn status_state_filter(states: &[cli::StateArg], all: bool) -> plan::StateFilter {
    if all || states.is_empty() {
        return plan::StateFilter::All;
    }
    let mut kinds = Vec::new();
    for state in states {
        match state {
            cli::StateArg::Attention => kinds.extend_from_slice(plan::StateKind::ATTENTION),
            cli::StateArg::TypeConflict => kinds.push(plan::StateKind::TypeConflict),
            cli::StateArg::MissingRemote => kinds.push(plan::StateKind::MissingRemote),
            cli::StateArg::Differs => kinds.push(plan::StateKind::Differs),
            cli::StateArg::RemoteOnly => kinds.push(plan::StateKind::RemoteOnly),
            cli::StateArg::InSync => kinds.push(plan::StateKind::InSync),
            cli::StateArg::Excluded => kinds.push(plan::StateKind::Excluded),
        }
    }
    kinds.sort_unstable();
    kinds.dedup();
    plan::StateFilter::Only(kinds)
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
    let scope = resolved_scope(settings)?;
    cancellation.phase("scan_local");
    let mut local = local::scan_scoped(
        &settings.source,
        &rules,
        &scope,
        false,
        usize::MAX,
        cancellation,
    )?
    .inventory;
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
    cancellation.phase("connect");
    let mut client = connect_client(
        &settings.connection.url,
        &settings.network,
        cancellation,
        logger.as_ref().map(request_observer),
    )?;
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
    cancellation.phase("authenticate");
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
        cancellation.phase("list_remote");
        let mut remote = client
            .remote_inventory_scoped(&root, &scope, usize::MAX, cancellation)?
            .inventory;
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

        cancellation.phase("compare");
        populate_content_for_plan(
            &client,
            &mut local,
            &mut remote,
            &rules,
            settings,
            server_copy,
            cancellation,
        )?;

        cancellation.phase("build_plan");
        let mut plan = plan::build_plan(
            &root,
            &local,
            &remote,
            &rules,
            &PlanOptions {
                delete: settings.safety.delete,
                allow_empty_source: settings.safety.allow_empty_source,
                max_delete: settings.safety.max_delete,
                compare: effective_compare_mode(settings),
                server_copy,
                scope: scope.clone(),
            },
        )?;
        // A comparison-set file that turned out to differ is now an upload, and uploads are
        // verified against a strong digest. Raise them here, before the plan is reported or
        // executed, so every consumer of it sees fingerprints of the strength it expects.
        plan::promote_upload_fingerprints(&mut plan, cancellation)?;
        let plan = plan;
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
            // Nothing was executed, so there is no convergence to verify. Reconciliation exists to
            // prove that the operations a run performed achieved the intended state; an empty plan
            // performed none. Rebuilding it here re-derives the identical answer from the identical
            // inputs at full price -- a second whole-tree scan, a second whole-tree content hash,
            // and a second remote inventory. On a large tree in content mode that second pass is
            // the single largest cost a no-op run has, and it buys nothing.
            //
            // This is deliberately not a safety check, and must not be reinstated as one. The
            // second pass observes the tree strictly later than the first, so the only difference
            // it can ever report is a change this run did not make -- whereupon `ensure_reconciled`
            // fails the run for someone else's concurrent edit. Dropping it therefore makes a no-op
            // run both cheaper and more correct. Post-execution reconciliation below is untouched:
            // that one verifies work that actually happened, which is the case it exists for.
            cancellation.check()?;
            return Ok((plan, None));
        }

        cancellation.check()?;
        cancellation.phase("upload");
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
            |event| {
                progress.record_execution_event(&event);
                let code = match event {
                    ExecutionEvent::DirectoryCreated { .. } => Some(EventCode::DirectoryCreated),
                    ExecutionEvent::TypeConflictDeleted { .. }
                    | ExecutionEvent::RemoteExtraDeleted { .. } => Some(EventCode::EntryDeleted),
                    ExecutionEvent::RemoteContentCopied { .. }
                    | ExecutionEvent::CopyFallbackUploaded { .. }
                    | ExecutionEvent::Uploaded { .. } => None,
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
                    eprintln!("  {event}");
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
        cancellation.phase("reconcile");
        let reconciliation =
            build_reconciliation_plan(&client, settings, &root, &rules, server_copy, cancellation)?;
        ensure_reconciled(&reconciliation)?;
        cancellation.check()?;
        Ok((plan, Some(report)))
    })();

    finish_authenticated_operation(&mut client, operation)
}

/// Fetch exactly the content digests the active compare mode needs, and no more.
///
/// Content mode needs local digests plus the remote digests that comparison, optional server-copy
/// reuse, and deletion guards require. Force mode compares nothing, so it needs no local hashing
/// and no comparison digests -- but a forced *mirror* still needs the same deletion guards content
/// mode would use. A guard must not weaken because a flag about uploading was passed.
fn populate_content_for_plan(
    client: &ApiClient,
    local: &mut local::LocalInventory,
    remote: &mut api::RemoteInventory,
    rules: &IgnoreRules,
    settings: &config::ResolvedSync,
    server_copy: bool,
    cancellation: &CancellationToken,
) -> Result<()> {
    match effective_compare_mode(settings) {
        CompareMode::Content => {
            client.require_content_fingerprint_api()?;
            let comparison = plan::select_comparison_remote_digests(local, remote, rules);
            let strong = plan::select_strong_remote_digests(
                local,
                remote,
                rules,
                server_copy,
                settings.safety.delete,
            );
            // Local strength mirrors remote strength, entry for entry: MD5 where the remote side
            // will only carry MD5, full strength everywhere else. Uploads promoted out of the
            // comparison set are raised by `plan::promote_upload_fingerprints` before execution.
            local::populate_content_md5_selective(local, &comparison, cancellation)?;
            client.populate_remote_content_digests(remote, &comparison, &strong, cancellation)?;
        }
        CompareMode::Force if settings.safety.delete => {
            client.require_content_fingerprint_api()?;
            // Forced runs compare nothing, so every digest here guards a deletion and must be
            // strong. Unchanged by the comparison split.
            let selected = plan::select_deletion_guard_hashes(local, remote, rules);
            client.populate_remote_content_fingerprints(remote, &selected, cancellation)?;
        }
        CompareMode::Force | CompareMode::Metadata | CompareMode::SizeOnly => {}
    }
    Ok(())
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
    // Reconciliation re-asks the question the run just answered, so it must cover exactly the
    // same scope. A whole-tree reconciliation after a scoped run would report every out-of-scope
    // difference the run deliberately left alone.
    let scope = resolved_scope(settings)?;
    let compare = reconciliation_compare_mode(settings);
    let mut local = local::scan_scoped(
        &settings.source,
        rules,
        &scope,
        false,
        usize::MAX,
        cancellation,
    )?
    .inventory;
    cancellation.check()?;
    let mut remote = client
        .remote_inventory_scoped(root, &scope, usize::MAX, cancellation)?
        .inventory;
    cancellation.check()?;
    if compare == CompareMode::Content {
        client.require_content_fingerprint_api()?;
        let comparison = plan::select_comparison_remote_digests(&local, &remote, rules);
        let strong = plan::select_strong_remote_digests(
            &local,
            &remote,
            rules,
            server_copy,
            settings.safety.delete,
        );
        local::populate_content_md5_selective(&mut local, &comparison, cancellation)?;
        client.populate_remote_content_digests(&mut remote, &comparison, &strong, cancellation)?;
    }
    // Deliberately no promotion here. A reconciliation plan is only ever inspected for emptiness
    // by `ensure_reconciled` and is never executed, so nothing consumes an upload's digest and
    // raising it would read every changed file again to answer a question nobody asks.
    let plan = plan::build_plan(
        root,
        &local,
        &remote,
        rules,
        &PlanOptions {
            delete: settings.safety.delete,
            allow_empty_source: settings.safety.allow_empty_source,
            max_delete: settings.safety.max_delete,
            compare,
            server_copy,
            scope: scope.clone(),
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

fn run_doctor(settings: config::ResolvedDoctor, record: Option<&Path>) -> Result<ExitCode> {
    let cancellation = attach_progress_record(
        install_cancellation_handler()?,
        record,
        synology_drive_sync::PROGRESS_OPERATION_DOCTOR,
    );
    let timed = run_doctor_job(&settings, &cancellation, true)?;
    write_doctor_output(&timed.result, timed.elapsed, &settings.output)?;
    if cancellation.is_cancelled() || timed.result.cancelled || timed.result.write_probe_cancelled {
        Err(Error::Cancelled)
    } else if let Some(error) = timed.result.failure {
        Err(Error::Message(format!(
            "target diagnostic failed; inspect the section breakdown: {error}"
        )))
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
        Ok(result) if !result.failed() => log_event(
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
    level: cli::DoctorLevel,
    sections: Vec<DoctorSection>,
    failure: Option<String>,
    cancelled: bool,
    authenticated: bool,
    remote_checked: bool,
    remote_exists: Option<bool>,
    remote_entries: Option<usize>,
    remote_inventory: Option<(DoctorInventoryScope, DiagnosticRemoteInventory)>,
    write_permission_scope: Option<&'static str>,
    write_permission_path: Option<String>,
    write_probe_requested: bool,
    write_probe_performed: bool,
    write_probe: Option<WriteProbeReport>,
    write_probe_error: Option<String>,
    write_probe_cancelled: bool,
    /// Requests made since the previous section closed, drained into each section as it records.
    call_log: DoctorCallLog,
    /// Unauthenticated transport measurement, taken before the client is built.
    reachability: Option<ReachabilityReport>,
    /// What sat between this client and DSM, folded over every response of the run.
    intermediary: Option<IntermediarySummary>,
    /// Every cookie the server set, followed across the whole run.
    cookies: Option<CookieLedger>,
    /// Everything DSM advertises, against everything this tool asks for.
    capabilities: Option<CapabilityEnumeration>,
    /// The same authenticated call, presented through one session channel at a time.
    channel_ablation: Option<[ChannelProbe; SESSION_CHANNEL_VARIANTS]>,
    /// What several simultaneous authenticated calls did to the session.
    concurrency: Option<ConcurrencyReport>,
    /// Which advertised File Station capabilities actually work for this account.
    capability_diagnosis: Option<CapabilityDiagnosis>,
    /// The destination path, walked one component at a time.
    path_resolution: Option<DestinationPathResolution>,
    /// Carries the progress sink, so recording a section also publishes how far the run has got.
    ///
    /// A diagnostic has no single place where a section begins -- each of the sixteen is reached
    /// by its own code path -- but it has exactly one place where a section is written down. That
    /// funnel is the honest boundary, and it makes the published step mean "this much is
    /// finished", which is what an operator watching a diagnostic is actually waiting to learn.
    progress: CancellationToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DoctorInventoryScope {
    DirectChildren,
    VisibleSharedFolders,
}

impl DoctorInventoryScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::DirectChildren => "direct_children",
            Self::VisibleSharedFolders => "visible_shared_folders",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::DirectChildren => "direct children",
            Self::VisibleSharedFolders => "visible shared-folder roots",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DoctorSectionStatus {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl DoctorSectionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// One DSM request a diagnostic section actually made.
#[derive(Clone, Copy, Debug)]
struct DoctorCall {
    /// 1-based position in the run's request sequence, shared with the transport transcript so a
    /// cookie the ledger reports can be found in the section listing that made the request.
    sequence: u32,
    api: &'static str,
    method: &'static str,
    version: u32,
    outcome: RequestOutcome,
    dsm_code: Option<i64>,
    http_status: Option<u16>,
    session: SessionTransport,
    elapsed_ms: u64,
    /// Why the body would not deserialize, for a call whose outcome is `decode`.
    decode: Option<DecodeFault>,
}

/// Collects the DSM calls made since the previous section closed, and the run-wide transcript.
///
/// The pending list is drained by each section as it records its result, so a section reports the
/// requests it is actually responsible for instead of a single undifferentiated run-wide list.
/// The transcript is never drained: cookie permanence and the intermediary fingerprint are
/// properties of the whole run, and neither can be answered from a list a section already took.
#[derive(Clone, Debug, Default)]
struct DoctorCallLog {
    pending: Arc<Mutex<Vec<DoctorCall>>>,
    transcript: Arc<Mutex<TransportTranscript>>,
}

impl DoctorCallLog {
    /// An observer that both logs every round trip and retains the completed ones for the report.
    fn observer(&self, logger: Option<&Arc<EventLogger>>) -> RequestObserver {
        let pending = Arc::clone(&self.pending);
        let transcript = Arc::clone(&self.transcript);
        let logger = logger.cloned();
        Arc::new(move |observation| {
            if let Some(logger) = &logger {
                let _ = logger.emit(observation_event(observation));
            }
            // Only completed calls are retained. A start record describes the same request, and a
            // retry record describes a wait rather than a round trip.
            if let ApiObservation::CallCompleted(call) = observation {
                // The transcript assigns the sequence number, so the number a section prints and
                // the number the cookie ledger cites are the same number by construction.
                let Ok(mut transcript) = transcript.lock() else {
                    return;
                };
                let sequence = transcript.record_call(&call);
                drop(transcript);
                if let Ok(mut pending) = pending.lock() {
                    pending.push(DoctorCall {
                        sequence,
                        api: call.api,
                        method: call.method,
                        version: call.version,
                        outcome: call.outcome,
                        dsm_code: call.dsm_code,
                        http_status: call.http_status,
                        session: call.session,
                        elapsed_ms: call.elapsed_ms,
                        decode: call.decode,
                    });
                }
            }
        })
    }

    fn drain(&self) -> Vec<DoctorCall> {
        self.pending
            .lock()
            .map(|mut calls| std::mem::take(&mut *calls))
            .unwrap_or_default()
    }

    /// A snapshot of every response seen so far, for the cross-call transport checks.
    fn transcript(&self) -> TransportTranscript {
        self.transcript
            .lock()
            .map(|transcript| transcript.clone())
            .unwrap_or_default()
    }

    /// Fold the unauthenticated transport probe's responses into the same transcript.
    fn record_probes(&self, probes: Vec<ProbeObservation>) {
        if let Ok(mut transcript) = self.transcript.lock() {
            for probe in probes {
                transcript.record_probe(probe);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct DoctorSection {
    id: &'static str,
    label: &'static str,
    /// Position in *execution* order, which is not the display order.
    step: u8,
    status: DoctorSectionStatus,
    detail: String,
    elapsed: Duration,
    timing_scope: &'static str,
    calls: Vec<DoctorCall>,
    remediation: Option<&'static str>,
}

/// Sections whose verdict is a summary of requests other sections already own.
///
/// They are recorded after the run finishes, so the generic "not run because X failed" placeholder
/// must never be written over them: a run that failed at authentication still has a transcript
/// worth reading, and saying otherwise would discard the evidence.
const DOCTOR_DERIVED_SECTION_IDS: [&str; 3] = [
    "network_reachability",
    "intermediary_transport",
    "session_cookie_ledger",
];

/// Timing scope for a section that reaches its verdict without contacting the server.
const TIMING_SCOPE_LOCAL_ONLY: &str = "local_only";

/// Timing scope for a section summarising requests other sections made and timed.
const TIMING_SCOPE_DERIVED: &str = "derived";

impl DoctorResult {
    fn new(
        settings: &config::ResolvedDoctor,
        perform_write_probe: bool,
        call_log: DoctorCallLog,
    ) -> Self {
        let mut result = Self {
            level: settings.level,
            sections: DOCTOR_SECTION_SPECS
                .iter()
                .map(|&(id, label, step)| DoctorSection {
                    id,
                    label,
                    step,
                    status: DoctorSectionStatus::Skip,
                    detail: "not reached".to_owned(),
                    elapsed: Duration::ZERO,
                    timing_scope: "section",
                    calls: Vec::new(),
                    remediation: None,
                })
                .collect(),
            call_log,
            failure: None,
            cancelled: false,
            authenticated: false,
            remote_checked: false,
            remote_exists: None,
            remote_entries: None,
            remote_inventory: None,
            write_permission_scope: None,
            write_permission_path: None,
            write_probe_requested: settings.write_test,
            write_probe_performed: false,
            progress: CancellationToken::default(),
            write_probe: None,
            write_probe_error: None,
            write_probe_cancelled: false,
            reachability: None,
            intermediary: None,
            cookies: None,
            capabilities: None,
            channel_ablation: None,
            concurrency: None,
            capability_diagnosis: None,
            path_resolution: None,
        };
        // The fan-out probe multiplies request load against a live NAS, so it stays behind the
        // level an operator chooses when they have already decided to pay for depth.
        if settings.level != cli::DoctorLevel::Extensive {
            result.set_section(
                "session_concurrency",
                DoctorSectionStatus::Skip,
                "the concurrent fan-out probe runs at the extensive level only",
                Duration::ZERO,
                "section",
            );
        }
        if settings.level == cli::DoctorLevel::Quick {
            result.set_section(
                "dsm_session_auth",
                DoctorSectionStatus::Skip,
                "quick level is deliberately unauthenticated",
                Duration::ZERO,
                "section",
            );
            result.set_section(
                "session_channel_ablation",
                DoctorSectionStatus::Skip,
                "quick level is deliberately unauthenticated; there is no session to present",
                Duration::ZERO,
                "section",
            );
            result.set_section(
                "capability_diagnosis",
                DoctorSectionStatus::Skip,
                "quick level is deliberately unauthenticated; capabilities were enumerated but not exercised",
                Duration::ZERO,
                "section",
            );
            result.set_section(
                "destination_path_resolution",
                DoctorSectionStatus::Skip,
                "quick level does not inspect a destination",
                Duration::ZERO,
                "section",
            );
            result.set_section(
                "destination_permissions",
                DoctorSectionStatus::Skip,
                "quick level does not inspect a destination",
                Duration::ZERO,
                "section",
            );
            result.set_section(
                "destination_inventory",
                DoctorSectionStatus::Skip,
                "quick level does not enumerate remote entries",
                Duration::ZERO,
                "section",
            );
            result.set_section(
                "session_logout",
                DoctorSectionStatus::Skip,
                "no DSM session was created",
                Duration::ZERO,
                "section",
            );
        }
        let probe_detail = if !settings.write_test {
            "not requested; remote mutation requires the separate --write-test opt-in"
        } else if !perform_write_probe {
            "preflight only; mutation was deliberately disabled"
        } else {
            "not reached"
        };
        result.set_section(
            "disposable_write_verify_cleanup",
            DoctorSectionStatus::Skip,
            probe_detail,
            Duration::ZERO,
            "section",
        );
        result
    }

    fn set_section(
        &mut self,
        id: &str,
        status: DoctorSectionStatus,
        detail: impl Into<String>,
        elapsed: Duration,
        timing_scope: &'static str,
    ) {
        // Drained before the section is borrowed, and unconditionally: a section that made no
        // request records an empty list, which is itself evidence.
        let calls = self.call_log.drain();
        let section = self
            .sections
            .iter_mut()
            .find(|section| section.id == id)
            .expect("Doctor section ID is fixed by the report contract");
        section.status = status;
        section.detail = bounded_doctor_detail(&detail.into());
        section.elapsed = elapsed;
        section.timing_scope = timing_scope;
        section.calls = calls;
        // Deliberately not in `set_derived_section`: those are recorded once the run has already
        // ended, and publishing one would walk the step count backwards at the very moment the
        // result itself becomes available.
        self.progress.phase(id);
    }

    /// Record a section that summarises requests other sections already own.
    ///
    /// Deliberately not [`Self::set_section`]: that drains the pending call log, and a summary
    /// section made no request of its own. Draining here would move another section's requests
    /// onto this one and report them twice.
    fn set_derived_section(
        &mut self,
        id: &str,
        status: DoctorSectionStatus,
        detail: impl Into<String>,
        remediation: Option<&'static str>,
    ) {
        let section = self
            .sections
            .iter_mut()
            .find(|section| section.id == id)
            .expect("Doctor section ID is fixed by the report contract");
        section.status = status;
        section.detail = bounded_doctor_detail(&detail.into());
        section.timing_scope = TIMING_SCOPE_DERIVED;
        section.remediation = remediation;
    }

    /// Correct a section's timing scope after the fact, for a failure path that shares the
    /// generic `fail_section` recording but did not perform a request.
    fn set_timing_scope(&mut self, id: &str, timing_scope: &'static str) {
        if let Some(section) = self.sections.iter_mut().find(|section| section.id == id) {
            section.timing_scope = timing_scope;
        }
    }

    fn fail_section(&mut self, id: &str, error: &Error, elapsed: Duration) {
        let detail = safe_doctor_error(error);
        let remediation = doctor_remediation(id, error);
        self.set_section(
            id,
            DoctorSectionStatus::Fail,
            detail.clone(),
            elapsed,
            "section",
        );
        if let Some(section) = self.sections.iter_mut().find(|section| section.id == id) {
            section.remediation = remediation;
        }
        self.failure.get_or_insert(detail);
        self.cancelled |= matches!(error, Error::Cancelled);
        self.explain_dependent_skips(id);
    }

    fn explain_dependent_skips(&mut self, failed_id: &str) {
        let Some(failed_index) = self
            .sections
            .iter()
            .position(|section| section.id == failed_id)
        else {
            return;
        };
        let failed_label = self.sections[failed_index].label;
        for section in self.sections.iter_mut().skip(failed_index + 1) {
            // A derived section is recorded after the run ends and still has something to say
            // about a run that failed early, so it is never written off as unreached.
            if DOCTOR_DERIVED_SECTION_IDS.contains(&section.id) {
                continue;
            }
            if section.status == DoctorSectionStatus::Skip && section.detail == "not reached" {
                section.detail =
                    bounded_doctor_detail(&format!("not run because {failed_label} failed"));
            }
        }
    }

    fn failed(&self) -> bool {
        self.failure.is_some()
            || self
                .sections
                .iter()
                .any(|section| section.status == DoctorSectionStatus::Fail)
            || self.write_probe_error.is_some()
    }

    fn section_succeeded(&self, id: &str) -> bool {
        self.sections.iter().any(|section| {
            section.id == id
                && matches!(
                    section.status,
                    DoctorSectionStatus::Pass | DoctorSectionStatus::Warn
                )
        })
    }
}

fn bounded_doctor_detail(detail: &str) -> String {
    const MAX_CHARS: usize = 512;
    let mut characters = detail.chars();
    let mut bounded = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn safe_doctor_error(error: &Error) -> String {
    match error {
        Error::InvalidUrl(_) => "reverse-proxy URL is invalid".to_owned(),
        Error::HttpsRequired => "HTTPS is required by the configured transport policy".to_owned(),
        Error::Http { operation, source } if source.is_timeout() => {
            format!("{operation} timed out")
        }
        Error::Http { operation, .. } => format!("{operation} could not reach the DSM endpoint"),
        Error::HttpBody { operation, .. } => {
            format!("{operation} returned an unreadable response body")
        }
        Error::HttpStatus {
            operation, status, ..
        } => format!("reverse proxy returned HTTP {status} during {operation}"),
        Error::InvalidResponse { operation, .. } => {
            format!("DSM returned an invalid response during {operation}")
        }
        Error::Api {
            api,
            operation,
            code,
            description,
            ..
        } => format!("{api}.{operation} failed with code {code}{description}"),
        Error::UnsupportedApiVersion {
            api,
            version,
            min,
            max,
        } => format!("{api} version {version} is required; DSM reported {min}..={max}"),
        Error::MissingApi(api) => format!("DSM API discovery did not report {api}"),
        Error::ShareNotWritable(_) => {
            "the destination shared folder is unavailable or not writable by this account"
                .to_owned()
        }
        Error::UnsafeRemotePath { reason, .. } => {
            format!("destination path is unsafe: {reason}")
        }
        Error::RemoteEscape(_) => {
            "File Station returned an entry outside the selected destination".to_owned()
        }
        Error::RemoteMountRoot { .. } => {
            "the destination is a mounted filesystem boundary and cannot be tested safely"
                .to_owned()
        }
        Error::OperationTimedOut { operation } => format!("{operation} timed out"),
        Error::Cancelled => "operation cancelled".to_owned(),
        Error::Vault { operation, reason } => {
            format!("OS credential vault {operation} failed: {reason}")
        }
        Error::FileIo { .. } => "a configured local file could not be read".to_owned(),
        Error::Configuration(_) => "diagnostic configuration was rejected".to_owned(),
        Error::Message(message)
            if message.starts_with(
                "File Station API discovery failed through the reverse proxy;",
            ) =>
        {
            "DSM API discovery failed on both entry.cgi and query.cgi routes; inspect the protected logs"
                .to_owned()
        }
        Error::Message(_) => "diagnostic operation failed; inspect the protected logs".to_owned(),
        _ => "File Station rejected the diagnostic operation".to_owned(),
    }
}

/// Next steps for the transport findings that are not failures but change what to try next.
///
/// These are the counterparts of the 106/107/119 hint in [`doctor_remediation`], reached by
/// evidence rather than by an error code: the transport sections observe the same condition from
/// the other side, before a session has had a chance to be rejected by it.
const DOCTOR_MULTIPLE_PATH_HINT: &str = "consecutive requests do not all look like they reach the same DSM host. Compare this run \
     against one made through a direct address: a LAN address, a Synology DDNS name, or the \
     [alias].direct.quickconnect.to form. If the session errors disappear there, the path is the \
     cause and no client-side change will fix it.";

/// Sockets open, and every HTTP sample is abandoned at the probe's own ceiling.
///
/// A transport finding gets a transport next step. This one used to inherit the session-affinity
/// hint below, which sent an operator hunting a session problem on the strength of a probe that
/// had simply not waited long enough for a relayed round trip.
const DOCTOR_PROBE_TIMEOUT_HINT: &str = "TCP connects succeeded and every HTTP sample was abandoned at the probe's per-sample \
     ceiling, so this is a slow path rather than a closed one, and it says nothing about the \
     session. Read the authenticated sections below first: if they succeeded, only the timing \
     figures are missing. If they failed too, raise --request-timeout, or reach the NAS by a \
     route with fewer hops than a QuickConnect relay.";

/// The probe stopped itself, so its silence is about the budget rather than about the path.
///
/// Deliberately not one of the two hints above: both of those name a fault on the path, and
/// naming one here would send the reader after a problem this evidence does not support. The
/// probe running out of time says nothing about whether DSM answers -- the authenticated sections
/// below made real requests and are the better witness.
const DOCTOR_PROBE_BUDGET_HINT: &str = "the probe ran out of its own time budget before taking the samples it wanted, so this is a \
     statement about the diagnostic rather than about the path. Read the authenticated sections \
     below: they made real requests, and if they succeeded then only the timing detail is \
     missing. Slow but successful TCP connects are the usual cause, and they are worth a look on \
     their own.";

/// Sockets open and the HTTP request is refused, reset, or terminated before any response.
const DOCTOR_PROBE_TRANSPORT_HINT: &str = "TCP connects succeeded but no HTTP request completed, which points at the layer between the \
     socket and DSM rather than at the session: TLS termination, a reverse proxy that does not \
     forward /webapi/*, or an interception that closes the connection. The failure reason printed \
     with this finding names the layer that refused.";

const DOCTOR_RELAY_HINT: &str = "a QuickConnect relay carries this run. The relay offers no session-affinity guarantee, so a \
     DSM session accepted on one request can be presented to a different backend on the next. \
     Re-run against [alias].direct.quickconnect.to, a Synology DDNS name, or the LAN address to \
     establish whether the relay is what breaks the session.";

/// The ablation finding that names a client-side fix, which is the only kind we can apply.
const DOCTOR_COOKIE_CHANNEL_HINT: &str = "DSM accepted this session when it was presented only as the documented _sid request field, \
     and rejected it once the synthesised Cookie: id=<sid> header was attached as well. Logging \
     in with format=sid is defined as \"cookie will not be set\", so DSM never issued that cookie \
     and is being asked to resolve a cookie session it does not have. Stop sending the cookie \
     header for sid-format logins.";

/// Only the full combination is accepted, and no separate login showed a way out of it.
///
/// Deliberately does *not* advise dropping the cookie. On a DSM answering this way the cookie is
/// half of what makes the session work, and removing it is the one change guaranteed to break a
/// setup that is currently functioning.
const DOCTOR_COMBINED_CHANNEL_HINT: &str = "this DSM accepts the session only as the `id` cookie and the X-SYNO-TOKEN header together, \
     which is how its own web UI authenticates, and refuses the _sid/SynoToken request-parameter \
     path its published guide describes. Keep sending both channels: dropping either one is what \
     would break this setup. It also means a later 106/107/119 is not a channel this client picks \
     wrongly -- read it as a session that has genuinely gone away, and look at the relay and \
     intermediary findings below for why.";

/// The tokenless login was accepted through the documented field, which is a real finding.
const DOCTOR_TOKENLESS_LOGIN_HINT: &str = "a login made *without* enable_syno_token is accepted through the documented _sid request \
     field alone, while this run's own token-bound session is not. That is a guide-conformant \
     configuration this DSM does support: the parameter path is refused only for sessions created \
     the browser-style way. Nothing needs changing while the current run works, but if session \
     rejections persist across a relay, a client logging in without enable_syno_token can carry \
     its session in the request field alone and stop depending on cookie handling entirely.";

/// The ablation finding that names no client-side fix, and says so.
const DOCTOR_SESSION_DEAD_HINT: &str = "every session channel was rejected, so the session identifier itself is no longer valid \
     server-side rather than being mis-carried by one channel. That is consistent with a relay \
     that re-establishes its tunnel between requests, or with a concurrent login on the same \
     account. Connect directly, or through a single reverse-proxy origin, to establish which.";

/// A required API DSM offers only at versions this tool cannot use.
const DOCTOR_CAPABILITY_VERSION_HINT: &str = "this DSM does not offer an API version this tool requires. The capability enumeration block \
     below names the required version and the range DSM advertised for each one. Updating DSM is \
     the usual fix; where the API is optional, the feature that needs it is what stops working.";

/// Discovery said an API exists and the call for it said otherwise.
const DOCTOR_CAPABILITY_ROUTING_HINT: &str = "discovery advertised an API that the request for it answered with \"the requested API does \
     not exist\". Discovery and the call went to the same origin, so this is a reverse-proxy path \
     problem rather than a DSM one: the proxy is not forwarding that API's CGI path.";

/// The destination's first component -- the shared folder -- is not there.
const DOCTOR_MISSING_SHARE_HINT: &str = "the shared folder named by the first path component does not exist or is not visible to this \
     account. Run doctor without a destination to list the shared-folder roots this account can \
     actually see, then correct the remote path or grant access in DSM Control Panel.";

/// Only later components of the destination are missing, which sync itself can fix.
const DOCTOR_MISSING_COMPONENT_HINT: &str = "the destination's parent exists and is a directory; only components below it are missing. \
     Create them, or let the first sync create them, once the write-permission check passes.";

const DOCTOR_COOKIE_ROTATION_HINT: &str = "the server re-issued a session cookie on a call that succeeded, and this client keeps no \
     cookie jar, so the new value was discarded and the previous one was sent again. If the calls \
     after it fail with 106/107/119, this is the cause rather than the path. Log in with \
     format=cookie and honour the cookie that comes back, or with format=sid and send _sid as a \
     request parameter, but not the present mix of the two.";

/// A concrete next step for a failed diagnostic section, when one can be named.
///
/// The transport hints are the constants the API layer already uses for the same conditions, so
/// the two cannot drift into saying different things about one status code. Returning `None` is
/// the honest answer whenever the failure does not imply a specific action.
fn doctor_remediation(section_id: &str, error: &Error) -> Option<&'static str> {
    if matches!(error, Error::Cancelled) {
        return None;
    }
    if let Some(code) = error.api_code() {
        return match code {
            106 | 107 | 119 => Some(
                "the DSM session was rejected after it had been accepted. Check that every \
                 request reaches the same DSM host: a QuickConnect relay or a load balancer \
                 without session affinity will send consecutive requests to different backends. \
                 Connecting directly, or through a single reverse-proxy origin, rules this out.",
            ),
            150 => Some(
                "DSM saw a different source IP than the one that logged in. Fix the reverse \
                 proxy's X-Real-IP/X-Forwarded-For handling, or disable DSM's IP-checking for \
                 this account.",
            ),
            105 => Some(
                "the authenticated DSM account does not have File Station permission on this \
                 shared folder. Grant it in DSM Control Panel, then rerun.",
            ),
            407 => Some(
                "File Station refused the operation. Check the shared folder's permissions and \
                 whether it is mounted read-only.",
            ),
            408 if section_id == "destination_permissions" => Some(
                "neither the destination nor any ancestor of it exists. Create the shared folder \
                 first, or correct the remote path.",
            ),
            411 => Some("the remote filesystem is mounted read-only."),
            415 | 416 => Some("the destination is out of quota or out of space."),
            _ => None,
        };
    }
    match error {
        Error::HttpStatus { status, .. } => match status.as_u16() {
            301 | 302 | 303 | 307 | 308 => Some(api::REDIRECT_REFUSED_HINT),
            413 => Some(api::BODY_TOO_LARGE_HINT),
            502 => Some(api::BAD_GATEWAY_HINT),
            504 => Some(api::GATEWAY_TIMEOUT_HINT),
            _ => None,
        },
        _ => None,
    }
}

/// Report whether an error proves the DSM session itself is no longer usable.
///
/// These are the codes for which every later authenticated request is guaranteed to fail, so a
/// diagnostic must stop rather than issue a request that cannot succeed. A permission refusal
/// (105, 407) is deliberately excluded: the session is still valid, and what the account *can*
/// read remains worth reporting.
fn session_is_unusable(error: &Error) -> bool {
    matches!(error, Error::Cancelled)
        || matches!(
            error.api_code(),
            // 106 session timeout, 107 duplicate-login interruption, 119 invalid session.
            Some(106 | 107 | 119)
        )
}

fn doctor_requires_content_fingerprint(level: cli::DoctorLevel, compare: cli::CompareArg) -> bool {
    level != cli::DoctorLevel::Quick
        && (compare == cli::CompareArg::Content || level == cli::DoctorLevel::Extensive)
}

fn doctor_requires_delete_capability(level: cli::DoctorLevel, delete: bool) -> bool {
    level != cli::DoctorLevel::Quick && (delete || level == cli::DoctorLevel::Extensive)
}

fn record_doctor_logout(client: &mut ApiClient, result: &mut DoctorResult) {
    let logout_started = Instant::now();
    match client.logout() {
        Ok(()) => result.set_section(
            "session_logout",
            DoctorSectionStatus::Pass,
            if result.authenticated {
                "the authenticated DSM session was closed"
            } else {
                "the unconfirmed DSM session was closed after File Station session confirmation failed"
            },
            logout_started.elapsed(),
            "section",
        ),
        Err(error) => {
            result.fail_section("session_logout", &error, logout_started.elapsed());
            if result.write_probe_performed {
                append_doctor_failure_context(
                    result,
                    "File Station logout also failed",
                    &safe_doctor_error(&error),
                );
            }
        }
    }
}

/// How much unauthenticated probing each diagnostic level pays for.
///
/// Quick is the level an operator reaches for when something is already wrong, so it still
/// measures -- just with fewer samples. Extensive buys enough samples that a bimodal connect time
/// is unmistakable rather than merely suggestive.
fn reachability_budget(level: cli::DoctorLevel) -> ReachabilityBudget {
    match level {
        cli::DoctorLevel::Quick => ReachabilityBudget::quick(),
        cli::DoctorLevel::Standard => ReachabilityBudget::standard(),
        cli::DoctorLevel::Extensive => ReachabilityBudget::extensive(),
    }
}

/// Run the diagnostic, bracketed by the transport checks that span the whole of it.
///
/// The reachability probe runs first and outside the client, because a latency figure taken
/// through an already-open pooled connection measures nothing. The intermediary fingerprint and
/// the cookie ledger run last, over the transcript every request fed, because both are properties
/// of the run rather than of any one section -- and because DSM's logout response sets a cookie
/// too, which a summary taken before logout would miss.
fn doctor_checks(
    settings: &config::ResolvedDoctor,
    logger: Option<Arc<EventLogger>>,
    cancellation: &CancellationToken,
    perform_write_probe: bool,
) -> Result<DoctorResult> {
    let call_log = DoctorCallLog::default();
    let reachability_started = Instant::now();
    // Held rather than passed inline: the section reports what the probe was asked for as well as
    // what it got, so a run that stopped short can say by how much.
    let budget = reachability_budget(settings.level);
    let (reachability, probes) = measure_reachability(
        &client_options(&settings.url, &settings.network),
        budget,
        cancellation,
    );
    let reachability_elapsed = reachability_started.elapsed();
    // Recorded before the run so the probe responses keep the sequence numbers they earned: they
    // happened first, and a ledger that renumbered them would misreport when a cookie first
    // appeared.
    call_log.record_probes(probes);

    let mut result = doctor_run(
        settings,
        logger,
        cancellation,
        perform_write_probe,
        call_log,
    )?;
    record_reachability_section(&mut result, reachability, budget, reachability_elapsed);
    record_transport_summary_sections(&mut result, &settings.url);
    Ok(result)
}

/// Record the unauthenticated reachability measurement.
///
/// This section never fails the run. It measures; `routing_tls` is what gates. A probe that could
/// not open a socket against a host the client then reached successfully is a transient artefact,
/// and turning that into a failed diagnostic would be worse than useless.
fn record_reachability_section(
    result: &mut DoctorResult,
    reachability: ReachabilityReport,
    budget: ReachabilityBudget,
    elapsed: Duration,
) {
    // The hint is chosen by the branch that was actually taken, not by a condition tested
    // alongside it. A relay hostname resolving to several addresses makes `suggests_multiple_paths`
    // true on almost every QuickConnect run, so gating the session-affinity hint on that alone
    // attached it to findings it does not explain -- including "no HTTP sample completed", where
    // it sent the reader after a session problem that the evidence never pointed at.
    let (status, detail, remediation) = if reachability.cancelled {
        (
            DoctorSectionStatus::Skip,
            "the transport probe stopped when the run was cancelled".to_owned(),
            None,
        )
    } else if !reachability.reached() {
        (
            DoctorSectionStatus::Warn,
            format!(
                "no TCP connection to {}:{} completed{}",
                reachability.host,
                reachability.port,
                reachability
                    .tcp_failure_reason
                    .as_deref()
                    .or(reachability.dns.error.as_deref())
                    .map(|reason| format!("; {reason}"))
                    .unwrap_or_default(),
            ),
            None,
        )
    } else if reachability.connects_but_does_not_answer() {
        let timed_out = reachability.http.timed_out;
        (
            DoctorSectionStatus::Warn,
            format!(
                "TCP connections to {}:{} succeed, but no HTTP sample completed{}{}",
                reachability.host,
                reachability.port,
                if timed_out {
                    reachability
                        .http
                        .request_timeout
                        .map(|timeout| {
                            format!(" within the {:.1} s probe ceiling", timeout.as_secs_f64())
                        })
                        .unwrap_or_default()
                } else {
                    String::new()
                },
                reachability
                    .http
                    .failure_reason
                    .as_deref()
                    .map(|reason| format!("; {reason}"))
                    .unwrap_or_default(),
            ),
            Some(if timed_out {
                DOCTOR_PROBE_TIMEOUT_HINT
            } else {
                DOCTOR_PROBE_TRANSPORT_HINT
            }),
        )
    } else if reachability.budget_exhausted {
        // Placed below the two findings above and above the consistent verdict, deliberately.
        // Those two are real conclusions about the path and stay reportable on a probe that ran
        // short. "Consistent" is not: the samples that would have contradicted it are exactly the
        // ones the ceiling prevented, so a probe that stopped early must not be allowed to pass
        // itself off as one that looked and found nothing wrong. That verdict is reachable with
        // `http.failures == 0`, which `connects_but_does_not_answer` excludes, so before this
        // branch existed a probe that took no HTTP sample at all reported PASS while the
        // transport block underneath said it had stopped early.
        (
            DoctorSectionStatus::Warn,
            format!(
                "the probe stopped at its own {:.0} s ceiling after {} of {} HTTP sample(s), so \
                 this run did not measure the path as fully as the {} level asks for",
                budget.total.as_secs_f64(),
                reachability.http.first_byte.len(),
                budget.http_samples,
                result.level.as_str(),
            ),
            Some(DOCTOR_PROBE_BUDGET_HINT),
        )
    } else if reachability.suggests_multiple_paths() {
        let mut reasons = Vec::new();
        if reachability.dns.address_count > 1 {
            reasons.push(format!(
                "the hostname resolves to {} addresses",
                reachability.dns.address_count
            ));
        }
        if reachability.tcp_connect.is_widely_spread() {
            reasons.push("TCP connect time varies more than a single path should".to_owned());
        }
        if reachability.http.first_byte.is_widely_spread() {
            reasons.push("first-byte time varies more than a single path should".to_owned());
        }
        (
            DoctorSectionStatus::Warn,
            format!(
                "the endpoint responded, but consecutive connections do not look like they reach \
                 one host: {}",
                reasons.join("; ")
            ),
            Some(DOCTOR_MULTIPLE_PATH_HINT),
        )
    } else {
        (
            DoctorSectionStatus::Pass,
            format!(
                "TCP reachability is consistent: connect {}",
                reachability.tcp_connect.describe()
            ),
            None,
        )
    };
    result.set_derived_section("network_reachability", status, detail, remediation);
    if let Some(section) = result
        .sections
        .iter_mut()
        .find(|section| section.id == "network_reachability")
    {
        // A real elapsed time, not a derived scope: this section did its own measuring.
        section.elapsed = elapsed;
        section.timing_scope = "section";
    }
    result.reachability = Some(reachability);
}

/// Record the two sections that summarise the run's whole transcript.
fn record_transport_summary_sections(result: &mut DoctorResult, url: &str) {
    let transcript = result.call_log.transcript();
    let endpoint = classify_endpoint(&endpoint_host(url));
    let relayed = endpoint.form.is_relayed();
    let intermediary = transcript.intermediary_summary(endpoint);
    let cookies = transcript.cookie_ledger();

    let (status, detail, remediation) = if intermediary.responses_observed == 0 {
        (
            DoctorSectionStatus::Skip,
            "no response reached this client, so nothing on the path could be fingerprinted"
                .to_owned(),
            None,
        )
    } else if intermediary.distinct_server_banners() > 1 {
        (
            DoctorSectionStatus::Warn,
            format!(
                "responses came back under {} different Server banners, so more than one origin \
                 answered during this run",
                intermediary.distinct_server_banners()
            ),
            Some(DOCTOR_MULTIPLE_PATH_HINT),
        )
    } else if relayed {
        (
            DoctorSectionStatus::Warn,
            "this run goes through a QuickConnect relay, so Synology relay infrastructure is in \
             the data path and nothing in the hostname pins consecutive requests to one DSM host"
                .to_owned(),
            Some(DOCTOR_RELAY_HINT),
        )
    } else if !intermediary.foreign_cookie_names.is_empty() {
        (
            DoctorSectionStatus::Warn,
            format!(
                "an intermediary set {} cookie(s) under names DSM does not use",
                intermediary.foreign_cookie_names.len()
            ),
            None,
        )
    } else if intermediary.intermediary_detected() {
        (
            DoctorSectionStatus::Warn,
            "a proxy or cache announced itself in front of DSM".to_owned(),
            None,
        )
    } else {
        (
            DoctorSectionStatus::Pass,
            "no proxy, relay, or cache announced itself on any response".to_owned(),
            None,
        )
    };
    result.set_derived_section("intermediary_transport", status, detail, remediation);

    let (status, detail, remediation) = if let Some(rotated) = cookies.rotated_on_success() {
        let at = rotated
            .rotation_on_success()
            .map(|rotation| rotation.at.describe())
            .unwrap_or_else(|| "an unrecorded call".to_owned());
        (
            DoctorSectionStatus::Warn,
            format!(
                "the server issued a NEW value for cookie {} on {}, a call it also reported as \
                 successful; this client keeps no cookie jar, so that value was discarded",
                rotated.name, at
            ),
            Some(DOCTOR_COOKIE_ROTATION_HINT),
        )
    } else if cookies.is_empty() {
        (
            DoctorSectionStatus::Pass,
            "the server set no cookies at any point in this run".to_owned(),
            None,
        )
    } else {
        (
            DoctorSectionStatus::Pass,
            format!(
                "{} cookie name(s) were set and none was re-issued under a changed value on a \
                 successful call",
                cookies.entries.len()
            ),
            None,
        )
    };
    result.set_derived_section("session_cookie_ledger", status, detail, remediation);

    result.intermediary = Some(intermediary);
    result.cookies = Some(cookies);
}

/// The host of a configured endpoint URL, for classification.
///
/// An unparseable URL yields an empty host, which classifies as an ordinary hostname and says
/// nothing. The routing section reports the parse failure properly.
fn endpoint_host(url: &str) -> String {
    api::normalize_base_url(url, true)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// How many File Station APIs are listed individually, and how many namespaces are summarised.
///
/// A DSM with a full package set advertises several hundred APIs. Printing them all helps nobody,
/// and the bound also means a server cannot make this report arbitrarily long.
const DOCTOR_NAMESPACE_LIMIT: usize = 15;
const DOCTOR_ADVERTISED_API_LIMIT: usize = 40;

/// One API DSM advertises, with the version this tool asks of it when it asks for one.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AdvertisedApi {
    /// Sanitized for display. The wire name is chosen by whoever wrote the package that
    /// advertises it, so it is never treated as trusted terminal output.
    name: String,
    min_version: Option<u32>,
    max_version: Option<u32>,
    /// The version this tool requests, when this is an API it uses.
    required: Option<u32>,
    optional: bool,
}

impl AdvertisedApi {
    /// The offered range, or an honest note that DSM did not state one.
    fn range(&self) -> String {
        match (self.min_version, self.max_version) {
            (Some(min), Some(max)) if min == max => format!("v{min}"),
            (Some(min), Some(max)) => format!("v{min}-{max}"),
            _ => "version range not advertised".to_owned(),
        }
    }
}

/// What this tool needs, measured against what DSM advertised.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RequirementVerdict {
    requirement: ApiRequirement,
    present: bool,
    offered: Option<(u32, u32)>,
    satisfied: bool,
}

impl RequirementVerdict {
    fn evaluate(requirement: ApiRequirement, catalogue: &ApiCatalogue) -> Self {
        let entry = catalogue.apis.get(requirement.api);
        let offered = entry.and_then(|api| Some((api.min_version?, api.max_version?)));
        let satisfied = entry
            .and_then(|api| api.offers_version(requirement.version))
            .unwrap_or(false);
        Self {
            requirement,
            present: entry.is_some(),
            offered,
            satisfied,
        }
    }

    /// Whether this verdict should fail the run: a required API that is absent or offered only at
    /// versions this tool cannot use.
    fn blocking(&self) -> bool {
        !self.satisfied && !self.requirement.optional
    }

    fn describe(&self) -> String {
        if self.satisfied {
            return "ok".to_owned();
        }
        if !self.present {
            return "NOT ADVERTISED".to_owned();
        }
        match self.offered {
            Some((min, max)) => format!("INCOMPATIBLE (offered v{min}-{max})"),
            None => "INCOMPATIBLE (no version range advertised)".to_owned(),
        }
    }
}

/// The three-tier view of what DSM offers, computed once and rendered twice.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CapabilityEnumeration {
    total: usize,
    unusable_entries: usize,
    file_station: Vec<AdvertisedApi>,
    file_station_truncated: usize,
    requirements: Vec<RequirementVerdict>,
    /// Every namespace outside `SYNO.FileStation`, largest first.
    namespaces: Vec<(String, usize)>,
    /// APIs and namespaces beyond [`DOCTOR_NAMESPACE_LIMIT`], counted rather than listed.
    namespace_overflow: Option<(usize, usize)>,
}

impl CapabilityEnumeration {
    /// Fold DSM's advertised map into the three tiers the report prints.
    fn from_catalogue(catalogue: &ApiCatalogue) -> Self {
        let required: BTreeMap<&str, ApiRequirement> = API_REQUIREMENTS
            .iter()
            .map(|requirement| (requirement.api, *requirement))
            .collect();
        let mut file_station = Vec::new();
        let mut namespace_counts: BTreeMap<String, usize> = BTreeMap::new();
        for (name, api) in &catalogue.apis {
            if name.starts_with("SYNO.FileStation.") {
                let requirement = required.get(name.as_str());
                file_station.push(AdvertisedApi {
                    name: BoundedText::sanitized(name).as_str().to_owned(),
                    min_version: api.min_version,
                    max_version: api.max_version,
                    required: requirement.map(|requirement| requirement.version),
                    optional: requirement.is_some_and(|requirement| requirement.optional),
                });
                continue;
            }
            *namespace_counts.entry(namespace_of(name)).or_default() += 1;
        }
        let file_station_truncated = file_station
            .len()
            .saturating_sub(DOCTOR_ADVERTISED_API_LIMIT);
        file_station.truncate(DOCTOR_ADVERTISED_API_LIMIT);

        let mut namespaces = namespace_counts.into_iter().collect::<Vec<_>>();
        // Largest first, then alphabetically, so the order is stable across runs.
        namespaces.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let namespace_overflow = (namespaces.len() > DOCTOR_NAMESPACE_LIMIT).then(|| {
            let tail = &namespaces[DOCTOR_NAMESPACE_LIMIT..];
            (tail.iter().map(|(_, count)| count).sum(), tail.len())
        });
        namespaces.truncate(DOCTOR_NAMESPACE_LIMIT);

        Self {
            total: catalogue.apis.len(),
            unusable_entries: catalogue.unusable_entries,
            file_station,
            file_station_truncated,
            requirements: API_REQUIREMENTS
                .iter()
                .map(|requirement| RequirementVerdict::evaluate(*requirement, catalogue))
                .collect(),
            namespaces,
            namespace_overflow,
        }
    }

    fn blocking_requirements(&self) -> Vec<&RequirementVerdict> {
        self.requirements
            .iter()
            .filter(|verdict| verdict.blocking())
            .collect()
    }

    fn unsatisfied_optional(&self) -> usize {
        self.requirements
            .iter()
            .filter(|verdict| !verdict.satisfied && verdict.requirement.optional)
            .count()
    }
}

/// The namespace an API name belongs to, as `SYNO.Core.*`.
///
/// Two components, because that is the level at which Synology's own naming separates products.
/// A name with fewer components is its own namespace rather than being forced into a bucket.
fn namespace_of(name: &str) -> String {
    let sanitized = BoundedText::sanitized(name);
    let sanitized = sanitized.as_str();
    let mut components = sanitized.split('.');
    match (components.next(), components.next()) {
        (Some(first), Some(second)) => format!("{first}.{second}.*"),
        _ => sanitized.to_owned(),
    }
}

/// What several simultaneous authenticated calls did to one DSM session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ConcurrencyReport {
    parallel: usize,
    succeeded: usize,
    /// How many concurrent calls were answered with 106, 107, or 119.
    session_rejected: usize,
    other_failures: usize,
    /// Whether one ordinary sequential call still worked after the burst.
    follow_up_succeeded: bool,
    follow_up_session_rejected: bool,
    elapsed_ms: u64,
}

/// What one probed capability turned out to be, for this account, through this path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapabilityVerdict {
    Works,
    /// DSM 102: discovery advertised it, the call said the API does not exist.
    NotRoutable,
    /// DSM 103: the API exists at a version without this method.
    MethodUnavailable,
    /// DSM 104: contradicts the discovery map outright.
    VersionUnsupported,
    /// DSM 105 or 407: the API works, this account may not use it here.
    NoPermission,
    /// The session died before this probe could run, or during it.
    NotProbed,
    /// Anything else: a transport failure, an HTTP status, an undecodable body.
    Failed,
}

impl CapabilityVerdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Works => "works",
            Self::NotRoutable => "advertised but not routable",
            Self::MethodUnavailable => "method unavailable",
            Self::VersionUnsupported => "version unsupported",
            Self::NoPermission => "no permission for this account",
            Self::NotProbed => "not probed",
            Self::Failed => "failed",
        }
    }

    /// Classify one probe's answer. Session codes are deliberately absent here: the caller stops
    /// probing on them, so they can never reach this function as a capability verdict.
    fn classify(probe: CapabilityProbe) -> Self {
        if probe.outcome == RequestOutcome::Ok {
            return Self::Works;
        }
        match probe.dsm_code {
            // 408 is File Station answering the question asked: the path is not there. That is a
            // fact about the path, which the resolution section reports, not about the API.
            Some(408) => Self::Works,
            Some(102) => Self::NotRoutable,
            Some(103) => Self::MethodUnavailable,
            Some(104) => Self::VersionUnsupported,
            Some(105 | 407) => Self::NoPermission,
            Some(106 | 107 | 119) => Self::NotProbed,
            _ => Self::Failed,
        }
    }
}

/// One probed capability and its verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CapabilityRecord {
    api: &'static str,
    method: &'static str,
    version: u32,
    verdict: CapabilityVerdict,
    dsm_code: Option<i64>,
    elapsed_ms: u64,
    /// Whether this tool needs the capability to work, or merely reports on it.
    required: bool,
}

/// Which advertised File Station capabilities actually work for this account.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CapabilityDiagnosis {
    records: Vec<CapabilityRecord>,
    info: Option<FileStationInfo>,
    /// DSM's own name for the host, read at the start and again at the end of the section.
    first_hostname: Option<BoundedText>,
    last_hostname: Option<BoundedText>,
    /// True only when both reads succeeded and disagreed. That is the one positive observation
    /// that proves consecutive requests reached different hosts.
    hostname_changed: bool,
    /// Set when a session rejection stopped the remaining probes.
    session_aborted: bool,
    /// Advertised capabilities this diagnostic deliberately does not exercise.
    unprobed: Vec<&'static str>,
}

impl CapabilityDiagnosis {
    fn working(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.verdict == CapabilityVerdict::Works)
            .count()
    }

    fn probed(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.verdict != CapabilityVerdict::NotProbed)
            .count()
    }

    /// A required capability that is advertised and demonstrably non-functional. This is the only
    /// condition that fails the section: a broken optional API must not fail a sync diagnostic.
    fn broken_required(&self) -> usize {
        self.records
            .iter()
            .filter(|record| {
                record.required
                    && !matches!(
                        record.verdict,
                        CapabilityVerdict::Works | CapabilityVerdict::NotProbed
                    )
            })
            .count()
    }
}

/// Capabilities DSM advertises that this diagnostic will not exercise, and why not.
///
/// Stated in the report rather than left to inference: a matrix that silently omitted these would
/// imply they had been verified.
const DOCTOR_UNPROBED_CAPABILITIES: &[&str] = &[
    "CreateFolder, Rename, Delete, CopyMove, Upload, Extract, Compress (mutating; the --write-test \
     probe is where the ones sync uses get proven)",
    "DirSize, MD5, Search (their start methods spawn background tasks that can walk a whole share)",
    "Thumb, Download (need a real user file, which a diagnostic has no business choosing)",
    "List.getinfo (the destination path resolution already exercises it against a real path)",
    "Sharing (its list returns existing share links, which are credentials by URL)",
];

/// Read everything DSM advertises, and measure it against everything this tool asks for.
///
/// This never changes what the client requires. Discovery keeps its ten-entry allowlist and its
/// strict wire type, because that map feeds every call the client makes; this is a separate,
/// lenient read whose worst outcome is a warning on a diagnostic section.
fn record_capability_enumeration(
    result: &mut DoctorResult,
    client: &ApiClient,
    cancellation: &CancellationToken,
) {
    let started = Instant::now();
    if let Err(error) = cancellation.check() {
        result.fail_section("capability_enumeration", &error, started.elapsed());
        return;
    }
    let catalogue = match client.enumerate_all_apis() {
        Ok(catalogue) => catalogue,
        Err(error) => {
            // Never a failure. Discovery already validated the APIs this run depends on, so all
            // that is lost here is the wider picture.
            result.set_section(
                "capability_enumeration",
                DoctorSectionStatus::Warn,
                format!(
                    "DSM did not answer the full capability query, so only the APIs this tool \
                     requires are known: {}",
                    safe_doctor_error(&error)
                ),
                started.elapsed(),
                "section",
            );
            return;
        }
    };
    let enumeration = CapabilityEnumeration::from_catalogue(&catalogue);
    let (status, detail, remediation) = capability_enumeration_verdict(&enumeration);
    record_diagnostic_section(
        result,
        "capability_enumeration",
        status,
        detail,
        started.elapsed(),
        remediation,
    );
    result.capabilities = Some(enumeration);
}

/// Reach a verdict on what DSM advertises against what this tool asks for.
fn capability_enumeration_verdict(
    enumeration: &CapabilityEnumeration,
) -> (DoctorSectionStatus, String, Option<&'static str>) {
    let blocking = enumeration.blocking_requirements();
    if !blocking.is_empty() {
        let names = blocking
            .iter()
            .map(|verdict| verdict.requirement.api)
            .collect::<Vec<_>>()
            .join(", ");
        (
            DoctorSectionStatus::Fail,
            format!(
                "DSM advertises {} APIs; {} required API(s) are missing or version-incompatible: {}",
                enumeration.total,
                blocking.len(),
                names
            ),
            Some(DOCTOR_CAPABILITY_VERSION_HINT),
        )
    } else if enumeration.unusable_entries > 0 || enumeration.unsatisfied_optional() > 0 {
        (
            DoctorSectionStatus::Warn,
            format!(
                "DSM advertises {} APIs; all required APIs are present with compatible versions, \
                 but {} optional API(s) are unavailable and {} advertised entries could not be \
                 read",
                enumeration.total,
                enumeration.unsatisfied_optional(),
                enumeration.unusable_entries
            ),
            None,
        )
    } else {
        (
            DoctorSectionStatus::Pass,
            format!(
                "DSM advertises {} APIs; all {} APIs this tool uses are present with compatible \
                 versions",
                enumeration.total,
                enumeration.requirements.len()
            ),
            None,
        )
    }
}

/// Record one diagnostic section that reaches a verdict without aborting the run.
///
/// Deliberately not [`DoctorResult::fail_section`]: these sections can legitimately fail --
/// proving the session is mis-carried is the whole job of one of them -- and the run continues
/// afterwards. Writing "not run because X failed" over the sections that follow would be false,
/// because they do run and do record their own results.
fn record_diagnostic_section(
    result: &mut DoctorResult,
    id: &'static str,
    status: DoctorSectionStatus,
    detail: String,
    elapsed: Duration,
    remediation: Option<&'static str>,
) {
    result.set_section(id, status, detail.clone(), elapsed, "section");
    if let Some(section) = result.sections.iter_mut().find(|section| section.id == id) {
        section.remediation = remediation;
    }
    if status == DoctorSectionStatus::Fail {
        result.failure.get_or_insert(detail);
    }
}

/// Present the same authenticated call through one session channel at a time.
///
/// The variants run whatever they find: a `119` here is the observation, not an error, so this
/// deliberately does not route through [`session_is_unusable`]. Aborting on the first rejection is
/// exactly what would make the probe useless.
///
/// The fifth variant -- the one that varies how the session was *created* rather than how it is
/// presented -- is not run here. It needs a second login, and this section runs at step 7 with
/// nine sections still to come on the primary session; [`record_tokenless_login_variant`] takes
/// it at the end of the run instead, and rewrites this section's verdict with its answer. Until
/// then the slot records why it has not run, which is not the same thing as a rejection.
fn record_session_channel_ablation(
    result: &mut DoctorResult,
    client: &ApiClient,
    tokenless: &TokenlessProbeLogin,
) {
    let started = Instant::now();
    let probes = match client.probe_session_channels(tokenless.deferral_reason()) {
        Ok(probes) => probes,
        Err(error) => {
            result.set_section(
                "session_channel_ablation",
                DoctorSectionStatus::Skip,
                format!(
                    "the ablation probe could not be issued: {}",
                    safe_doctor_error(&error)
                ),
                started.elapsed(),
                "section",
            );
            return;
        }
    };
    let (status, detail, remediation) = ablation_verdict(&probes);
    record_diagnostic_section(
        result,
        "session_channel_ablation",
        status,
        detail,
        started.elapsed(),
        remediation,
    );
    result.channel_ablation = Some(probes);
}

/// The section this verdict belongs to, named once because two functions now write it.
const ABLATION_SECTION: &str = "session_channel_ablation";

/// Run the deferred tokenless-login variant and fold its answer into the ablation verdict.
///
/// Called after [`record_doctor_logout`], which is the whole point. The variant logs in a second
/// time, and both this client's logins and its logouts name the same DSM session -- `session=
/// FileStation` in `login` and in `logout` alike. DSM binds one session per (account, session
/// name) and has a dedicated error for a collision: `107`, "session interrupted by duplicate
/// login", which [`session_is_unusable`] treats as fatal. A second login taken at step 7 could
/// therefore invalidate the run's own session server-side and abort steps 8 through 16, and the
/// second session's logout could tear down the shared name outright.
///
/// Running it once the primary session is already closed removes both hazards by construction
/// rather than by hoping DSM is lenient: there is no session left to interrupt, and no shared name
/// left to tear down. Nothing in the run reads the primary session after this point.
fn record_tokenless_login_variant(
    result: &mut DoctorResult,
    client: &ApiClient,
    login: &TokenlessProbeLogin,
    cancellation: &CancellationToken,
) {
    let Some(probes) = result.channel_ablation.as_ref() else {
        // The ablation did not run, so there is no verdict to refine and no reason to log in.
        return;
    };
    let slot = probes.len() - 1;
    if probes[slot].ran() {
        return;
    }
    let started = Instant::now();
    let probe = if cancellation.is_cancelled() {
        ChannelProbe::skipped(
            api::SessionChannels::SidFieldOnlyTokenlessLogin,
            "the run was cancelled before the deferred variant could be issued",
        )
    } else {
        run_tokenless_login_variant(client, login)
    };
    let Some(probes) = result.channel_ablation.as_mut() else {
        return;
    };
    probes[slot] = probe;
    let probes = *probes;

    let (status, detail, remediation) = ablation_verdict(&probes);
    // Written into the section directly rather than through `set_section`, which would replace
    // the four variants' own calls with these. The requests this variant made are appended to
    // them instead, keeping their real -- and visibly out of order -- sequence numbers.
    let calls = result.call_log.drain();
    let elapsed = started.elapsed();
    if let Some(section) = result
        .sections
        .iter_mut()
        .find(|section| section.id == ABLATION_SECTION)
    {
        section.status = status;
        section.detail = bounded_doctor_detail(&detail);
        section.remediation = remediation;
        section.elapsed = section.elapsed.saturating_add(elapsed);
        section.calls.extend(calls);
    }
    if status == DoctorSectionStatus::Fail {
        result.failure.get_or_insert(detail);
    }
}

/// What the ablation needs in order to make its second, deliberately tokenless login.
///
/// The password is held rather than a pre-made session, because the second login is deliberately
/// deferred to the end of the run -- see [`record_tokenless_login_variant`] for why. The plaintext
/// lives in a `Zeroizing` buffer, only at the extensive level, and is erased when this value is
/// dropped.
enum TokenlessProbeLogin {
    Available {
        username: String,
        password: Zeroizing<String>,
    },
    Unavailable(&'static str),
}

impl TokenlessProbeLogin {
    /// Why the variant will not run, for the ablation to record before the deferred attempt.
    fn deferral_reason(&self) -> &'static str {
        match self {
            Self::Available { .. } => {
                "deferred to the end of the run, after the primary session is closed"
            }
            Self::Unavailable(reason) => reason,
        }
    }
}

/// How long the extra ablation session is given to log itself out.
const DOCTOR_TOKENLESS_LOGOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the tokenless-login variant: a second session, one read-only call, then a logout.
///
/// The client is cloned from the primary one so no second API discovery is paid for. Cloning is
/// safe in-process: `session` is a plain field copied by value, so a login here cannot replace the
/// primary client's in-memory session, and no client this crate builds enables `cookie_store`, so
/// there is no shared jar for one session's identifier to reach the other's requests. The
/// `reqwest` connection pool, the observer, and the cancellation token are shared, none of which
/// carries session state -- and the observer being shared is what puts these requests in the
/// run's own transcript.
///
/// Only the no-OTP path is attempted: replaying the one-time code the first login consumed is
/// exactly what a read-only diagnostic must not do, and prompting an operator again for a probe is
/// worse than reporting that the variant did not run.
///
/// The second session is logged out on every path out of this function, successful or not.
fn run_tokenless_login_variant(client: &ApiClient, login: &TokenlessProbeLogin) -> ChannelProbe {
    let skipped =
        |reason| ChannelProbe::skipped(api::SessionChannels::SidFieldOnlyTokenlessLogin, reason);
    let TokenlessProbeLogin::Available { username, password } = login else {
        return skipped(login.deferral_reason());
    };
    let mut probe_client = client.clone();
    match probe_client.login_without_syno_token(username, password, None) {
        Ok(()) => {}
        // 403/406 is DSM asking for a one-time code: the account has two-factor authentication,
        // and this variant is not worth a second prompt.
        Err(error) if matches!(error.api_code(), Some(403 | 406)) => {
            return skipped(
                "a second login would need another one-time code, so it was not attempted",
            );
        }
        Err(_) => {
            return skipped("a second login without enable_syno_token could not be established");
        }
    }
    let probe = probe_client.probe_tokenless_sid_field();
    let _ = probe_client.logout_bounded(DOCTOR_TOKENLESS_LOGOUT_TIMEOUT);
    probe.unwrap_or_else(|_| skipped("the second session's probe request could not be issued"))
}

/// Reach a verdict from the ablation variants.
///
/// Only the first three decide it. `TokenHeaderOnly` carries no session identifier at all, so DSM
/// is *expected* to reject it; it is the control that proves the probe can tell acceptance from
/// rejection, and treating its rejection as a finding would be a false alarm on every healthy NAS.
/// The tokenless-login variant refines the verdict when it ran, and is silent when it did not.
fn ablation_verdict(
    probes: &[ChannelProbe; SESSION_CHANNEL_VARIANTS],
) -> (DoctorSectionStatus, String, Option<&'static str>) {
    let all = probes[0];
    let sid = probes[1];
    let cookie = probes[2];
    let control = probes[3];
    let tokenless = probes[4];
    let cookie_note = if cookie.accepted() {
        "the synthesised cookie alone was also accepted"
    } else {
        "the synthesised cookie alone was rejected, which is what a format=sid login implies"
    };
    if all.accepted() && sid.accepted() {
        (
            DoctorSectionStatus::Pass,
            format!(
                "the session is accepted both as this client normally presents it and through the \
                 documented _sid request field alone; {cookie_note}. Channel selection is not the \
                 fault here"
            ),
            None,
        )
    } else if sid.accepted() {
        (
            DoctorSectionStatus::Fail,
            format!(
                "the session is accepted with the _sid request field alone and rejected \
                 ({}) when this client's usual combination of channels is attached",
                all.dsm_code
                    .map(|code| format!("DSM {code}"))
                    .unwrap_or_else(|| all.outcome.as_str().to_owned())
            ),
            Some(DOCTOR_COOKIE_CHANNEL_HINT),
        )
    } else if all.accepted() && cookie.accepted() {
        (
            DoctorSectionStatus::Warn,
            "DSM accepted this client's usual channel combination and the synthesised cookie on \
             its own, but rejected the documented _sid request field, so this DSM is resolving \
             the session from the cookie rather than from the field its own guide specifies for \
             a format=sid login"
                .to_owned(),
            None,
        )
    } else if all.accepted() {
        // Everything except the full combination was rejected, the cookie alone included. The
        // channels are not interchangeable here: it is their *combination* that DSM accepts, and
        // saying the cookie resolves the session -- as this once did, on the strength of the
        // `_sid` rejection alone -- would point an operator at removing the cookie, which is
        // precisely the change that would break a NAS behaving this way.
        let tokenless_note = if !tokenless.ran() {
            String::new()
        } else if tokenless.accepted() {
            " -- and a second login made without enable_syno_token *was* accepted through the \
             _sid field alone, so the parameter path is refused for token-bound sessions rather \
             than by this DSM in general"
                .to_owned()
        } else {
            format!(
                " -- a second login made without enable_syno_token was rejected through the _sid \
                 field as well ({}), so the parameter path is refused regardless of how the \
                 session was created",
                tokenless
                    .dsm_code
                    .map(|code| format!("DSM {code}"))
                    .unwrap_or_else(|| tokenless.outcome.as_str().to_owned())
            )
        };
        (
            DoctorSectionStatus::Warn,
            format!(
                "only the full combination of channels was accepted: DSM took the `id` cookie and \
                 the X-SYNO-TOKEN header together, and rejected the cookie alone, the token \
                 header alone, and the documented _sid/SynoToken request fields{tokenless_note}"
            ),
            Some(if tokenless.accepted() {
                DOCTOR_TOKENLESS_LOGIN_HINT
            } else {
                DOCTOR_COMBINED_CHANNEL_HINT
            }),
        )
    } else if !cookie.accepted() && !control.accepted() {
        (
            DoctorSectionStatus::Fail,
            "every session channel was rejected, so the session identifier itself is no longer \
             valid server-side rather than being mis-carried by one channel"
                .to_owned(),
            Some(DOCTOR_SESSION_DEAD_HINT),
        )
    } else {
        (
            DoctorSectionStatus::Warn,
            format!(
                "the session was rejected through this client's usual channels and through the \
                 _sid request field, but {cookie_note}"
            ),
            None,
        )
    }
}

/// How many simultaneous authenticated calls the fan-out probe makes.
///
/// A constant rather than a flag: this is a diagnostic against someone's live NAS, and letting an
/// operator dial it up turns a measurement into a load test.
const DOCTOR_CONCURRENCY_FANOUT: usize = 4;

/// Issue several authenticated calls at once, then one more on its own.
///
/// `reqwest`'s pool opens additional TCP connections under concurrency, so this is the closest
/// available test of "a second connection loses the session". The sequential call afterwards is
/// what separates "the burst was rejected" from "the burst invalidated the session".
fn record_session_concurrency(result: &mut DoctorResult, client: &ApiClient) {
    let started = Instant::now();
    let mut outcomes = Vec::with_capacity(DOCTOR_CONCURRENCY_FANOUT);
    std::thread::scope(|scope| {
        let handles = (0..DOCTOR_CONCURRENCY_FANOUT)
            .map(|_| {
                let client = client.clone();
                scope.spawn(move || client.confirm_file_station_session())
            })
            .collect::<Vec<_>>();
        for handle in handles {
            outcomes.push(handle.join().unwrap_or_else(|_| {
                Err(Error::Message(
                    "a concurrent session probe thread panicked".to_owned(),
                ))
            }));
        }
    });
    let follow_up = client.confirm_file_station_session();

    let mut report = ConcurrencyReport {
        parallel: DOCTOR_CONCURRENCY_FANOUT,
        follow_up_succeeded: follow_up.is_ok(),
        follow_up_session_rejected: follow_up
            .as_ref()
            .err()
            .is_some_and(|error| matches!(error.api_code(), Some(106 | 107 | 119))),
        elapsed_ms: duration_millis(started.elapsed()),
        ..ConcurrencyReport::default()
    };
    for outcome in &outcomes {
        match outcome {
            Ok(()) => report.succeeded += 1,
            Err(error) if matches!(error.api_code(), Some(106 | 107 | 119)) => {
                report.session_rejected += 1;
            }
            Err(_) => report.other_failures += 1,
        }
    }

    let (status, detail, remediation) = concurrency_verdict(&report);
    record_diagnostic_section(
        result,
        "session_concurrency",
        status,
        detail,
        started.elapsed(),
        remediation,
    );
    result.concurrency = Some(report);
}

/// Reach a verdict from the fan-out probe.
///
/// The sequential call after the burst is what separates "the burst was rejected" from "the burst
/// invalidated the session", and those need different answers.
fn concurrency_verdict(
    report: &ConcurrencyReport,
) -> (DoctorSectionStatus, String, Option<&'static str>) {
    if report.succeeded == report.parallel && report.follow_up_succeeded {
        (
            DoctorSectionStatus::Pass,
            format!(
                "all {} concurrent requests succeeded and the sequential follow-up succeeded, so \
                 the session tolerates being used from several connections at once",
                report.parallel
            ),
            None,
        )
    } else if report.session_rejected > 0 {
        (
            DoctorSectionStatus::Warn,
            format!(
                "{} of {} concurrent requests succeeded and {} were rejected with a session \
                 error; the sequential follow-up {}",
                report.succeeded,
                report.parallel,
                report.session_rejected,
                if report.follow_up_succeeded {
                    "succeeded, so the session itself survived the burst"
                } else {
                    "also failed, so the burst left the session unusable"
                }
            ),
            Some(DOCTOR_MULTIPLE_PATH_HINT),
        )
    } else if report.succeeded == report.parallel && !report.follow_up_succeeded {
        (
            DoctorSectionStatus::Warn,
            "every concurrent request succeeded but the sequential call after them did not, so \
             the burst itself is what invalidated the session"
                .to_owned(),
            Some(DOCTOR_MULTIPLE_PATH_HINT),
        )
    } else {
        (
            DoctorSectionStatus::Warn,
            format!(
                "{} of {} concurrent requests succeeded; {} failed for reasons other than a \
                 rejected session",
                report.succeeded, report.parallel, report.other_failures
            ),
            None,
        )
    }
}

/// Exercise the advertised File Station capabilities that are safe to exercise.
///
/// Advertised is not functional: an API can sit in the discovery map and still answer 105 for
/// this account, or 102 because a proxy does not forward its CGI path. Only a live call separates
/// those, and only for the account actually running.
fn record_capability_diagnosis(result: &mut DoctorResult, client: &ApiClient) {
    let started = Instant::now();
    let mut diagnosis = CapabilityDiagnosis {
        unprobed: DOCTOR_UNPROBED_CAPABILITIES.to_vec(),
        ..CapabilityDiagnosis::default()
    };

    // The authoritative per-account report comes first: `is_manager` and the virtual-protocol
    // list change what every other line of this section means.
    let info_started = Instant::now();
    let first_info = client.file_station_info();
    diagnosis.records.push(CapabilityRecord {
        api: "SYNO.FileStation.Info",
        method: "get",
        version: 2,
        verdict: verdict_for(&first_info),
        dsm_code: first_info.as_ref().err().and_then(Error::api_code),
        elapsed_ms: duration_millis(info_started.elapsed()),
        required: false,
    });
    match &first_info {
        Ok(info) => {
            diagnosis.info = Some(*info);
            diagnosis.first_hostname = info.hostname;
        }
        Err(error) if session_is_unusable(error) => {
            diagnosis.session_aborted = true;
        }
        Err(_) => {}
    }

    if !diagnosis.session_aborted {
        for spec in capability_probe_specs(result.capabilities.as_ref()) {
            let required = spec.api == "SYNO.FileStation.List";
            match client.probe_capability(spec) {
                Ok(probe) => {
                    let verdict = CapabilityVerdict::classify(probe);
                    diagnosis.records.push(CapabilityRecord {
                        api: probe.api,
                        method: probe.method,
                        version: probe.version,
                        verdict,
                        dsm_code: probe.dsm_code,
                        elapsed_ms: probe.elapsed_ms,
                        required,
                    });
                    // A session code means the session died, not that the capability is missing.
                    // Continuing would paint every remaining row red and tell the operator
                    // nothing about their capabilities.
                    if matches!(probe.dsm_code, Some(106 | 107 | 119)) {
                        diagnosis.session_aborted = true;
                        break;
                    }
                }
                Err(error) => {
                    diagnosis.session_aborted |= session_is_unusable(&error);
                    if diagnosis.session_aborted {
                        break;
                    }
                }
            }
        }
    }

    // The second host read is the point of the whole section: a single NAS behind a relay has one
    // hostname, so two different ones inside one run is the only positive proof that consecutive
    // requests did not reach the same host.
    if !diagnosis.session_aborted
        && let Ok(info) = client.file_station_info()
    {
        diagnosis.last_hostname = info.hostname;
        diagnosis.hostname_changed = match (diagnosis.first_hostname, info.hostname) {
            (Some(first), Some(last)) => first.as_str() != last.as_str(),
            _ => false,
        };
    }

    let (status, detail, remediation) = capability_diagnosis_verdict(&diagnosis);
    record_diagnostic_section(
        result,
        "capability_diagnosis",
        status,
        detail,
        started.elapsed(),
        remediation,
    );
    result.capability_diagnosis = Some(diagnosis);
}

/// Reach a verdict on what the probed capabilities said.
///
/// A rejected session is reported as a rejected session, never as a wall of broken capabilities,
/// and only a *required* capability that is advertised and demonstrably non-functional fails the
/// section: a broken optional API must not fail a sync diagnostic.
fn capability_diagnosis_verdict(
    diagnosis: &CapabilityDiagnosis,
) -> (DoctorSectionStatus, String, Option<&'static str>) {
    if diagnosis.hostname_changed {
        (
            DoctorSectionStatus::Fail,
            "File Station reported two different host names inside this one run, so consecutive \
             requests demonstrably reached different DSM hosts"
                .to_owned(),
            Some(DOCTOR_MULTIPLE_PATH_HINT),
        )
    } else if diagnosis.session_aborted {
        (
            DoctorSectionStatus::Warn,
            "capability probing stopped after the DSM session was rejected; the results above \
             describe the session rather than the capabilities, and the channel ablation section \
             is where that verdict is reached"
                .to_owned(),
            None,
        )
    } else if diagnosis.broken_required() > 0 {
        (
            DoctorSectionStatus::Fail,
            format!(
                "{} of {} probed capabilities work; {} that this tool requires are advertised but \
                 not functional",
                diagnosis.working(),
                diagnosis.probed(),
                diagnosis.broken_required()
            ),
            diagnosis
                .records
                .iter()
                .any(|record| record.verdict == CapabilityVerdict::NotRoutable)
                .then_some(DOCTOR_CAPABILITY_ROUTING_HINT),
        )
    } else if diagnosis.working() < diagnosis.probed() {
        (
            DoctorSectionStatus::Warn,
            format!(
                "{} of {} probed capabilities work; the rest are optional for this tool",
                diagnosis.working(),
                diagnosis.probed()
            ),
            None,
        )
    } else {
        (
            DoctorSectionStatus::Pass,
            format!(
                "all {} probed capabilities work for this account",
                diagnosis.probed()
            ),
            None,
        )
    }
}

fn verdict_for<T>(outcome: &Result<T>) -> CapabilityVerdict {
    match outcome {
        Ok(_) => CapabilityVerdict::Works,
        Err(error) => match error.api_code() {
            Some(102) => CapabilityVerdict::NotRoutable,
            Some(103) => CapabilityVerdict::MethodUnavailable,
            Some(104) => CapabilityVerdict::VersionUnsupported,
            Some(105 | 407) => CapabilityVerdict::NoPermission,
            Some(106 | 107 | 119) => CapabilityVerdict::NotProbed,
            _ => CapabilityVerdict::Failed,
        },
    }
}

/// The read-only probes worth making, given what DSM advertised and what this run was pointed at.
///
/// Each entry is bounded, non-mutating, and free of side effects. An API advertised at a CGI path
/// this client does not already use is deliberately left unprobed rather than followed: a
/// diagnostic has no business being the first thing to request an unknown endpoint.
fn capability_probe_specs(
    capabilities: Option<&CapabilityEnumeration>,
) -> Vec<CapabilityProbeSpec> {
    // `getinfo` is deliberately absent: the destination path resolution exercises it against a
    // real path on every run that has a destination, and probing it a second time here would buy
    // nothing but another round trip.
    let mut specs = vec![CapabilityProbeSpec {
        api: "SYNO.FileStation.List",
        method: "list_share",
        version: 2,
        cgi_path: "entry.cgi",
        parameters: vec![
            ("offset".to_owned(), "0".to_owned()),
            ("limit".to_owned(), "1".to_owned()),
        ],
    }];
    let advertised = |name: &str, version: u32| {
        capabilities.is_some_and(|capabilities| {
            capabilities.file_station.iter().any(|api| {
                api.name == name
                    && api.min_version.is_some_and(|min| min <= version)
                    && api.max_version.is_some_and(|max| max >= version)
            })
        })
    };
    if advertised("SYNO.FileStation.VirtualFolder", 2) {
        specs.push(CapabilityProbeSpec {
            api: "SYNO.FileStation.VirtualFolder",
            method: "list",
            version: 2,
            cgi_path: "entry.cgi",
            parameters: vec![
                ("type".to_owned(), "\"cifs\"".to_owned()),
                ("offset".to_owned(), "0".to_owned()),
                ("limit".to_owned(), "1".to_owned()),
            ],
        });
    }
    if advertised("SYNO.FileStation.BackgroundTask", 3) {
        specs.push(CapabilityProbeSpec {
            api: "SYNO.FileStation.BackgroundTask",
            method: "list",
            version: 3,
            cgi_path: "entry.cgi",
            parameters: vec![
                ("offset".to_owned(), "0".to_owned()),
                ("limit".to_owned(), "1".to_owned()),
            ],
        });
    }
    specs
}

/// Report the destination path one component at a time.
///
/// The walk already happens inside the permission check; this renders what that check discards.
/// "Neither the destination nor any ancestor of it exists" and "only the last component is
/// missing" are different problems with different fixes, and only the component list separates
/// them.
fn record_destination_path_resolution(
    result: &mut DoctorResult,
    resolution: &DestinationPathResolution,
) {
    let (status, detail, remediation) = path_resolution_verdict(resolution);
    // Deliberately derived rather than a section of its own: the `getinfo` requests belong to the
    // permission check that issued them, and draining them onto this section would report the
    // same round trips twice.
    result.set_derived_section(
        "destination_path_resolution",
        status,
        detail.clone(),
        remediation,
    );
    if status == DoctorSectionStatus::Fail {
        result.failure.get_or_insert(detail);
    }
    result.path_resolution = Some(resolution.clone());
}

/// Reach a verdict from the destination walk.
fn path_resolution_verdict(
    resolution: &DestinationPathResolution,
) -> (DoctorSectionStatus, String, Option<&'static str>) {
    if resolution.segments.is_empty() {
        (
            DoctorSectionStatus::Skip,
            "the destination path was not walked; the permission check could not start".to_owned(),
            None,
        )
    } else if resolution.fully_resolved() {
        (
            DoctorSectionStatus::Pass,
            format!(
                "all {} components of the destination exist and are directories",
                resolution.total_components
            ),
            None,
        )
    } else if resolution.share_root_missing() {
        (
            DoctorSectionStatus::Fail,
            format!(
                "resolution stops at the first component of {}: the shared folder itself is \
                 absent or invisible to this account",
                resolution.total_components
            ),
            Some(DOCTOR_MISSING_SHARE_HINT),
        )
    } else if let Some(missing) = resolution.first_missing {
        (
            DoctorSectionStatus::Warn,
            format!(
                "resolution stops at component {missing} of {}; every component before it exists \
                 and is a directory",
                resolution.total_components
            ),
            Some(DOCTOR_MISSING_COMPONENT_HINT),
        )
    } else {
        // The walk stopped for a reason other than a component being absent -- a rejected
        // session, a refused permission -- and saying "missing" would be a different diagnosis
        // from the one the evidence supports.
        let stopped_by = resolution
            .segments
            .last()
            .and_then(|segment| segment.dsm_code)
            .map(|code| format!(" after DSM answered {code}"))
            .unwrap_or_default();
        (
            DoctorSectionStatus::Warn,
            format!(
                "the walk stopped at component {} of {}{stopped_by}, so the components below it \
                 were never inspected and are not known to be missing",
                resolution.segments.len(),
                resolution.total_components
            ),
            None,
        )
    }
}

fn doctor_run(
    settings: &config::ResolvedDoctor,
    logger: Option<Arc<EventLogger>>,
    cancellation: &CancellationToken,
    perform_write_probe: bool,
    call_log: DoctorCallLog,
) -> Result<DoctorResult> {
    let mut result = DoctorResult::new(settings, perform_write_probe, call_log);
    result.progress = cancellation.clone();
    if let Err(error) = cancellation.check() {
        result.fail_section("routing_tls", &error, Duration::ZERO);
        return Ok(result);
    }
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::ApiDiscoveryStarted),
    )?;
    let connection_started = Instant::now();
    // The doctor's observer both logs each round trip and retains the completed ones, so every
    // section can report the requests it is responsible for.
    let mut client = match connect_client(
        &settings.url,
        &settings.network,
        cancellation,
        Some(result.call_log.observer(logger.as_ref())),
    ) {
        Ok(client) => client,
        Err(error) => {
            let elapsed = connection_started.elapsed();
            if matches!(
                &error,
                Error::MissingApi(_) | Error::UnsupportedApiVersion { .. }
            ) || is_discovery_response_failure(&error)
            {
                let warning = settings.network.danger_accept_invalid_certs
                    || (settings.network.allow_http
                        && settings
                            .url
                            .trim_start()
                            .get(..7)
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://")));
                // Discovery is recorded first so the discovery requests are attributed to it
                // rather than to routing, which performs no request of its own. Display order is
                // fixed by the section list and is unaffected.
                result.fail_section("dsm_api_discovery", &error, elapsed);
                result.set_section(
                    "routing_tls",
                    if warning {
                        DoctorSectionStatus::Warn
                    } else {
                        DoctorSectionStatus::Pass
                    },
                    "the DSM discovery route responded; transport and discovery shared this timing",
                    elapsed,
                    "shared_connection",
                );
            } else {
                result.fail_section("routing_tls", &error, elapsed);
                result.set_section(
                    "dsm_api_discovery",
                    DoctorSectionStatus::Skip,
                    "the DSM route did not yield a validated discovery response",
                    elapsed,
                    "shared_connection",
                );
            }
            return Ok(result);
        }
    };
    let connection_elapsed = connection_started.elapsed();
    let insecure_http = settings.network.allow_http
        && settings
            .url
            .trim_start()
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"));
    let transport_warning = insecure_http || settings.network.danger_accept_invalid_certs;
    let transport_detail = match (
        insecure_http,
        settings.network.danger_accept_invalid_certs,
        settings.network.ca_certificate.is_some(),
    ) {
        (true, true, _) => {
            "DSM discovery route responded over explicitly allowed HTTP with certificate verification disabled"
        }
        (true, false, _) => {
            "DSM discovery route responded over explicitly allowed HTTP; use only on a trusted test or LAN endpoint"
        }
        (false, true, _) => {
            "HTTPS route responded, but certificate verification is explicitly disabled"
        }
        (false, false, true) => {
            "HTTPS route and TLS negotiation succeeded with the configured CA certificate"
        }
        (false, false, false) => {
            "HTTPS route and TLS negotiation succeeded with certificate verification enabled"
        }
    };
    // Discovery is recorded first so the discovery requests are attributed to it rather than to
    // routing, which performs no request of its own. Display order is fixed by the section list.
    result.set_section(
        "dsm_api_discovery",
        DoctorSectionStatus::Pass,
        "DSM Auth and baseline File Station API versions were validated from the discovery response",
        connection_elapsed,
        "shared_connection",
    );
    result.set_section(
        "routing_tls",
        if transport_warning {
            DoctorSectionStatus::Warn
        } else {
            DoctorSectionStatus::Pass
        },
        transport_detail,
        connection_elapsed,
        "shared_connection",
    );

    // Enumeration runs before the requirement check and before authentication: it needs no
    // session, so an operator whose credentials are broken still learns exactly what their DSM
    // offers, and every later section can name what it is working against.
    record_capability_enumeration(&mut result, &client, cancellation);

    let capability_started = Instant::now();
    let capability_result = (|| {
        if doctor_requires_content_fingerprint(settings.level, settings.compare) {
            client.require_content_fingerprint_api()?;
        }
        if doctor_requires_delete_capability(settings.level, settings.delete) {
            client.require_delete_api()?;
        }
        Ok(())
    })();
    if let Err(error) = capability_result {
        result.fail_section(
            "file_station_capabilities",
            &error,
            capability_started.elapsed(),
        );
        result.set_timing_scope("file_station_capabilities", TIMING_SCOPE_LOCAL_ONLY);
        if settings.level != cli::DoctorLevel::Quick {
            result.set_section(
                "dsm_session_auth",
                DoctorSectionStatus::Skip,
                "authentication was not attempted because required File Station capabilities are unavailable",
                Duration::ZERO,
                "section",
            );
        }
        return Ok(result);
    }
    let copy_supported = client.supports_server_copy();
    let copy_warning = settings.level == cli::DoctorLevel::Extensive && !copy_supported;
    let capability_detail = if copy_warning {
        "required List/Create/Upload/Permission/Download/Delete APIs are available; optional server-side copy is unavailable and verified upload fallback will be used"
    } else if settings.level == cli::DoctorLevel::Extensive {
        "required List/Create/Upload/Permission/Download/Delete APIs and optional server-side copy are available"
    } else {
        "the APIs required by the selected quick or standard diagnostic are available"
    };
    result.set_section(
        "file_station_capabilities",
        if copy_warning {
            DoctorSectionStatus::Warn
        } else {
            DoctorSectionStatus::Pass
        },
        capability_detail,
        capability_started.elapsed(),
        // No request is made: this compares the required APIs against the discovery response the
        // connection already fetched, which is why it always reports near-zero time.
        TIMING_SCOPE_LOCAL_ONLY,
    );

    if let Err(error) = cancellation.check() {
        result.fail_section("dsm_session_auth", &error, Duration::ZERO);
        return Ok(result);
    }
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::ApiDiscoveryCompleted),
    )?;
    if settings.level == cli::DoctorLevel::Quick {
        return Ok(result);
    }

    let Some(username) = settings.username.as_deref() else {
        let error = Error::Configuration(
            "--username is required for standard and extensive doctor checks".to_owned(),
        );
        result.fail_section("dsm_session_auth", &error, Duration::ZERO);
        return Ok(result);
    };
    log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::AuthenticationStarted),
    )?;
    let authentication_started = Instant::now();
    // The ablation's tokenless variant needs a second login, so it needs the password after this
    // closure has returned. The buffer is `Zeroizing` and is dropped as soon as the ablation
    // section is done with it.
    let mut tokenless_probe = TokenlessProbeLogin::Unavailable(
        "the tokenless-login variant runs only at the extensive level",
    );
    let authentication = (|| {
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
        let authenticated = credentials::authenticate_with_sources(
            &mut client,
            username,
            &password,
            &mut vault,
            settings.authentication.totp_secret_file.as_deref(),
        );
        if authenticated.is_ok() && settings.level == cli::DoctorLevel::Extensive {
            tokenless_probe = TokenlessProbeLogin::Available {
                username: username.to_owned(),
                password: password.clone(),
            };
        }
        drop(password);
        authenticated
    })();
    if let Err(error) = authentication {
        result.fail_section("dsm_session_auth", &error, authentication_started.elapsed());
        result.set_section(
            "session_logout",
            DoctorSectionStatus::Skip,
            "authentication did not create a confirmed DSM session",
            Duration::ZERO,
            "section",
        );
        return Ok(result);
    }
    if let Err(error) = client.confirm_file_station_session() {
        result.fail_section("dsm_session_auth", &error, authentication_started.elapsed());
        record_doctor_logout(&mut client, &mut result);
        return Ok(result);
    }
    result.authenticated = true;
    result.set_section(
        "dsm_session_auth",
        DoctorSectionStatus::Pass,
        "DSM credentials were accepted and an authenticated File Station session was established",
        authentication_started.elapsed(),
        "section",
    );
    // Tracked separately from `result.failed()`. The session diagnostics below can legitimately
    // fail -- proving the session is mis-carried is their job -- and a run that reached a verdict
    // about the session must still go on to inspect the destination.
    let session_logging_failed = log_event(
        logger.as_ref(),
        LogEvent::new(EventLogLevel::Info, EventCode::AuthenticationCompleted),
    )
    .inspect_err(|error| {
        result.fail_section("dsm_session_auth", error, authentication_started.elapsed());
    })
    .is_err();

    // The session diagnostics run immediately after the session is established and before
    // anything else uses it, so their verdict is about the session rather than about whatever a
    // later section happened to ask for. The ablation comes first because the capability
    // diagnosis reads its verdict: without it, a dead session masquerades as broken capabilities.
    if !session_logging_failed && cancellation.check().is_ok() {
        record_session_channel_ablation(&mut result, &client, &tokenless_probe);
        if settings.level == cli::DoctorLevel::Extensive && cancellation.check().is_ok() {
            record_session_concurrency(&mut result, &client);
        }
        if cancellation.check().is_ok() {
            record_capability_diagnosis(&mut result, &client);
        }
    }

    if session_logging_failed {
        result.set_section(
            "destination_permissions",
            DoctorSectionStatus::Skip,
            "not run because authenticated-session completion logging failed",
            Duration::ZERO,
            "section",
        );
        result.set_section(
            "destination_inventory",
            DoctorSectionStatus::Skip,
            "not run because authenticated-session completion logging failed",
            Duration::ZERO,
            "section",
        );
        result.set_section(
            "destination_path_resolution",
            DoctorSectionStatus::Skip,
            "not run because authenticated-session completion logging failed",
            Duration::ZERO,
            "section",
        );
    } else if let Some(remote) = settings.remote.as_deref() {
        match RemoteRoot::parse(remote) {
            Err(error) => {
                result.set_section(
                    "destination_path_resolution",
                    DoctorSectionStatus::Skip,
                    "the destination path was rejected before it could be walked",
                    Duration::ZERO,
                    "section",
                );
                result.fail_section("destination_permissions", &error, Duration::ZERO);
                result.set_section(
                    "destination_inventory",
                    DoctorSectionStatus::Skip,
                    "the destination path was rejected before enumeration",
                    Duration::ZERO,
                    "section",
                );
            }
            Ok(root) => {
                let permission_started = Instant::now();
                // One walk, reported twice: the resolution section renders the components the
                // permission check inspects, so naming exactly where the path stops existing
                // costs no additional request.
                let (resolution, write_result) = match cancellation.check() {
                    Ok(()) => client.verify_destination_writable_with_resolution(&root),
                    Err(error) => (DestinationPathResolution::default(), Err(error)),
                };
                // The permission section records first so the walk's requests are attributed to
                // the check that made them; the resolution section then renders the same walk
                // without claiming the requests a second time.
                let session_was_rejected =
                    write_result.as_ref().err().is_some_and(session_is_unusable);
                // `destination_exists` is set only where the walk ran every component to the end
                // without returning early, which means the destination's own `getinfo` came back
                // as an existing directory that is not a mount boundary. That is precisely what
                // the inventory's opening `getinfo` re-establishes, so the inventory can start at
                // its listing instead. Any other outcome -- an absent component, a mount root, a
                // refused permission -- leaves the question open and keeps the full check.
                let destination_walked_and_exists =
                    matches!(&write_result, Ok(check) if check.destination_exists);
                match write_result {
                    Ok(write_check) => {
                        result.write_permission_scope = Some(if write_check.destination_exists {
                            "exact_destination"
                        } else {
                            "nearest_existing_ancestor"
                        });
                        result.write_permission_path = Some(write_check.checked_directory);
                        result.set_section(
                            "destination_permissions",
                            DoctorSectionStatus::Pass,
                            if write_check.destination_exists {
                                "the authenticated account can create a child in the exact destination"
                            } else {
                                "the destination is absent; its nearest existing ancestor permits creating the first missing component"
                            },
                            permission_started.elapsed(),
                            "section",
                        );
                    }
                    Err(error) => {
                        // A rejected session cannot enumerate either. Falling through would
                        // issue a second request that cannot succeed and would report one dead
                        // session as two independent failures. A permission-only refusal is
                        // different: "can read but cannot write" is a real diagnosis, so the
                        // inventory still runs for those.
                        result.fail_section(
                            "destination_permissions",
                            &error,
                            permission_started.elapsed(),
                        );
                        if session_was_rejected {
                            record_destination_path_resolution(&mut result, &resolution);
                            result.set_section(
                                "destination_inventory",
                                DoctorSectionStatus::Skip,
                                if matches!(error, Error::Cancelled) {
                                    "not attempted; the run was cancelled during the permission \
                                     check"
                                } else {
                                    "not attempted; the DSM session was already rejected by the \
                                     permission check"
                                },
                                Duration::ZERO,
                                "section",
                            );
                            record_doctor_logout(&mut client, &mut result);
                            return Ok(result);
                        }
                    }
                }
                record_destination_path_resolution(&mut result, &resolution);

                let inventory_started = Instant::now();
                let inventory_start_log = log_event(
                    logger.as_ref(),
                    LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanStarted),
                );
                if let Err(error) = inventory_start_log {
                    result.fail_section(
                        "destination_inventory",
                        &error,
                        inventory_started.elapsed(),
                    );
                } else {
                    match cancellation.check().and_then(|()| {
                        if destination_walked_and_exists {
                            client.diagnostic_remote_inventory_of_existing_root(&root)
                        } else {
                            client.diagnostic_remote_inventory(&root)
                        }
                    }) {
                        Ok(inventory) => {
                            result.remote_checked = true;
                            result.remote_exists = Some(inventory.root_exists);
                            result.remote_entries = Some(inventory.total_entries);
                            let inventory_detail = if inventory.root_exists {
                                format!(
                                    "one direct-child page was inspected without recursion; {} total entries, {} sampled, {} truncated",
                                    inventory.total_entries,
                                    inventory.sample.len(),
                                    inventory.truncated_count
                                )
                            } else {
                                "the destination does not yet exist; no entries were enumerated"
                                    .to_owned()
                            };
                            let inventory_log = log_event(
                                logger.as_ref(),
                                LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanCompleted)
                                    .metrics(EventMetrics {
                                        operations: inventory.total_entries as u64,
                                        ..EventMetrics::default()
                                    }),
                            );
                            result.remote_inventory =
                                Some((DoctorInventoryScope::DirectChildren, inventory));
                            match inventory_log {
                                Ok(()) => result.set_section(
                                    "destination_inventory",
                                    DoctorSectionStatus::Pass,
                                    inventory_detail,
                                    inventory_started.elapsed(),
                                    "section",
                                ),
                                Err(error) => result.fail_section(
                                    "destination_inventory",
                                    &error,
                                    inventory_started.elapsed(),
                                ),
                            }
                        }
                        Err(error) => result.fail_section(
                            "destination_inventory",
                            &error,
                            inventory_started.elapsed(),
                        ),
                    }
                }

                if settings.write_test && result.remote_exists == Some(false) {
                    let detail = "the disposable probe requires an existing destination; no mutation was attempted";
                    result.set_section(
                        "disposable_write_verify_cleanup",
                        DoctorSectionStatus::Fail,
                        detail,
                        Duration::ZERO,
                        "section",
                    );
                    result.failure.get_or_insert_with(|| detail.to_owned());
                }

                if settings.write_test && perform_write_probe && result.remote_exists != Some(false)
                {
                    let probe_started = Instant::now();
                    let probe_prerequisites_pass = result
                        .section_succeeded("destination_permissions")
                        && result.section_succeeded("destination_inventory")
                        && result.remote_exists == Some(true);
                    if !probe_prerequisites_pass {
                        let detail = if result.remote_exists == Some(false) {
                            "the disposable probe requires an existing destination; no mutation was attempted"
                        } else {
                            "permission or bounded inventory prerequisites failed; no mutation was attempted"
                        };
                        result.set_section(
                            "disposable_write_verify_cleanup",
                            DoctorSectionStatus::Fail,
                            detail,
                            probe_started.elapsed(),
                            "section",
                        );
                        result.failure.get_or_insert_with(|| detail.to_owned());
                    } else {
                        result.write_probe_performed = true;
                        match client.run_write_probe(&root, cancellation) {
                            Ok(report) => {
                                let copy_detail = if report.server_copy_supported {
                                    "including server-side copy verification"
                                } else {
                                    "using verified upload because server-side copy is unavailable"
                                };
                                result.set_section(
                                    "disposable_write_verify_cleanup",
                                    DoctorSectionStatus::Pass,
                                    format!(
                                        "unique create-only probe uploaded and verified with MD5/CRC32/SHA-256 and mtime, {copy_detail}; cleanup confirmed"
                                    ),
                                    probe_started.elapsed(),
                                    "section",
                                );
                                result.write_probe = Some(report);
                            }
                            Err(failure) => {
                                let mut detail = safe_doctor_error(&failure.cause);
                                if failure.cleanup_error.is_some()
                                    || !failure.report.cleanup_completed
                                {
                                    detail.push_str("; cleanup could not be fully confirmed");
                                }
                                if failure.report.leftover_remote_probe_path.is_some() {
                                    detail.push_str("; inspect the reported leftover probe path");
                                }
                                result.set_section(
                                    "disposable_write_verify_cleanup",
                                    DoctorSectionStatus::Fail,
                                    detail.clone(),
                                    probe_started.elapsed(),
                                    "section",
                                );
                                result.failure.get_or_insert(detail);
                                result.write_probe_cancelled =
                                    matches!(&failure.cause, Error::Cancelled);
                                result.cancelled |= result.write_probe_cancelled;
                                result.write_probe_error = Some(failure.to_string());
                                result.write_probe = Some(failure.report);
                            }
                        }
                    }
                }
            }
        }
    } else {
        result.set_section(
            "destination_path_resolution",
            DoctorSectionStatus::Skip,
            "no destination was selected; there was no path to walk",
            Duration::ZERO,
            "section",
        );
        result.set_section(
            "destination_permissions",
            DoctorSectionStatus::Skip,
            "no destination was selected; no write-permission check was attempted",
            Duration::ZERO,
            "section",
        );
        let inventory_started = Instant::now();
        let inventory_start_log = log_event(
            logger.as_ref(),
            LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanStarted),
        );
        if let Err(error) = inventory_start_log {
            result.fail_section("destination_inventory", &error, inventory_started.elapsed());
        } else {
            match cancellation
                .check()
                .and_then(|()| client.diagnostic_visible_shared_folders())
            {
                Ok(inventory) => {
                    let inventory_detail = format!(
                        "no destination was selected; one visible shared-folder page was inspected without choosing a target; {} reported roots, {} sampled, {} truncated",
                        inventory.total_entries,
                        inventory.sample.len(),
                        inventory.truncated_count,
                    );
                    let inventory_log = log_event(
                        logger.as_ref(),
                        LogEvent::new(EventLogLevel::Info, EventCode::RemoteScanCompleted).metrics(
                            EventMetrics {
                                operations: inventory.total_entries as u64,
                                ..EventMetrics::default()
                            },
                        ),
                    );
                    result.remote_inventory =
                        Some((DoctorInventoryScope::VisibleSharedFolders, inventory));
                    match inventory_log {
                        Ok(()) => result.set_section(
                            "destination_inventory",
                            DoctorSectionStatus::Pass,
                            inventory_detail,
                            inventory_started.elapsed(),
                            "section",
                        ),
                        Err(error) => result.fail_section(
                            "destination_inventory",
                            &error,
                            inventory_started.elapsed(),
                        ),
                    }
                }
                Err(error) => result.fail_section(
                    "destination_inventory",
                    &error,
                    inventory_started.elapsed(),
                ),
            }
        }
    }

    record_doctor_logout(&mut client, &mut result);
    // Deliberately after the logout above: this is the only place in the run that opens a second
    // DSM session, and once the primary one is closed there is nothing left for a duplicate login
    // to interrupt. Nothing below reads the primary session.
    record_tokenless_login_variant(&mut result, &client, &tokenless_probe, cancellation);
    drop(tokenless_probe);
    Ok(result)
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
    let cancellation = install_cancellation_handler()?;
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
                Ok(result) => {
                    let error = doctor_result_error(&result.result);
                    cancelled |= result.result.cancelled;
                    outcomes.push(DoctorBatchOutcome {
                        name: job.name.clone(),
                        status: if error.is_some() {
                            DoctorBatchStatus::Failed
                        } else {
                            DoctorBatchStatus::Preflighted
                        },
                        result: Some(result),
                        error,
                    });
                }
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
    let all_targets_preflighted_before_mutation =
        write_tests && all_doctor_targets_preflighted(&outcomes, cancelled);

    if write_tests
        && (cancelled
            || outcomes
                .iter()
                .any(|outcome| outcome.status == DoctorBatchStatus::Failed))
    {
        write_doctor_batch_output(
            &outcomes,
            &output,
            true,
            all_targets_preflighted_before_mutation,
        )?;
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
                    let diagnostic_error = doctor_result_error(&result.result);
                    let diagnostic_cancelled =
                        result.result.cancelled || result.result.write_probe_cancelled;
                    let may_have_mutated = doctor_result_may_have_mutated(&result.result);
                    outcome.result = Some(result);
                    if let Some(error) = diagnostic_error {
                        outcome.status = if may_have_mutated {
                            DoctorBatchStatus::Partial
                        } else {
                            DoctorBatchStatus::Failed
                        };
                        outcome.error = Some(error);
                        cancelled |= diagnostic_cancelled;
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
                Ok(result) => {
                    let error = doctor_result_error(&result.result);
                    cancelled |= result.result.cancelled;
                    outcomes.push(DoctorBatchOutcome {
                        name: job.name.clone(),
                        status: if error.is_some() {
                            DoctorBatchStatus::Failed
                        } else {
                            DoctorBatchStatus::Success
                        },
                        result: Some(result),
                        error,
                    });
                }
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

    write_doctor_batch_output(
        &outcomes,
        &output,
        write_tests,
        all_targets_preflighted_before_mutation,
    )?;
    doctor_batch_completion(&outcomes, cancelled || cancellation.is_cancelled())
}

fn doctor_batch_completion(outcomes: &[DoctorBatchOutcome], cancelled: bool) -> Result<ExitCode> {
    if cancelled {
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

fn all_doctor_targets_preflighted(outcomes: &[DoctorBatchOutcome], cancelled: bool) -> bool {
    !cancelled
        && !outcomes.is_empty()
        && outcomes
            .iter()
            .all(|outcome| outcome.status == DoctorBatchStatus::Preflighted)
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

fn doctor_result_error(result: &DoctorResult) -> Option<String> {
    result
        .failure
        .clone()
        .or_else(|| result.write_probe_error.clone())
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

/// The transport options a run derives from its URL and resolved network settings.
///
/// Shared with the unauthenticated transport probe so the probe measures the same endpoint, under
/// the same timeouts and the same certificate trust, that the run itself will use. A probe built
/// from separately assembled options could report on a path the client never takes.
fn client_options(url: &str, network: &config::ResolvedNetwork) -> ClientOptions {
    ClientOptions {
        base_url: url.to_owned(),
        allow_http: network.allow_http,
        accept_invalid_certs: network.danger_accept_invalid_certs,
        ca_certificate: network.ca_certificate.clone(),
        connect_timeout: Duration::from_secs(network.connect_timeout),
        request_timeout: Duration::from_secs(network.timeout),
        retries: u32::from(network.retries),
    }
}

fn connect_client(
    url: &str,
    network: &config::ResolvedNetwork,
    cancellation: &CancellationToken,
    observer: Option<RequestObserver>,
) -> Result<ApiClient> {
    // The observer is handed to `connect_observed` rather than applied afterwards so API
    // discovery -- two requests, with a route fallback worth seeing -- is instrumented too.
    ApiClient::connect_observed(&client_options(url, network), observer)
        // The limit is applied to the connected client rather than to the HTTP transport: it paces
        // the upload body, and it is deliberately shared by every worker clone of this client. The
        // cancellation token rides along so control-request backoff wakes on Ctrl-C.
        .map(|client| {
            client
                .with_max_upload_rate(network.max_rate)
                .with_cancellation(cancellation)
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
        // Deletes and directory creates carry no bytes but are real work: a mirror that
        // removes ninety entries and uploads one file must not render as a single upload
        // and then sit still. `files` and `bytes` stay upload-shaped, and a server-side
        // copy counts there too because it delivers a file and falls back to a real
        // upload when the server refuses the copy.
        let tracker = ProgressTracker::new(ProgressTotals {
            operations: operation_count(plan),
            files: (plan.uploads.len() + plan.copies.len()) as u64,
            bytes: plan.upload_bytes.saturating_add(copy_fallback_bytes(plan)),
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

    /// Account one completed remote mutation reported by `sync::execute_observed`.
    ///
    /// `ExecutionEvent::Uploaded` is deliberately ignored: `observer_factory` already
    /// opened and closed a tracker operation for that transfer, so counting it here
    /// would double it. Every other variant runs without an upload observer, making this
    /// its only path into the tracker.
    fn record_execution_event(&self, event: &ExecutionEvent) {
        let (kind, bytes) = match event {
            ExecutionEvent::TypeConflictDeleted { .. }
            | ExecutionEvent::RemoteExtraDeleted { .. } => (OperationKind::DeleteEntry, 0),
            ExecutionEvent::DirectoryCreated { .. } => (OperationKind::CreateDirectory, 0),
            ExecutionEvent::RemoteContentCopied { bytes, .. }
            | ExecutionEvent::CopyFallbackUploaded { bytes, .. } => (OperationKind::Upload, *bytes),
            ExecutionEvent::Uploaded { .. } => return,
        };
        // The event arrives only after the mutation succeeded, so the operation opens and
        // closes together; `finish_success` credits the whole declared byte count.
        let operation = self.tracker.start(kind, bytes);
        let Ok(update) = operation.finish_success() else {
            record_progress_failure(
                &self.failure,
                "progress operation state became inconsistent".to_owned(),
            );
            self.cancellation.cancel();
            return;
        };
        let snapshot = self.tracker.snapshot();
        match self.renderer.lock() {
            Ok(mut renderer) => {
                if let Err(error) = renderer.render(&snapshot, Some(&update)) {
                    record_progress_failure(&self.failure, error.to_string());
                }
            }
            Err(_) => record_progress_failure(
                &self.failure,
                "progress renderer lock was poisoned".to_owned(),
            ),
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
    renderer_progress_mode_for_terminal(output, io::stderr().is_terminal())
}

fn renderer_progress_mode_for_terminal(
    output: &config::ResolvedOutput,
    stderr_is_terminal: bool,
) -> RendererProgressMode {
    if !output.terminal_progress_enabled(stderr_is_terminal) {
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

/// The one token every signal handler cancels, and every subcommand shares.
///
/// `ctrlc::set_handler` refuses a second installation for the life of the process, so the token
/// is process-wide rather than per-subcommand. Only one subcommand runs per process today; this
/// makes a future second call idempotent instead of a hard failure.
static PROCESS_CANCELLATION: OnceLock<CancellationToken> = OnceLock::new();

/// Signals received so far. The first requests cooperative cancellation; any later one force-exits.
static SIGNALS_RECEIVED: AtomicUsize = AtomicUsize::new(0);

const SECOND_SIGNAL_HINT: &[u8] =
    b"\ncancelling; finishing the current operation. Press Ctrl-C again to exit immediately.\n";

/// Install the escalating termination handler and return the process cancellation token.
///
/// The first SIGINT/SIGTERM/SIGHUP (and the Windows console control events the `termination`
/// feature covers) sets the token so every phase unwinds cooperatively: in-flight uploads abort,
/// remote state stays consistent, and the run exits 130. A second signal means the operator is
/// no longer willing to wait, so the process exits immediately with the same code.
///
/// `ctrlc` runs this closure on its own dedicated thread rather than in an async-signal context,
/// so allocation here would in fact be legal. It is still kept allocation-free and lock-free: a
/// single `write_all` of a constant to stderr cannot deadlock against the progress renderer or a
/// panicking main thread, and cannot break if the crate ever moves to a real signal handler.
/// `std::process::exit` is used for the escalation because it is the only portable way to choose
/// the documented exit code 130 without `unsafe`.
///
/// Only the subcommands that actually poll the token install this. `config`, `completions`,
/// `manpage`, and `credentials` deliberately keep the default signal disposition, which already
/// terminates them at once; taking the signal over for them would turn a working single Ctrl-C
/// into a swallowed one on paths that have nothing to unwind.
///
/// The `sdsync-dsm-api` binary installs its own handler (`dsm_api::install_consumer_termination_handler`).
/// The two never share a process: `dsm_api.rs` is compiled only into that binary and nothing here
/// references it. Merging them would make whichever installs second fail.
fn install_cancellation_handler() -> Result<CancellationToken> {
    if let Some(cancellation) = PROCESS_CANCELLATION.get() {
        return Ok(cancellation.clone());
    }
    let cancellation = CancellationToken::default();
    let handler_token = cancellation.clone();
    ctrlc::set_handler(move || {
        if SIGNALS_RECEIVED.fetch_add(1, Ordering::AcqRel) == 0 {
            handler_token.cancel();
            let _ = io::stderr().write_all(SECOND_SIGNAL_HINT);
        } else {
            std::process::exit(CANCELLED_EXIT_CODE.into());
        }
    })
    .map_err(|error| Error::Message(format!("failed to install Ctrl-C handler: {error}")))?;
    Ok(PROCESS_CANCELLATION.get_or_init(|| cancellation).clone())
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
    let logger = EventLogger::new(LoggerConfig {
        level,
        stderr,
        file,
        remote,
    })
    .map(Arc::new)
    .map_err(observability_error)?;
    // The first record every sink receives, so a pasted log or a rotated file always names the
    // build that produced it. Emitting here rather than at each subcommand covers sync, plan,
    // both doctors, and every batch job -- each of which builds its own logger and so wants its
    // own file to be self-describing. `--log-level off` returned above and never reaches this.
    logger
        .emit(LogEvent::new(EventLogLevel::Info, EventCode::RunBuild).build(BUILD))
        .map_err(observability_error)?;
    Ok(Some(logger))
}

/// Turn one transport observation into a log event, or `None` when it is not worth a record.
///
/// The level policy lives here rather than in the API layer, and its most important choice is that
/// a *failed* call is DEBUG while a successful one is TRACE: `--log-level debug` alone must be
/// enough to diagnose a live failure, without asking the user for another run.
fn observation_event(observation: ApiObservation) -> LogEvent {
    match observation {
        // The connection record names the endpoint host, so it is deliberately DEBUG: a
        // default-level run that ships events to a remote collector must not begin disclosing a
        // hostname it did not disclose before.
        ApiObservation::Connected(connection) => {
            LogEvent::new(EventLogLevel::Debug, EventCode::ConnectionEstablished)
                .connection(connection)
        }
        ApiObservation::SessionEstablished(session) => {
            LogEvent::new(EventLogLevel::Debug, EventCode::SessionEstablished).session(session)
        }
        ApiObservation::CallStarted(call) => {
            let code = if call.retry_backoff_ms.is_some() {
                EventCode::RetryScheduled
            } else {
                EventCode::ApiCallStarted
            };
            let level = if call.retry_backoff_ms.is_some() {
                EventLogLevel::Debug
            } else {
                EventLogLevel::Trace
            };
            LogEvent::new(level, code).call(call)
        }
        ApiObservation::CallCompleted(call) => {
            let redirected = call.outcome == RequestOutcome::Redirect;
            let (level, code) = if redirected {
                // Redirects are refused by policy, so meeting one is always a misconfiguration.
                (EventLogLevel::Warn, EventCode::ApiCallRedirected)
            } else if call.outcome.is_failure() {
                (EventLogLevel::Debug, EventCode::ApiCallCompleted)
            } else {
                (EventLogLevel::Trace, EventCode::ApiCallCompleted)
            };
            LogEvent::new(level, code).call(call)
        }
    }
}

/// Build a transport observer that writes into `logger`.
///
/// Emission failures are deliberately swallowed: instrumentation must never be able to fail a
/// sync or a diagnostic. A sink that is genuinely broken is still reported by the shutdown path,
/// which is where a delivery failure belongs.
fn request_observer(logger: &Arc<EventLogger>) -> RequestObserver {
    let logger = Arc::clone(logger);
    Arc::new(move |observation| {
        let _ = logger.emit(observation_event(observation));
    })
}

fn log_event(logger: Option<&Arc<EventLogger>>, event: LogEvent) -> Result<()> {
    if let Some(logger) = logger {
        logger.emit(event).map_err(observability_error)?;
    }
    Ok(())
}

/// How long to wait for observability delivery before giving up on it.
///
/// A cancelled run gets the short window: the operator has already asked the process to stop, and
/// the shutdown result cannot change what they receive, because a failure on a cancelled run is
/// reported as a warning while the cancellation itself is returned.
fn logger_shutdown_timeout<T>(operation: &Result<T>) -> Duration {
    if matches!(operation, Err(Error::Cancelled)) {
        CANCELLED_LOGGER_SHUTDOWN_TIMEOUT
    } else {
        LOGGER_SHUTDOWN_TIMEOUT
    }
}

fn finish_logger<T>(
    logger: Option<&Arc<EventLogger>>,
    operation: Result<T>,
    quiet: bool,
) -> Result<T> {
    let shutdown = logger
        .map(|logger| logger.shutdown(logger_shutdown_timeout(&operation)))
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
        .map(|logger| logger.shutdown(logger_shutdown_timeout(&operation)))
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

/// The shipped example is the starter file, so `config init` and the documented example can
/// never drift apart.
const STARTER_CONFIGURATION: &str = include_str!("../config.example.toml");

fn run_config(arguments: &cli::Cli, action: cli::ConfigAction) -> Result<()> {
    // Neither action can load a configuration: `path` reports where one belongs and `init`
    // creates the file that every other action requires.
    let requested_format = arguments
        .global
        .output
        .output
        .unwrap_or(cli::OutputFormat::Human);
    if action == cli::ConfigAction::Path {
        let path = configured_path(&arguments.global).ok_or_else(|| {
            Error::Configuration("no platform configuration directory is available".to_owned())
        })?;
        return write_simple_value(
            requested_format,
            path.display().to_string(),
            json!({"schema": "sdsync.config-path.v1", "path": path}),
        );
    }
    if let cli::ConfigAction::Init { force } = action {
        return run_config_init(&arguments.global, force, requested_format);
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
        cli::ConfigAction::Path | cli::ConfigAction::Init { .. } => unreachable!(),
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

fn run_config_init(global: &cli::GlobalArgs, force: bool, format: cli::OutputFormat) -> Result<()> {
    let path = configured_path(global).ok_or_else(|| {
        Error::Configuration("no platform configuration directory is available".to_owned())
    })?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|source| Error::FileIo {
            path: parent.to_owned(),
            source,
        })?;
    }
    let replaced = write_starter_configuration(&path, force)?;

    let action = if replaced { "Replaced" } else { "Wrote" };
    write_simple_value(
        format,
        format!(
            "{action} the starter configuration at {}. Edit it, then run `config validate`.",
            path.display()
        ),
        json!({
            "schema": "sdsync.config-init.v1",
            "path": path,
            "replaced": replaced,
        }),
    )
}

/// Write the starter configuration, reporting whether an existing file was replaced. Without
/// `force` the create is exclusive, so a configuration that appears between the decision and the
/// write is still never clobbered.
fn write_starter_configuration(path: &std::path::Path, force: bool) -> Result<bool> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    // Truncation destroys the evidence, so record the prior state before opening.
    let replaced = force && path.exists();

    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(Error::Configuration(format!(
                "a configuration file already exists at {}; pass --force to replace it",
                path.display()
            )));
        }
        Err(source) => {
            return Err(Error::FileIo {
                path: path.to_owned(),
                source,
            });
        }
    };
    file.write_all(STARTER_CONFIGURATION.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|source| Error::FileIo {
            path: path.to_owned(),
            source,
        })?;
    Ok(replaced)
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
    scope: &plan::Scope,
) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_sync_output_to(&mut stdout, plan, report, elapsed, output, plan_only, scope)
}

fn write_sync_output_to<W: Write>(
    writer: &mut W,
    plan: &SyncPlan,
    report: Option<&ExecutionReport>,
    elapsed: Duration,
    output: &config::ResolvedOutput,
    plan_only: bool,
    scope: &plan::Scope,
) -> Result<()> {
    match output.output {
        cli::OutputFormat::Human => {
            write_plan_human_to(writer, plan, plan_only || output.verbosity > 0, scope)?;
            if let Some(report) = report {
                writeln!(
                    writer,
                    "Sync complete: {} uploaded ({}), {} copied on NAS, {} directories created, {} remote entries deleted in {} ms.",
                    report.uploaded,
                    format_bytes(report.uploaded_bytes),
                    report.copied,
                    report.created,
                    report.deleted,
                    duration_millis(elapsed),
                )
                .map_err(output_error)?;
            } else if !plan_only && plan.is_empty() {
                writeln!(writer, "Already in sync; no remote changes were needed.")
                    .map_err(output_error)?;
            } else if plan_only {
                writeln!(writer, "Plan only; no remote changes were made.")
                    .map_err(output_error)?;
            }
            writer.flush().map_err(output_error)
        }
        cli::OutputFormat::Json => write_json_to(
            writer,
            &command_json_value(plan, report, elapsed, plan_only),
        ),
        cli::OutputFormat::Ndjson => {
            write_plan_ndjson_to(writer, plan)?;
            if let Some(report) = report {
                write_json_line_to(
                    writer,
                    &json!({
                        "schema": "sdsync.output.v1",
                        "kind": "completion",
                        "result": execution_value(report, elapsed),
                    }),
                )?;
            } else if !plan_only {
                write_json_line_to(
                    writer,
                    &json!({
                        "schema": "sdsync.output.v1",
                        "kind": "completion",
                        "changed": false,
                    }),
                )?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
fn sync_output(
    plan: &SyncPlan,
    report: Option<&ExecutionReport>,
    elapsed: Duration,
    output: &config::ResolvedOutput,
    plan_only: bool,
) -> RenderedOutput {
    let mut buffer = Vec::new();
    write_sync_output_to(
        &mut buffer,
        plan,
        report,
        elapsed,
        output,
        plan_only,
        &plan::Scope::root(),
    )
    .expect("writing rendered sync output to a Vec cannot fail");
    captured_rendered_output(output.output, buffer)
}

fn write_plan_human_to<W: Write>(
    writer: &mut W,
    plan: &SyncPlan,
    detailed: bool,
    scope: &plan::Scope,
) -> Result<()> {
    writeln!(
        writer,
        "Plan: {} uploads ({}), {} server copies (verified upload fallback up to {}), {} directories, {} deletions, {} unchanged files, {} protected remote entries.",
        plan.uploads.len(),
        format_bytes(plan.upload_bytes),
        plan.copies.len(),
        format_bytes(copy_fallback_bytes(plan)),
        plan.creates.len(),
        plan.delete_count(),
        plan.unchanged_files,
        plan.protected_entries
    )
    .map_err(output_error)?;
    write_plan_scope_notice(writer, plan, scope)?;
    if !detailed {
        return Ok(());
    }
    for action in &plan.pre_deletes {
        writeln!(
            writer,
            "  DELETE-CONFLICT {} (remote snapshot guarded)",
            action.remote_path
        )
        .map_err(output_error)?;
    }
    for action in &plan.creates {
        writeln!(
            writer,
            "  MKDIR  {} ({}: {})",
            action.remote_path,
            action.reason.as_str(),
            action.reason.detail()
        )
        .map_err(output_error)?;
    }
    for action in &plan.copies {
        writeln!(
            writer,
            "  COPY   {} -> {} ({}; verified upload fallback allowed before task start)",
            action.from_remote_path,
            action.to_remote_path,
            format_bytes(action.expected_size)
        )
        .map_err(output_error)?;
    }
    for action in &plan.uploads {
        writeln!(
            writer,
            "  UPLOAD {} -> {} ({}: {})",
            action.local.relative,
            action.remote_path,
            action.reason.as_str(),
            action.reason.detail()
        )
        .map_err(output_error)?;
    }
    for action in &plan.post_deletes {
        if let Some(guard) = &action.destination_guard {
            writeln!(
                writer,
                "  DELETE {} (remote snapshot guarded; destination guarded by {} bytes+mtime+MD5+CRC32+SHA-256 at {})",
                action.remote_path, guard.expected_size, guard.remote_path
            )
            .map_err(output_error)?;
        } else {
            writeln!(
                writer,
                "  DELETE {} (remote snapshot guarded)",
                action.remote_path
            )
            .map_err(output_error)?;
        }
    }
    Ok(())
}

/// State what a forced plan will overwrite, and what a scoped plan did not look at.
///
/// The forced count comes from the plan itself rather than from the invocation, so it reports what
/// is actually scheduled. It is printed before any mutation, which is what makes an inspected plan
/// the confirmation step for a forced run.
fn write_plan_scope_notice<W: Write>(
    writer: &mut W,
    plan: &SyncPlan,
    scope: &plan::Scope,
) -> Result<()> {
    let forced: Vec<_> = plan
        .uploads
        .iter()
        .filter(|action| action.reason == plan::ChangeReason::Forced)
        .collect();
    if !forced.is_empty() {
        let bytes = forced.iter().fold(0_u64, |total, action| {
            total.saturating_add(action.local.size)
        });
        writeln!(
            writer,
            "Forced comparison: {} remote files will be overwritten without being compared ({}). Remote copies that are identical or newer are replaced. Nothing is deleted by --compare force alone.",
            forced.len(),
            format_bytes(bytes)
        )
        .map_err(output_error)?;
    }
    if !scope.is_root() {
        writeln!(
            writer,
            "Scoped to {:?}: entries outside it were neither compared nor modified, and the check for paths differing only by letter case covered this scope alone.",
            scope.as_str()
        )
        .map_err(output_error)?;
    }
    Ok(())
}

#[cfg(test)]
fn plan_human(plan: &SyncPlan, detailed: bool) -> String {
    let mut output = Vec::new();
    write_plan_human_to(&mut output, plan, detailed, &plan::Scope::root())
        .expect("writing rendered human plan to a Vec cannot fail");
    String::from_utf8(output).expect("human plan output is UTF-8")
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
                "reason": action.reason.as_str(),
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
                "reason": action.reason.as_str(),
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

fn write_plan_ndjson_to<W: Write>(writer: &mut W, plan: &SyncPlan) -> Result<()> {
    write_json_line_to(writer, &plan_summary_record(plan))?;
    for action in &plan.pre_deletes {
        write_json_line_to(
            writer,
            &json!({
                "schema": "sdsync.plan-action.v1", "action": "delete-conflict",
                "relative": action.relative, "remote_path": action.remote_path,
                "entry_kind": action.kind.as_str(),
                "snapshot_guard": remote_snapshot_value(&action.snapshot),
            }),
        )?;
    }
    for action in &plan.creates {
        write_json_line_to(
            writer,
            &json!({
                "schema": "sdsync.plan-action.v1", "action": "create-directory",
                "relative": action.relative, "remote_path": action.remote_path,
                "reason": action.reason.as_str(),
            }),
        )?;
    }
    for action in &plan.copies {
        write_json_line_to(
            writer,
            &json!({
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
            }),
        )?;
    }
    for action in &plan.uploads {
        write_json_line_to(
            writer,
            &json!({
                "schema": "sdsync.plan-action.v1", "action": "upload",
                "relative": action.local.relative, "remote_path": action.remote_path,
                "bytes": action.local.size, "mtime_ms": action.local.mtime_ms,
                "reason": action.reason.as_str(),
            }),
        )?;
    }
    for action in &plan.post_deletes {
        write_json_line_to(
            writer,
            &json!({
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
            }),
        )?;
    }
    Ok(())
}

#[cfg(test)]
fn plan_ndjson_values(plan: &SyncPlan) -> Vec<Value> {
    let mut output = Vec::new();
    write_plan_ndjson_to(&mut output, plan)
        .expect("writing rendered NDJSON plan to a Vec cannot fail");
    parse_ndjson_output(&output)
}

fn write_status_output(
    page: &plan::StatusPage,
    scope: &plan::Scope,
    compare: CompareMode,
    cache: &status_cache::CacheReport,
    output: &config::ResolvedOutput,
) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_status_output_to(&mut stdout, page, scope, compare, cache, output)
}

/// Describe how much of this answer rests on stored evidence, and how old the oldest of it is.
///
/// Emitted on every status answer, cached or not, so a caller never has to infer freshness from
/// the absence of a field. `oldest_evidence_epoch` is deliberately the oldest and not the newest
/// contributing observation: it is the only claim that is true of every row in the answer, and it
/// is exactly the width of the window in which an undetected change could be hiding.
fn status_cache_value(cache: &status_cache::CacheReport) -> Value {
    json!({
        "state": cache.state.as_str(),
        "entries_reused": cache.digests_reused,
        "entries_verified_live": cache.digests_computed,
        "entries_canary_checked": cache.canary_checked,
        "oldest_evidence_epoch": cache.oldest_evidence_epoch,
    })
}

fn write_status_output_to<W: Write>(
    writer: &mut W,
    page: &plan::StatusPage,
    scope: &plan::Scope,
    compare: CompareMode,
    cache: &status_cache::CacheReport,
    output: &config::ResolvedOutput,
) -> Result<()> {
    match output.output {
        cli::OutputFormat::Human => {
            write_status_human_to(writer, page, scope, compare, cache)?;
            writer.flush().map_err(output_error)
        }
        cli::OutputFormat::Json => write_json_to(
            writer,
            &json!({
                "schema": "sdsync.status.v1",
                "kind": "status",
                "scope": page.scope,
                "compare": compare_label(compare),
                "limit": page.limit,
                "truncated": page.truncated,
                "next_cursor": page.next_cursor.as_ref().map(plan::StatusCursor::as_str),
                "stats": status_stats_value(&page.stats),
                "cache": status_cache_value(cache),
                "entries": page
                    .entries
                    .iter()
                    .map(status_entry_value)
                    .collect::<Vec<_>>(),
            }),
        ),
        cli::OutputFormat::Ndjson => {
            write_json_line_to(
                writer,
                &json!({
                    "schema": "sdsync.status.v1",
                    "kind": "summary",
                    "scope": page.scope,
                    "compare": compare_label(compare),
                    "limit": page.limit,
                    "truncated": page.truncated,
                    "next_cursor": page.next_cursor.as_ref().map(plan::StatusCursor::as_str),
                    "stats": status_stats_value(&page.stats),
                    "cache": status_cache_value(cache),
                }),
            )?;
            for entry in &page.entries {
                write_json_line_to(writer, &status_entry_value(entry))?;
            }
            Ok(())
        }
    }
}

fn compare_label(compare: CompareMode) -> &'static str {
    match compare {
        CompareMode::Content => "content",
        CompareMode::Metadata => "metadata",
        CompareMode::SizeOnly => "size-only",
        CompareMode::Force => "force",
    }
}

fn status_stats_value(stats: &plan::StatusStats) -> Value {
    json!({
        "compare": compare_label(stats.compare),
        "in_sync_files": stats.in_sync_files,
        "differing_files": stats.differing_files,
        "missing_remote_files": stats.missing_remote_files,
        "remote_only_entries": stats.remote_only_entries,
        "type_conflicts": stats.type_conflicts,
        "excluded_entries": stats.excluded_entries,
        "directories": stats.directories,
        "in_sync_bytes": stats.in_sync_bytes,
        "transfer_bytes": stats.transfer_bytes,
        "total_entries": stats.total_entries,
        "attention_entries": stats.attention_entries,
        "complete": stats.complete,
    })
}

fn status_entry_value(entry: &plan::StatusEntry) -> Value {
    let mut value = json!({
        "schema": "sdsync.status-entry.v1",
        "relative": entry.relative,
        "remote_path": entry.remote_path,
        "entry_kind": entry.kind.as_str(),
        "state": entry.state.kind().as_str(),
    });
    let object = value
        .as_object_mut()
        .expect("status entry is constructed as a JSON object");
    if let Some(reason) = entry.state.reason() {
        object.insert("reason".to_owned(), json!(reason.as_str()));
        object.insert("detail".to_owned(), json!(reason.detail()));
    }
    if let plan::StatusState::TypeConflict {
        local_kind,
        remote_kind,
    } = entry.state
    {
        object.insert("local_kind".to_owned(), json!(local_kind.as_str()));
        object.insert("remote_kind".to_owned(), json!(remote_kind.as_str()));
    }
    if let plan::StatusState::Excluded(cause) = entry.state {
        object.insert("exclusion".to_owned(), json!(cause.as_str()));
    }
    if let Some(side) = entry.local {
        object.insert(
            "local".to_owned(),
            json!({"size": side.size, "mtime_seconds": side.mtime_seconds}),
        );
    }
    if let Some(side) = entry.remote {
        object.insert(
            "remote".to_owned(),
            json!({"size": side.size, "mtime_seconds": side.mtime_seconds}),
        );
    }
    value
}

fn write_status_human_to<W: Write>(
    writer: &mut W,
    page: &plan::StatusPage,
    scope: &plan::Scope,
    compare: CompareMode,
    cache: &status_cache::CacheReport,
) -> Result<()> {
    let target = if scope.is_root() {
        "the whole tree".to_owned()
    } else {
        format!("{:?}", scope.as_str())
    };
    let stats = &page.stats;
    let qualifier = if stats.complete { "" } else { " at least" };
    writeln!(
        writer,
        "Status of {target} compared by {}:{qualifier} {} in sync, {} differing, {} missing remotely, {} remote-only, {} type conflicts, {} excluded ({} to transfer).",
        compare_label(compare),
        stats.in_sync_files,
        stats.differing_files,
        stats.missing_remote_files,
        stats.remote_only_entries,
        stats.type_conflicts,
        stats.excluded_entries,
        format_bytes(stats.transfer_bytes),
    )
    .map_err(output_error)?;

    for entry in &page.entries {
        let detail = match entry.state {
            plan::StatusState::InSync => "in sync".to_owned(),
            plan::StatusState::Differs(reason) => format!("differs ({})", reason.detail()),
            plan::StatusState::MissingRemote => "missing remotely".to_owned(),
            plan::StatusState::RemoteOnly => "remote-only".to_owned(),
            plan::StatusState::TypeConflict {
                local_kind,
                remote_kind,
            } => format!(
                "type conflict (local {}, remote {})",
                local_kind.as_str(),
                remote_kind.as_str()
            ),
            plan::StatusState::Excluded(cause) => format!("excluded ({})", cause.as_str()),
        };
        writeln!(writer, "  {} — {detail}", entry.relative).map_err(output_error)?;
    }

    if let Some(cursor) = &page.next_cursor {
        writeln!(
            writer,
            "More entries remain; continue with --cursor {:?}.",
            cursor.as_str()
        )
        .map_err(output_error)?;
    }
    if !stats.complete {
        writeln!(
            writer,
            "The scan budget stopped this walk, so every total above is a lower bound; narrow the query with --scope."
        )
        .map_err(output_error)?;
    }
    if !scope.is_root() {
        writeln!(
            writer,
            "Only {target} was examined; entries outside it were neither compared nor reported."
        )
        .map_err(output_error)?;
    }
    // Stated as an age rather than a timestamp: a reader should not have to subtract to learn how
    // old the evidence is, and how old it is *is* the point. Only the oldest contributing
    // observation is quoted, because it is the only bound true of every row above.
    if cache.digests_reused > 0
        && let Some(oldest) = cache.oldest_evidence_epoch
    {
        writeln!(
            writer,
            "{} of {} compared digests were reused from stored evidence, the oldest {}.",
            cache.digests_reused,
            cache.digests_reused + cache.digests_computed,
            describe_evidence_age(oldest),
        )
        .map_err(output_error)?;
    }
    if cache.state == status_cache::CacheState::Unusable {
        writeln!(
            writer,
            "Stored digests disagreed with live evidence, so the cache was discarded and this answer was recomputed in full."
        )
        .map_err(output_error)?;
    }
    Ok(())
}

/// Render an evidence timestamp as an age, which is what a reader actually needs from it.
fn describe_evidence_age(observed_at_epoch: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok());
    let Some(seconds) = now.map(|now| now.saturating_sub(observed_at_epoch)) else {
        return format!("recorded at epoch {observed_at_epoch}");
    };
    if seconds < 0 {
        return format!("recorded at epoch {observed_at_epoch}");
    }
    if seconds < 90 {
        return "moments ago".to_owned();
    }
    if seconds < 3 * 3600 {
        return format!("{} minutes ago", seconds / 60);
    }
    if seconds < 48 * 3600 {
        return format!("{} hours ago", seconds / 3600);
    }
    format!("{} days ago", seconds / 86_400)
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
        "uploaded_crc32": report.uploaded_md5.crc32_hex(),
        "uploaded_sha256": report.uploaded_md5.sha256_hex(),
        "fingerprint_complete": report.uploaded_md5.has_full_proof(),
        "uploaded_mtime_seconds": report.uploaded_mtime_seconds,
        "server_copy_supported": report.server_copy_supported,
        "server_copy_attempted": report.server_copy_attempted,
        "server_copy_verified": report.server_copy_verified,
        "cleanup_completed": report.cleanup_completed,
        "leftover_remote_probe_path": report.leftover_remote_probe_path,
    })
}

fn doctor_inventory_value(
    scope: DoctorInventoryScope,
    inventory: &DiagnosticRemoteInventory,
) -> Value {
    json!({
        "scope": scope.as_str(),
        "root_exists": inventory.root_exists,
        "total_entries": inventory.total_entries,
        "sample_count": inventory.sample.len(),
        "sample_limit": 5,
        "truncated": inventory.truncated,
        "truncated_count": inventory.truncated_count,
        "truncated_reason": inventory.truncated_reason,
        "budget": {
            "pages_requested": inventory.pages_requested,
            "traversal_depth": inventory.traversal_depth,
            "deadline_ms": inventory.deadline_ms,
        },
        "sample": inventory.sample.iter().map(|entry| json!({
            "relative_path": entry.relative_path,
            "relative_path_truncated": entry.relative_path_truncated,
            "name": entry.name,
            "name_truncated": entry.name_truncated,
            "kind": entry.kind.as_str(),
            "size_bytes": entry.size_bytes,
            "mtime_seconds": entry.mtime_seconds,
            "mount_boundary": entry.mount_boundary,
        })).collect::<Vec<_>>(),
    })
}

fn doctor_overall_status(result: &DoctorResult) -> &'static str {
    if result.failed() {
        "fail"
    } else if result
        .sections
        .iter()
        .any(|section| section.status == DoctorSectionStatus::Warn)
    {
        "warn"
    } else {
        "pass"
    }
}

/// Render one recorded DSM request.
///
/// Deliberately the same closed facts the log records carry: names, versions, a status, a code,
/// which session channels were attached, and a duration. No response body or credential material
/// is representable here.
fn doctor_call_value(call: &DoctorCall) -> Value {
    json!({
        "sequence": call.sequence,
        "api": call.api,
        "method": call.method,
        "version": call.version,
        "outcome": call.outcome.as_str(),
        "dsm_code": call.dsm_code,
        "http_status": call.http_status,
        "session": {
            "cookie_header": call.session.cookie_header,
            "syno_token_header": call.session.syno_token_header,
            "sid_field": call.session.sid_field,
            "syno_token_field": call.session.syno_token_field,
        },
        "elapsed_ms": call.elapsed_ms,
        // Schema, not content: a member path, a JSON type, and the deserializer's own
        // expectation. Present only when the call actually failed to decode.
        "decode": call.decode.map(|fault| json!({
            "kind": fault.kind.as_str(),
            "path": fault.path.as_str(),
            "field": fault.field.as_str(),
            "expected": fault.expected.as_str(),
            "found": fault.found.as_str(),
        })),
    })
}

/// The machine-readable view of what DSM advertises.
///
/// Bounded in the same three tiers the human report prints, on purpose: `query=all` on a NAS with
/// a full package set returns hundreds of entries, and a JSON document a server can make
/// arbitrarily long is a denial-of-service surface rather than a diagnostic.
fn capability_enumeration_value(capabilities: &CapabilityEnumeration) -> Value {
    json!({
        "advertised_apis": capabilities.total,
        "unusable_entries": capabilities.unusable_entries,
        "file_station": capabilities.file_station.iter().map(|api| json!({
            "name": api.name,
            "min_version": api.min_version,
            "max_version": api.max_version,
            "required_version": api.required,
            "optional": api.optional,
            "used": api.required.is_some(),
        })).collect::<Vec<_>>(),
        "file_station_not_listed": capabilities.file_station_truncated,
        "requirements": capabilities.requirements.iter().map(|verdict| json!({
            "api": verdict.requirement.api,
            "required_version": verdict.requirement.version,
            "optional": verdict.requirement.optional,
            "purpose": verdict.requirement.purpose,
            "advertised": verdict.present,
            "offered_min": verdict.offered.map(|(min, _)| min),
            "offered_max": verdict.offered.map(|(_, max)| max),
            "satisfied": verdict.satisfied,
        })).collect::<Vec<_>>(),
        "namespaces": capabilities.namespaces.iter().map(|(name, count)| json!({
            "namespace": name,
            "apis": count,
        })).collect::<Vec<_>>(),
        "namespaces_not_listed": capabilities.namespace_overflow.map(|(apis, namespaces)| json!({
            "apis": apis,
            "namespaces": namespaces,
        })),
    })
}

fn channel_probe_value(probe: &ChannelProbe) -> Value {
    json!({
        "channels": probe.channels.as_str(),
        "cookie_header": probe.channels.sends_cookie_header(),
        "syno_token_header": probe.channels.sends_token_header(),
        "sid_field": probe.channels.sends_sid_field(),
        "syno_token_field": probe.channels.sends_token_field(),
        "outcome": probe.outcome.as_str(),
        "dsm_code": probe.dsm_code,
        "http_status": probe.http_status,
        "elapsed_ms": probe.elapsed_ms,
        // A variant that did not run is distinguishable from one that was rejected, so a reader
        // parsing this cannot mistake an unattempted variant for evidence.
        "ran": probe.ran(),
        "skipped_reason": probe.skipped,
    })
}

fn concurrency_value(report: &ConcurrencyReport) -> Value {
    json!({
        "parallel_requests": report.parallel,
        "succeeded": report.succeeded,
        "session_rejected": report.session_rejected,
        "other_failures": report.other_failures,
        "follow_up_succeeded": report.follow_up_succeeded,
        "follow_up_session_rejected": report.follow_up_session_rejected,
        "elapsed_ms": report.elapsed_ms,
    })
}

fn capability_diagnosis_value(diagnosis: &CapabilityDiagnosis) -> Value {
    json!({
        "probed": diagnosis.probed(),
        "working": diagnosis.working(),
        "session_aborted": diagnosis.session_aborted,
        "hostname_changed": diagnosis.hostname_changed,
        "host": diagnosis.info.and_then(|info| info.hostname).map(|host| host.as_str().to_owned()),
        "is_manager": diagnosis.info.map(|info| info.is_manager),
        "supports_sharing": diagnosis.info.map(|info| info.support_sharing),
        "virtual_protocols": diagnosis
            .info
            .and_then(|info| info.support_virtual_protocol)
            .map(|protocols| protocols.as_str().to_owned()),
        "capabilities": diagnosis.records.iter().map(|record| json!({
            "api": record.api,
            "method": record.method,
            "version": record.version,
            "verdict": record.verdict.as_str(),
            "dsm_code": record.dsm_code,
            "required": record.required,
            "elapsed_ms": record.elapsed_ms,
        })).collect::<Vec<_>>(),
        "not_probed": diagnosis.unprobed,
    })
}

fn path_resolution_value(resolution: &DestinationPathResolution) -> Value {
    json!({
        "total_components": resolution.total_components,
        "first_missing": resolution.first_missing,
        "fully_resolved": resolution.fully_resolved(),
        "segments": resolution.segments.iter().map(|segment| json!({
            "path": segment.path,
            "depth": segment.depth,
            "exists": segment.exists,
            "is_directory": segment.is_directory,
            "mount_boundary": segment.mount_boundary,
            "dsm_code": segment.dsm_code,
        })).collect::<Vec<_>>(),
    })
}

/// Describe a section's timing in terms a reader can act on.
///
/// A bare `0 ms` on a section that never contacted the server reads as a broken measurement; it is
/// in fact the correct answer to a question settled locally.
fn doctor_section_timing(section: &DoctorSection) -> String {
    match section.timing_scope {
        TIMING_SCOPE_LOCAL_ONLY => "no request; answered from the discovery response".to_owned(),
        TIMING_SCOPE_DERIVED => {
            "no request of its own; summarised from the requests other sections made".to_owned()
        }
        "shared_connection" => format!(
            "{} ms shared with routing and discovery",
            duration_millis(section.elapsed)
        ),
        _ => format!("{} ms", duration_millis(section.elapsed)),
    }
}

fn doctor_value(result: &DoctorResult, elapsed: Duration) -> Value {
    let status_count = |status| {
        result
            .sections
            .iter()
            .filter(|section| section.status == status)
            .count()
    };
    json!({
        "schema": "sdsync.doctor.v1",
        "level": result.level.as_str(),
        "build": {
            "name": BUILD.name,
            "version": BUILD.version,
            "target": BUILD.target,
            "profile": BUILD.profile,
            "commit": BUILD.commit,
        },
        "status": doctor_overall_status(result),
        "summary": {
            "pass": status_count(DoctorSectionStatus::Pass),
            "warn": status_count(DoctorSectionStatus::Warn),
            "fail": status_count(DoctorSectionStatus::Fail),
            "skip": status_count(DoctorSectionStatus::Skip),
        },
        "sections": result.sections.iter().map(|section| json!({
            "id": section.id,
            "label": section.label,
            "step": section.step,
            "status": section.status.as_str(),
            "detail": section.detail,
            "elapsed_ms": duration_millis(section.elapsed),
            "timing_scope": section.timing_scope,
            "remediation": section.remediation,
            "calls": section.calls.iter().map(doctor_call_value).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "error": result.failure,
        "transport": {
            // `tcp-connect` is stated in the payload as well as the prose: a consumer reading
            // "ping" here must not assume ICMP, which this never uses.
            "probe_method": "tcp-connect",
            "reachability": result.reachability.as_ref().map(ReachabilityReport::json_value),
            "intermediary": result.intermediary.as_ref().map(IntermediarySummary::json_value),
            "cookies": result.cookies.as_ref().map(CookieLedger::json_value),
        },
        "capabilities": result.capabilities.as_ref().map(capability_enumeration_value),
        "session_channels": result.channel_ablation.as_ref().map(|probes| {
            probes.iter().map(channel_probe_value).collect::<Vec<_>>()
        }),
        "session_concurrency": result.concurrency.as_ref().map(concurrency_value),
        "capability_diagnosis": result.capability_diagnosis.as_ref().map(capability_diagnosis_value),
        "path_resolution": result.path_resolution.as_ref().map(path_resolution_value),
        "routing": result.section_succeeded("routing_tls"),
        "api_discovery": result.section_succeeded("dsm_api_discovery"),
        "authenticated": result.authenticated,
        "remote_checked": result.remote_checked,
        "remote_exists": result.remote_exists,
        "remote_entries": result.remote_entries,
        "remote_inventory": result.remote_inventory.as_ref().map(|(scope, inventory)| {
            doctor_inventory_value(*scope, inventory)
        }),
        "write_permission_scope": result.write_permission_scope,
        "write_permission_path": result.write_permission_path,
        "write_test": {
            "requested": result.write_probe_requested,
            "status": if !result.write_probe_requested {
                "not-requested"
            } else if result.failed() {
                "failed"
            } else if !result.write_probe_performed {
                "preflighted"
            } else {
                "success"
            },
            "report": result.write_probe.as_ref().map(write_probe_value),
            "error": result.write_probe_error,
        },
        "elapsed_ms": duration_millis(elapsed),
    })
}

/// Write one transport block: a heading, then a bullet per fact.
///
/// A line the producer indented is a continuation of the bullet above it -- a per-address
/// breakdown under its aggregate, a rotation under the cookie it happened to -- so it is indented
/// further instead of being given a bullet of its own, which would read as a peer fact.
fn write_transport_block(human: &mut String, heading: &str, lines: Vec<String>) {
    writeln!(human, "{heading}").expect("writing to a String cannot fail");
    for line in lines {
        match line.strip_prefix("  ") {
            Some(continuation) => writeln!(human, "      {continuation}"),
            None => writeln!(human, "  - {line}"),
        }
        .expect("writing to a String cannot fail");
    }
}

/// The enumeration block, in the three tiers a reader can actually use.
///
/// Tier 1 is the File Station surface the operator asked about, tier 2 is the requirement matrix
/// that turns a runtime version surprise into something they saw coming, and tier 3 is a count per
/// namespace. Dumping several hundred API names would be neither.
fn capability_enumeration_lines(capabilities: &CapabilityEnumeration) -> Vec<String> {
    let mut lines = vec![format!(
        "DSM advertises {} APIs across every installed package",
        capabilities.total
    )];
    if capabilities.unusable_entries > 0 {
        lines.push(format!(
            "{} advertised entries could not be read: DSM described them in a shape the \
             documented API map does not use",
            capabilities.unusable_entries
        ));
    }
    lines.push(format!(
        "File Station APIs offered ({}):",
        capabilities.file_station.len() + capabilities.file_station_truncated
    ));
    for api in &capabilities.file_station {
        let usage = match (api.required, api.optional) {
            (Some(version), true) => format!("required v{version} when used (optional)"),
            (Some(version), false) => format!("required v{version}"),
            (None, _) => "unused by this tool".to_owned(),
        };
        lines.push(format!("  {:<34}{:<16}{usage}", api.name, api.range()));
    }
    if capabilities.file_station_truncated > 0 {
        lines.push(format!(
            "  {} further File Station APIs not listed",
            capabilities.file_station_truncated
        ));
    }
    lines.push("APIs this tool requires:".to_owned());
    for verdict in &capabilities.requirements {
        lines.push(format!(
            "  {:<34}v{:<15}{:<14}{}",
            verdict.requirement.api,
            verdict.requirement.version,
            verdict.describe(),
            verdict.requirement.purpose,
        ));
    }
    if !capabilities.namespaces.is_empty() {
        lines.push("other namespaces, by API count:".to_owned());
        for (namespace, count) in &capabilities.namespaces {
            lines.push(format!("  {namespace} ({count})"));
        }
    }
    if let Some((apis, namespaces)) = capabilities.namespace_overflow {
        lines.push(format!(
            "  others ({apis} APIs across {namespaces} further namespaces)"
        ));
    }
    lines
}

/// The ablation block: one line per variant, then what the four together mean.
fn channel_ablation_lines(probes: &[ChannelProbe; SESSION_CHANNEL_VARIANTS]) -> Vec<String> {
    let mut lines = vec![
        "the same read-only SYNO.FileStation.List.list_share request, varying only how the \
         session is presented"
            .to_owned(),
    ];
    for probe in probes {
        if let Some(reason) = probe.skipped {
            lines.push(format!(
                "  {:<30}{:<48}not run: {reason}",
                probe.channels.as_str(),
                probe.channels.describe(),
            ));
            continue;
        }
        let answer = match (probe.outcome, probe.dsm_code) {
            (RequestOutcome::Ok, _) => "accepted".to_owned(),
            (_, Some(code)) => format!("rejected with DSM {code}"),
            (outcome, None) => format!("failed ({})", outcome.as_str()),
        };
        lines.push(format!(
            "  {:<30}{:<48}{answer} in {} ms",
            probe.channels.as_str(),
            probe.channels.describe(),
            probe.elapsed_ms,
        ));
    }
    lines.push(
        "the token-header-only variant carries no session identifier at all, so DSM is expected \
         to reject it; it is the control that proves this probe can tell acceptance from rejection"
            .to_owned(),
    );
    lines.push(
        "the last variant is the only one that varies how the session was *created* rather than \
         how it is presented: a second login without enable_syno_token, offered the documented \
         _sid request field alone. It is read-only, and it is measured at the very end of the run \
         -- after this run's own session has been logged out -- because DSM binds one session per \
         account and session name, and a duplicate login can interrupt the other. Its own session \
         is logged out immediately afterwards. Its request numbers are therefore higher than the \
         rest of this block's"
            .to_owned(),
    );
    lines
}

fn capability_diagnosis_lines(diagnosis: &CapabilityDiagnosis) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(info) = diagnosis.info {
        // "DSM did not report it" is printed as itself. File Station omits members between DSM
        // releases, and rendering an absent flag as a denial invents a permission verdict.
        lines.push(format!(
            "host {:?} -- this account {} a DSM administrator",
            info.hostname
                .map(|host| host.as_str().to_owned())
                .unwrap_or_else(|| "(not reported)".to_owned()),
            match info.is_manager {
                Some(true) => "is",
                Some(false) => "is not",
                None => "was not reported as",
            },
        ));
        lines.push(format!(
            "sharing links: {}; mountable virtual filesystems: {}",
            match info.support_sharing {
                Some(true) => "supported for this account",
                Some(false) => "not available to this account",
                None => "not reported",
            },
            info.support_virtual_protocol
                .map(|protocols| protocols.as_str().to_owned())
                .unwrap_or_else(|| "none reported".to_owned()),
        ));
    }
    for record in &diagnosis.records {
        lines.push(format!(
            "  {:<34}{:<12}v{:<4}{}{} in {} ms",
            record.api,
            record.method,
            record.version,
            record.verdict.as_str(),
            record
                .dsm_code
                .map(|code| format!(" ({code})"))
                .unwrap_or_default(),
            record.elapsed_ms,
        ));
    }
    lines.push(match (diagnosis.first_hostname, diagnosis.last_hostname) {
        (Some(first), Some(last)) if first.as_str() == last.as_str() => {
            "File Station reported the same host name at the first and last probe of this run. \
             That is consistent with one DSM host, and it is also what a single NAS behind a \
             relay looks like, so it excludes nothing on its own"
                .to_owned()
        }
        (Some(_), Some(_)) => "File Station reported DIFFERENT host names at the first and last \
             probe of this run: consecutive requests demonstrably reached different DSM hosts"
            .to_owned(),
        _ => "File Station's host name could not be read at both ends of this run, so the \
             two-host comparison is unavailable"
            .to_owned(),
    });
    lines.push("advertised but deliberately not probed:".to_owned());
    for reason in &diagnosis.unprobed {
        lines.push(format!("  {reason}"));
    }
    lines
}

fn path_resolution_lines(resolution: &DestinationPathResolution) -> Vec<String> {
    let mut lines = Vec::new();
    for segment in &resolution.segments {
        let state = if !segment.exists {
            match segment.dsm_code {
                // 408 is the only code that means the component is not there. Any other one
                // stopped the walk for a reason of its own, and calling that "absent" would
                // report a dead session as a missing directory.
                Some(408) => "absent (DSM 408)".to_owned(),
                Some(code) => format!("not resolved; DSM {code} stopped the walk here"),
                None => "not resolved".to_owned(),
            }
        } else if segment.mount_boundary {
            "exists, mounted filesystem boundary".to_owned()
        } else if segment.is_directory {
            "exists, directory".to_owned()
        } else {
            "exists, not a directory".to_owned()
        };
        lines.push(format!("{:<48}{state}", segment.path));
    }
    if resolution.segments.len() < resolution.total_components {
        lines.push(format!(
            "{} further component(s) were not inspected: the walk stops at the first one it \
             cannot confirm",
            resolution.total_components - resolution.segments.len()
        ));
    }
    lines
}

fn doctor_human(result: &DoctorResult) -> String {
    let mut human = String::new();
    // The report is what users paste into an issue, so it names the build that produced it.
    writeln!(
        human,
        "{} {} ({}) {}",
        BUILD.name, BUILD.version, BUILD.commit, BUILD.target
    )
    .expect("writing to a String cannot fail");
    writeln!(
        human,
        "Doctor {}: {}",
        result.level.as_str(),
        doctor_overall_status(result).to_ascii_uppercase()
    )
    .expect("writing to a String cannot fail");
    let total_steps = result.sections.len();
    for section in &result.sections {
        // The step number is printed because sections are grouped for reading, not listed in the
        // order they run: capabilities are settled from the discovery response before
        // authentication, yet belong next to the other File Station checks.
        writeln!(
            human,
            "  [{:>4}] step {}/{} {} ({}): {}",
            section.status.as_str().to_ascii_uppercase(),
            section.step,
            total_steps,
            section.label,
            doctor_section_timing(section),
            section.detail,
        )
        .expect("writing to a String cannot fail");
        for call in &section.calls {
            writeln!(
                human,
                "           #{} {}.{} v{} session={} -> {}{}{} in {} ms",
                call.sequence,
                call.api,
                call.method,
                call.version,
                call.session.describe(),
                call.outcome.as_str(),
                call.http_status
                    .map(|status| format!(" http {status}"))
                    .unwrap_or_default(),
                call.dsm_code
                    .map(|code| format!(" dsm {code}"))
                    .unwrap_or_default(),
                call.elapsed_ms,
            )
            .expect("writing to a String cannot fail");
            // A bare `decode` names no next step. The member path and the JSON type do, and both
            // are schema rather than response content, so they can be printed as they are.
            if let Some(fault) = call.decode {
                writeln!(human, "              {}", fault.describe())
                    .expect("writing to a String cannot fail");
            }
        }
        if let Some(remediation) = section.remediation {
            writeln!(human, "         hint: {remediation}")
                .expect("writing to a String cannot fail");
        }
    }
    if let Some(reachability) = &result.reachability {
        write_transport_block(
            &mut human,
            "Network reachability (TCP connect, not ICMP):",
            reachability.human_lines(),
        );
    }
    if let Some(intermediary) = &result.intermediary {
        write_transport_block(
            &mut human,
            "Intermediaries between this client and DSM:",
            intermediary.human_lines(),
        );
    }
    if let Some(cookies) = &result.cookies {
        write_transport_block(
            &mut human,
            "Cookie permanence across the run:",
            cookies.human_lines(),
        );
    }
    if let Some(capabilities) = &result.capabilities {
        write_transport_block(
            &mut human,
            "DSM capability enumeration:",
            capability_enumeration_lines(capabilities),
        );
    }
    if let Some(probes) = &result.channel_ablation {
        write_transport_block(
            &mut human,
            "DSM session channel ablation:",
            channel_ablation_lines(probes),
        );
    }
    if let Some(diagnosis) = &result.capability_diagnosis {
        write_transport_block(
            &mut human,
            "File Station capability diagnosis:",
            capability_diagnosis_lines(diagnosis),
        );
    }
    if let Some(resolution) = &result.path_resolution {
        write_transport_block(
            &mut human,
            "Destination path resolution:",
            path_resolution_lines(resolution),
        );
    }
    if let Some((scope, inventory)) = &result.remote_inventory {
        let scope_description = scope.description();
        writeln!(
            human,
            "Remote inventory: {} {scope_description}; {} sampled; {} truncated (one page, no recursion).",
            inventory.total_entries,
            inventory.sample.len(),
            inventory.truncated_count,
        )
        .expect("writing to a String cannot fail");
        for entry in &inventory.sample {
            writeln!(
                human,
                "  - path={}{}; name={}{}; kind={}; size_bytes={}; mtime_seconds={}; mount_boundary={}",
                entry.relative_path,
                if entry.relative_path_truncated {
                    " (truncated)"
                } else {
                    ""
                },
                entry.name,
                if entry.name_truncated {
                    " (truncated)"
                } else {
                    ""
                },
                entry.kind.as_str(),
                entry
                    .size_bytes
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                entry
                    .mtime_seconds
                    .map(|mtime| mtime.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                entry.mount_boundary,
            )
            .expect("writing to a String cannot fail");
        }
    }
    if !result.failed() && result.authenticated {
        if result.remote_checked {
            writeln!(
                human,
                "Doctor: routing, API discovery, authentication, and remote access are healthy ({} direct entries; destination {}; write permission checked at {} {}).",
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
            )
            .expect("writing to a String cannot fail");
        } else if result
            .remote_inventory
            .as_ref()
            .is_some_and(|(scope, _)| *scope == DoctorInventoryScope::VisibleSharedFolders)
        {
            writeln!(
                human,
                "Doctor: routing, API discovery, authentication, and shared-folder discovery are healthy; no destination was selected or permission-checked."
            )
            .expect("writing to a String cannot fail");
        } else {
            writeln!(
                human,
                "Doctor: routing, API discovery, and authentication are healthy."
            )
            .expect("writing to a String cannot fail");
        }
    } else if !result.failed() {
        writeln!(
            human,
            "Doctor: reverse-proxy routing and File Station API discovery are healthy."
        )
        .expect("writing to a String cannot fail");
    }
    if result.write_probe_requested {
        if !result.write_probe_performed {
            if !result.failed() {
                writeln!(
                    human,
                    "Disposable write probe prerequisites passed; no remote probe mutation was attempted."
                )
                .expect("writing to a String cannot fail");
            }
        } else if result.write_probe_error.is_some() {
            let detail = result
                .sections
                .iter()
                .find(|section| section.id == "disposable_write_verify_cleanup")
                .map(|section| section.detail.as_str())
                .unwrap_or("probe failed");
            writeln!(human, "Disposable write probe failed: {detail}")
                .expect("writing to a String cannot fail");
        } else if let Some(report) = &result.write_probe {
            writeln!(
                human,
                "Disposable write probe passed: directory creation, {}-byte upload with size/MD5/CRC32/SHA-256/mtime verification{}; cleanup completed{}.",
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
            )
            .expect("writing to a String cannot fail");
        }
    }
    human
}

fn write_doctor_output(
    result: &DoctorResult,
    elapsed: Duration,
    output: &config::ResolvedOutput,
) -> Result<()> {
    write_rendered_output(doctor_output(result, elapsed, output.output))
}

fn doctor_output(
    result: &DoctorResult,
    elapsed: Duration,
    format: cli::OutputFormat,
) -> RenderedOutput {
    let value = doctor_value(result, elapsed);
    match format {
        cli::OutputFormat::Human => RenderedOutput::Human(doctor_human(result)),
        cli::OutputFormat::Json => RenderedOutput::Json(value),
        cli::OutputFormat::Ndjson => RenderedOutput::Ndjson(vec![value]),
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

fn doctor_batch_summary_value(
    outcomes: &[DoctorBatchOutcome],
    write_tests: bool,
    all_targets_preflighted_before_mutation: bool,
) -> Value {
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
            && all_targets_preflighted_before_mutation,
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
    all_targets_preflighted_before_mutation: bool,
) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_doctor_batch_output_to(
        &mut stdout,
        outcomes,
        output.output,
        write_tests,
        all_targets_preflighted_before_mutation,
    )
}

fn write_doctor_batch_output_to<W: Write>(
    writer: &mut W,
    outcomes: &[DoctorBatchOutcome],
    format: cli::OutputFormat,
    write_tests: bool,
    all_targets_preflighted_before_mutation: bool,
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
    let summary = doctor_batch_summary_value(
        outcomes,
        write_tests,
        all_targets_preflighted_before_mutation,
    );
    match format {
        cli::OutputFormat::Human => {
            writeln!(
                writer,
                "Target diagnostic batch: status {status}; {succeeded} succeeded, {preflighted} preflighted, {partial} potentially partial, {failed} failed before a probe could mutate, {not_run} not run."
            )
            .map_err(output_error)?;
            for outcome in outcomes {
                writeln!(writer, "\n[{}] {}", outcome.name, outcome.status.as_str())
                    .map_err(output_error)?;
                if let Some(result) = &outcome.result {
                    write!(writer, "{}", doctor_human(&result.result)).map_err(output_error)?;
                }
                if let Some(error) = &outcome.error {
                    writeln!(writer, "Error: {error}").map_err(output_error)?;
                }
            }
            writer.flush().map_err(output_error)
        }
        cli::OutputFormat::Json => {
            let job_values = outcomes
                .iter()
                .map(doctor_batch_job_value)
                .collect::<Vec<_>>();
            let mut summary = summary;
            summary["jobs"] = Value::Array(job_values);
            write_json_to(writer, &summary)
        }
        cli::OutputFormat::Ndjson => {
            for outcome in outcomes {
                write_json_line_to(writer, &doctor_batch_job_value(outcome))?;
            }
            write_json_line_to(writer, &summary)
        }
    }
}

#[cfg(test)]
fn doctor_batch_output(
    outcomes: &[DoctorBatchOutcome],
    format: cli::OutputFormat,
    write_tests: bool,
    all_targets_preflighted_before_mutation: bool,
) -> RenderedOutput {
    let mut buffer = Vec::new();
    write_doctor_batch_output_to(
        &mut buffer,
        outcomes,
        format,
        write_tests,
        all_targets_preflighted_before_mutation,
    )
    .expect("writing rendered doctor batch output to a Vec cannot fail");
    captured_rendered_output(format, buffer)
}

fn write_credential_output(
    result: credentials::CredentialOutcome,
    format: cli::OutputFormat,
) -> Result<()> {
    write_rendered_output(credential_output(result, format))
}

fn credential_output(
    result: credentials::CredentialOutcome,
    format: cli::OutputFormat,
) -> RenderedOutput {
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
    simple_output(format, human, value)
}

fn write_simple_value(format: cli::OutputFormat, human: String, value: Value) -> Result<()> {
    write_rendered_output(simple_output(format, human, value))
}

#[derive(Debug, PartialEq)]
enum RenderedOutput {
    Human(String),
    Json(Value),
    Ndjson(Vec<Value>),
}

#[cfg(test)]
fn captured_rendered_output(format: cli::OutputFormat, output: Vec<u8>) -> RenderedOutput {
    match format {
        cli::OutputFormat::Human => RenderedOutput::Human(
            String::from_utf8(output).expect("captured human output is UTF-8"),
        ),
        cli::OutputFormat::Json => RenderedOutput::Json(
            serde_json::from_slice(&output).expect("captured JSON output is valid"),
        ),
        cli::OutputFormat::Ndjson => RenderedOutput::Ndjson(parse_ndjson_output(&output)),
    }
}

#[cfg(test)]
fn parse_ndjson_output(output: &[u8]) -> Vec<Value> {
    String::from_utf8(output.to_vec())
        .expect("captured NDJSON output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("captured NDJSON record is valid"))
        .collect()
}

fn simple_output(format: cli::OutputFormat, human: String, value: Value) -> RenderedOutput {
    match format {
        cli::OutputFormat::Human => RenderedOutput::Human(format!("{human}\n")),
        cli::OutputFormat::Json => RenderedOutput::Json(value),
        cli::OutputFormat::Ndjson => RenderedOutput::Ndjson(vec![value]),
    }
}

fn write_rendered_output(output: RenderedOutput) -> Result<()> {
    match output {
        RenderedOutput::Human(value) => {
            print!("{value}");
            io::stdout().flush().map_err(output_error)
        }
        RenderedOutput::Json(value) => write_json(&value),
        RenderedOutput::Ndjson(values) => {
            for value in values {
                write_json_line(&value)?;
            }
            Ok(())
        }
    }
}

fn write_json(value: &Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    write_json_to(&mut stdout, value)
}

fn write_json_to<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    serde_json::to_writer_pretty(&mut *writer, value)
        .map_err(|error| Error::Message(format!("failed to write JSON output: {error}")))?;
    writeln!(writer).map_err(output_error)
}

fn write_json_line(value: &Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    write_json_line_to(&mut stdout, value)
}

fn write_json_line_to<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| Error::Message(format!("failed to write JSON output: {error}")))?;
    writeln!(writer).map_err(output_error)
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

/// The scope a run is restricted to, or the whole tree when none was given.
fn resolved_scope(settings: &config::ResolvedSync) -> Result<plan::Scope> {
    match settings.behavior.scope.as_deref() {
        Some(value) => plan::Scope::parse(value),
        None => Ok(plan::Scope::root()),
    }
}

/// The comparison a run actually plans with.
///
/// `Force` is reachable only through `resync`, which sets `force_resync` on settings whose plan the
/// caller has already been shown and confirmed. It is deliberately not a `CompareArg`, so neither a
/// command-line comparison nor a configuration profile can express it.
fn effective_compare_mode(settings: &config::ResolvedSync) -> CompareMode {
    if settings.behavior.force_resync {
        CompareMode::Force
    } else {
        compare_mode(settings.behavior.compare)
    }
}

/// The comparison a post-run reconciliation verifies convergence with.
///
/// Never `Force`: a forced re-plan schedules every file unconditionally, so reconciling against it
/// would always report pending work. Reconciliation asks whether the destination has converged,
/// which is a question about content, so it uses the ordinary comparison throughout.
fn reconciliation_compare_mode(settings: &config::ResolvedSync) -> CompareMode {
    compare_mode(settings.behavior.compare)
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
        Error::Cancelled => CANCELLED_EXIT_CODE,
        Error::Configuration(_)
        | Error::InvalidUrl(_)
        | Error::HttpsRequired
        | Error::UnsafeRemotePath { .. } => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use synology_drive_sync::api::{DiscoveredApi, SessionChannels};

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

    fn resolved_output(format: cli::OutputFormat) -> config::ResolvedOutput {
        config::ResolvedOutput {
            verbosity: 0,
            quiet: true,
            log_level: cli::LogLevel::Off,
            log_format: cli::LogFormat::Human,
            log_file: None,
            remote_log_url: None,
            remote_log_token: None,
            remote_log_mode: cli::RemoteLogMode::BestEffort,
            progress: cli::ProgressMode::Never,
            output: format,
        }
    }

    fn resolved_authentication(password_stdin: bool) -> config::ResolvedAuthentication {
        config::ResolvedAuthentication {
            password_stdin,
            password_file: None,
            totp_secret_file: None,
            no_vault: true,
        }
    }

    fn resolved_network() -> config::ResolvedNetwork {
        config::ResolvedNetwork {
            retries: 0,
            timeout: 30,
            connect_timeout: 2,
            max_rate: None,
            ca_certificate: None,
            allow_http: false,
            danger_accept_invalid_certs: false,
        }
    }

    /// A stand-in DSM that answers exactly one API discovery request and stops, which is the
    /// least `connect_client` needs to hand back a client.
    fn discovery_only_server() -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let received = stream.read(&mut request).unwrap();
            assert!(received > 0, "the client must send a discovery request");
            let entry = serde_json::json!({"path": "entry.cgi", "minVersion": 1, "maxVersion": 7});
            let body = serde_json::json!({
                "success": true,
                "data": {
                    "SYNO.API.Auth": entry,
                    "SYNO.FileStation.List": entry,
                    "SYNO.FileStation.CreateFolder": entry,
                    "SYNO.FileStation.Upload": entry,
                    "SYNO.FileStation.CheckPermission": entry,
                }
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            stream.flush().unwrap();
        });
        (format!("http://{address}/prefix/"), handle)
    }

    /// The resolved limit has to reach the client. A flag that parses and validates and is then
    /// dropped on the way to the transfer is worse than no flag at all, so this asserts the
    /// value arrives rather than merely that it was accepted.
    #[test]
    fn connect_client_applies_the_resolved_upload_rate_limit() {
        let (url, server) = discovery_only_server();
        let limited = config::ResolvedNetwork {
            max_rate: Some(65536),
            allow_http: true,
            ..resolved_network()
        };
        let client = connect_client(&url, &limited, &CancellationToken::default(), None).unwrap();
        assert_eq!(client.max_upload_rate(), Some(65536));
        server.join().unwrap();

        // Unset must stay unset: no default may be invented on the way to the client.
        let (url, server) = discovery_only_server();
        let unlimited = config::ResolvedNetwork {
            allow_http: true,
            ..resolved_network()
        };
        let client = connect_client(&url, &unlimited, &CancellationToken::default(), None).unwrap();
        assert_eq!(client.max_upload_rate(), None);
        server.join().unwrap();
    }

    fn resolved_sync(url: &str, username: &str, remote: &str) -> config::ResolvedSync {
        config::ResolvedSync {
            source: PathBuf::from("source"),
            remote: remote.to_owned(),
            connection: config::ResolvedConnection {
                url: url.to_owned(),
                username: username.to_owned(),
            },
            authentication: resolved_authentication(false),
            behavior: config::ResolvedSyncBehavior {
                compare: cli::CompareArg::Content,
                jobs: 2,
                excludes: Vec::new(),
                scope: None,
                force_resync: false,
            },
            safety: config::ResolvedSafety {
                delete: false,
                allow_empty_source: false,
                max_delete: 10,
            },
            network: resolved_network(),
            output: resolved_output(cli::OutputFormat::Json),
        }
    }

    fn resolved_doctor(
        url: &str,
        username: Option<&str>,
        remote: Option<&str>,
    ) -> config::ResolvedDoctor {
        config::ResolvedDoctor {
            remote: remote.map(str::to_owned),
            routing_only: false,
            level: cli::DoctorLevel::Standard,
            compare: cli::CompareArg::Content,
            delete: false,
            write_test: false,
            url: url.to_owned(),
            username: username.map(str::to_owned),
            authentication: resolved_authentication(false),
            network: resolved_network(),
            output: resolved_output(cli::OutputFormat::Json),
        }
    }

    fn local_file(relative: &str, size: u64, mtime_ms: i64) -> LocalEntry {
        LocalEntry {
            relative: relative.to_owned(),
            full_path: PathBuf::from("source").join(relative),
            kind: local::EntryKind::File,
            size,
            mtime_ms,
            identity: Default::default(),
            content_md5: Some(synology_drive_sync::integrity::ContentMd5::from_digests(
                [0x2a; 16],
                0x2a2a_2a2a,
                [0x2a; 32],
            )),
        }
    }

    fn rich_plan() -> SyncPlan {
        let digest = synology_drive_sync::integrity::ContentMd5::from_digests(
            [0x2a; 16],
            0x2a2a_2a2a,
            [0x2a; 32],
        );
        let copied = local_file("copied.bin", 7, 1_700_000_000_250);
        let uploaded = local_file("uploaded.bin", 11, 1_700_000_000_500);
        let guarded_local = local_file("guarded.bin", 13, 1_700_000_001_000);
        let file_snapshot = RemoteSnapshot {
            kind: local::EntryKind::File,
            size: 7,
            mtime_seconds: 1_700_000_000,
            content_md5: Some(digest),
            require_mtime: true,
        };
        SyncPlan {
            pre_deletes: vec![plan::DeleteAction {
                relative: "conflict".to_owned(),
                remote_path: "/share/root/conflict".to_owned(),
                kind: local::EntryKind::Directory,
                type_conflict: true,
                snapshot: RemoteSnapshot {
                    kind: local::EntryKind::Directory,
                    size: 0,
                    mtime_seconds: 1_700_000_000,
                    content_md5: None,
                    require_mtime: false,
                },
                destination_guard: None,
            }],
            creates: vec![plan::CreateAction {
                relative: "new-directory".to_owned(),
                remote_path: "/share/root/new-directory".to_owned(),
                reason: plan::ChangeReason::MissingRemote,
            }],
            copies: vec![plan::CopyAction {
                from_relative: "source/copied.bin".to_owned(),
                from_remote_path: "/share/root/source/copied.bin".to_owned(),
                to_relative: copied.relative.clone(),
                to_remote_path: "/share/root/copied.bin".to_owned(),
                local: copied,
                expected_size: 7,
                content_md5: digest,
                source_snapshot: file_snapshot.clone(),
            }],
            uploads: vec![plan::UploadAction {
                local: uploaded,
                remote_path: "/share/root/uploaded.bin".to_owned(),
                reason: plan::ChangeReason::ContentDiffers,
            }],
            post_deletes: vec![plan::DeleteAction {
                relative: "old/guarded.bin".to_owned(),
                remote_path: "/share/root/old/guarded.bin".to_owned(),
                kind: local::EntryKind::File,
                type_conflict: false,
                snapshot: file_snapshot,
                destination_guard: Some(plan::DestinationGuard {
                    remote_path: "/share/root/guarded.bin".to_owned(),
                    local: guarded_local,
                    expected_size: 13,
                    expected_mtime_seconds: 1_700_000_001,
                    content_md5: digest,
                }),
            }],
            unchanged_files: 3,
            protected_entries: 2,
            upload_bytes: 11,
        }
    }

    fn loaded_profiles(names: &[&str]) -> config::LoadedConfig {
        let profiles = names
            .iter()
            .map(|name| ((*name).to_owned(), config::Profile::default()))
            .collect::<BTreeMap<_, _>>();
        config::LoadedConfig {
            path: PathBuf::from("config.toml"),
            values: config::ConfigFile {
                default_profile: names.first().map(|name| (*name).to_owned()),
                profiles,
            },
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

    /// The transport report as it reads on a QuickConnect relay that rotates the session.
    ///
    /// This is the shape of the live investigation the transport checks were built for: a relay
    /// hostname, a name that resolves to more than one address, a bimodal connect time, two
    /// different `Server` banners inside one run, and a session cookie re-issued on a call that
    /// succeeded. Every one of those is a separate piece of evidence, and the report has to put
    /// all of them where an operator can read them in one pass.
    #[test]
    fn the_human_report_lays_out_the_relay_evidence_an_operator_needs() {
        use synology_drive_sync::observability::{
            ApiCallDetail, BoundedText, CdnMarker, CookieFact, CookieFacts, CookiePersistence,
            CookieSameSite, IntermediaryFacts, RequestTransport, ShortToken,
        };
        use synology_drive_sync::transport_diagnostics::{
            DnsObservation, EndpointForm, HttpTimingObservation, LatencySamples, ReachabilityReport,
        };

        let describe = |name: &str, fingerprint: u32| CookieFact {
            name: ShortToken::sanitized(name),
            fingerprint,
            value_length: 43,
            persistence: CookiePersistence::Session,
            secure: true,
            http_only: true,
            same_site: CookieSameSite::Lax,
            path_present: true,
            domain_present: false,
            expires_present: false,
            max_age_present: false,
        };
        let call =
            |api: &'static str, method: &'static str, banner: &str, cookie: Option<CookieFact>| {
                let mut call = ApiCallDetail::started(
                    api,
                    method,
                    2,
                    BoundedText::sanitized("/webapi/entry.cgi"),
                    RequestTransport::Form,
                );
                call.outcome = RequestOutcome::Ok;
                call.http_status = Some(200);
                call.intermediary = IntermediaryFacts {
                    via: ShortToken::sanitized("1.1 quickconnect-relay"),
                    server: ShortToken::sanitized(banner),
                    forwarded_for_reflected: true,
                    cdn_marker: CdnMarker::Other,
                    ..IntermediaryFacts::default()
                };
                if let Some(cookie) = cookie {
                    let mut cookies = CookieFacts::default();
                    cookies.push(cookie);
                    call.cookies = cookies;
                }
                call
            };

        let mut transcript = TransportTranscript::default();
        transcript.record_call(&call("SYNO.API.Info", "query", "nginx", None));
        transcript.record_call(&call(
            "SYNO.API.Auth",
            "login",
            "nginx",
            Some(describe("id", 0x1111_1111)),
        ));
        transcript.record_call(&call(
            "SYNO.FileStation.List",
            "list_share",
            "nginx",
            Some(describe("id", 0x2222_2222)),
        ));
        transcript.record_call(&call("SYNO.FileStation.List", "getinfo", "Apache", None));

        let mut result = doctor_result(false, None, None);
        // One address answers quickly and the other does not, which is what a name fanned out
        // across two relays looks like from here.
        let samples = |values: &[u64]| {
            let mut samples = LatencySamples::default();
            for micros in values {
                samples.push(Duration::from_micros(*micros));
            }
            samples
        };
        let near = samples(&[23_100, 24_400, 23_800]);
        let far = samples(&[118_900, 121_600]);
        let tcp = samples(&[23_100, 24_400, 118_900, 23_800, 121_600]);
        let mut first_byte = LatencySamples::default();
        let mut body = LatencySamples::default();
        let mut total = LatencySamples::default();
        for (headers, payload) in [(96_200_u64, 3_100_u64), (191_400, 3_400), (98_700, 3_000)] {
            first_byte.push(Duration::from_micros(headers));
            body.push(Duration::from_micros(payload));
            total.push(Duration::from_micros(headers + payload));
        }
        let reachability = ReachabilityReport {
            host: "nascheckoffice.fr3.quickconnect.to".to_owned(),
            port: 443,
            tls: true,
            dns: DnsObservation {
                elapsed: Some(Duration::from_micros(18_400)),
                address_count: 2,
                ipv4_count: 2,
                ipv6_count: 0,
                literal: false,
                error: None,
            },
            tcp_per_address: vec![near, far],
            tcp_connect: tcp,
            tcp_failures: 0,
            tcp_failure_reason: None,
            http: HttpTimingObservation {
                first_byte,
                body,
                total,
                statuses: [200].into_iter().collect(),
                failures: 0,
                failure_reason: None,
                timed_out: false,
                request_timeout: Some(Duration::from_secs(12)),
                route: Some("entry.cgi"),
                routes_rejected: 0,
            },
            budget_exhausted: false,
            cancelled: false,
        };
        assert_eq!(
            classify_endpoint("nascheckoffice.fr3.quickconnect.to").form,
            EndpointForm::QuickConnectRelay
        );

        // The real recorders, not hand-set fields: what the operator reads is what the run
        // produces, including the statuses and hints the recorders decide on.
        *result
            .call_log
            .transcript
            .lock()
            .expect("transport transcript lock") = transcript;
        record_reachability_section(
            &mut result,
            reachability,
            ReachabilityBudget::extensive(),
            Duration::from_millis(742),
        );
        record_transport_summary_sections(
            &mut result,
            "https://nascheckoffice.fr3.quickconnect.to/",
        );

        let reachability_section = result
            .sections
            .iter()
            .find(|section| section.id == "network_reachability")
            .expect("the reachability section");
        assert_eq!(reachability_section.status, DoctorSectionStatus::Warn);
        assert_eq!(reachability_section.step, 1);
        let intermediary_section = result
            .sections
            .iter()
            .find(|section| section.id == "intermediary_transport")
            .expect("the intermediary section");
        assert_eq!(intermediary_section.status, DoctorSectionStatus::Warn);
        let ledger_section = result
            .sections
            .iter()
            .find(|section| section.id == "session_cookie_ledger")
            .expect("the ledger section");
        assert_eq!(ledger_section.status, DoctorSectionStatus::Warn);

        let human = doctor_human(&result);

        // Reachability: TCP, said in those words, with the spread called out.
        assert!(human.contains("Network reachability (TCP connect, not ICMP):"));
        assert!(human.contains("2 addresses"));
        assert!(human.contains("consecutive connections can therefore land on different hosts"));
        assert!(human.contains("TCP connect: min 23.1 ms / median 24.4 ms / max 121.6 ms"));
        // The per-address split is the shape of the answer: one relay near, one far.
        assert!(human.contains("address 1 of 2: min 23.1 ms / median 23.8 ms / max 24.4 ms"));
        assert!(human.contains("address 2 of 2: min 118.9 ms / median 120.2 ms / max 121.6 ms"));
        // The unmeasurable phase is named rather than divided by a guess.
        assert!(human.contains("not separable"));
        assert!(human.contains("a derived remainder, not a measured phase"));

        // Intermediary: the relay form, and two origins inside one run.
        assert!(human.contains("QuickConnect relay hostname (relay id fr3)"));
        assert!(human.contains("MORE THAN ONE Server banner across this run"));
        assert!(human.contains("reflected client-address headers"));

        // Cookies: the rotation, where it happened, and what became of the new value.
        assert!(human.contains("ROTATED ON A SUCCESSFUL RESPONSE"));
        assert!(human.contains("call 3 (SYNO.FileStation.List.list_share)"));
        assert!(human.contains("a session cookie"));
        assert!(human.contains("Secure, HttpOnly, SameSite=lax, Path"));
        assert!(human.contains("keeps no cookie jar"));

        // The digest is the only thing the report knows about a value, and it is not the value.
        assert!(!human.contains("11111111"));
    }

    fn doctor_result(
        write_probe_performed: bool,
        write_probe: Option<WriteProbeReport>,
        write_probe_error: Option<&str>,
    ) -> DoctorResult {
        let mut result = DoctorResult::new(
            &resolved_doctor(
                "https://files.example.test",
                Some("doctor-user"),
                Some("/share/acceptance"),
            ),
            true,
            DoctorCallLog::default(),
        );
        result.authenticated = true;
        result.remote_checked = true;
        result.remote_exists = Some(true);
        result.remote_entries = Some(4);
        result.write_permission_scope = Some("exact_destination");
        result.write_permission_path = Some("/share/acceptance".to_owned());
        result.write_probe_requested = true;
        result.write_probe_performed = write_probe_performed;
        result.write_probe = write_probe;
        result.write_probe_error = write_probe_error.map(str::to_owned);
        if result.write_probe_error.is_some() {
            result.failure = result.write_probe_error.clone();
        }
        result
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

    fn source_result() -> TimedSourceDiagnostic {
        let root = PathBuf::from("canonical-source");
        TimedSourceDiagnostic {
            report: SourceDiagnosticReport {
                canonical_root: root.clone(),
                entries: 3,
                files: 2,
                directories: 1,
                bytes: 1_536,
                hashed_files: 2,
                inventory: local::LocalInventory {
                    root,
                    entries: BTreeMap::new(),
                },
            },
            hash_content: true,
            elapsed: Duration::from_millis(9),
        }
    }

    use synology_drive_sync::transport_diagnostics::{
        DnsObservation, HttpTimingObservation, LatencySamples,
    };

    /// A reachability report with TCP samples and whatever HTTP outcome a case needs.
    fn reachability_fixture(addresses: usize, http: HttpTimingObservation) -> ReachabilityReport {
        let mut tcp = LatencySamples::default();
        for micros in [21_000_u64, 21_400, 20_800] {
            tcp.push(Duration::from_micros(micros));
        }
        ReachabilityReport {
            host: "nascheckoffice.fr3.quickconnect.to".to_owned(),
            port: 443,
            tls: true,
            dns: DnsObservation {
                elapsed: Some(Duration::from_micros(18_400)),
                address_count: addresses,
                ipv4_count: addresses,
                ipv6_count: 0,
                literal: false,
                error: None,
            },
            tcp_per_address: vec![tcp.clone(); addresses],
            tcp_connect: tcp,
            tcp_failures: 0,
            tcp_failure_reason: None,
            http,
            budget_exhausted: false,
            cancelled: false,
        }
    }

    /// A `decode` outcome has to explain itself where the operator reads the failing call.
    ///
    /// The fixture is the record a live DSM produced: `getinfo` on a path that does not exist,
    /// whose single entry carries a per-entry `408` and therefore no `name`. Printing only
    /// `decode` there cost a round trip with the operator to learn which member had disagreed.
    #[test]
    fn a_decode_failure_is_explained_beside_the_call_that_failed() {
        use synology_drive_sync::observability::{DecodeFaultKind, JsonKind, ShortToken};

        let call = DoctorCall {
            sequence: 25,
            api: "SYNO.FileStation.List",
            method: "getinfo",
            version: 2,
            outcome: RequestOutcome::Decode,
            dsm_code: None,
            http_status: Some(200),
            session: SessionTransport::default(),
            elapsed_ms: 174,
            decode: Some(DecodeFault {
                kind: DecodeFaultKind::MissingField,
                path: BoundedText::sanitized("data.files.0.name"),
                field: ShortToken::sanitized("name"),
                expected: ShortToken::default(),
                found: JsonKind::Absent,
                line: 1,
                column: 76,
            }),
        };

        let value = doctor_call_value(&call);
        assert_eq!(value["outcome"], "decode");
        assert_eq!(value["decode"]["kind"], "missing-field");
        assert_eq!(value["decode"]["path"], "data.files.0.name");
        assert_eq!(value["decode"]["found"], "absent");

        let mut result = routing_doctor_result();
        result
            .sections
            .iter_mut()
            .find(|section| section.id == "dsm_api_discovery")
            .expect("a section to attach the call to")
            .calls = vec![call];
        let human = doctor_human(&result);
        assert!(
            human.contains("#25 SYNO.FileStation.List.getinfo v2"),
            "the call itself must still render: {human}"
        );
        assert!(
            human.contains("missing-field at data.files.0.name"),
            "the decode reason belongs next to the call: {human}"
        );
        // A call that decoded fine adds no line, so the common case is not noisier for this.
        let mut clean = call;
        clean.outcome = RequestOutcome::Ok;
        clean.decode = None;
        let mut result = routing_doctor_result();
        result
            .sections
            .iter_mut()
            .find(|section| section.id == "dsm_api_discovery")
            .expect("a section to attach the call to")
            .calls = vec![clean];
        assert!(!doctor_human(&result).contains("missing-field"));
    }

    /// Each reachability finding gets the hint that explains *it*.
    ///
    /// A QuickConnect hostname resolves to several addresses, which makes
    /// `suggests_multiple_paths` true on nearly every relayed run. Gating the session-affinity
    /// hint on that condition alone -- rather than on the branch that was taken -- attached it to
    /// "no HTTP sample completed", sending an operator after a session problem on the strength of
    /// a probe that had merely timed out.
    #[test]
    fn a_reachability_hint_is_chosen_by_the_finding_it_explains() {
        let mut first_byte = LatencySamples::default();
        for micros in [96_200_u64, 97_100, 96_800] {
            first_byte.push(Duration::from_micros(micros));
        }
        let case = |report: ReachabilityReport| {
            let mut result = routing_doctor_result();
            record_reachability_section(
                &mut result,
                report,
                ReachabilityBudget::extensive(),
                Duration::from_millis(742),
            );
            let section = result
                .sections
                .iter()
                .find(|section| section.id == "network_reachability")
                .expect("the reachability section")
                .clone();
            (section.status, section.detail.clone(), section.remediation)
        };

        // Two addresses and no HTTP sample: a transport finding, and a transport hint.
        let timed_out = reachability_fixture(
            2,
            HttpTimingObservation {
                failures: 2,
                failure_reason: Some(
                    "error sending request for url (https://host/webapi/entry.cgi): operation \
                     timed out"
                        .to_owned(),
                ),
                timed_out: true,
                request_timeout: Some(Duration::from_secs(12)),
                route: Some("entry.cgi"),
                ..HttpTimingObservation::default()
            },
        );
        let (status, detail, remediation) = case(timed_out.clone());
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("no HTTP sample completed"));
        assert!(
            detail.contains("within the 12.0 s probe ceiling"),
            "the ceiling the samples ran under belongs in the finding: {detail}"
        );
        assert_eq!(remediation, Some(DOCTOR_PROBE_TIMEOUT_HINT));
        assert_ne!(
            remediation,
            Some(DOCTOR_MULTIPLE_PATH_HINT),
            "a timed-out probe says nothing about session affinity"
        );

        // The same finding, refused rather than timed out: a different transport hint.
        let mut refused = timed_out;
        refused.http.timed_out = false;
        refused.http.failure_reason = Some("connection closed before message completed".to_owned());
        let (_, detail, remediation) = case(refused);
        assert!(!detail.contains("probe ceiling"));
        assert_eq!(remediation, Some(DOCTOR_PROBE_TRANSPORT_HINT));

        // HTTP answers, and the several addresses are now the finding: the affinity hint earns it.
        let answered = reachability_fixture(
            2,
            HttpTimingObservation {
                first_byte: first_byte.clone(),
                statuses: [200].into_iter().collect(),
                request_timeout: Some(Duration::from_secs(12)),
                route: Some("entry.cgi"),
                ..HttpTimingObservation::default()
            },
        );
        let (status, detail, remediation) = case(answered);
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("resolves to 2 addresses"));
        assert_eq!(remediation, Some(DOCTOR_MULTIPLE_PATH_HINT));

        // One address and a clean answer: nothing to say.
        let healthy = reachability_fixture(
            1,
            HttpTimingObservation {
                first_byte,
                statuses: [200].into_iter().collect(),
                request_timeout: Some(Duration::from_secs(12)),
                route: Some("entry.cgi"),
                ..HttpTimingObservation::default()
            },
        );
        let (status, _, remediation) = case(healthy.clone());
        assert_eq!(status, DoctorSectionStatus::Pass);
        assert_eq!(remediation, None);

        // The same clean evidence, from a probe that stopped at its own ceiling. Nothing here
        // points at a fault -- one address, samples that agree -- which is exactly why this used
        // to report PASS: `connects_but_does_not_answer` requires `failures > 0`, so a probe that
        // ran short without failing anything fell through to the consistent verdict and claimed a
        // path it had not finished measuring. The transport block underneath said it had stopped
        // early, so the run contradicted itself and the headline was the half most people read.
        let mut stopped_early = healthy;
        stopped_early.budget_exhausted = true;
        stopped_early.http.first_byte = {
            let mut taken = LatencySamples::default();
            taken.push(Duration::from_micros(96_200));
            taken
        };
        let (status, detail, remediation) = case(stopped_early);
        assert_eq!(
            status,
            DoctorSectionStatus::Warn,
            "a probe that ran out of budget must not report a consistent path: {detail}"
        );
        assert!(
            detail.contains("20 s ceiling"),
            "the finding names the ceiling that stopped it: {detail}"
        );
        assert!(
            detail.contains("1 of 3 HTTP sample"),
            "and how far short it fell: {detail}"
        );
        assert_eq!(remediation, Some(DOCTOR_PROBE_BUDGET_HINT));
        assert_ne!(
            remediation,
            Some(DOCTOR_PROBE_TRANSPORT_HINT),
            "the probe stopping itself is not evidence of a fault on the path"
        );
    }

    fn routing_doctor_result() -> DoctorResult {
        let mut settings = resolved_doctor("https://files.example.test", None, None);
        settings.routing_only = true;
        settings.level = cli::DoctorLevel::Quick;
        let mut result = DoctorResult::new(&settings, true, DoctorCallLog::default());
        result.set_section(
            "routing_tls",
            DoctorSectionStatus::Pass,
            "HTTPS route responded",
            Duration::from_millis(3),
            "shared_connection",
        );
        result.set_section(
            "dsm_api_discovery",
            DoctorSectionStatus::Pass,
            "DSM APIs discovered",
            Duration::from_millis(3),
            "shared_connection",
        );
        result.set_section(
            "file_station_capabilities",
            DoctorSectionStatus::Pass,
            "baseline File Station APIs available",
            Duration::ZERO,
            "section",
        );
        result
    }

    fn rendered_human(output: RenderedOutput) -> String {
        match output {
            RenderedOutput::Human(value) => value,
            other => panic!("expected human output, got {other:?}"),
        }
    }

    fn rendered_json(output: RenderedOutput) -> Value {
        match output {
            RenderedOutput::Json(value) => value,
            other => panic!("expected JSON output, got {other:?}"),
        }
    }

    fn rendered_ndjson(output: RenderedOutput) -> Vec<Value> {
        match output {
            RenderedOutput::Ndjson(values) => values,
            other => panic!("expected NDJSON output, got {other:?}"),
        }
    }

    #[test]
    fn quick_doctor_does_not_inherit_authenticated_profile_capabilities() {
        assert!(!doctor_requires_content_fingerprint(
            cli::DoctorLevel::Quick,
            cli::CompareArg::Content
        ));
        assert!(!doctor_requires_delete_capability(
            cli::DoctorLevel::Quick,
            true
        ));

        assert!(doctor_requires_content_fingerprint(
            cli::DoctorLevel::Standard,
            cli::CompareArg::Content
        ));
        assert!(!doctor_requires_content_fingerprint(
            cli::DoctorLevel::Standard,
            cli::CompareArg::Metadata
        ));
        assert!(doctor_requires_delete_capability(
            cli::DoctorLevel::Standard,
            true
        ));

        assert!(doctor_requires_content_fingerprint(
            cli::DoctorLevel::Extensive,
            cli::CompareArg::Metadata
        ));
        assert!(doctor_requires_delete_capability(
            cli::DoctorLevel::Extensive,
            false
        ));
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
        assert_eq!(
            CANCELLED_LOGGER_SHUTDOWN_TIMEOUT,
            Duration::from_millis(500)
        );
    }

    /// A run that completed keeps the full delivery window; one the operator stopped does not
    /// get to spend another five seconds flushing after the cancellation was already observed.
    #[test]
    fn a_cancelled_run_shortens_the_observability_flush_window() {
        assert_eq!(
            logger_shutdown_timeout(&Ok::<u8, Error>(0)),
            LOGGER_SHUTDOWN_TIMEOUT
        );
        assert_eq!(
            logger_shutdown_timeout(&Err::<u8, Error>(Error::Message("failed".to_owned()))),
            LOGGER_SHUTDOWN_TIMEOUT
        );
        assert_eq!(
            logger_shutdown_timeout(&Err::<u8, Error>(Error::Cancelled)),
            CANCELLED_LOGGER_SHUTDOWN_TIMEOUT
        );
    }

    /// `ctrlc::set_handler` refuses a second installation for the life of the process, so every
    /// subcommand has to share one handler and one token. A later call must hand back the token
    /// the handler already cancels rather than failing or, worse, returning a token no signal
    /// will ever reach.
    #[test]
    fn the_termination_handler_is_installed_once_and_shares_one_token() {
        let first = install_cancellation_handler().expect("first installation succeeds");
        assert!(!first.is_cancelled());
        let second = install_cancellation_handler().expect("a second installation is a no-op");
        assert!(!second.is_cancelled());

        // What the handler cancels is what every subcommand polls, and it maps to exit 130.
        first.cancel();
        assert!(second.is_cancelled());
        assert_eq!(
            error_exit_code(&second.check().unwrap_err()),
            CANCELLED_EXIT_CODE
        );
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

        let summary = doctor_batch_summary_value(&outcomes, true, true);
        assert_eq!(summary["schema"], "sdsync.doctor-batch.v1");
        assert_eq!(summary["status"], "partial");
        assert_eq!(summary["execution"], "sequential");
        assert_eq!(summary["write_tests_requested"], true);
        assert_eq!(summary["all_targets_preflighted_before_mutation"], true);
        assert_eq!(summary["summary"]["succeeded"], 1);
        assert_eq!(summary["summary"]["partial"], 1);
        assert_eq!(summary["summary"]["not_run"], 1);

        let failed_preflight = vec![doctor_outcome(
            "delta",
            DoctorBatchStatus::Failed,
            Some(doctor_result(true, None, Some("preflight failed"))),
        )];
        assert!(!all_doctor_targets_preflighted(&failed_preflight, false));
        let summary = doctor_batch_summary_value(&failed_preflight, true, false);
        assert_eq!(summary["status"], "failed");
        assert_eq!(summary["all_targets_preflighted_before_mutation"], false);

        let non_mutating = doctor_batch_summary_value(&outcomes, false, true);
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

    fn starter_directory(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sdsync-starter-{}-{name}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create isolated starter directory");
        root
    }

    #[test]
    fn the_starter_configuration_is_the_shipped_example_and_parses_cleanly() {
        let root = starter_directory("valid");
        let path = root.join("config.toml");
        assert!(!write_starter_configuration(&path, false).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), STARTER_CONFIGURATION);
        assert_eq!(
            STARTER_CONFIGURATION,
            include_str!("../config.example.toml"),
            "config init must hand out the documented example verbatim"
        );

        // A starter a user cannot validate is not a starter.
        let loaded = config::LoadedConfig::load(&path).expect("the starter must parse");
        for profile in loaded.values.profiles.values() {
            config::validate_profile(profile).expect("every starter profile must validate");
        }
        assert!(!loaded.values.profiles.is_empty());

        fs::remove_dir_all(&root).ok();
    }

    /// The starter's commented-out options are documentation, not configuration: nothing parses
    /// them, so a key renamed in the schema or misspelled here would drift silently until a user
    /// uncommented it and got a rejection. Parse each one on its own -- individually, because
    /// several are mutually exclusive by design and would fail validation together.
    #[test]
    fn every_commented_starter_option_still_matches_the_schema() {
        let commented = STARTER_CONFIGURATION
            .lines()
            .filter_map(|line| line.strip_prefix("# "))
            .filter(|line| {
                let Some((key, _)) = line.split_once(" = ") else {
                    return false;
                };
                !key.is_empty()
                    && key
                        .chars()
                        .all(|character| character.is_ascii_lowercase() || character == '-')
            })
            .collect::<Vec<_>>();
        assert!(
            commented.iter().any(|line| line.starts_with("max-rate = ")),
            "the documented options are no longer being found: {commented:?}"
        );

        for option in commented {
            let document = format!("[profiles.example]\n{option}\n");
            config::LoadedConfig::from_toml("config.example.toml", &document).unwrap_or_else(
                |error| panic!("commented starter option {option:?} is not in the schema: {error}"),
            );
        }
    }

    #[test]
    fn a_starter_never_replaces_an_existing_configuration_without_force() {
        let root = starter_directory("force");
        let path = root.join("config.toml");
        fs::write(&path, "default-profile = \"mine\"\n").expect("write a configuration to protect");

        let refusal = write_starter_configuration(&path, false).unwrap_err();
        assert!(
            refusal.to_string().contains("already exists at ")
                && refusal.to_string().contains("pass --force to replace it"),
            "unexpected refusal: {refusal}"
        );
        assert_eq!(error_exit_code(&refusal), 2);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "default-profile = \"mine\"\n",
            "a refused init must leave the existing configuration byte-identical"
        );

        assert!(
            write_starter_configuration(&path, true).unwrap(),
            "replacing an existing file is reported as a replacement"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), STARTER_CONFIGURATION);

        // Forcing onto a fresh path is a creation, not a replacement.
        assert!(!write_starter_configuration(&root.join("new.toml"), true).unwrap());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn config_init_creates_missing_parent_directories_and_reports_the_path() {
        let root = starter_directory("parents");
        let path = root.join("nested").join("deep").join("config.toml");
        let invocation = cli::Cli::try_parse_checked_from([
            "synology-drive-sync",
            "--config",
            path.to_str().expect("UTF-8 fixture path"),
            "config",
            "init",
        ])
        .expect("the fixture invocation must parse");

        run_config(&invocation, cli::ConfigAction::Init { force: false })
            .expect("init must succeed");
        assert_eq!(fs::read_to_string(&path).unwrap(), STARTER_CONFIGURATION);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn final_reconciliation_rejects_any_remaining_in_scope_action() {
        assert!(ensure_reconciled(&empty_plan()).is_ok());
        let mut pending = empty_plan();
        pending.creates.push(plan::CreateAction {
            relative: "late".to_owned(),
            remote_path: "/share/root/late".to_owned(),
            reason: plan::ChangeReason::MissingRemote,
        });
        assert!(matches!(
            ensure_reconciled(&pending),
            Err(Error::ReconciliationPending { operations: 1 })
        ));
    }

    #[test]
    fn rich_plan_values_preserve_every_action_snapshot_and_fallback_guard() {
        let plan = rich_plan();
        let value = plan_value(&plan);

        assert_eq!(operation_count(&plan), 5);
        assert_eq!(copy_fallback_bytes(&plan), 7);
        assert_eq!(value["summary"]["changes"], true);
        assert_eq!(value["summary"]["server_copy_fallback_bytes"], 7);
        assert_eq!(
            value["actions"]["pre_deletes"][0]["entry_kind"],
            "directory"
        );
        assert_eq!(
            value["actions"]["pre_deletes"][0]["snapshot_guard"]["require_mtime"],
            false
        );
        assert_eq!(value["actions"]["creates"][0]["relative"], "new-directory");
        assert_eq!(value["actions"]["creates"][0]["reason"], "missing-remote");
        assert_eq!(value["actions"]["uploads"][0]["reason"], "content-differs");
        assert_eq!(value["actions"]["copies"][0]["expected_size"], 7);
        assert_eq!(
            value["actions"]["copies"][0]["verified_upload_fallback"],
            "only-before-copy-task-start"
        );
        assert_eq!(value["actions"]["uploads"][0]["bytes"], 11);
        assert_eq!(
            value["actions"]["post_deletes"][0]["destination_guard"]["expected_size"],
            13
        );
        assert_eq!(
            value["actions"]["post_deletes"][0]["snapshot_guard"]["content_md5"],
            "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a"
        );

        assert!(matches!(
            ensure_reconciled(&plan),
            Err(Error::ReconciliationPending { operations: 5 })
        ));
        assert_eq!(
            json_object_keys(&value["actions"]["creates"][0]),
            BTreeSet::from(["reason", "relative", "remote_path"])
        );
        assert_eq!(
            json_object_keys(&value["actions"]["uploads"][0]),
            BTreeSet::from(["bytes", "mtime_ms", "reason", "relative", "remote_path"])
        );
    }

    #[test]
    fn every_change_reason_reaches_human_json_and_ndjson_output() {
        for (reason, tag, detail) in [
            (
                plan::ChangeReason::MissingRemote,
                "missing-remote",
                "no remote entry at this path",
            ),
            (
                plan::ChangeReason::SizeDiffers,
                "size-differs",
                "local and remote sizes differ",
            ),
            (
                plan::ChangeReason::MtimeDiffers,
                "mtime-differs",
                "size equal, modification time differs",
            ),
            (
                plan::ChangeReason::ContentDiffers,
                "content-differs",
                "size equal, complete MD5/CRC32/SHA-256 fingerprint did not match",
            ),
            (
                plan::ChangeReason::TypeReplaced,
                "type-replaced",
                "remote entry has the conflicting kind",
            ),
        ] {
            let mut plan = rich_plan();
            plan.uploads[0].reason = reason;
            plan.creates[0].reason = reason;

            let human = plan_human(&plan, true);
            assert!(
                human.contains(&format!(
                    "UPLOAD uploaded.bin -> /share/root/uploaded.bin ({tag}: {detail})"
                )),
                "{tag} is missing from the human plan:\n{human}"
            );
            assert!(
                human.contains(&format!(
                    "MKDIR  /share/root/new-directory ({tag}: {detail})"
                )),
                "{tag} is missing from the human plan:\n{human}"
            );
            assert!(
                !plan_human(&plan, false).contains(tag),
                "a concise plan stays a one-line summary"
            );

            let value = plan_value(&plan);
            assert_eq!(value["actions"]["uploads"][0]["reason"], tag);
            assert_eq!(value["actions"]["creates"][0]["reason"], tag);

            let records = plan_ndjson_values(&plan);
            assert_eq!(records[2]["action"], "create-directory");
            assert_eq!(records[2]["reason"], tag);
            assert_eq!(records[4]["action"], "upload");
            assert_eq!(records[4]["reason"], tag);
        }
    }

    #[test]
    fn status_labels_and_aggregate_statuses_cover_every_terminal_state() {
        assert_eq!(SyncBatchStatus::Preflighted.as_str(), "preflighted");
        assert_eq!(SyncBatchStatus::Success.as_str(), "success");
        assert_eq!(SyncBatchStatus::Partial.as_str(), "partial");
        assert_eq!(SyncBatchStatus::Failed.as_str(), "failed");
        assert_eq!(SyncBatchStatus::NotRun.as_str(), "not-run");
        assert_eq!(DoctorBatchStatus::Preflighted.as_str(), "preflighted");
        assert_eq!(DoctorBatchStatus::Success.as_str(), "success");
        assert_eq!(DoctorBatchStatus::Partial.as_str(), "partial");
        assert_eq!(DoctorBatchStatus::Failed.as_str(), "failed");
        assert_eq!(DoctorBatchStatus::NotRun.as_str(), "not-run");

        let sync_success = vec![sync_outcome(
            "alpha",
            SyncBatchStatus::Success,
            Some(empty_plan()),
            Some(empty_plan()),
            false,
        )];
        assert_eq!(sync_batch_status(&sync_success, None), "success");
        let sync_partial = vec![
            sync_outcome(
                "alpha",
                SyncBatchStatus::Success,
                Some(empty_plan()),
                Some(empty_plan()),
                false,
            ),
            sync_outcome("beta", SyncBatchStatus::NotRun, None, None, false),
        ];
        assert_eq!(sync_batch_status(&sync_partial, None), "partial");

        let doctor_success = vec![doctor_outcome(
            "alpha",
            DoctorBatchStatus::Success,
            Some(doctor_result(false, None, None)),
        )];
        assert_eq!(doctor_batch_status(&doctor_success), "success");
        let doctor_partial = vec![
            doctor_outcome(
                "alpha",
                DoctorBatchStatus::Success,
                Some(doctor_result(false, None, None)),
            ),
            doctor_outcome("beta", DoctorBatchStatus::Failed, None),
        ];
        assert_eq!(doctor_batch_status(&doctor_partial), "partial");
        assert_eq!(
            doctor_batch_status(&[doctor_outcome("beta", DoctorBatchStatus::NotRun, None)]),
            "failed"
        );
    }

    #[test]
    fn outcome_values_include_reports_elapsed_errors_and_write_probe_states() {
        let mut outcome = sync_outcome(
            "alpha",
            SyncBatchStatus::Success,
            Some(empty_plan()),
            Some(empty_plan()),
            true,
        );
        outcome.report = Some(ExecutionReport {
            deleted: 1,
            created: 2,
            copied: 3,
            uploaded: 4,
            uploaded_bytes: 5,
        });
        outcome.elapsed = Some(Duration::from_millis(17));
        outcome.error = Some("retained diagnostic".to_owned());
        let value = sync_batch_job_value(&outcome);
        assert_eq!(value["result"]["changed"], true);
        assert_eq!(value["result"]["server_copied"], 3);
        assert_eq!(value["elapsed_ms"], 17);
        assert_eq!(value["error"], "retained diagnostic");

        let mut preflighted = doctor_result(false, None, None);
        assert_eq!(
            doctor_value(&preflighted, Duration::ZERO)["write_test"]["status"],
            "preflighted"
        );
        preflighted.write_probe_performed = true;
        preflighted.write_probe = Some(write_probe_report());
        assert_eq!(
            doctor_value(&preflighted, Duration::ZERO)["write_test"]["status"],
            "success"
        );
        preflighted.write_probe_error = Some("probe failed".to_owned());
        let failed = doctor_value(&preflighted, Duration::ZERO);
        assert_eq!(failed["write_test"]["status"], "failed");
        assert_eq!(failed["write_test"]["report"]["uploaded_size"], 23);
        assert_eq!(failed["write_test"]["error"], "probe failed");
    }

    #[test]
    fn common_output_and_profile_selection_fail_closed_for_ambiguous_batches() {
        let empty = Vec::<config::ResolvedOutput>::new();
        assert!(matches!(
            common_batch_output(empty.iter()),
            Err(Error::Configuration(_))
        ));
        let json_output = resolved_output(cli::OutputFormat::Json);
        let ndjson_output = resolved_output(cli::OutputFormat::Ndjson);
        assert!(matches!(
            common_batch_output([&json_output, &ndjson_output].into_iter()),
            Err(Error::Configuration(_))
        ));
        assert_eq!(
            common_batch_output([&json_output, &json_output].into_iter())
                .unwrap()
                .output,
            cli::OutputFormat::Json
        );

        assert!(select_optional_profile(None, None).unwrap().is_none());
        assert!(matches!(
            select_optional_profile(None, Some("alpha")),
            Err(Error::Configuration(_))
        ));
        let loaded = loaded_profiles(&["alpha", "beta"]);
        assert_eq!(
            select_optional_profile(Some(&loaded), Some("beta"))
                .unwrap()
                .unwrap()
                .name,
            "beta"
        );

        let all = cli::BatchArgs {
            profiles: Vec::new(),
            all_profiles: true,
            max_total_delete: None,
        };
        let selected = select_job_profiles(Some(&loaded), None, &all).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert!(matches!(
            select_job_profiles(None, None, &all),
            Err(Error::Configuration(_))
        ));
        let empty_loaded = loaded_profiles(&[]);
        assert!(matches!(
            select_job_profiles(Some(&empty_loaded), None, &all),
            Err(Error::Configuration(_))
        ));
        let invalid = cli::BatchArgs {
            profiles: vec![" alpha".to_owned()],
            all_profiles: false,
            max_total_delete: None,
        };
        assert!(matches!(
            select_job_profiles(Some(&loaded), None, &invalid),
            Err(Error::Configuration(_))
        ));
        let missing = cli::BatchArgs {
            profiles: vec!["gamma".to_owned()],
            all_profiles: false,
            max_total_delete: None,
        };
        assert!(matches!(
            select_job_profiles(Some(&loaded), None, &missing),
            Err(Error::Configuration(_))
        ));
    }

    #[test]
    fn sync_and_doctor_batch_validators_reject_stdin_and_overlapping_targets() {
        let mut sync_jobs = vec![
            NamedSyncSettings {
                name: "alpha".to_owned(),
                settings: resolved_sync(
                    "https://files.example.test/proxy",
                    "alpha-user",
                    "/team/alpha",
                ),
            },
            NamedSyncSettings {
                name: "beta".to_owned(),
                settings: resolved_sync(
                    "https://files.example.test/proxy",
                    "beta-user",
                    "/team/beta",
                ),
            },
        ];
        validate_sync_batch(&sync_jobs).unwrap();
        sync_jobs[0].settings.authentication.password_stdin = true;
        assert!(matches!(
            validate_sync_batch(&sync_jobs),
            Err(Error::Configuration(_))
        ));
        sync_jobs[0].settings.authentication.password_stdin = false;
        sync_jobs[1].settings.remote = "/team/alpha/nested".to_owned();
        let overlap = validate_sync_batch(&sync_jobs).unwrap_err();
        assert!(overlap.to_string().contains("invalid batch configuration"));

        let mut doctor_jobs = vec![
            NamedDoctorSettings {
                name: "alpha".to_owned(),
                settings: resolved_doctor(
                    "https://files.example.test/proxy",
                    Some("alpha-user"),
                    Some("/team/alpha"),
                ),
            },
            NamedDoctorSettings {
                name: "beta".to_owned(),
                settings: resolved_doctor(
                    "https://files.example.test/proxy",
                    Some("beta-user"),
                    Some("/team/beta"),
                ),
            },
        ];
        validate_doctor_batch(&doctor_jobs).unwrap();
        doctor_jobs[0].settings.authentication.password_stdin = true;
        assert!(matches!(
            validate_doctor_batch(&doctor_jobs),
            Err(Error::Configuration(_))
        ));
        doctor_jobs[0].settings.authentication.password_stdin = false;
        doctor_jobs[1].settings.remote = Some("/team/alpha/nested".to_owned());
        assert!(matches!(
            validate_doctor_batch(&doctor_jobs),
            Err(Error::Configuration(_))
        ));
        assert!(
            validate_doctor_batch(&[NamedDoctorSettings {
                name: "routing-only".to_owned(),
                settings: resolved_doctor("https://files.example.test", None, None),
            }])
            .is_ok()
        );
    }

    #[test]
    fn progress_totals_count_every_planned_action_kind() {
        let plan = rich_plan();
        let wiring = ProgressWiring::new(
            &plan,
            &resolved_output(cli::OutputFormat::Human),
            None,
            CancellationToken::default(),
        );

        // One pre-delete, one create, one copy, one upload and one post-delete.
        assert_eq!(
            wiring.tracker.snapshot().totals,
            ProgressTotals {
                operations: 5,
                // The copy delivers a file just as the upload does.
                files: 2,
                // 11 upload bytes plus the 7-byte copy, which becomes a real upload when
                // the server refuses the copy.
                bytes: 18,
            }
        );
        wiring.finish().unwrap();
    }

    #[test]
    fn execution_events_advance_progress_without_double_counting_observed_uploads() {
        let plan = rich_plan();
        let wiring = ProgressWiring::new(
            &plan,
            &resolved_output(cli::OutputFormat::Human),
            None,
            CancellationToken::default(),
        );

        for event in [
            ExecutionEvent::TypeConflictDeleted {
                remote_path: "/share/root/conflict".to_owned(),
            },
            ExecutionEvent::DirectoryCreated {
                remote_path: "/share/root/new-directory".to_owned(),
            },
            ExecutionEvent::RemoteContentCopied {
                from_remote_path: "/share/root/source/copied.bin".to_owned(),
                to_remote_path: "/share/root/copied.bin".to_owned(),
                bytes: 7,
            },
            ExecutionEvent::RemoteExtraDeleted {
                remote_path: "/share/root/old/guarded.bin".to_owned(),
            },
        ] {
            wiring.record_execution_event(&event);
        }

        let snapshot = wiring.tracker.snapshot();
        assert_eq!(snapshot.completed_operations, 4);
        assert_eq!(snapshot.failed_operations, 0);
        // Only the copy is file-shaped; deletes and creates carry no file or byte count.
        assert_eq!(snapshot.completed_files, 1);
        assert_eq!(snapshot.logical_bytes, 7);
        assert_eq!(snapshot.active_operations, 0);

        // `observer_factory` already accounted this transfer, so the event must not
        // count it a second time.
        wiring.record_execution_event(&ExecutionEvent::Uploaded {
            relative: "uploaded.bin".to_owned(),
            bytes: 11,
        });
        let after_upload = wiring.tracker.snapshot();
        assert_eq!(after_upload.completed_operations, 4);
        assert_eq!(after_upload.completed_files, 1);
        assert_eq!(after_upload.logical_bytes, 7);

        // The copy fallback has no observer, so its event carries the bytes instead.
        wiring.record_execution_event(&ExecutionEvent::CopyFallbackUploaded {
            relative: "copied.bin".to_owned(),
            bytes: 11,
        });
        let after_fallback = wiring.tracker.snapshot();
        assert_eq!(after_fallback.completed_operations, 5);
        assert_eq!(after_fallback.completed_files, 2);
        assert_eq!(after_fallback.logical_bytes, 18);
        wiring.finish().unwrap();
    }

    #[test]
    fn progress_observer_tracks_success_failure_and_attempt_mismatch_without_output() {
        let plan = rich_plan();
        let output = resolved_output(cli::OutputFormat::Human);
        let cancellation = CancellationToken::default();
        let wiring = ProgressWiring::new(&plan, &output, None, cancellation.clone());
        let factory = wiring.observer_factory();
        let observer = factory(&plan.uploads[0].local).unwrap();
        assert!(observer(UploadTransferEvent::AttemptStarted { attempt: 1 }));
        assert!(observer(UploadTransferEvent::Advanced { bytes: 4 }));
        assert!(observer(UploadTransferEvent::Completed));
        let metrics = progress_metrics(&wiring.tracker.snapshot());
        assert_eq!(metrics.operations, 1);
        assert_eq!(metrics.files, 1);
        assert_eq!(metrics.bytes, 11);
        wiring.finish().unwrap();

        let cancellation = CancellationToken::default();
        let failed = ProgressWiring::new(&plan, &output, None, cancellation.clone());
        let observer = failed.observer_factory()(&plan.uploads[0].local).unwrap();
        assert!(observer(UploadTransferEvent::AttemptStarted { attempt: 1 }));
        assert!(observer(UploadTransferEvent::Failed));
        failed.finish().unwrap();

        let cancellation = CancellationToken::default();
        let mismatched = ProgressWiring::new(&plan, &output, None, cancellation.clone());
        let observer = mismatched.observer_factory()(&plan.uploads[0].local).unwrap();
        assert!(!observer(UploadTransferEvent::AttemptStarted {
            attempt: 2
        }));
        assert!(cancellation.is_cancelled());
        assert!(
            mismatched
                .finish()
                .unwrap_err()
                .to_string()
                .contains("attempt accounting")
        );
    }

    #[test]
    fn progress_decisions_and_failure_slot_are_deterministic() {
        let mut output = resolved_output(cli::OutputFormat::Human);
        output.quiet = false;
        output.progress = cli::ProgressMode::Auto;
        assert_eq!(
            renderer_progress_mode_for_terminal(&output, true),
            RendererProgressMode::Auto
        );
        assert_eq!(
            renderer_progress_mode_for_terminal(&output, false),
            RendererProgressMode::Never
        );
        output.progress = cli::ProgressMode::Always;
        assert_eq!(
            renderer_progress_mode_for_terminal(&output, false),
            RendererProgressMode::Always
        );
        output.progress = cli::ProgressMode::Never;
        assert_eq!(
            renderer_progress_mode_for_terminal(&output, true),
            RendererProgressMode::Never
        );

        let recent = Mutex::new(Instant::now());
        assert!(!render_is_due(&recent));
        let old = Mutex::new(Instant::now() - PROGRESS_RENDER_INTERVAL);
        assert!(render_is_due(&old));

        let failure = Mutex::new(None);
        record_progress_failure(&failure, "first".to_owned());
        record_progress_failure(&failure, "second".to_owned());
        assert!(has_progress_failure(&failure));
        assert_eq!(
            take_recorded_failure(&failure).unwrap().as_deref(),
            Some("first")
        );
        assert!(!has_progress_failure(&failure));
        let cancellation = CancellationToken::default();
        assert!(continue_after_progress_event(&cancellation, &failure));
        cancellation.cancel();
        assert!(!continue_after_progress_event(&cancellation, &failure));
    }

    /// Poisons `mutex` by panicking while holding its lock on a scoped thread, then swallows
    /// the resulting join error. This is the deliberate fault-injection path for the assertions
    /// below: no production seam is needed because the state these helpers act on is already
    /// taken by reference.
    fn poison<T: Send>(mutex: &Mutex<T>) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _guard = mutex.lock().unwrap();
                panic!("deliberately poisoning mutex for test coverage");
            });
            let _ = handle.join();
        });
    }

    #[test]
    fn progress_state_helpers_fail_safe_when_their_mutex_is_poisoned() {
        let last_render = Mutex::new(Instant::now());
        poison(&last_render);
        assert!(last_render.is_poisoned());
        // render_is_due treats a poisoned clock as "due": rendering degrades gracefully
        // instead of silently freezing progress output.
        assert!(render_is_due(&last_render));

        let failure: Mutex<Option<String>> = Mutex::new(None);
        poison(&failure);
        assert!(failure.is_poisoned());
        // record_progress_failure cannot record into a poisoned slot; it silently no-ops
        // rather than panicking or propagating the poison.
        record_progress_failure(&failure, "dropped because the lock is poisoned".to_owned());
        // has_progress_failure fails closed: a poisoned lock reads as "a failure occurred".
        assert!(has_progress_failure(&failure));
        assert!(matches!(
            take_recorded_failure(&failure),
            Err(Error::Message(message))
                if message == "observability failure lock was poisoned"
        ));

        let cancellation = CancellationToken::default();
        // has_progress_failure() reads true, so continue_after_progress_event cancels and
        // reports "stop" even though nothing ever called cancellation.cancel() directly.
        assert!(!continue_after_progress_event(&cancellation, &failure));
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn file_logger_and_error_adapters_preserve_classification_without_global_output() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("sdsync-main-logger-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let log_path = root.join("events.ndjson");
        let mut output = resolved_output(cli::OutputFormat::Json);
        output.log_level = cli::LogLevel::Info;
        output.log_format = cli::LogFormat::Json;
        output.log_file = Some(log_path.clone());
        let logger = build_logger(&output).unwrap().unwrap();
        log_event(
            Some(&logger),
            LogEvent::new(EventLogLevel::Info, EventCode::RunStarted),
        )
        .unwrap();
        assert_eq!(finish_logger(Some(&logger), Ok(41_u8), true).unwrap(), 41);
        let logged = fs::read_to_string(&log_path).unwrap();
        assert!(logged.contains("\"event\":\"run.started\""));
        fs::remove_dir_all(&root).unwrap();

        assert!(
            build_logger(&resolved_output(cli::OutputFormat::Human))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            observability_error(
                synology_drive_sync::observability::ObservabilityError::InvalidLogLevel
            ),
            Error::Configuration(_)
        ));
        assert!(matches!(
            observability_error(
                synology_drive_sync::observability::ObservabilityError::RemoteQueueFull
            ),
            Error::Message(_)
        ));
        assert!(
            output_error(io::Error::other("closed"))
                .to_string()
                .contains("failed to write command output")
        );
        assert_eq!(error_exit_code(&Error::InvalidUrl("bad".to_owned())), 2);
        assert_eq!(error_exit_code(&Error::HttpsRequired), 2);
        assert_eq!(
            error_exit_code(&Error::UnsafeRemotePath {
                path: "../escape".to_owned(),
                reason: "traversal".to_owned(),
            }),
            2
        );
        assert_eq!(
            error_exit_code(&Error::InvalidSource(PathBuf::from("missing"))),
            1
        );
    }

    #[test]
    fn comparison_duration_and_size_helpers_cover_boundary_values() {
        assert_eq!(compare_mode(cli::CompareArg::Content), CompareMode::Content);
        assert_eq!(
            compare_mode(cli::CompareArg::Metadata),
            CompareMode::Metadata
        );
        assert_eq!(
            compare_mode(cli::CompareArg::SizeOnly),
            CompareMode::SizeOnly
        );
        assert_eq!(duration_millis(Duration::MAX), u64::MAX);
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(1024_u64.pow(4)), "1.0 TiB");
    }

    #[test]
    fn plan_and_sync_renderers_cover_every_format_and_completion_state() {
        let plan = rich_plan();
        let concise = plan_human(&plan, false);
        assert!(concise.starts_with("Plan: 1 uploads (11 B), 1 server copies"));
        assert!(!concise.contains("DELETE-CONFLICT"));

        let detailed = plan_human(&plan, true);
        assert!(detailed.contains("DELETE-CONFLICT /share/root/conflict"));
        assert!(detailed.contains(
            "MKDIR  /share/root/new-directory (missing-remote: no remote entry at this path)"
        ));
        assert!(detailed.contains("COPY   /share/root/source/copied.bin"));
        assert!(detailed.contains(
            "UPLOAD uploaded.bin -> /share/root/uploaded.bin (content-differs: size equal, complete MD5/CRC32/SHA-256 fingerprint did not match)"
        ));
        assert!(detailed.contains("destination guarded by 13 bytes+mtime+MD5+CRC32+SHA-256"));
        assert!(
            plan_human(&plan_with_deletions(1), true)
                .contains("DELETE /share/root/stale-0 (remote snapshot guarded)")
        );

        let records = plan_ndjson_values(&plan);
        assert_eq!(records.len(), 6);
        assert_eq!(records[0]["kind"], "summary");
        assert_eq!(records[1]["action"], "delete-conflict");
        assert_eq!(records[2]["action"], "create-directory");
        assert_eq!(records[3]["action"], "copy-remote-content");
        assert_eq!(records[2]["reason"], "missing-remote");
        assert_eq!(records[4]["action"], "upload");
        assert_eq!(records[4]["reason"], "content-differs");
        assert_eq!(records[5]["action"], "delete");
        assert_eq!(records[5]["destination_guard"]["expected_size"], 13);
        assert!(plan_ndjson_values(&plan_with_deletions(1))[1]["destination_guard"].is_null());

        let report = ExecutionReport {
            deleted: 1,
            created: 2,
            copied: 3,
            uploaded: 4,
            uploaded_bytes: 5,
        };
        let mut human_output = resolved_output(cli::OutputFormat::Human);
        human_output.verbosity = 1;
        let completed = rendered_human(sync_output(
            &plan,
            Some(&report),
            Duration::from_millis(17),
            &human_output,
            false,
        ));
        assert!(completed.contains("Sync complete: 4 uploaded (5 B), 3 copied on NAS"));
        let unchanged = rendered_human(sync_output(
            &empty_plan(),
            None,
            Duration::ZERO,
            &human_output,
            false,
        ));
        assert!(unchanged.contains("Already in sync"));
        let planned = rendered_human(sync_output(
            &plan,
            None,
            Duration::ZERO,
            &human_output,
            true,
        ));
        assert!(planned.contains("Plan only; no remote changes were made."));

        let json_output = resolved_output(cli::OutputFormat::Json);
        let json = rendered_json(sync_output(
            &plan,
            Some(&report),
            Duration::from_millis(17),
            &json_output,
            false,
        ));
        assert_eq!(json["schema"], "sdsync.sync.v1");
        assert_eq!(json["result"]["uploaded"], 4);
        let unchanged = rendered_json(sync_output(
            &empty_plan(),
            None,
            Duration::ZERO,
            &json_output,
            false,
        ));
        assert_eq!(unchanged["result"]["changed"], false);
        let plan_json = rendered_json(sync_output(&plan, None, Duration::ZERO, &json_output, true));
        assert!(plan_json.get("result").is_none());

        let ndjson_output = resolved_output(cli::OutputFormat::Ndjson);
        let completed = rendered_ndjson(sync_output(
            &plan,
            Some(&report),
            Duration::from_millis(17),
            &ndjson_output,
            false,
        ));
        assert_eq!(completed.last().unwrap()["kind"], "completion");
        assert_eq!(completed.last().unwrap()["result"]["uploaded"], 4);
        let unchanged = rendered_ndjson(sync_output(
            &empty_plan(),
            None,
            Duration::ZERO,
            &ndjson_output,
            false,
        ));
        assert_eq!(unchanged.last().unwrap()["changed"], false);
        assert_eq!(
            rendered_ndjson(sync_output(
                &plan,
                None,
                Duration::ZERO,
                &ndjson_output,
                true,
            ))
            .len(),
            6
        );
    }

    #[test]
    fn sync_batch_renderers_cover_drift_reports_errors_and_stream_shapes() {
        let mut completed = sync_outcome(
            "alpha",
            SyncBatchStatus::Success,
            Some(plan_with_deletions(2)),
            Some(plan_with_deletions(1)),
            true,
        );
        completed.report = Some(ExecutionReport {
            deleted: 1,
            created: 2,
            copied: 3,
            uploaded: 4,
            uploaded_bytes: 5,
        });
        completed.elapsed = Some(Duration::from_millis(19));
        completed.error = Some("retained warning".to_owned());
        let outcomes = vec![
            completed,
            sync_outcome("beta", SyncBatchStatus::NotRun, None, None, false),
        ];

        let mut human_output = resolved_output(cli::OutputFormat::Human);
        human_output.verbosity = 1;
        let human = rendered_human(sync_batch_output(
            &outcomes,
            &human_output,
            false,
            20,
            Some(2),
            Some(1),
            Some("aggregate guard rejected the plan"),
        ));
        assert!(human.contains("Batch sync: status failed"));
        assert!(human.contains("Batch safety check failed: aggregate guard rejected the plan"));
        assert!(human.contains("Deletion-plan drift: preflight 2, fresh execution 1."));
        assert!(human.contains("Result: 4 uploaded (5 B), 3 copied on NAS"));
        assert!(human.contains("Error: retained warning"));
        let planned = rendered_human(sync_batch_output(
            &outcomes,
            &human_output,
            true,
            20,
            Some(2),
            None,
            None,
        ));
        assert!(planned.contains("Batch plan: status partial"));

        let json = rendered_json(sync_batch_output(
            &outcomes,
            &resolved_output(cli::OutputFormat::Json),
            false,
            20,
            Some(2),
            Some(1),
            None,
        ));
        assert_eq!(json["schema"], "sdsync.batch.v1");
        assert_eq!(json["jobs"].as_array().unwrap().len(), 2);
        assert_eq!(json["summary"]["not_run"], 1);

        let records = rendered_ndjson(sync_batch_output(
            &outcomes,
            &resolved_output(cli::OutputFormat::Ndjson),
            false,
            20,
            Some(2),
            Some(1),
            None,
        ));
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["schema"], "sdsync.batch-job.v1");
        assert_eq!(records[2]["kind"], "summary");
    }

    #[test]
    fn doctor_renderers_cover_routing_remote_probe_and_batch_states() {
        let routing = routing_doctor_result();
        assert!(doctor_human(&routing).contains("reverse-proxy routing"));

        let mut authenticated = routing.clone();
        authenticated.authenticated = true;
        assert!(doctor_human(&authenticated).contains("authentication are healthy"));

        let preflight = doctor_result(false, None, None);
        let preflight_human = doctor_human(&preflight);
        assert!(preflight_human.contains("destination exists"));
        assert!(preflight_human.contains("probe prerequisites passed"));

        let mut inventoried = preflight.clone();
        inventoried.remote_inventory = Some((
            DoctorInventoryScope::DirectChildren,
            DiagnosticRemoteInventory {
                root_exists: true,
                total_entries: 8,
                sample: vec![synology_drive_sync::api::DiagnosticRemoteEntry {
                    relative_path: "report.bin".to_owned(),
                    relative_path_truncated: false,
                    name: "report.bin".to_owned(),
                    name_truncated: false,
                    kind: local::EntryKind::File,
                    size_bytes: Some(23),
                    mtime_seconds: Some(1_700_000_000),
                    mount_boundary: false,
                }],
                truncated: true,
                truncated_count: 7,
                truncated_reason: Some("sample_limit"),
                pages_requested: 1,
                traversal_depth: 1,
                deadline_ms: 5_000,
            },
        ));
        let inventory_human = doctor_human(&inventoried);
        assert!(inventory_human.contains("8 direct children; 1 sampled; 7 truncated"));
        assert!(inventory_human.contains("path=report.bin; name=report.bin; kind=file"));
        assert!(inventory_human.contains("size_bytes=23; mtime_seconds=1700000000"));
        assert!(inventory_human.contains("mount_boundary=false"));

        let mut ancestor = preflight.clone();
        ancestor.remote_exists = Some(false);
        ancestor.write_permission_scope = Some("nearest_existing_ancestor");
        ancestor.write_permission_path = None;
        let ancestor_human = doctor_human(&ancestor);
        assert!(ancestor_human.contains("will be created"));
        assert!(ancestor_human.contains("nearest existing ancestor <unknown>"));

        let failed = doctor_result(true, None, Some("probe upload failed"));
        assert!(doctor_human(&failed).contains("Disposable write probe failed"));

        let mut passed_report = write_probe_report();
        passed_report.leftover_remote_probe_path = Some("/share/leftover".to_owned());
        let passed = doctor_result(true, Some(passed_report), None);
        let passed_human = doctor_human(&passed);
        assert!(passed_human.contains("server-side copy verification"));
        assert!(passed_human.contains("unexpected leftover: /share/leftover"));
        let mut no_copy_report = write_probe_report();
        no_copy_report.server_copy_supported = false;
        let no_copy = doctor_result(true, Some(no_copy_report), None);
        assert!(!doctor_human(&no_copy).contains("server-side copy verification"));

        assert!(
            rendered_human(doctor_output(
                &routing,
                Duration::from_millis(3),
                cli::OutputFormat::Human,
            ))
            .contains("reverse-proxy")
        );
        assert_eq!(
            rendered_json(doctor_output(
                &passed,
                Duration::from_millis(3),
                cli::OutputFormat::Json,
            ))["write_test"]["status"],
            "success"
        );
        assert_eq!(
            rendered_ndjson(doctor_output(
                &failed,
                Duration::from_millis(3),
                cli::OutputFormat::Ndjson,
            ))
            .len(),
            1
        );

        let mut failed_outcome = doctor_outcome("beta", DoctorBatchStatus::Failed, None);
        failed_outcome.error = Some("authentication failed".to_owned());
        let outcomes = vec![
            doctor_outcome("alpha", DoctorBatchStatus::Success, Some(passed)),
            failed_outcome,
            doctor_outcome("gamma", DoctorBatchStatus::Preflighted, Some(preflight)),
            doctor_outcome("omega", DoctorBatchStatus::NotRun, None),
        ];
        let batch_human = rendered_human(doctor_batch_output(
            &outcomes,
            cli::OutputFormat::Human,
            true,
            false,
        ));
        assert!(batch_human.contains("Target diagnostic batch: status partial"));
        assert!(batch_human.contains("[alpha] success"));
        assert!(batch_human.contains("Error: authentication failed"));
        let batch_json = rendered_json(doctor_batch_output(
            &outcomes,
            cli::OutputFormat::Json,
            true,
            false,
        ));
        assert_eq!(batch_json["jobs"].as_array().unwrap().len(), 4);
        let batch_records = rendered_ndjson(doctor_batch_output(
            &outcomes,
            cli::OutputFormat::Ndjson,
            true,
            false,
        ));
        assert_eq!(batch_records.len(), 5);
        assert_eq!(batch_records.last().unwrap()["kind"], "summary");
    }

    #[test]
    fn credential_renderers_cover_every_outcome_without_touching_stdout() {
        assert!(
            rendered_human(credential_output(
                credentials::CredentialOutcome::StoredPassword,
                cli::OutputFormat::Human,
            ))
            .contains("Stored the DSM password")
        );
        assert_eq!(
            rendered_json(credential_output(
                credentials::CredentialOutcome::StoredTotp,
                cli::OutputFormat::Json,
            ))["credential"],
            "totp"
        );
        assert_eq!(
            rendered_ndjson(credential_output(
                credentials::CredentialOutcome::StoredPassword,
                cli::OutputFormat::Ndjson,
            ))[0]["kind"],
            "stored"
        );

        let status = rendered_human(credential_output(
            credentials::CredentialOutcome::Status {
                password_stored: true,
                totp_stored: false,
            },
            cli::OutputFormat::Human,
        ));
        assert_eq!(status, "Password: stored; TOTP seed: not stored.\n");
        let inverse = rendered_json(credential_output(
            credentials::CredentialOutcome::Status {
                password_stored: false,
                totp_stored: true,
            },
            cli::OutputFormat::Json,
        ));
        assert_eq!(inverse["password_stored"], false);
        assert_eq!(inverse["totp_stored"], true);

        let removed = rendered_human(credential_output(
            credentials::CredentialOutcome::Removed {
                password_removed: Some(true),
                totp_removed: Some(false),
            },
            cli::OutputFormat::Human,
        ));
        assert_eq!(removed, "Password: removed; TOTP seed: not stored.\n");
        let absent = rendered_json(credential_output(
            credentials::CredentialOutcome::Removed {
                password_removed: None,
                totp_removed: None,
            },
            cli::OutputFormat::Json,
        ));
        assert!(absent["password_removed"].is_null());
        assert!(absent["totp_removed"].is_null());
    }

    #[test]
    fn source_renderers_and_completion_decisions_cover_success_failure_and_cancel() {
        let source = source_result();
        let human = rendered_human(source_doctor_output(&source, cli::OutputFormat::Human));
        assert!(human.contains("2 files, 1 directories, 1.5 KiB"));
        assert_eq!(
            rendered_json(source_doctor_output(&source, cli::OutputFormat::Json))["source"]["hashed_files"],
            2
        );
        assert_eq!(
            rendered_ndjson(source_doctor_output(&source, cli::OutputFormat::Ndjson)).len(),
            1
        );

        let outcomes = vec![
            SourceBatchOutcome {
                name: "alpha".to_owned(),
                result: Some(source_result()),
                error: None,
                not_run: false,
            },
            SourceBatchOutcome {
                name: "beta".to_owned(),
                result: None,
                error: None,
                not_run: true,
            },
            SourceBatchOutcome {
                name: "gamma".to_owned(),
                result: None,
                error: Some("scan failed".to_owned()),
                not_run: false,
            },
            SourceBatchOutcome {
                name: "omega".to_owned(),
                result: None,
                error: None,
                not_run: false,
            },
        ];
        let batch_human = rendered_human(source_batch_output(&outcomes, cli::OutputFormat::Human));
        assert!(batch_human.contains("1 succeeded, 1 failed, 1 not run"));
        assert!(batch_human.contains("[alpha] healthy"));
        assert!(batch_human.contains("[beta] not run"));
        assert!(batch_human.contains("[gamma] failed: scan failed"));
        assert!(batch_human.contains("[omega] failed: unknown failure"));
        let batch_json = rendered_json(source_batch_output(&outcomes, cli::OutputFormat::Json));
        assert_eq!(batch_json["status"], "partial");
        assert_eq!(batch_json["jobs"].as_array().unwrap().len(), 4);
        let records = rendered_ndjson(source_batch_output(&outcomes, cli::OutputFormat::Ndjson));
        assert_eq!(records.len(), 5);
        assert_eq!(
            records.last().unwrap()["schema"],
            "sdsync.source-doctor-batch.v1"
        );

        assert!(matches!(
            source_batch_completion(&outcomes, true),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            source_batch_completion(&outcomes, false),
            Err(Error::Message(_))
        ));
        let success = [SourceBatchOutcome {
            name: "alpha".to_owned(),
            result: Some(source_result()),
            error: None,
            not_run: false,
        }];
        assert!(source_batch_completion(&success, false).is_ok());
    }

    #[test]
    fn completion_helpers_map_changes_partial_failures_and_cancellation() {
        assert!(matches!(
            changes_completion(true, true, true),
            Err(Error::Cancelled)
        ));
        assert_eq!(
            changes_completion(true, false, true).unwrap(),
            ExitCode::from(cli::PLAN_CHANGES_EXIT_CODE)
        );
        assert_eq!(
            changes_completion(false, false, true).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            changes_completion(true, false, false).unwrap(),
            ExitCode::SUCCESS
        );

        let sync_success = [sync_outcome(
            "alpha",
            SyncBatchStatus::Success,
            Some(empty_plan()),
            Some(empty_plan()),
            false,
        )];
        assert_eq!(
            sync_batch_completion(&sync_success, false).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(matches!(
            sync_batch_completion(&sync_success, true),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            sync_batch_completion(
                &[sync_outcome(
                    "alpha",
                    SyncBatchStatus::Partial,
                    Some(empty_plan()),
                    Some(empty_plan()),
                    true,
                )],
                false,
            ),
            Err(Error::Message(_))
        ));

        let doctor_success = [doctor_outcome(
            "alpha",
            DoctorBatchStatus::Success,
            Some(routing_doctor_result()),
        )];
        assert_eq!(
            doctor_batch_completion(&doctor_success, false).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(matches!(
            doctor_batch_completion(&doctor_success, true),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            doctor_batch_completion(
                &[doctor_outcome("alpha", DoctorBatchStatus::Failed, None)],
                false,
            ),
            Err(Error::Message(_))
        ));
    }

    fn probe(channels: SessionChannels, code: Option<i64>) -> ChannelProbe {
        ChannelProbe {
            channels,
            outcome: if code.is_none() {
                RequestOutcome::Ok
            } else {
                RequestOutcome::DsmError
            },
            dsm_code: code,
            http_status: Some(200),
            elapsed_ms: 12,
            skipped: None,
        }
    }

    /// The variants, ordered as the probe runs them.
    ///
    /// The tokenless-login variant defaults to not having run, which is what a standard-level run
    /// and every two-factor account produce: the verdict has to be reached without it.
    fn ablation(
        all: Option<i64>,
        sid: Option<i64>,
        cookie: Option<i64>,
    ) -> [ChannelProbe; SESSION_CHANNEL_VARIANTS] {
        [
            probe(SessionChannels::All, all),
            probe(SessionChannels::SidFieldOnly, sid),
            probe(SessionChannels::CookieOnly, cookie),
            // The control: no session identifier at all, so a healthy DSM rejects it.
            probe(SessionChannels::TokenHeaderOnly, Some(119)),
            ChannelProbe::skipped(
                SessionChannels::SidFieldOnlyTokenlessLogin,
                "not attempted by this fixture",
            ),
        ]
    }

    /// The same variants with the tokenless second login having answered.
    fn with_tokenless_login(
        mut probes: [ChannelProbe; SESSION_CHANNEL_VARIANTS],
        code: Option<i64>,
    ) -> [ChannelProbe; SESSION_CHANNEL_VARIANTS] {
        probes[4] = probe(SessionChannels::SidFieldOnlyTokenlessLogin, code);
        probes
    }

    /// The ablation's verdict table is the whole diagnostic, so every row of it is pinned.
    #[test]
    fn the_ablation_verdict_separates_a_mis_carried_session_from_a_dead_one() {
        // Healthy: the client's usual combination works, and so does the documented field alone.
        let (status, detail, remediation) = ablation_verdict(&ablation(None, None, Some(119)));
        assert_eq!(status, DoctorSectionStatus::Pass);
        assert!(detail.contains("Channel selection is not the fault here"));
        assert!(
            detail.contains("which is what a format=sid login implies"),
            "a rejected cookie-only variant is expected, not a finding: {detail}"
        );
        assert_eq!(remediation, None);

        // A DSM that accepts the cookie on its own too is still healthy, and says so differently.
        let (status, detail, _) = ablation_verdict(&ablation(None, None, None));
        assert_eq!(status, DoctorSectionStatus::Pass);
        assert!(detail.contains("the synthesised cookie alone was also accepted"));

        // The reported live bug: the documented field works, the usual combination does not.
        let (status, detail, remediation) = ablation_verdict(&ablation(Some(119), None, Some(119)));
        assert_eq!(status, DoctorSectionStatus::Fail);
        assert!(detail.contains("accepted with the _sid request field alone"));
        assert!(detail.contains("rejected (DSM 119)"));
        assert_eq!(remediation, Some(DOCTOR_COOKIE_CHANNEL_HINT));

        // The mirror image: the cookie alone is accepted, so DSM really is resolving the session
        // from it and the documented field is the odd one out.
        let (status, detail, remediation) = ablation_verdict(&ablation(None, Some(119), None));
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("resolving the session from the cookie"));
        assert_eq!(remediation, None);

        // The shape a live QuickConnect-relayed DSM 7 actually produced: only the full
        // combination is accepted. The cookie alone was rejected too, so nothing here says the
        // cookie resolves the session, and the advice must never be to stop sending it.
        let combination_only = ablation(None, Some(119), Some(119));
        let (status, detail, remediation) = ablation_verdict(&combination_only);
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(
            detail.contains("only the full combination of channels was accepted"),
            "got: {detail}"
        );
        assert!(
            !detail.contains("resolving the session from the cookie"),
            "cookie-only was rejected, so this verdict must not claim the cookie carries it: \
             {detail}"
        );
        assert_eq!(remediation, Some(DOCTOR_COMBINED_CHANNEL_HINT));
        let hint = remediation.unwrap();
        assert!(
            hint.contains("Keep sending both channels"),
            "the remediation must not advise removing a channel this setup depends on: {hint}"
        );

        // The same shape, with the tokenless second login accepted through the documented field:
        // a guide-conformant configuration exists, and the report says so.
        let (status, detail, remediation) =
            ablation_verdict(&with_tokenless_login(combination_only, None));
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(
            detail.contains("without enable_syno_token *was* accepted"),
            "got: {detail}"
        );
        assert_eq!(remediation, Some(DOCTOR_TOKENLESS_LOGIN_HINT));

        // And with it rejected as well: the parameter path is refused however the session is made.
        let (_, detail, remediation) =
            ablation_verdict(&with_tokenless_login(combination_only, Some(119)));
        assert!(
            detail.contains("regardless of how the session was created"),
            "got: {detail}"
        );
        assert_eq!(remediation, Some(DOCTOR_COMBINED_CHANNEL_HINT));

        // A variant that did not run contributes nothing to the verdict either way.
        let (_, detail, _) = ablation_verdict(&combination_only);
        assert!(
            !detail.contains("enable_syno_token"),
            "an unattempted variant must not appear as evidence: {detail}"
        );

        // Everything rejected: not a channel problem at all.
        let (status, detail, remediation) =
            ablation_verdict(&ablation(Some(119), Some(119), Some(119)));
        assert_eq!(status, DoctorSectionStatus::Fail);
        assert!(detail.contains("no longer valid server-side"));
        assert_eq!(remediation, Some(DOCTOR_SESSION_DEAD_HINT));

        // The usual combination and the field both rejected, but the cookie alone accepted: an
        // odd shape that is reported factually rather than forced into one of the named rows.
        let (status, detail, remediation) = ablation_verdict(&ablation(Some(119), Some(119), None));
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("the synthesised cookie alone was also accepted"));
        assert_eq!(remediation, None);

        // A transport failure has no DSM code, and the detail names the outcome instead.
        let mut transport = ablation(Some(119), None, Some(119));
        transport[0].outcome = RequestOutcome::Transport;
        transport[0].dsm_code = None;
        let (_, detail, _) = ablation_verdict(&transport);
        assert!(detail.contains("rejected (transport)"), "got: {detail}");

        // Every variant renders in the human block, with the control explained.
        let lines = channel_ablation_lines(&ablation(Some(119), None, Some(119))).join("\n");
        for channels in SessionChannels::ABLATION_ORDER {
            assert!(lines.contains(channels.as_str()), "missing {channels:?}");
        }
        assert!(lines.contains("rejected with DSM 119"));
        assert!(lines.contains("accepted"));
        assert!(lines.contains("it is the control"));
        assert!(
            lines.contains("not run: not attempted by this fixture"),
            "a variant that did not run must say so rather than look rejected: {lines}"
        );
        let ran = channel_ablation_lines(&with_tokenless_login(
            ablation(Some(119), None, Some(119)),
            None,
        ))
        .join("\n");
        assert!(!ran.contains("not run:"));
        assert!(ran.contains("second login without SynoToken"));
    }

    fn catalogue(entries: &[(&str, Option<u32>, Option<u32>)], unusable: usize) -> ApiCatalogue {
        ApiCatalogue {
            apis: entries
                .iter()
                .map(|(name, min, max)| {
                    (
                        (*name).to_owned(),
                        DiscoveredApi {
                            path: Some("entry.cgi".to_owned()),
                            min_version: *min,
                            max_version: *max,
                            request_format: None,
                        },
                    )
                })
                .collect(),
            unusable_entries: unusable,
        }
    }

    /// Everything this tool requires, at a version DSM offers.
    fn satisfying_entries() -> Vec<(&'static str, Option<u32>, Option<u32>)> {
        API_REQUIREMENTS
            .iter()
            .map(|requirement| {
                (
                    requirement.api,
                    Some(requirement.version),
                    Some(requirement.version),
                )
            })
            .collect()
    }

    #[test]
    fn the_capability_enumeration_folds_dsm_into_three_tiers_a_reader_can_use() {
        let mut entries = satisfying_entries();
        entries.push(("SYNO.FileStation.Rename", Some(1), Some(2)));
        for index in 0..3 {
            entries.push((
                match index {
                    0 => "SYNO.Core.System",
                    1 => "SYNO.Core.Share",
                    _ => "SYNO.DownloadStation.Task",
                },
                Some(1),
                Some(1),
            ));
        }
        let enumeration = CapabilityEnumeration::from_catalogue(&catalogue(&entries, 0));
        assert_eq!(enumeration.total, entries.len());
        assert!(enumeration.blocking_requirements().is_empty());
        assert_eq!(enumeration.unsatisfied_optional(), 0);

        // Tier 1: every File Station entry, with the version this tool asks of it when it asks.
        let rename = enumeration
            .file_station
            .iter()
            .find(|api| api.name == "SYNO.FileStation.Rename")
            .expect("an advertised API this tool does not use");
        assert_eq!(rename.required, None);
        assert_eq!(rename.range(), "v1-2");
        // Tier 3: namespaces outside File Station, largest first.
        assert_eq!(
            enumeration.namespaces,
            [
                ("SYNO.Core.*".to_owned(), 2),
                ("SYNO.API.*".to_owned(), 1),
                ("SYNO.DownloadStation.*".to_owned(), 1),
            ]
        );
        assert_eq!(enumeration.namespace_overflow, None);

        let (status, detail, _) = capability_enumeration_verdict(&enumeration);
        assert_eq!(status, DoctorSectionStatus::Pass);
        assert!(detail.contains("all 10 APIs this tool uses are present"));

        let lines = capability_enumeration_lines(&enumeration).join("\n");
        assert!(lines.contains("SYNO.FileStation.Rename"));
        assert!(lines.contains("unused by this tool"));
        assert!(lines.contains("SYNO.Core.* (2)"));
        assert!(lines.contains("APIs this tool requires:"));
    }

    #[test]
    fn the_capability_enumeration_fails_only_on_a_requirement_that_actually_blocks() {
        // An optional API missing is a warning: the feature that needs it stops, not the sync.
        let entries = satisfying_entries()
            .into_iter()
            .filter(|(api, _, _)| *api != "SYNO.FileStation.CopyMove")
            .collect::<Vec<_>>();
        let enumeration = CapabilityEnumeration::from_catalogue(&catalogue(&entries, 0));
        assert_eq!(enumeration.unsatisfied_optional(), 1);
        assert!(enumeration.blocking_requirements().is_empty());
        let (status, detail, remediation) = capability_enumeration_verdict(&enumeration);
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("1 optional API(s) are unavailable"));
        assert_eq!(remediation, None);

        // A required API offered only at versions this tool cannot use is a failure, and the
        // report states both ranges rather than leaving the reader to guess.
        let entries = satisfying_entries()
            .into_iter()
            .map(|(api, min, max)| {
                if api == "SYNO.FileStation.CheckPermission" {
                    (api, Some(1), Some(2))
                } else {
                    (api, min, max)
                }
            })
            .collect::<Vec<_>>();
        let enumeration = CapabilityEnumeration::from_catalogue(&catalogue(&entries, 0));
        let blocking = enumeration.blocking_requirements();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].describe(), "INCOMPATIBLE (offered v1-2)");
        let (status, detail, remediation) = capability_enumeration_verdict(&enumeration);
        assert_eq!(status, DoctorSectionStatus::Fail);
        assert!(detail.contains("SYNO.FileStation.CheckPermission"));
        assert_eq!(remediation, Some(DOCTOR_CAPABILITY_VERSION_HINT));

        // An API advertised with no version range cannot answer the question, and says so.
        let entries = satisfying_entries()
            .into_iter()
            .map(|(api, min, max)| {
                if api == "SYNO.FileStation.List" {
                    (api, None, None)
                } else {
                    (api, min, max)
                }
            })
            .collect::<Vec<_>>();
        let enumeration = CapabilityEnumeration::from_catalogue(&catalogue(&entries, 0));
        assert_eq!(
            enumeration.blocking_requirements()[0].describe(),
            "INCOMPATIBLE (no version range advertised)"
        );

        // An API absent altogether is named as such rather than as a version mismatch.
        let entries = satisfying_entries()
            .into_iter()
            .filter(|(api, _, _)| *api != "SYNO.FileStation.List")
            .collect::<Vec<_>>();
        let enumeration = CapabilityEnumeration::from_catalogue(&catalogue(&entries, 0));
        assert_eq!(
            enumeration.blocking_requirements()[0].describe(),
            "NOT ADVERTISED"
        );

        // An unreadable entry is a warning of its own: a DSM advertising one is worth a line.
        let enumeration =
            CapabilityEnumeration::from_catalogue(&catalogue(&satisfying_entries(), 3));
        let (status, detail, _) = capability_enumeration_verdict(&enumeration);
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("3 advertised entries could not be read"));
        assert!(
            capability_enumeration_lines(&enumeration)
                .join("\n")
                .contains("3 advertised entries could not be read")
        );
    }

    /// A server can advertise arbitrarily many APIs, so the report has to bound what it prints.
    #[test]
    fn the_capability_enumeration_bounds_what_a_server_can_make_it_print() {
        let mut entries = satisfying_entries();
        let names = (0..DOCTOR_ADVERTISED_API_LIMIT + 5)
            .map(|index| format!("SYNO.FileStation.Extra{index:03}"))
            .chain(
                (0..DOCTOR_NAMESPACE_LIMIT + 4)
                    .map(|index| format!("SYNO.Package{index:03}.Service")),
            )
            .collect::<Vec<_>>();
        entries.extend(names.iter().map(|name| (name.as_str(), Some(1), Some(1))));
        let enumeration = CapabilityEnumeration::from_catalogue(&catalogue(&entries, 0));

        assert_eq!(enumeration.file_station.len(), DOCTOR_ADVERTISED_API_LIMIT);
        assert!(enumeration.file_station_truncated > 0);
        assert_eq!(enumeration.namespaces.len(), DOCTOR_NAMESPACE_LIMIT);
        let (apis, namespaces) = enumeration
            .namespace_overflow
            .expect("namespaces beyond the limit are counted");
        assert!(apis > 0 && namespaces > 0);
        let lines = capability_enumeration_lines(&enumeration).join("\n");
        assert!(lines.contains("further File Station APIs not listed"));
        assert!(lines.contains(&format!(
            "others ({apis} APIs across {namespaces} further namespaces)"
        )));

        // A name is server-supplied text and is sanitized before it reaches a terminal.
        assert_eq!(
            namespace_of("SYNO.Core\u{1b}[31m.System"),
            "SYNO.Core__31m.*"
        );
        assert_eq!(namespace_of("Flat"), "Flat");
    }

    fn capability_record(
        api: &'static str,
        verdict: CapabilityVerdict,
        dsm_code: Option<i64>,
        required: bool,
    ) -> CapabilityRecord {
        CapabilityRecord {
            api,
            method: "get",
            version: 2,
            verdict,
            dsm_code,
            elapsed_ms: 9,
            required,
        }
    }

    #[test]
    fn a_dead_session_is_reported_as_a_dead_session_not_as_broken_capabilities() {
        let station_info = FileStationInfo {
            hostname: Some(BoundedText::sanitized("DiskStation")),
            is_manager: Some(false),
            support_sharing: Some(true),
            support_virtual_protocol: Some(BoundedText::sanitized("cifs,nfs")),
        };
        let base = |records: Vec<CapabilityRecord>| CapabilityDiagnosis {
            records,
            info: Some(station_info),
            first_hostname: station_info.hostname,
            last_hostname: station_info.hostname,
            hostname_changed: false,
            session_aborted: false,
            unprobed: DOCTOR_UNPROBED_CAPABILITIES.to_vec(),
        };

        // Everything works.
        let healthy = base(vec![capability_record(
            "SYNO.FileStation.List",
            CapabilityVerdict::Works,
            None,
            true,
        )]);
        let (status, detail, _) = capability_diagnosis_verdict(&healthy);
        assert_eq!(status, DoctorSectionStatus::Pass);
        assert!(detail.contains("all 1 probed capabilities work"));

        // An optional capability refused for this account warns; it must not fail a sync check.
        let optional = base(vec![
            capability_record(
                "SYNO.FileStation.List",
                CapabilityVerdict::Works,
                None,
                true,
            ),
            capability_record(
                "SYNO.FileStation.BackgroundTask",
                CapabilityVerdict::NoPermission,
                Some(105),
                false,
            ),
        ]);
        assert_eq!(optional.broken_required(), 0);
        let (status, detail, _) = capability_diagnosis_verdict(&optional);
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("the rest are optional"));

        // A required capability advertised and demonstrably unroutable fails, and the hint names
        // the proxy rather than DSM: discovery and the call went to the same origin.
        let unroutable = base(vec![capability_record(
            "SYNO.FileStation.List",
            CapabilityVerdict::NotRoutable,
            Some(102),
            true,
        )]);
        let (status, detail, remediation) = capability_diagnosis_verdict(&unroutable);
        assert_eq!(status, DoctorSectionStatus::Fail);
        assert!(detail.contains("advertised but not functional"));
        assert_eq!(remediation, Some(DOCTOR_CAPABILITY_ROUTING_HINT));

        // The rule that matters most: a session error stops the probing and is never a verdict
        // about a capability. Warn, not fail, and pointing at the ablation.
        let mut aborted = base(vec![capability_record(
            "SYNO.FileStation.List",
            CapabilityVerdict::NotProbed,
            Some(119),
            true,
        )]);
        aborted.session_aborted = true;
        aborted.last_hostname = None;
        assert_eq!(aborted.probed(), 0);
        assert_eq!(aborted.broken_required(), 0);
        let (status, detail, _) = capability_diagnosis_verdict(&aborted);
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("describe the session rather than the capabilities"));

        // Two host names in one run outrank everything else: it is the only positive proof that
        // consecutive requests reached different hosts.
        let mut two_hosts = base(vec![capability_record(
            "SYNO.FileStation.List",
            CapabilityVerdict::Works,
            None,
            true,
        )]);
        two_hosts.hostname_changed = true;
        two_hosts.last_hostname = Some(BoundedText::sanitized("OtherStation"));
        let (status, detail, remediation) = capability_diagnosis_verdict(&two_hosts);
        assert_eq!(status, DoctorSectionStatus::Fail);
        assert!(detail.contains("two different host names"));
        assert_eq!(remediation, Some(DOCTOR_MULTIPLE_PATH_HINT));

        let lines = capability_diagnosis_lines(&two_hosts).join("\n");
        assert!(lines.contains("is not a DSM administrator"));
        assert!(lines.contains("cifs,nfs"));
        assert!(lines.contains("DIFFERENT host names"));
        assert!(lines.contains("advertised but deliberately not probed:"));
        let same_host = capability_diagnosis_lines(&healthy).join("\n");
        assert!(same_host.contains("the same host name"));
        assert!(same_host.contains("excludes nothing on its own"));
        let unknown_host = capability_diagnosis_lines(&aborted).join("\n");
        assert!(unknown_host.contains("could not be read at both ends"));
    }

    #[test]
    fn every_dsm_answer_a_capability_probe_can_get_maps_to_one_verdict() {
        let probe = |code: Option<i64>| CapabilityProbe {
            api: "SYNO.FileStation.List",
            method: "list_share",
            version: 2,
            outcome: if code.is_none() {
                RequestOutcome::Ok
            } else {
                RequestOutcome::DsmError
            },
            dsm_code: code,
            http_status: Some(200),
            elapsed_ms: 4,
        };
        for (code, expected) in [
            (None, CapabilityVerdict::Works),
            // File Station answering "no such path" is a fact about the path, not the API.
            (Some(408), CapabilityVerdict::Works),
            (Some(102), CapabilityVerdict::NotRoutable),
            (Some(103), CapabilityVerdict::MethodUnavailable),
            (Some(104), CapabilityVerdict::VersionUnsupported),
            (Some(105), CapabilityVerdict::NoPermission),
            (Some(407), CapabilityVerdict::NoPermission),
            (Some(106), CapabilityVerdict::NotProbed),
            (Some(107), CapabilityVerdict::NotProbed),
            (Some(119), CapabilityVerdict::NotProbed),
            (Some(999), CapabilityVerdict::Failed),
        ] {
            assert_eq!(
                CapabilityVerdict::classify(probe(code)),
                expected,
                "DSM {code:?}"
            );
            assert!(!expected.as_str().is_empty());
        }
        // A transport failure has no code at all and is not silently called a permission problem.
        let mut transport = probe(None);
        transport.outcome = RequestOutcome::Transport;
        assert_eq!(
            CapabilityVerdict::classify(transport),
            CapabilityVerdict::Failed
        );

        // The same table, reached from an `Error` rather than from a probe record.
        assert_eq!(
            verdict_for::<()>(&Err(Error::Cancelled)),
            CapabilityVerdict::Failed
        );
        assert_eq!(verdict_for(&Ok(())), CapabilityVerdict::Works);
    }

    /// The probe list is bounded by what DSM advertised, and never includes a mutating method.
    #[test]
    fn only_advertised_read_only_capabilities_are_probed() {
        let baseline = capability_probe_specs(None);
        assert_eq!(
            baseline
                .iter()
                .map(|spec| (spec.api, spec.method))
                .collect::<Vec<_>>(),
            [("SYNO.FileStation.List", "list_share")],
            "with nothing enumerated, only the always-safe probe runs"
        );

        let mut entries = satisfying_entries();
        entries.push(("SYNO.FileStation.VirtualFolder", Some(1), Some(2)));
        entries.push(("SYNO.FileStation.BackgroundTask", Some(1), Some(3)));
        // Advertised at a version the probe does not ask for, so it stays unprobed.
        entries.push(("SYNO.FileStation.DirSize", Some(1), Some(2)));
        let enumeration = CapabilityEnumeration::from_catalogue(&catalogue(&entries, 0));
        let specs = capability_probe_specs(Some(&enumeration));
        assert_eq!(
            specs
                .iter()
                .map(|spec| (spec.api, spec.method))
                .collect::<Vec<_>>(),
            [
                ("SYNO.FileStation.List", "list_share"),
                ("SYNO.FileStation.VirtualFolder", "list"),
                ("SYNO.FileStation.BackgroundTask", "list"),
            ]
        );
        for spec in &specs {
            assert_eq!(spec.cgi_path, "entry.cgi");
            assert!(
                !matches!(
                    spec.method,
                    "create" | "delete" | "rename" | "upload" | "start"
                ),
                "{}.{} mutates or spawns work",
                spec.api,
                spec.method
            );
        }

        // An advertised API whose range excludes the probed version is left alone.
        let mut entries = satisfying_entries();
        entries.push(("SYNO.FileStation.VirtualFolder", Some(3), Some(4)));
        let enumeration = CapabilityEnumeration::from_catalogue(&catalogue(&entries, 0));
        assert_eq!(capability_probe_specs(Some(&enumeration)).len(), 1);
    }

    #[test]
    fn the_fan_out_verdict_separates_a_rejected_burst_from_one_that_killed_the_session() {
        let report = |succeeded, session_rejected, other_failures, follow_up| ConcurrencyReport {
            parallel: 4,
            succeeded,
            session_rejected,
            other_failures,
            follow_up_succeeded: follow_up,
            follow_up_session_rejected: !follow_up,
            elapsed_ms: 431,
        };

        let (status, detail, remediation) = concurrency_verdict(&report(4, 0, 0, true));
        assert_eq!(status, DoctorSectionStatus::Pass);
        assert!(detail.contains("tolerates being used from several connections"));
        assert_eq!(remediation, None);

        // Some rejected, session survives: per-connection session state, which is the finding.
        let (status, detail, remediation) = concurrency_verdict(&report(3, 1, 0, true));
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("3 of 4 concurrent requests succeeded"));
        assert!(detail.contains("the session itself survived the burst"));
        assert_eq!(remediation, Some(DOCTOR_MULTIPLE_PATH_HINT));

        let (_, detail, _) = concurrency_verdict(&report(3, 1, 0, false));
        assert!(detail.contains("left the session unusable"));

        // All succeeded, the follow-up did not: the burst itself invalidated the session.
        let (status, detail, remediation) = concurrency_verdict(&report(4, 0, 0, false));
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("the burst itself is what invalidated the session"));
        assert_eq!(remediation, Some(DOCTOR_MULTIPLE_PATH_HINT));

        // Failures that are not session rejections are reported as what they are.
        let (status, detail, remediation) = concurrency_verdict(&report(2, 0, 2, true));
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("failed for reasons other than a rejected session"));
        assert_eq!(remediation, None);
    }

    fn segment(path: &str, depth: u8, exists: bool) -> api::PathSegmentProbe {
        api::PathSegmentProbe {
            path: path.to_owned(),
            depth,
            exists,
            is_directory: exists,
            mount_boundary: false,
            dsm_code: (!exists).then_some(408),
        }
    }

    #[test]
    fn the_path_resolution_verdict_names_the_component_that_stops_it() {
        // Nothing walked: the check could not start, and the section says so instead of guessing.
        let (status, detail, remediation) =
            path_resolution_verdict(&DestinationPathResolution::default());
        assert_eq!(status, DoctorSectionStatus::Skip);
        assert!(detail.contains("could not start"));
        assert_eq!(remediation, None);

        let resolved = DestinationPathResolution {
            segments: vec![segment("/team", 1, true), segment("/team/target", 2, true)],
            total_components: 2,
            first_missing: None,
        };
        let (status, detail, _) = path_resolution_verdict(&resolved);
        assert_eq!(status, DoctorSectionStatus::Pass);
        assert!(detail.contains("all 2 components"));

        // The share itself is missing: creating directories cannot fix that.
        let missing_share = DestinationPathResolution {
            segments: vec![segment("/team", 1, false)],
            total_components: 2,
            first_missing: Some(1),
        };
        let (status, detail, remediation) = path_resolution_verdict(&missing_share);
        assert_eq!(status, DoctorSectionStatus::Fail);
        assert!(detail.contains("the shared folder itself is absent"));
        assert_eq!(remediation, Some(DOCTOR_MISSING_SHARE_HINT));

        // Only a later component is missing: sync can create it, so this warns.
        let missing_leaf = DestinationPathResolution {
            segments: vec![segment("/team", 1, true), segment("/team/target", 2, false)],
            total_components: 2,
            first_missing: Some(2),
        };
        let (status, detail, remediation) = path_resolution_verdict(&missing_leaf);
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("stops at component 2 of 2"));
        assert_eq!(remediation, Some(DOCTOR_MISSING_COMPONENT_HINT));

        // The walk stopped for a reason other than absence -- here a rejected session -- and the
        // report says so rather than calling the component missing, which is a different fault.
        let mut interrupted = missing_leaf.clone();
        interrupted.first_missing = None;
        interrupted.segments[1].dsm_code = Some(119);
        let (status, detail, remediation) = path_resolution_verdict(&interrupted);
        assert_eq!(status, DoctorSectionStatus::Warn);
        assert!(detail.contains("stopped at component 2 of 2 after DSM answered 119"));
        assert!(detail.contains("not known to be missing"));
        assert_eq!(remediation, None);
        assert!(path_resolution_lines(&interrupted)[1].contains("DSM 119 stopped the walk here"));

        // Each component renders with what DSM said about it, in walk order.
        let mut mounted = resolved.clone();
        mounted.segments[1].mount_boundary = true;
        mounted
            .segments
            .push(segment("/team/target/child", 3, true));
        mounted.segments[2].is_directory = false;
        mounted.total_components = 4;
        let lines = path_resolution_lines(&mounted);
        assert!(lines[0].contains("/team") && lines[0].contains("exists, directory"));
        assert!(lines[1].contains("mounted filesystem boundary"));
        assert!(lines[2].contains("exists, not a directory"));
        assert!(lines[3].contains("1 further component(s) were not inspected"));
        assert!(path_resolution_lines(&missing_leaf)[1].contains("absent (DSM 408)"));
    }

    /// A diagnostic section that fails must not write "not run" over the sections after it: they
    /// do run, and they record their own results.
    #[test]
    fn a_failed_diagnostic_section_does_not_disown_the_rest_of_the_run() {
        let mut result = routing_doctor_result();
        record_diagnostic_section(
            &mut result,
            "session_channel_ablation",
            DoctorSectionStatus::Fail,
            "the cookie poisoned the session".to_owned(),
            Duration::from_millis(4),
            Some(DOCTOR_COOKIE_CHANNEL_HINT),
        );
        let section = result
            .sections
            .iter()
            .find(|section| section.id == "session_channel_ablation")
            .expect("the ablation section");
        assert_eq!(section.status, DoctorSectionStatus::Fail);
        assert_eq!(section.remediation, Some(DOCTOR_COOKIE_CHANNEL_HINT));
        assert_eq!(
            result.failure.as_deref(),
            Some("the cookie poisoned the session")
        );
        assert!(result.failed());
        // Nothing after it was written off.
        assert!(
            result
                .sections
                .iter()
                .filter(|section| section.id != "session_channel_ablation")
                .all(|section| !section.detail.contains("not run because")),
            "a failed diagnostic must not disown the sections that follow it"
        );
    }
}

#[cfg(test)]
mod status_command_tests {
    use super::*;

    fn output(format: cli::OutputFormat) -> config::ResolvedOutput {
        config::ResolvedOutput {
            verbosity: 0,
            quiet: true,
            log_level: cli::LogLevel::Off,
            log_format: cli::LogFormat::Human,
            log_file: None,
            remote_log_url: None,
            remote_log_token: None,
            remote_log_mode: cli::RemoteLogMode::BestEffort,
            progress: cli::ProgressMode::Never,
            output: format,
        }
    }

    fn entry(relative: &str, state: plan::StatusState) -> plan::StatusEntry {
        plan::StatusEntry {
            relative: relative.to_owned(),
            remote_path: format!("/team/export/{relative}"),
            kind: local::EntryKind::File,
            state,
            local: Some(plan::SideInfo {
                size: 12,
                mtime_seconds: 7,
            }),
            remote: Some(plan::SideInfo {
                size: 34,
                mtime_seconds: 9,
            }),
        }
    }

    fn sample_page() -> plan::StatusPage {
        plan::StatusPage {
            scope: "docs".to_owned(),
            entries: vec![
                entry(
                    "docs/changed.txt",
                    plan::StatusState::Differs(plan::ChangeReason::SizeDiffers),
                ),
                entry("docs/new.txt", plan::StatusState::MissingRemote),
            ],
            limit: 100,
            next_cursor: Some(plan::StatusCursor::new("docs/new.txt")),
            truncated: true,
            stats: plan::StatusStats {
                compare: CompareMode::Metadata,
                in_sync_files: 4,
                differing_files: 1,
                missing_remote_files: 1,
                remote_only_entries: 2,
                type_conflicts: 0,
                excluded_entries: 3,
                directories: 1,
                in_sync_bytes: 400,
                transfer_bytes: 46,
                total_entries: 11,
                attention_entries: 4,
                complete: true,
            },
        }
    }

    fn render(page: &plan::StatusPage, format: cli::OutputFormat) -> RenderedOutput {
        let scope = plan::Scope::parse(&page.scope).unwrap();
        let mut buffer = Vec::new();
        write_status_output_to(
            &mut buffer,
            page,
            &scope,
            CompareMode::Metadata,
            &status_cache::CacheReport::off(),
            &output(format),
        )
        .expect("writing rendered status output to a Vec cannot fail");
        captured_rendered_output(format, buffer)
    }

    fn rendered_human(output: RenderedOutput) -> String {
        match output {
            RenderedOutput::Human(value) => value,
            other => panic!("expected human output, got {other:?}"),
        }
    }

    fn rendered_json(output: RenderedOutput) -> Value {
        match output {
            RenderedOutput::Json(value) => value,
            other => panic!("expected JSON output, got {other:?}"),
        }
    }

    fn rendered_ndjson(output: RenderedOutput) -> Vec<Value> {
        match output {
            RenderedOutput::Ndjson(values) => values,
            other => panic!("expected NDJSON output, got {other:?}"),
        }
    }

    fn sync_settings() -> config::ResolvedSync {
        config::ResolvedSync {
            source: std::path::PathBuf::from("/source"),
            remote: "/team/export".to_owned(),
            connection: config::ResolvedConnection {
                url: "https://nas.example.com".to_owned(),
                username: "bot".to_owned(),
            },
            authentication: config::ResolvedAuthentication {
                password_stdin: false,
                password_file: None,
                totp_secret_file: None,
                no_vault: true,
            },
            behavior: config::ResolvedSyncBehavior {
                compare: cli::CompareArg::Content,
                jobs: 2,
                excludes: Vec::new(),
                scope: None,
                force_resync: false,
            },
            safety: config::ResolvedSafety {
                delete: false,
                allow_empty_source: false,
                max_delete: 10,
            },
            network: config::ResolvedNetwork {
                retries: 1,
                timeout: 30,
                connect_timeout: 10,
                max_rate: None,
                ca_certificate: None,
                allow_http: false,
                danger_accept_invalid_certs: false,
            },
            output: output(cli::OutputFormat::Json),
        }
    }

    // --- Compare-mode plumbing -------------------------------------------

    #[test]
    fn force_is_not_expressible_as_a_comparison_at_all() {
        // `resync` is the only route to an unconditional re-upload, so no CompareArg maps to it
        // and neither a command line nor a profile can name it.
        for mode in [
            cli::CompareArg::Content,
            cli::CompareArg::Metadata,
            cli::CompareArg::SizeOnly,
        ] {
            assert_ne!(compare_mode(mode), CompareMode::Force);
        }
    }

    #[test]
    fn only_a_forced_resync_reaches_the_force_compare_mode() {
        let mut settings = sync_settings();
        assert_eq!(effective_compare_mode(&settings), CompareMode::Content);
        settings.behavior.force_resync = true;
        assert_eq!(effective_compare_mode(&settings), CompareMode::Force);
    }

    #[test]
    fn reconciliation_never_compares_with_force() {
        // A forced re-plan schedules every file, so reconciling against it would always report
        // pending work. Reconciliation asks a question about content.
        let mut settings = sync_settings();
        settings.behavior.force_resync = true;
        assert_eq!(reconciliation_compare_mode(&settings), CompareMode::Content);

        for mode in [
            cli::CompareArg::Content,
            cli::CompareArg::Metadata,
            cli::CompareArg::SizeOnly,
        ] {
            settings.behavior.compare = mode;
            assert_eq!(reconciliation_compare_mode(&settings), compare_mode(mode));
        }
    }

    #[test]
    fn compare_labels_are_stable_for_machine_output() {
        assert_eq!(compare_label(CompareMode::Content), "content");
        assert_eq!(compare_label(CompareMode::Metadata), "metadata");
        assert_eq!(compare_label(CompareMode::SizeOnly), "size-only");
        assert_eq!(compare_label(CompareMode::Force), "force");
    }

    // --- Scope resolution -------------------------------------------------

    #[test]
    fn an_absent_scope_resolves_to_the_whole_tree() {
        let settings = sync_settings();
        assert!(resolved_scope(&settings).unwrap().is_root());
    }

    #[test]
    fn a_configured_scope_is_parsed_and_normalized() {
        let mut settings = sync_settings();
        settings.behavior.scope = Some("/docs/q3/".to_owned());
        assert_eq!(resolved_scope(&settings).unwrap().as_str(), "docs/q3");
    }

    #[test]
    fn an_unsafe_scope_is_rejected_before_any_scan() {
        let mut settings = sync_settings();
        settings.behavior.scope = Some("../escape".to_owned());
        assert!(resolved_scope(&settings).is_err());
    }

    // --- State filter -----------------------------------------------------

    #[test]
    fn the_attention_shorthand_expands_to_the_engines_own_set() {
        // Expanded from the engine constant so the command line cannot drift from it.
        let filter = status_state_filter(&[cli::StateArg::Attention], false);
        let mut expected = plan::StateKind::ATTENTION.to_vec();
        expected.sort_unstable();
        assert_eq!(filter, plan::StateFilter::Only(expected));
    }

    #[test]
    fn no_state_argument_lists_every_state() {
        assert_eq!(status_state_filter(&[], false), plan::StateFilter::All);
        assert_eq!(
            status_state_filter(&[cli::StateArg::Differs], true),
            plan::StateFilter::All
        );
    }

    #[test]
    fn repeated_states_are_deduplicated() {
        let filter = status_state_filter(
            &[
                cli::StateArg::Differs,
                cli::StateArg::Attention,
                cli::StateArg::Differs,
            ],
            false,
        );
        let plan::StateFilter::Only(kinds) = filter else {
            panic!("expected an explicit state selection");
        };
        assert_eq!(kinds.len(), plan::StateKind::ATTENTION.len());
    }

    #[test]
    fn a_single_state_selects_only_itself() {
        assert_eq!(
            status_state_filter(&[cli::StateArg::InSync], false),
            plan::StateFilter::Only(vec![plan::StateKind::InSync])
        );
    }

    // --- Rendering --------------------------------------------------------

    #[test]
    fn human_status_leads_with_whole_scope_totals() {
        let text = rendered_human(render(&sample_page(), cli::OutputFormat::Human));
        assert!(text.contains("4 in sync"), "{text}");
        assert!(text.contains("1 differing"), "{text}");
        assert!(text.contains("2 remote-only"), "{text}");
        assert!(text.contains("compared by metadata"), "{text}");
        assert!(text.contains("docs/changed.txt"), "{text}");
        assert!(
            text.contains("local and remote sizes differ"),
            "the planner's own reason should be the row detail: {text}"
        );
    }

    #[test]
    fn human_status_offers_the_cursor_that_continues_the_listing() {
        let text = rendered_human(render(&sample_page(), cli::OutputFormat::Human));
        assert!(text.contains("--cursor \"docs/new.txt\""), "{text}");
    }

    #[test]
    fn a_scoped_listing_says_what_it_did_not_examine() {
        let text = rendered_human(render(&sample_page(), cli::OutputFormat::Human));
        assert!(
            text.contains("Only \"docs\" was examined"),
            "a scoped result must not read as a whole-tree result: {text}"
        );
    }

    #[test]
    fn an_incomplete_scan_says_its_totals_are_lower_bounds() {
        let mut page = sample_page();
        page.stats.complete = false;
        let text = rendered_human(render(&page, cli::OutputFormat::Human));
        assert!(text.contains("at least"), "{text}");
        assert!(text.contains("lower bound"), "{text}");
    }

    #[test]
    fn json_status_carries_the_page_the_totals_and_the_cursor() {
        let value = rendered_json(render(&sample_page(), cli::OutputFormat::Json));
        assert_eq!(value["schema"], "sdsync.status.v1");
        assert_eq!(value["scope"], "docs");
        assert_eq!(value["truncated"], true);
        assert_eq!(value["next_cursor"], "docs/new.txt");
        assert_eq!(value["stats"]["in_sync_files"], 4);
        assert_eq!(value["stats"]["transfer_bytes"], 46);
        assert_eq!(value["stats"]["attention_entries"], 4);
        assert_eq!(value["stats"]["complete"], true);
        assert_eq!(value["entries"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_status_row_carries_both_sides_and_the_reason() {
        let value = rendered_json(render(&sample_page(), cli::OutputFormat::Json));
        let row = &value["entries"][0];
        assert_eq!(row["schema"], "sdsync.status-entry.v1");
        assert_eq!(row["relative"], "docs/changed.txt");
        assert_eq!(row["remote_path"], "/team/export/docs/changed.txt");
        assert_eq!(row["state"], "differs");
        assert_eq!(row["reason"], "size-differs");
        assert_eq!(row["detail"], "local and remote sizes differ");
        assert_eq!(row["local"]["size"], 12);
        assert_eq!(row["remote"]["size"], 34);
    }

    #[test]
    fn ndjson_status_emits_a_summary_then_one_record_per_entry() {
        let records = rendered_ndjson(render(&sample_page(), cli::OutputFormat::Ndjson));
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["schema"], "sdsync.status.v1");
        assert_eq!(records[0]["kind"], "summary");
        assert_eq!(records[0]["stats"]["total_entries"], 11);
        assert_eq!(records[1]["schema"], "sdsync.status-entry.v1");
        assert_eq!(records[1]["state"], "differs");
        assert_eq!(records[2]["state"], "missing-remote");
    }

    #[test]
    fn a_type_conflict_row_names_both_kinds() {
        let mut page = sample_page();
        page.entries = vec![entry(
            "docs/conflict",
            plan::StatusState::TypeConflict {
                local_kind: local::EntryKind::Directory,
                remote_kind: local::EntryKind::File,
            },
        )];
        let value = rendered_json(render(&page, cli::OutputFormat::Json));
        let row = &value["entries"][0];
        assert_eq!(row["state"], "type-conflict");
        assert_eq!(row["local_kind"], "directory");
        assert_eq!(row["remote_kind"], "file");
    }

    #[test]
    fn an_excluded_row_names_its_cause() {
        let mut page = sample_page();
        page.entries = vec![entry(
            "cache/big.tmp",
            plan::StatusState::Excluded(plan::ExclusionCause::IgnoreRule),
        )];
        let value = rendered_json(render(&page, cli::OutputFormat::Json));
        assert_eq!(value["entries"][0]["state"], "excluded");
        assert_eq!(value["entries"][0]["exclusion"], "ignore-rule");
    }

    #[test]
    fn an_in_sync_row_carries_no_change_reason() {
        let mut page = sample_page();
        page.entries = vec![entry("docs/same.txt", plan::StatusState::InSync)];
        let value = rendered_json(render(&page, cli::OutputFormat::Json));
        assert_eq!(value["entries"][0]["state"], "in-sync");
        assert!(value["entries"][0].get("reason").is_none());
    }

    // --- Forced plan notice ----------------------------------------------

    fn forced_plan() -> SyncPlan {
        SyncPlan {
            pre_deletes: Vec::new(),
            creates: Vec::new(),
            copies: Vec::new(),
            uploads: vec![
                plan::UploadAction {
                    local: local::LocalEntry {
                        relative: "a.txt".to_owned(),
                        full_path: std::path::PathBuf::from("a.txt"),
                        kind: local::EntryKind::File,
                        size: 1_000,
                        mtime_ms: 0,
                        identity: Default::default(),
                        content_md5: None,
                    },
                    remote_path: "/team/export/a.txt".to_owned(),
                    reason: plan::ChangeReason::Forced,
                },
                plan::UploadAction {
                    local: local::LocalEntry {
                        relative: "b.txt".to_owned(),
                        full_path: std::path::PathBuf::from("b.txt"),
                        kind: local::EntryKind::File,
                        size: 24,
                        mtime_ms: 0,
                        identity: Default::default(),
                        content_md5: None,
                    },
                    remote_path: "/team/export/b.txt".to_owned(),
                    reason: plan::ChangeReason::Forced,
                },
            ],
            post_deletes: Vec::new(),
            unchanged_files: 0,
            protected_entries: 0,
            upload_bytes: 1_024,
        }
    }

    #[test]
    fn a_forced_plan_states_what_it_will_overwrite_before_anything_runs() {
        let text = plan_human(&forced_plan(), false);
        assert!(
            text.contains("2 remote files will be overwritten without being compared"),
            "{text}"
        );
        assert!(text.contains("1.0 KiB") || text.contains("1024"), "{text}");
        assert!(
            text.contains("Nothing is deleted by --compare force alone"),
            "the notice must not imply deletion: {text}"
        );
    }

    #[test]
    fn an_ordinary_plan_carries_no_forced_notice() {
        let mut plan = forced_plan();
        for upload in &mut plan.uploads {
            upload.reason = plan::ChangeReason::MissingRemote;
        }
        let text = plan_human(&plan, false);
        assert!(
            !text.contains("overwritten without being compared"),
            "{text}"
        );
    }

    #[test]
    fn a_scoped_plan_says_the_case_check_covered_the_scope_alone() {
        let scope = plan::Scope::parse("docs/q3").unwrap();
        let mut buffer = Vec::new();
        write_plan_human_to(&mut buffer, &forced_plan(), false, &scope).unwrap();
        let text = String::from_utf8(buffer).unwrap();
        assert!(text.contains("Scoped to \"docs/q3\""), "{text}");
        assert!(
            text.contains("letter case covered this scope alone"),
            "a clean scoped run must not read as a whole-tree clean bill of health: {text}"
        );
    }

    #[test]
    fn an_unscoped_plan_makes_no_scope_claim() {
        let text = plan_human(&forced_plan(), false);
        assert!(!text.contains("Scoped to"), "{text}");
    }
}

#[cfg(test)]
mod resync_output_tests {
    use super::*;

    fn output(format: cli::OutputFormat) -> config::ResolvedOutput {
        config::ResolvedOutput {
            verbosity: 0,
            quiet: true,
            log_level: cli::LogLevel::Off,
            log_format: cli::LogFormat::Human,
            log_file: None,
            remote_log_url: None,
            remote_log_token: None,
            remote_log_mode: cli::RemoteLogMode::BestEffort,
            progress: cli::ProgressMode::Never,
            output: format,
        }
    }

    fn upload(relative: &str, size: u64) -> plan::UploadAction {
        plan::UploadAction {
            local: local::LocalEntry {
                relative: relative.to_owned(),
                full_path: std::path::PathBuf::from(relative),
                kind: local::EntryKind::File,
                size,
                mtime_ms: 1_000,
                identity: Default::default(),
                content_md5: None,
            },
            remote_path: format!("/team/export/{relative}"),
            reason: plan::ChangeReason::Forced,
        }
    }

    fn forced_plan() -> SyncPlan {
        SyncPlan {
            pre_deletes: Vec::new(),
            creates: Vec::new(),
            copies: Vec::new(),
            uploads: vec![upload("docs/a.txt", 1_000), upload("docs/b.txt", 24)],
            post_deletes: Vec::new(),
            unchanged_files: 0,
            protected_entries: 0,
            upload_bytes: 1_024,
        }
    }

    fn render(
        format: cli::OutputFormat,
        ticket: Option<&str>,
        stale: Option<&str>,
        report: Option<&ExecutionReport>,
    ) -> RenderedOutput {
        let scope = plan::Scope::parse("docs").unwrap();
        let mut buffer = Vec::new();
        write_resync_output_to(
            &mut buffer,
            &forced_plan(),
            report,
            Duration::from_millis(7),
            &output(format),
            &scope,
            ticket,
            stale,
        )
        .expect("writing rendered resync output to a Vec cannot fail");
        captured_rendered_output(format, buffer)
    }

    fn human(output: RenderedOutput) -> String {
        match output {
            RenderedOutput::Human(value) => value,
            other => panic!("expected human output, got {other:?}"),
        }
    }

    fn json(output: RenderedOutput) -> Value {
        match output {
            RenderedOutput::Json(value) => value,
            other => panic!("expected JSON output, got {other:?}"),
        }
    }

    #[test]
    fn a_planning_run_says_nothing_changed_and_offers_the_ticket() {
        let text = human(render(
            cli::OutputFormat::Human,
            Some("abc1230000000000"),
            None,
            None,
        ));
        assert!(text.contains("Nothing has been changed"), "{text}");
        assert!(text.contains("--confirm abc1230000000000"), "{text}");
        // The overwrite framing comes from the plan itself.
        assert!(
            text.contains("2 remote files will be overwritten without being compared"),
            "{text}"
        );
        assert!(text.contains("Nothing is deleted"), "{text}");
    }

    #[test]
    fn a_planning_run_lists_the_files_it_would_overwrite() {
        let text = human(render(
            cli::OutputFormat::Human,
            Some("abc1230000000000"),
            None,
            None,
        ));
        // The caller has to see the set, not just its size, or confirming means nothing.
        assert!(text.contains("docs/a.txt"), "{text}");
        assert!(text.contains("docs/b.txt"), "{text}");
    }

    #[test]
    fn a_stale_ticket_is_refused_and_answered_with_a_fresh_plan_and_ticket() {
        // One step, not two: a changing destination must not trap the caller in a loop.
        let text = human(render(
            cli::OutputFormat::Human,
            Some("newticket0000000"),
            Some("staleticket00000"),
            None,
        ));
        assert!(text.contains("staleticket00000"), "{text}");
        assert!(text.contains("no longer matches"), "{text}");
        assert!(text.contains("nothing was changed"), "{text}");
        assert!(text.contains("refreshed plan follows"), "{text}");
        assert!(text.contains("--confirm newticket0000000"), "{text}");
    }

    #[test]
    fn a_confirmed_run_reports_what_it_uploaded_and_offers_no_ticket() {
        let report = ExecutionReport {
            created: 0,
            deleted: 0,
            copied: 0,
            uploaded: 2,
            uploaded_bytes: 1_024,
        };
        let text = human(render(cli::OutputFormat::Human, None, None, Some(&report)));
        assert!(text.contains("Resync complete: 2 re-uploaded"), "{text}");
        assert!(!text.contains("--confirm"), "{text}");
    }

    #[test]
    fn json_planning_output_carries_the_paths_bytes_and_ticket() {
        let value = json(render(
            cli::OutputFormat::Json,
            Some("abc1230000000000"),
            None,
            None,
        ));
        assert_eq!(value["schema"], "sdsync.resync.v1");
        assert_eq!(value["kind"], "plan");
        assert_eq!(value["scope"], "docs");
        assert_eq!(value["confirmed"], false);
        assert_eq!(value["ticket"], "abc1230000000000");
        assert_eq!(value["overwrites"], 2);
        assert_eq!(value["overwrite_bytes"], 1_024);
        assert_eq!(value["deletes"], 0);
        assert_eq!(value["paths"].as_array().unwrap().len(), 2);
        assert_eq!(value["paths"][0]["relative"], "docs/a.txt");
        assert_eq!(value["paths"][0]["remote_path"], "/team/export/docs/a.txt");
    }

    #[test]
    fn json_names_the_stale_ticket_alongside_its_replacement() {
        let value = json(render(
            cli::OutputFormat::Json,
            Some("newticket0000000"),
            Some("staleticket00000"),
            None,
        ));
        assert_eq!(value["stale_ticket"], "staleticket00000");
        assert_eq!(value["ticket"], "newticket0000000");
        assert_eq!(value["confirmed"], false);
    }

    #[test]
    fn a_resync_plan_never_reports_deletions() {
        let value = json(render(
            cli::OutputFormat::Json,
            Some("abc1230000000000"),
            None,
            None,
        ));
        assert_eq!(value["deletes"], 0);
    }

    #[test]
    fn the_status_stats_echo_the_comparison_that_produced_them() {
        // A view that deliberately asks for a cheap comparison needs to be able to label it.
        let stats = plan::StatusStats {
            compare: CompareMode::Metadata,
            in_sync_files: 1,
            differing_files: 0,
            missing_remote_files: 0,
            remote_only_entries: 0,
            type_conflicts: 0,
            excluded_entries: 0,
            directories: 0,
            in_sync_bytes: 4,
            transfer_bytes: 0,
            total_entries: 1,
            attention_entries: 0,
            complete: true,
        };
        assert_eq!(status_stats_value(&stats)["compare"], "metadata");

        let content = plan::StatusStats {
            compare: CompareMode::Content,
            ..stats
        };
        assert_eq!(status_stats_value(&content)["compare"], "content");
    }
}
