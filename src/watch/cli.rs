//! `sdsync-dsm-api --watch` and `sdsync-dsm-api --watch-probe`.
//!
//! `--watch` is the `inotifywait` subset the shell controller spawns for a realtime routine:
//!
//! ```text
//! sdsync-dsm-api --watch [--profile-fragment <toml>] [--setup-deadline <s>] \
//!     -m -q -r --format x -e close_write,create,delete,move,attrib <source>
//! ```
//!
//! It writes the `--format` literal once per coalesced batch of changes (at most one line per
//! [`super::COALESCE_INTERVAL`]), and `<format> overflow` when the kernel dropped events. The controller
//! reads only the marker file's size, inode and mtime, never its text. On any failure it writes
//! one line to stderr -- a `backend_reason` token first, then the numbers or the path -- and exits
//! non-zero; the controller reads that first line into the routine's watch-failure record.
//!
//! `--watch-probe` arms the same tree, says how its size compares with the watch limit, and prints
//! what arrives while an operator changes files, so whether inotify works for a share as the
//! package user can be answered on the NAS itself.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use super::{EventSet, WatchError};

/// How long the probe waits for events unless told otherwise.
pub const PROBE_DEFAULT_SECONDS: u64 = 60;

/// A parsed `--watch` command line.
#[derive(Debug, Eq, PartialEq)]
pub struct WatchArgs {
    pub source: PathBuf,
    pub format: String,
    pub events: EventSet,
    pub fragment: Option<PathBuf>,
    pub setup_deadline: Duration,
}

/// A parsed `--watch-probe` command line.
#[derive(Debug, Eq, PartialEq)]
pub struct ProbeArgs {
    pub source: PathBuf,
    pub seconds: u64,
    pub fragment: Option<PathBuf>,
    pub setup_deadline: Duration,
}

fn usage(detail: impl Into<String>) -> WatchError {
    WatchError::Usage(detail.into())
}

fn value_of(
    arguments: &mut std::slice::Iter<'_, OsString>,
    option: &str,
) -> Result<OsString, WatchError> {
    arguments
        .next()
        .cloned()
        .ok_or_else(|| usage(format!("{option} needs a value")))
}

fn seconds_of(value: OsString, option: &str, maximum: u64) -> Result<u64, WatchError> {
    value
        .to_str()
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|seconds| (1..=maximum).contains(seconds))
        .ok_or_else(|| usage(format!("{option} takes whole seconds from 1 to {maximum}")))
}

/// The arguments after `--watch`.
///
/// Monitor and recursive mode are required rather than defaulted: a caller that meant a one-shot
/// or a single directory would otherwise get something it did not ask for in silence.
pub fn parse_watch_args(arguments: &[OsString]) -> Result<WatchArgs, WatchError> {
    let mut monitor = false;
    let mut recursive = false;
    let mut format = None;
    let mut events = EventSet::default();
    let mut fragment = None;
    let mut setup_deadline = super::DEFAULT_SETUP_DEADLINE;
    let mut source = None;
    let mut iterator = arguments.iter();
    let mut options_ended = false;
    while let Some(argument) = iterator.next() {
        let text = argument.to_string_lossy();
        if options_ended || !text.starts_with('-') || text == "-" {
            if source.replace(PathBuf::from(argument)).is_some() {
                return Err(usage("exactly one source directory is watched"));
            }
            continue;
        }
        match text.as_ref() {
            "--" => options_ended = true,
            "-m" | "--monitor" => monitor = true,
            "-r" | "--recursive" => recursive = true,
            "-q" | "-qq" | "--quiet" => {}
            "--format" => {
                let value = value_of(&mut iterator, "--format")?;
                let value = value
                    .into_string()
                    .map_err(|_| usage("--format must be text"))?;
                if value.is_empty() || value.contains('%') || value.chars().any(char::is_control) {
                    return Err(usage(
                        "--format must be a non-empty literal without % directives",
                    ));
                }
                format = Some(value);
            }
            "-e" | "--event" => {
                let value = value_of(&mut iterator, "-e")?;
                for name in value.to_string_lossy().split(',') {
                    events.add(name.trim())?;
                }
            }
            "--profile-fragment" => {
                fragment = Some(PathBuf::from(value_of(
                    &mut iterator,
                    "--profile-fragment",
                )?));
            }
            "--setup-deadline" => {
                let value = value_of(&mut iterator, "--setup-deadline")?;
                setup_deadline = Duration::from_secs(seconds_of(value, "--setup-deadline", 3600)?);
            }
            other => return Err(usage(format!("unsupported argument {other:?}"))),
        }
    }
    if !monitor || !recursive {
        return Err(usage(
            "only monitor (-m) recursive (-r) watching is supported",
        ));
    }
    let source = source.ok_or_else(|| usage("a source directory is required"))?;
    if events.is_empty() {
        events = EventSet::all();
    }
    Ok(WatchArgs {
        source,
        format: format.unwrap_or_else(|| "x".to_owned()),
        events,
        fragment,
        setup_deadline,
    })
}

/// The arguments after `--watch-probe`.
pub fn parse_probe_args(arguments: &[OsString]) -> Result<ProbeArgs, WatchError> {
    let mut seconds = PROBE_DEFAULT_SECONDS;
    let mut fragment = None;
    let mut setup_deadline = super::DEFAULT_SETUP_DEADLINE;
    let mut source = None;
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        match argument.to_string_lossy().as_ref() {
            "--seconds" => {
                seconds = seconds_of(value_of(&mut iterator, "--seconds")?, "--seconds", 3600)?;
            }
            "--profile-fragment" => {
                fragment = Some(PathBuf::from(value_of(
                    &mut iterator,
                    "--profile-fragment",
                )?));
            }
            "--setup-deadline" => {
                let value = value_of(&mut iterator, "--setup-deadline")?;
                setup_deadline = Duration::from_secs(seconds_of(value, "--setup-deadline", 3600)?);
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(usage(format!("unsupported argument {other:?}")));
            }
            _ => {
                if source.replace(PathBuf::from(argument)).is_some() {
                    return Err(usage("exactly one source directory is probed"));
                }
            }
        }
    }
    Ok(ProbeArgs {
        source: source.ok_or_else(|| {
            usage("usage: --watch-probe [--seconds N] [--profile-fragment <toml>] <source>")
        })?,
        seconds,
        fragment,
        setup_deadline,
    })
}

/// Write the failure line and turn it into an exit status. A closed stderr cannot be reported
/// anywhere, so its write error is the one thing deliberately dropped.
fn fail(error: &WatchError) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "{error}");
    ExitCode::from(error.exit_code())
}

/// `sdsync-dsm-api --watch ...`. Runs until it is signalled or fails.
pub fn watch_main(arguments: &[OsString]) -> ExitCode {
    let parsed = match parse_watch_args(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    fail(&native::watch(&parsed))
}

/// `sdsync-dsm-api --watch-probe ...`: 0 when events arrived, 1 when armed but none did.
pub fn probe_main(arguments: &[OsString]) -> ExitCode {
    let parsed = match parse_probe_args(arguments) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match native::probe(&parsed, &mut out) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            let _ = writeln!(out, "refused: {error}");
            let _ = out.flush();
            fail(&error)
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod native {
    use super::{ProbeArgs, WatchArgs, WatchError};

    fn unavailable() -> WatchError {
        WatchError::Unavailable("native watching needs Linux inotify".to_owned())
    }

    pub(super) fn watch(_: &WatchArgs) -> WatchError {
        unavailable()
    }

    pub(super) fn probe(_: &ProbeArgs, _: &mut dyn std::io::Write) -> Result<bool, WatchError> {
        Err(unavailable())
    }
}

#[cfg(target_os = "linux")]
mod native {
    use std::io::{self, Write};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::{ProbeArgs, WatchArgs};
    use crate::watch::{
        COALESCE_INTERVAL, Change, Coalescer, EventSet, Filter, LimitPolicy, TreeWatch, WatchError,
        WatchOptions, profile_excludes,
    };

    /// Events the probe prints before it only counts them.
    const PROBE_PRINTED_EVENTS: usize = 200;

    /// How long the watcher sleeps when nothing is pending. Only bounds how stale the process
    /// can be when a signal does not interrupt it; any event wakes it at once.
    const IDLE_WAIT: Duration = Duration::from_secs(60);

    fn arm(
        source: &Path,
        fragment: Option<&Path>,
        events: EventSet,
        setup_deadline: Duration,
    ) -> Result<TreeWatch, WatchError> {
        let extra = match fragment {
            Some(fragment) => profile_excludes(fragment)?,
            None => Vec::new(),
        };
        let filter = Filter::build(source, extra)?;
        TreeWatch::arm(
            source,
            WatchOptions {
                filter,
                events,
                limit: LimitPolicy::System,
                setup_deadline,
            },
        )
    }

    fn write_marker(out: &mut impl Write, format: &str, overflow: bool) -> io::Result<()> {
        if overflow {
            writeln!(out, "{format} overflow")?;
        } else {
            writeln!(out, "{format}")?;
        }
        out.flush()
    }

    /// Watch until something fails, and return what did.
    pub(super) fn watch(arguments: &WatchArgs) -> WatchError {
        // The controller that spawned this watcher is its only reader. If the controller dies
        // without stopping it -- killed, or a crash -- the watcher must not run on unattended,
        // holding watches against the per-user budget for a marker nobody reads.
        if let Err(error) = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM) {
            return WatchError::Setup {
                path: arguments.source.clone(),
                detail: format!(
                    "cannot arrange to stop with the controller: {}",
                    error.desc()
                ),
            };
        }
        let mut tree = match arm(
            &arguments.source,
            arguments.fragment.as_deref(),
            arguments.events,
            arguments.setup_deadline,
        ) {
            Ok(tree) => tree,
            Err(error) => return error,
        };
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let mut coalescer = Coalescer::new(COALESCE_INTERVAL);
        loop {
            let timeout = coalescer.wait_hint(Instant::now()).unwrap_or(IDLE_WAIT);
            match tree.wait(timeout) {
                Ok(changes) => {
                    for change in changes {
                        coalescer.note(change == Change::Overflow);
                    }
                }
                Err(error) => {
                    // What was seen is written before the watcher stops, and a source that went
                    // away is itself a change.
                    let pending = coalescer.take_now();
                    if pending.is_some() || matches!(error, WatchError::SourceGone { .. }) {
                        let _ = write_marker(&mut out, &arguments.format, pending.unwrap_or(false));
                    }
                    return error;
                }
            }
            if let Some(overflow) = coalescer.take(Instant::now())
                && let Err(error) = write_marker(&mut out, &arguments.format, overflow)
            {
                return WatchError::Output(format!("cannot write the event marker: {error}"));
            }
        }
    }

    fn line(out: &mut dyn Write, text: std::fmt::Arguments<'_>) -> Result<(), WatchError> {
        out.write_fmt(text)
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush())
            .map_err(|error| WatchError::Output(format!("cannot write the probe report: {error}")))
    }

    /// Arm, report the size against the limit, then print what arrives. `Ok(true)` when at least
    /// one change was seen.
    pub(super) fn probe(arguments: &ProbeArgs, out: &mut dyn Write) -> Result<bool, WatchError> {
        line(out, format_args!("sdsync-dsm-api --watch-probe"))?;
        line(out, format_args!("source: {}", arguments.source.display()))?;
        match std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("Uid:").map(str::trim).map(str::to_owned))
            }) {
            Some(uids) => line(
                out,
                format_args!("user: uid {uids} (real, effective, saved, fs)"),
            )?,
            None => line(
                out,
                format_args!("user: uid unknown (/proc/self/status unreadable)"),
            )?,
        }
        match &arguments.fragment {
            Some(fragment) => {
                let count = profile_excludes(fragment)?.len();
                line(
                    out,
                    format_args!(
                        "profile excludes: {count} pattern(s) from {}",
                        fragment.display()
                    ),
                )?;
            }
            None => line(
                out,
                format_args!(
                    "profile excludes: none given (DSM-managed names and .sdsyncignore still apply)"
                ),
            )?,
        }
        let started = Instant::now();
        let mut tree = arm(
            &arguments.source,
            arguments.fragment.as_deref(),
            EventSet::all(),
            arguments.setup_deadline,
        )?;
        let (limit, held, budget) = tree.limits();
        let shown =
            |value: Option<u64>| value.map_or_else(|| "unknown".to_owned(), |v| v.to_string());
        line(
            out,
            format_args!(
                "limit: max_user_watches={} held_by_this_user={held} budget={} (80% of the limit, \
                 less what is held)",
                shown(limit),
                shown(budget)
            ),
        )?;
        if let Some(note) = tree.limit_note() {
            line(out, format_args!("limit note: {note}"))?;
        }
        let watched = tree.watch_count();
        let share = budget
            .filter(|budget| *budget > 0)
            .map(|budget| format!(" ({}% of the budget)", watched as u64 * 100 / budget))
            .unwrap_or_default();
        line(
            out,
            format_args!(
                "armed: {watched} directories watched in {:.2} s{share}",
                started.elapsed().as_secs_f64()
            ),
        )?;
        line(
            out,
            format_args!(
                "waiting {} s for events; change something in the source now",
                arguments.seconds
            ),
        )?;
        let deadline = Instant::now() + Duration::from_secs(arguments.seconds);
        let waiting = Instant::now();
        let (mut seen, mut overflows, mut new_dirs) = (0usize, 0usize, 0usize);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if remaining.is_zero() {
                break;
            }
            let changes = match tree.wait(remaining) {
                Ok(changes) => changes,
                Err(error) => {
                    line(out, format_args!("stopped: {error}"))?;
                    return Err(error);
                }
            };
            for change in changes {
                let offset = waiting.elapsed().as_secs_f64();
                match change {
                    Change::Overflow => {
                        overflows += 1;
                        line(
                            out,
                            format_args!(
                                "+{offset:.3}s overflow: the kernel dropped events; the tree \
                                 was walked again"
                            ),
                        )?;
                    }
                    Change::Path { entry, kind, .. } => {
                        seen += 1;
                        if kind == "new_dir" {
                            new_dirs += 1;
                        }
                        if seen <= PROBE_PRINTED_EVENTS {
                            line(out, format_args!("+{offset:.3}s {kind:<11} {entry}"))?;
                        } else if seen == PROBE_PRINTED_EVENTS + 1 {
                            line(out, format_args!("(further events are counted, not shown)"))?;
                        }
                    }
                }
            }
        }
        line(
            out,
            format_args!(
                "summary: {seen} change(s), {overflows} overflow(s), {new_dirs} new \
                 director(ies), {} directories watched at the end",
                tree.watch_count()
            ),
        )?;
        if seen > 0 || overflows > 0 {
            line(
                out,
                format_args!("verdict: inotify reports changes in this source for this user"),
            )?;
            Ok(true)
        } else {
            line(
                out,
                format_args!(
                    "verdict: armed, but no change arrived in {} s. If files were changed in that \
                     time, inotify does not see writes made that way to this share; try SMB, \
                     File Station and Synology Drive separately",
                    arguments.seconds
                ),
            )?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_controllers_exact_argv_parses() {
        let parsed = parse_watch_args(&arguments(&[
            "--profile-fragment",
            "/var/packages/synology-drive-sync/home/config/profiles.d/alpha.toml",
            "-m",
            "-q",
            "-r",
            "--format",
            "x",
            "-e",
            "close_write,create,delete,move,attrib",
            "/volume1/Source Folder",
        ]))
        .unwrap();
        assert_eq!(parsed.source, PathBuf::from("/volume1/Source Folder"));
        assert_eq!(parsed.format, "x");
        assert!(parsed.events.close_write && parsed.events.moved_from && parsed.events.attrib);
        assert!(!parsed.events.modify);
        assert_eq!(
            parsed.fragment.as_deref(),
            Some(std::path::Path::new(
                "/var/packages/synology-drive-sync/home/config/profiles.d/alpha.toml"
            ))
        );
        assert_eq!(parsed.setup_deadline, crate::watch::DEFAULT_SETUP_DEADLINE);
        // The inotifywait argv the controller used before, unchanged, parses too.
        let legacy = parse_watch_args(&arguments(&[
            "-m",
            "-q",
            "-r",
            "--format",
            "x",
            "-e",
            "close_write,create,delete,move",
            "--",
            "-odd source",
        ]))
        .unwrap();
        assert_eq!(legacy.source, PathBuf::from("-odd source"));
        assert!(!legacy.events.attrib);
    }

    #[test]
    fn anything_outside_the_subset_is_refused_by_name() {
        for (argv, needle) in [
            (vec!["-r", "--format", "x", "/s"], "monitor"),
            (vec!["-m", "--format", "x", "/s"], "recursive"),
            (vec!["-m", "-r"], "source directory is required"),
            (vec!["-m", "-r", "/a", "/b"], "exactly one"),
            (vec!["-m", "-r", "--format", "%w%f", "/s"], "% directives"),
            (vec!["-m", "-r", "--format", "", "/s"], "non-empty"),
            (vec!["-m", "-r", "--format"], "needs a value"),
            (vec!["-m", "-r", "-e", "open", "/s"], "\"open\""),
            (vec!["-m", "-r", "--exclude", "x", "/s"], "--exclude"),
            (
                vec!["-m", "-r", "--setup-deadline", "0", "/s"],
                "whole seconds",
            ),
            (
                vec!["-m", "-r", "--setup-deadline", "soon", "/s"],
                "whole seconds",
            ),
        ] {
            let failure = parse_watch_args(&arguments(&argv)).unwrap_err();
            assert_eq!(failure.reason(), "watch_setup_failed", "{argv:?}");
            assert_eq!(failure.exit_code(), 64, "{argv:?}");
            assert!(failure.to_string().contains(needle), "{argv:?}: {failure}");
        }
    }

    #[test]
    fn without_an_event_list_every_supported_kind_is_reported() {
        let parsed =
            parse_watch_args(&arguments(&["-m", "-r", "--setup-deadline", "5", "/s"])).unwrap();
        assert_eq!(parsed.events, EventSet::all());
        assert_eq!(parsed.setup_deadline, Duration::from_secs(5));
    }

    #[test]
    fn the_probe_takes_a_source_and_optional_bounds() {
        let parsed = parse_probe_args(&arguments(&["/volume1/share"])).unwrap();
        assert_eq!(parsed.seconds, PROBE_DEFAULT_SECONDS);
        assert_eq!(parsed.fragment, None);
        let parsed = parse_probe_args(&arguments(&[
            "--seconds",
            "5",
            "--profile-fragment",
            "/p.toml",
            "--setup-deadline",
            "9",
            "/volume1/share",
        ]))
        .unwrap();
        assert_eq!(parsed.seconds, 5);
        assert_eq!(parsed.fragment, Some(PathBuf::from("/p.toml")));
        assert_eq!(parsed.setup_deadline, Duration::from_secs(9));
        for argv in [
            vec![],
            vec!["--seconds", "0", "/s"],
            vec!["--seconds", "3601", "/s"],
            vec!["--bogus", "/s"],
            vec!["/a", "/b"],
        ] {
            assert_eq!(
                parse_probe_args(&arguments(&argv)).unwrap_err().exit_code(),
                64
            );
        }
    }
}
