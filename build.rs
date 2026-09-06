use std::env;
use std::process::Command;

/// Longest accepted value for any build-stamped identifier.
const MAX_STAMP_BYTES: usize = 64;

fn main() {
    println!("cargo:rerun-if-env-changed=SDSYNC_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=SDSYNC_BUILD_GIT_SHA");

    let version = env::var("SDSYNC_BUILD_VERSION")
        .or_else(|_| env::var("CARGO_PKG_VERSION"))
        .expect("Cargo always provides CARGO_PKG_VERSION");

    assert!(
        release_safe(&version),
        "SDSYNC_BUILD_VERSION must be a short release-safe version such as 26.1"
    );

    println!("cargo:rustc-env=SDSYNC_VERSION={version}");

    // These reach a user-visible log line and the diagnostic report, so they pass the same
    // release-safe gate the version does rather than being trusted from the environment.
    for (name, value) in [
        ("SDSYNC_BUILD_TARGET", build_target()),
        ("SDSYNC_BUILD_PROFILE", build_profile()),
        ("SDSYNC_BUILD_COMMIT", build_commit()),
    ] {
        assert!(
            release_safe(&value),
            "{name} must be a short release-safe token"
        );
        println!("cargo:rustc-env={name}={value}");
    }
}

/// Accept only short, ASCII, punctuation-bounded identifiers.
fn release_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_STAMP_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

fn build_target() -> String {
    env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned())
}

fn build_profile() -> String {
    env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned())
}

/// Resolve the commit this binary was built from, preferring an explicitly supplied value.
///
/// A published crate, a vendored source tree, and a source archive all lack a repository, so a
/// missing or unusable commit degrades to `unknown` and must never fail the build. Release
/// pipelines that want a reproducible stamp set `SDSYNC_BUILD_GIT_SHA` instead of relying on the
/// repository being present.
fn build_commit() -> String {
    if let Ok(supplied) = env::var("SDSYNC_BUILD_GIT_SHA")
        && release_safe(&supplied)
    {
        return supplied;
    }
    Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_owned())
        .filter(|sha| release_safe(sha))
        .unwrap_or_else(|| "unknown".to_owned())
}
