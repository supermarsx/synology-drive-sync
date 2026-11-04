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
    let notice = format!(
        "SKIPPED {test}: {} is still readable at mode 000, so this user ignores permission bits \
         (root); the test proves nothing here and did not run its assertions. Run the suite as an \
         unprivileged user.",
        path.display()
    );
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
