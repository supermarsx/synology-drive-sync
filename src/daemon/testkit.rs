//! Helpers the daemon's unit tests share.

use std::fs;
use std::io::Write;
use std::path::Path;

/// Whether permission bits fail to bind this user, for a test that has just
/// `chmod 000`ed `path` (a file or a directory) and needs reads to be refused.
///
/// Root reads through any mode, so such a test proves nothing there. It must not
/// pass in silence: when the bits do not bind, this writes a `SKIPPED` line on
/// the process's real stderr (a plain `eprintln!` is captured by the harness and
/// shown only for failures) and returns `true`, and the caller returns. Set
/// `SDSYNC_REQUIRE_PERMISSION_TESTS` to turn the skip into a failure, for a gate
/// that must have run them.
pub(crate) fn permissions_do_not_bind(test: &str, path: &Path) -> bool {
    let meta =
        fs::metadata(path).unwrap_or_else(|error| panic!("{test}: {}: {error}", path.display()));
    let readable = if meta.is_dir() {
        fs::read_dir(path).is_ok()
    } else {
        fs::read(path).is_ok()
    };
    if !readable {
        return false;
    }
    skip_loudly(
        test,
        &format!(
            "SKIPPED {test}: {} is still readable at mode 000, so this user ignores permission \
             bits (root); the test proves nothing here and did not run its assertions. Run the \
             suite as an unprivileged user.",
            path.display()
        ),
    )
}

/// [`permissions_do_not_bind`] for a test that has just made directory `dir` read-only (mode
/// 0500) and needs creating a file in it to be refused: `true` (after the same loud notice) when
/// a file can be created there anyway.
pub(crate) fn write_permissions_do_not_bind(test: &str, dir: &Path) -> bool {
    let probe = dir.join(".sdsyncd-permission-probe");
    match fs::File::create_new(&probe) {
        Ok(_) => {
            fs::remove_file(&probe).unwrap_or_else(|error| {
                panic!(
                    "{test}: cannot remove its probe {}: {error}",
                    probe.display()
                )
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return false,
        Err(error) => panic!(
            "{test}: probing {} for write permission failed for another reason: {error}",
            dir.display()
        ),
    }
    skip_loudly(
        test,
        &format!(
            "SKIPPED {test}: a file can be created in {} at mode 0500, so this user ignores \
             permission bits (root); the test proves nothing here and did not run its assertions. \
             Run the suite as an unprivileged user.",
            dir.display()
        ),
    )
}

/// The one way a permission test skips: a `SKIPPED` line on the process's real stderr, or a
/// failure when `SDSYNC_REQUIRE_PERMISSION_TESTS` is set. Always `true`.
fn skip_loudly(test: &str, notice: &str) -> bool {
    assert!(
        std::env::var_os("SDSYNC_REQUIRE_PERMISSION_TESTS").is_none(),
        "{notice} (SDSYNC_REQUIRE_PERMISSION_TESTS is set)"
    );
    let mut stderr = std::io::stderr();
    writeln!(stderr, "{notice}")
        .and_then(|()| stderr.flush())
        .unwrap_or_else(|error| panic!("{test}: cannot write the skip notice: {error}"));
    true
}

/// Return once no process holds `path` open for writing, so that executing it cannot fail with
/// "Text file busy" (`ETXTBSY`).
///
/// A test writes a script and the code under test runs it. `fs::write` closes its descriptor, but
/// a child that another test thread forked between the `open` and the `close` holds a copy until
/// that child execs (close-on-exec acts then), and a program with a writer anywhere cannot be
/// executed. On an instrumented runner the child's gap is long enough to be met, and the failure
/// looked like the code under test failing to start its server. This waits for the copy to go,
/// bounded, and names the holders when it never does. Linux only (it reads `/proc`); elsewhere
/// it returns at once.
pub(crate) fn settle_for_exec(path: &Path) {
    #[cfg(target_os = "linux")]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let holders = writers_of(path);
            if holders.is_empty() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{} is still open for writing by {holders:?} after 10 s",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
}

/// The `pid/fd` of every descriptor of this user's processes that has `path` open for writing.
#[cfg(target_os = "linux")]
fn writers_of(path: &Path) -> Vec<String> {
    let Ok(target) = fs::canonicalize(path) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return found;
    };
    for process in processes.flatten() {
        let name = process.file_name().to_string_lossy().into_owned();
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            if fs::read_link(descriptor.path()).ok().as_deref() != Some(target.as_path()) {
                continue;
            }
            let info = process.path().join("fdinfo").join(descriptor.file_name());
            let writable = fs::read_to_string(info).ok().is_some_and(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("flags:"))
                    .and_then(|flags| i64::from_str_radix(flags.trim(), 8).ok())
                    // O_ACCMODE: 0 read-only, 1 write-only, 2 read-write.
                    .is_some_and(|flags| flags & 3 != 0)
            });
            if writable {
                found.push(format!(
                    "{name}/{}",
                    descriptor.file_name().to_string_lossy()
                ));
            }
        }
    }
    found
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A permission test that skips for root says so loudly, through this module, or
    /// fails when `SDSYNC_REQUIRE_PERMISSION_TESTS` is set. A private copy of the skip
    /// (an `eprintln!` the harness captures, a `return` with no word) passes in
    /// silence on a root runner, so none may exist outside this file.
    #[test]
    fn no_test_skips_for_root_in_silence() {
        fn sources(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    found.push(path);
                }
            }
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&src, &mut files);
        let marker = ["running as", "root"].join(" ");
        let private: Vec<String> = files
            .iter()
            .filter(|path| !path.ends_with("daemon/testkit.rs"))
            .filter(|path| {
                fs::read_to_string(path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
                    .contains(&marker)
            })
            .map(|path| path.display().to_string())
            .collect();
        assert!(
            private.is_empty(),
            "these files skip for root on their own instead of through testkit: {private:#?}"
        );
    }

    /// The write-side probe: a directory made read-only refuses it (unless this user is
    /// root, which says so), and the probe leaves nothing behind either way.
    #[test]
    fn a_read_only_directory_refuses_the_write_probe() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sdsyncd-writeprobe-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let skipped =
            write_permissions_do_not_bind("a_read_only_directory_refuses_the_write_probe", &dir);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        fs::remove_dir_all(&dir).unwrap();
        assert!(
            left.is_empty(),
            "the probe left {left:?} behind (the user ignores permission bits: {skipped})"
        );
    }

    #[test]
    fn settling_waits_for_a_write_descriptor_to_close() {
        let path = std::env::temp_dir().join(format!("sdsyncd-settle-{}", std::process::id()));
        fs::write(
            &path,
            b"#!/bin/sh
",
        )
        .unwrap();
        let writer = fs::OpenOptions::new().write(true).open(&path).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(writer);
        });
        let began = Instant::now();
        settle_for_exec(&path);
        let waited = began.elapsed();
        releaser.join().unwrap();
        assert!(
            waited >= Duration::from_millis(250),
            "returned while a writer was open: {waited:?}"
        );
        // Nothing open: immediate.
        let began = Instant::now();
        settle_for_exec(&path);
        assert!(began.elapsed() < Duration::from_millis(200));
        let _ = fs::remove_file(&path);
    }
}
