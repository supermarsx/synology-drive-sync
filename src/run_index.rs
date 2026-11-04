//! A run's use of the verified-pair index (`--pair-index`), from the probe that starts beside the
//! local walk to the end-of-run report.
//!
//! The rules live in the library ([`synology_drive_sync::pair_index`]); this is the command line's
//! wiring: when the index applies, how it is bound, and how what it did is named.

use std::sync::Arc;

use serde_json::{Value, json};
use synology_drive_sync::Result;
use synology_drive_sync::api::ApiClient;
use synology_drive_sync::observability::{
    EventCode, EventLogger, EventMetrics, EventReason, LogEvent, LogLevel as EventLogLevel,
};
#[cfg(unix)]
use synology_drive_sync::pair_index::{Binding, IndexOptions};
use synology_drive_sync::pair_index::{
    IndexIssue, IndexReport, IndexSession, IndexState, Severity,
};

use crate::{cli, config, log_event};

/// Where a run's index stands before it is opened.
pub(crate) enum IndexStart {
    /// No `--pair-index`.
    Off,
    /// Asked for, but not usable this run: the report says why.
    Report(IndexReport),
    /// The directory is claimed; the file is opened (and verified) once the listing is done.
    #[cfg(unix)]
    Ready {
        paths: synology_drive_sync::pair_index_redb::IndexPaths,
        cache_mib: u64,
    },
}

/// Start the index for a run: check it applies and claim its directory.
pub(crate) fn begin(profile: &str, settings: &config::ResolvedSync) -> IndexStart {
    let Some(directory) = settings.behavior.pair_index.as_deref() else {
        return IndexStart::Off;
    };
    if settings.behavior.compare != cli::CompareArg::Content || settings.behavior.force_resync {
        // Only a content comparison reads digests; nothing else may read or write evidence.
        return IndexStart::Report(IndexReport {
            state: IndexState::Off,
            issues: vec![IndexIssue::info(
                "pair-index-not-applicable",
                directory.display().to_string(),
                "only a content comparison uses the verified-pair index; this run neither reads \
                 nor writes it",
            )],
            ..IndexReport::default()
        });
    }
    #[cfg(not(unix))]
    {
        let _ = profile;
        IndexStart::Report(IndexReport {
            state: IndexState::Off,
            issues: vec![IndexIssue::info(
                "pair-index-unsupported-platform",
                directory.display().to_string(),
                "this platform exposes no status-change time or file identity to a directory \
                 walk, so stored evidence could not tell an in-place rewrite from an unchanged \
                 file; every pair is verified live",
            )],
            ..IndexReport::default()
        })
    }
    #[cfg(unix)]
    {
        use synology_drive_sync::pair_index_redb::{IndexPaths, prepare_directory};
        let paths =
            match prepare_directory(directory).and_then(|()| IndexPaths::new(directory, profile)) {
                Ok(paths) => paths,
                Err(issue) => return IndexStart::Report(IndexReport::unusable(issue)),
            };
        IndexStart::Ready {
            paths,
            cache_mib: settings.behavior.pair_index_cache_mib,
        }
    }
}

/// Open the index, verified, bound to this run.
///
/// `Err` carries the report for an index that could not be opened or bound; the run goes on with
/// every pair verified live.
pub(crate) fn open(
    start: IndexStart,
    profile: &str,
    settings: &config::ResolvedSync,
    client: &ApiClient,
) -> std::result::Result<Option<IndexSession>, Box<IndexReport>> {
    // Without the index (not Unix), nothing below needs the run's settings or its client.
    #[cfg(not(unix))]
    let _ = (profile, settings, client);
    match start {
        IndexStart::Off => Ok(None),
        IndexStart::Report(report) => Err(Box::new(report)),
        #[cfg(unix)]
        IndexStart::Ready { paths, cache_mib } => {
            use synology_drive_sync::pair_index_redb::RedbPairIndex;
            let label = paths.label();
            let opened = RedbPairIndex::open(paths, cache_mib);
            let Some(backend) = opened.backend else {
                let mut report = IndexReport {
                    state: IndexState::Unusable,
                    ..IndexReport::default()
                };
                report.issues = opened.issues;
                return Err(Box::new(report));
            };
            let binding = match binding(profile, settings, client) {
                Ok(binding) => binding,
                Err(cause) => {
                    drop(backend);
                    let mut report = IndexReport::unusable(IndexIssue::warning(
                        "pair-index-unbound",
                        label,
                        cause,
                    ));
                    report.issues.splice(0..0, opened.issues);
                    return Err(Box::new(report));
                }
            };
            let mut session = IndexSession::open(
                Box::new(backend),
                binding,
                IndexOptions {
                    max_age_seconds: i64::from(settings.behavior.pair_index_max_age_days) * 86_400,
                    canary: settings.behavior.pair_index_canary,
                    refresh: settings.behavior.pair_index_refresh,
                    racy: synology_drive_sync::pair_index::RacyWindows::for_host(),
                },
                std::time::SystemTime::now(),
            );
            if opened.clear {
                session.discard_evidence();
            }
            session.report.issues.splice(0..0, opened.issues);
            Ok(Some(session))
        }
    }
}

/// What the index is bound to: the profile, the canonical source and its device, the remote root,
/// and the endpoint plus the File Station hostname that answered.
#[cfg(unix)]
fn binding(
    profile: &str,
    settings: &config::ResolvedSync,
    client: &ApiClient,
) -> std::result::Result<Binding, String> {
    use std::os::unix::fs::MetadataExt as _;
    let source = std::fs::canonicalize(&settings.source)
        .map_err(|error| format!("the source could not be resolved: {error}"))?;
    let source_device = std::fs::metadata(&source)
        .map_err(|error| format!("the source could not be examined: {error}"))?
        .dev();
    let info = client.file_station_info().map_err(|error| {
        format!(
            "File Station did not name the host serving this run ({error}); the index binds \
             to it, so it was not used"
        )
    })?;
    let hostname = info
        .hostname
        .as_ref()
        .map(|hostname| hostname.as_str().to_owned())
        .ok_or_else(|| {
            "File Station did not name the host serving this run; the index binds to it, so \
             it was not used"
                .to_owned()
        })?;
    Ok(Binding {
        profile: profile.to_owned(),
        source: source.display().to_string(),
        source_device,
        remote: settings.remote.clone(),
        server: format!("{} {hostname}", settings.connection.url),
    })
}

/// Name every warning on standard error, and log the run's index events.
pub(crate) fn report(
    report: &IndexReport,
    output: &config::ResolvedOutput,
    logger: Option<&Arc<EventLogger>>,
    profile: &str,
) -> Result<()> {
    for issue in &report.issues {
        if issue.severity == Severity::Warning && !output.quiet {
            eprintln!("warning: {}: {} ({})", issue.code, issue.cause, issue.path);
        }
        let (level, code, reason) = match issue.code {
            "pair-index-canary-mismatch" => (
                EventLogLevel::Warn,
                EventCode::PairIndexCanaryMismatch,
                EventReason::EvidenceCanaryMismatch,
            ),
            "pair-index-store-failed" => (
                EventLogLevel::Warn,
                EventCode::PairIndexStoreFailed,
                EventReason::PairIndexStoreFailed,
            ),
            _ if issue.severity == Severity::Warning => (
                EventLogLevel::Warn,
                EventCode::PairIndexUnusable,
                EventReason::PairIndexUnusable,
            ),
            _ => continue,
        };
        log_event(
            logger,
            LogEvent::new(level, code)
                .reason(reason)
                .profile(profile)
                .path(&issue.path),
        )?;
    }
    if report.state != IndexState::Off {
        log_event(
            logger,
            LogEvent::new(EventLogLevel::Info, EventCode::PairIndexServed)
                .profile(profile)
                .metrics(EventMetrics {
                    operations: report.served as u64,
                    files: report.verified_live as u64,
                    ..EventMetrics::default()
                }),
        )?;
    }
    Ok(())
}

/// The `pair_index` member of a plan or a result.
pub(crate) fn value(report: &IndexReport) -> Value {
    json!({
        "state": report.state.as_str(),
        "served": report.served,
        "verified_live": report.verified_live,
        "racy": report.racy,
        "expired": report.expired,
        "canary_checked": report.canary_checked,
        "canary_inconclusive": report.canary_inconclusive,
        "canary_mismatches": report.canary_mismatches,
        "oldest_evidence_epoch": report.oldest_evidence_epoch,
        "remote_change_signal": report.remote_signal.map(|signal| signal.as_str()),
        "stored": report.stored,
        "removed": report.removed,
        "entries": report.entries,
        "issues": report.issues.iter().map(|issue| json!({
            "code": issue.code,
            "severity": issue.severity.as_str(),
            "path": issue.path,
            "cause": issue.cause,
        })).collect::<Vec<_>>(),
    })
}

/// One human line for the index, when there is one to say.
pub(crate) fn human_line(report: &IndexReport) -> Option<String> {
    if report.state == IndexState::Off && report.issues.is_empty() {
        return None;
    }
    let total = report.served + report.verified_live;
    Some(format!(
        "Verified-pair index ({}): {} of {} pair(s) taken from evidence; {} verified live, {} \
         as canary.",
        report.state.as_str(),
        report.served,
        total,
        report.verified_live,
        report.canary_checked
    ))
}
