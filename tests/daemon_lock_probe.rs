//! `sdsync-dsm-api --daemon-lock-probe <path>` (N-S5): the shell's `flock -n`
//! probe of `daemon.lock` for a NAS without flock(1).
//!
//! Contract (fix6): open the path read-only without creating it and without
//! following a symlink, try `flock(LOCK_EX|LOCK_NB)`, release at once. Exit 0
//! when the lock is free, 3 when it is held, 1 when it cannot be probed (one
//! stderr line `daemon-lock-probe: <path>: <cause>`). Nothing on stdout; no
//! config, password or network.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BRIDGE: &str = env!("CARGO_BIN_EXE_sdsync-dsm-api");

/// A DSM-shaped package root: `var` is a symlink to `@appdata`, as
/// `/var/packages/<pkg>/var` is on the NAS, with a private `run/`.
fn package_var(label: &str) -> (PathBuf, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("sdsync-lock-probe-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let appdata = root.join("@appdata");
    fs::create_dir_all(appdata.join("run")).unwrap();
    for dir in [&root, &appdata, &appdata.join("run")] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::os::unix::fs::symlink(&appdata, root.join("var")).unwrap();
    (root.clone(), root.join("var"))
}

fn probe(path: &Path) -> (Output, Duration) {
    let began = Instant::now();
    // As the shell calls it: an empty environment but PATH.
    let output = Command::new(BRIDGE)
        .arg("--daemon-lock-probe")
        .arg(path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (output, began.elapsed())
}

fn shown(output: &Output) -> String {
    format!(
        "status={:?} stdout={:?} stderr={:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn private_file(path: &Path) {
    fs::write(path, b"").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn flock(file: &fs::File, operation: libc::c_int) -> bool {
    // SAFETY: flock on a live descriptor this test owns.
    unsafe { libc::flock(file.as_raw_fd(), operation) == 0 }
}

#[test]
fn a_free_lock_is_0_and_left_free() {
    let (root, var) = package_var("free");
    let lock = var.join("run/daemon.lock");
    private_file(&lock);
    for _ in 0..2 {
        let (output, took) = probe(&lock);
        eprintln!("{} in {took:?}", shown(&output));
        assert_eq!(output.status.code(), Some(0), "{}", shown(&output));
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{}",
            shown(&output)
        );
        assert!(took < Duration::from_secs(5));
    }
    // The probe released what it took.
    let file = fs::File::open(&lock).unwrap();
    assert!(
        flock(&file, libc::LOCK_EX | libc::LOCK_NB),
        "the probe left the lock held"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_held_lock_is_3_and_never_waited_for() {
    let (root, var) = package_var("held");
    let lock = var.join("run/daemon.lock");
    private_file(&lock);
    // Held as the daemon holds it: flock on its own open file description.
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    assert!(flock(&holder, libc::LOCK_EX | libc::LOCK_NB));
    let (output, took) = probe(&lock);
    eprintln!("{} in {took:?}", shown(&output));
    assert_eq!(output.status.code(), Some(3), "{}", shown(&output));
    assert!(output.stdout.is_empty(), "{}", shown(&output));
    assert!(took < Duration::from_secs(5), "never waits: {took:?}");
    drop(holder);
    let (output, _) = probe(&lock);
    assert_eq!(
        output.status.code(),
        Some(0),
        "free once released: {}",
        shown(&output)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn what_cannot_be_probed_is_1_and_named() {
    let (root, var) = package_var("unprobeable");
    let run = var.join("run");
    let target = run.join("real.lock");
    private_file(&target);
    std::os::unix::fs::symlink(&target, run.join("link.lock")).unwrap();
    fs::create_dir(run.join("dir.lock")).unwrap();
    let fifo = run.join("fifo.lock");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: mkfifo with a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    for (path, cause) in [
        (run.join("missing.lock"), "No such file"),
        (run.join("link.lock"), "symbolic link"),
        (run.join("dir.lock"), "not a regular file"),
        (fifo.clone(), "not a regular file"),
    ] {
        let (output, took) = probe(&path);
        eprintln!("{}: {} in {took:?}", path.display(), shown(&output));
        assert_eq!(output.status.code(), Some(1), "{}", shown(&output));
        assert!(output.stdout.is_empty(), "{}", shown(&output));
        assert!(
            took < Duration::from_secs(5),
            "a FIFO never blocks: {took:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let expected = format!("daemon-lock-probe: {}: ", path.display());
        assert!(
            stderr.starts_with(&expected) && stderr.contains(cause) && stderr.lines().count() == 1,
            "{}",
            shown(&output)
        );
    }
    assert!(!run.join("missing.lock").exists(), "never created");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_usage_error_is_1_and_named() {
    let output = Command::new(BRIDGE)
        .arg("--daemon-lock-probe")
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    eprintln!("{}", shown(&output));
    assert_eq!(output.status.code(), Some(1), "{}", shown(&output));
    assert!(output.stdout.is_empty(), "{}", shown(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("daemon-lock-probe: "),
        "{}",
        shown(&output)
    );
}

/// S3-7: the shell reads the probe's stderr as one line, so a path with a newline or another
/// control byte in it (a name an attacker or a typo can plant) must not break that line or put
/// a terminal escape in front of an operator: every control byte is written as an escape.
#[test]
fn a_path_with_control_bytes_is_named_on_one_escaped_line() {
    use std::os::unix::ffi::OsStrExt;
    let (root, var) = package_var("control-bytes");
    let run = var.join("run");
    let mut name = b"odd".to_vec();
    name.extend_from_slice(b"\nforged: line\x1b[31m\x07\r\xff.lock");
    let path = run.join(std::ffi::OsStr::from_bytes(&name));
    let (output, _) = probe(&path);
    eprintln!("{}", shown(&output));
    assert_eq!(output.status.code(), Some(1), "{}", shown(&output));
    assert!(output.stdout.is_empty(), "{}", shown(&output));
    let stderr = output.stderr.clone();
    let body = stderr
        .strip_suffix(b"\n")
        .unwrap_or_else(|| panic!("one newline-terminated line: {}", shown(&output)));
    assert!(
        body.iter().all(|byte| *byte >= 0x20 && *byte != 0x7f),
        "a raw control byte reached stderr: {}",
        shown(&output)
    );
    let text = String::from_utf8_lossy(body);
    assert!(
        text.starts_with("daemon-lock-probe: ") && text.contains("odd\\nforged: line\\u{1b}[31m"),
        "the escaped path is named: {}",
        shown(&output)
    );
    assert!(
        text.contains("No such file"),
        "the cause is named: {}",
        shown(&output)
    );
    let _ = fs::remove_dir_all(root);
}
