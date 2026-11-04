//! `sdsyncd`: the single Rust daemon that replaces the NAS shell controller,
//! runner and queue (daemon design, Phase 2).
//!
//! Layout (design §2.1): portable, pure modules first, so Windows and macOS CI
//! build and test them; the Linux-only process, signal and socket code is
//! cfg-gated and keeps every `unsafe` call in `sys`.
//!
//! - `window`: daily windows and DST (port of `routine_window_state`).

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[cfg(target_os = "linux")]
pub(crate) mod api_server;
pub(crate) mod audit;
pub(crate) mod changes;
pub(crate) mod control;
#[cfg(target_os = "linux")]
pub(crate) mod ctl;
pub(crate) mod events;
pub(crate) mod executor;
pub(crate) mod lasting;
#[cfg(target_os = "linux")]
pub(crate) mod lifecycle;
pub(crate) mod poll;
pub(crate) mod preflight;
#[cfg(target_os = "linux")]
pub(crate) mod queue;
pub(crate) mod routines;
pub(crate) mod state;
#[cfg(target_os = "linux")]
pub(crate) mod sys;
pub(crate) mod watch_source;
pub(crate) mod window;

/// The engine `var/state/engine` names: `daemon` or `shell`, one word with an
/// optional trailing newline. Anything else is named, never guessed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn engine_word(bytes: &[u8]) -> Result<&'static str, String> {
    match bytes.strip_suffix(b"\n").unwrap_or(bytes) {
        b"daemon" => Ok("daemon"),
        b"shell" => Ok("shell"),
        other => Err(format!(
            "state/engine holds {:?}, not daemon or shell; the daemon runs",
            String::from_utf8_lossy(&other[..other.len().min(32)])
        )),
    }
}

/// The daemon's flags of `sdsync-dsm-api`: `--daemon`, `--launch-daemon`,
/// `--supervise` and `--ctl`. `None` for any other invocation, which the bridge
/// handles as before.
pub(crate) fn dispatch(arguments: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
    let flag = arguments.first()?.to_str()?;
    if !matches!(
        flag,
        "--daemon" | "--launch-daemon" | "--supervise" | "--ctl"
    ) {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let (options, rest) = match lifecycle::Options::parse(&arguments[1..]) {
            Ok(parsed) => parsed,
            Err(cause) => {
                eprintln!("sdsyncd: {cause}");
                return Some(std::process::ExitCode::from(64));
            }
        };
        if flag != "--ctl" && !rest.is_empty() {
            eprintln!("sdsyncd: unexpected arguments: {}", rest.join(" "));
            return Some(std::process::ExitCode::from(64));
        }
        Some(match flag {
            "--daemon" => lifecycle::run_daemon(&options),
            "--launch-daemon" => lifecycle::run_launch(&options),
            "--supervise" => lifecycle::run_supervise(&options),
            _ => lifecycle::run_ctl(&options, &rest),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("sdsyncd: {flag} is only available on Linux");
        Some(std::process::ExitCode::from(69))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_engine_file_is_one_known_word_or_named() {
        assert_eq!(
            super::engine_word(
                b"shell
"
            ),
            Ok("shell")
        );
        assert_eq!(super::engine_word(b"daemon"), Ok("daemon"));
        for bad in [
            &b""[..],
            b"Shell
",
            b"shell

",
            b"legacy",
        ] {
            let error = super::engine_word(bad).unwrap_err();
            assert!(error.contains("the daemon runs"), "{error}");
        }
    }
}
