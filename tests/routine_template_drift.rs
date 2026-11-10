//! The unit harness (`src/daemon/control_tests.rs`) runs on every CI platform, so
//! it cannot run the shell manager. It builds `routines.d/<name>.conf` from a
//! template (`src/daemon/testdata/manager-routine.conf`) holding the keys, order
//! and defaults the manager's `configure-routine` writes, and lets a test override
//! the values it cares about (and only keys the manager writes). A conf written
//! by hand with three or four keys, as the harness used to, is what a test author
//! imagines the manager writes; the real one holds fifteen.
//!
//! This test is what keeps the template honest. It runs the shipped manager
//! (`packaging/synology/package/bin/sdsync-dsm`, in the DSM-shaped fixture of
//! `tests/support/dsm_package.rs`) and compares:
//!
//! - the default render with the template, byte for byte;
//! - the key list, in order, of a render in each mode (interval, daily, realtime)
//!   and with the options that change what is written, so a key the manager adds
//!   or renames in one mode only cannot go unseen.
//!
//! Set `SDSYNC_REGEN_MANAGER_TEMPLATE=1` to rewrite the template after a deliberate
//! manager change, and review the diff.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "support/dsm_package.rs"]
mod dsm_package;
use dsm_package::DsmPackage;

const CORE: &str = env!("CARGO_BIN_EXE_synology-drive-sync");
const TEMPLATE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/daemon/testdata/manager-routine.conf"
);

static NEXT: AtomicU64 = AtomicU64::new(0);

fn package(label: &str) -> DsmPackage {
    let root = std::env::temp_dir().join(format!(
        "sdsync-routine-template-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let package = DsmPackage::build(&root, Path::new(CORE));
    for name in ["alpha", "beta"] {
        package.configure_profile(name, &[]);
    }
    package
}

/// What `sdsync-dsm configure-routine --profile alpha <options>` wrote. Panics,
/// naming stdout, stderr and the options, when the manager refuses.
fn render(package: &DsmPackage, options: &[&str]) -> String {
    let mut command: Command = package.manager();
    command
        .arg("configure-routine")
        .args(["--profile", "alpha"])
        .args(options);
    let output = dsm_package::output_retrying(&mut command)
        .unwrap_or_else(|error| panic!("run the manager: {error}"));
    assert!(
        output.status.success(),
        "the manager refused configure-routine {options:?}: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let conf: PathBuf = package.p("home/config/routines.d/alpha.conf");
    fs::read_to_string(&conf).unwrap_or_else(|error| panic!("read {}: {error}", conf.display()))
}

fn keys(conf: &str) -> Vec<&str> {
    conf.lines()
        .map(|line| line.split_once('=').map_or(line, |(key, _)| key))
        .collect()
}

#[test]
fn the_unit_harness_routine_template_is_what_the_real_manager_renders() {
    let package = package("template");
    let rendered = render(&package, &[]);
    if std::env::var_os("SDSYNC_REGEN_MANAGER_TEMPLATE").is_some() {
        fs::write(TEMPLATE, &rendered).unwrap_or_else(|error| panic!("write {TEMPLATE}: {error}"));
    }
    let template = fs::read_to_string(TEMPLATE)
        .unwrap_or_else(|error| panic!("read {TEMPLATE}: {error}"))
        .replace("\r\n", "\n");
    assert_eq!(
        rendered, template,
        "the manager renders a routine differently from the unit-harness template; \
         re-run with SDSYNC_REGEN_MANAGER_TEMPLATE=1 and review the diff"
    );
    assert_eq!(
        keys(&template).len(),
        15,
        "the manager writes fifteen keys: {template}"
    );
}

#[test]
fn every_mode_and_option_writes_the_templates_keys_in_its_order() {
    let package = package("modes");
    let template = fs::read_to_string(TEMPLATE)
        .unwrap_or_else(|error| panic!("read {TEMPLATE}: {error}"))
        .replace("\r\n", "\n");
    let expected = keys(&template);
    for (label, options) in [
        ("interval", vec!["--mode", "interval", "--interval", "60"]),
        (
            "daily",
            vec![
                "--mode",
                "daily",
                "--weekdays",
                "1,3,5",
                "--window-start",
                "02:15",
                "--window-end",
                "23:45",
            ],
        ),
        (
            "realtime",
            vec![
                "--mode",
                "realtime",
                "--debounce-seconds",
                "1",
                "--poll-seconds",
                "5",
            ],
        ),
        (
            "everything else",
            vec![
                "--enabled",
                "false",
                "--action",
                "plan",
                "--retry-attempts",
                "0",
                "--retry-backoff",
                "10",
                "--retry-exponential",
                "false",
                "--depends-on",
                "beta",
            ],
        ),
    ] {
        let rendered = render(&package, &options);
        assert_eq!(
            keys(&rendered),
            expected,
            "{label}: the manager's keys or their order differ from the template:\n{rendered}"
        );
    }
}
