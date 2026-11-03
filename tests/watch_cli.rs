//! `sdsync-dsm-api --watch` and `--watch-probe` as the controller and an operator run them: the
//! real binary, real inotify, a marker file opened for appending exactly as the controller's `>>`
//! opens it.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const BINARY: &str = env!("CARGO_BIN_EXE_sdsync-dsm-api");

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "sdsync-watch-cli-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(base.join("source")).unwrap();
        Self(base)
    }

    fn source(&self) -> PathBuf {
        self.0.join("source")
    }

    fn marker(&self) -> PathBuf {
        self.0.join("watch.events")
    }

    fn errors(&self) -> PathBuf {
        self.0.join("watch.stderr")
    }

    fn fragment(&self, excludes: &str) -> PathBuf {
        let fragment = self.0.join("profile.toml");
        fs::write(
            &fragment,
            format!(
                "source = \"{}\"\nexcludes = {excludes}\n",
                self.source().display()
            ),
        )
        .unwrap();
        fragment
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Watcher(Child);

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The controller's spawn, argument for argument.
fn spawn(scratch: &Scratch, fragment: &Path, extra_env: &[(&str, &str)]) -> Watcher {
    let marker = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(scratch.marker())
        .unwrap();
    let errors = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(scratch.errors())
        .unwrap();
    let mut command = Command::new(BINARY);
    command
        .arg("--watch")
        .arg("--profile-fragment")
        .arg(fragment)
        .args([
            "-m",
            "-q",
            "-r",
            "--format",
            "x",
            "-e",
            "close_write,create,delete,move,attrib,modify",
        ])
        .arg(scratch.source())
        .stdin(Stdio::null())
        .stdout(marker)
        .stderr(errors);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    Watcher(command.spawn().unwrap())
}

/// Watches the process holds, read the way the kernel publishes them.
fn watches_held(pid: u32) -> usize {
    let Ok(descriptors) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return 0;
    };
    descriptors
        .flatten()
        .filter(|entry| {
            fs::read_link(entry.path())
                .is_ok_and(|target| target.as_os_str() == "anon_inode:inotify")
        })
        .filter_map(|entry| {
            fs::read_to_string(format!(
                "/proc/{pid}/fdinfo/{}",
                entry.file_name().to_string_lossy()
            ))
            .ok()
        })
        .map(|info| {
            info.lines()
                .filter(|line| line.starts_with("inotify wd:"))
                .count()
        })
        .sum()
}

fn wait_armed(watcher: &Watcher, directories: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while watches_held(watcher.0.id()) < directories {
        assert!(
            Instant::now() < deadline,
            "the watcher never armed {directories} watches"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn marker_lines(scratch: &Scratch) -> Vec<String> {
    fs::read_to_string(scratch.marker())
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn wait_for_line(scratch: &Scratch, wanted: impl Fn(&[String]) -> bool) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let lines = marker_lines(scratch);
        if wanted(&lines) || Instant::now() >= deadline {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_burst_of_changes_is_at_most_one_marker_line_per_100_ms() {
    let scratch = Scratch::new("coalesce");
    fs::create_dir(scratch.source().join("inbox")).unwrap();
    let fragment = scratch.fragment("[]");
    let watcher = spawn(&scratch, &fragment, &[]);
    wait_armed(&watcher, 2);
    let started = Instant::now();
    for index in 0..3000 {
        fs::write(scratch.source().join(format!("inbox/{index}.txt")), b"x").unwrap();
    }
    let burst = started.elapsed();
    std::thread::sleep(Duration::from_millis(400));
    let lines = marker_lines(&scratch);
    let ceiling = (burst + Duration::from_millis(400)).as_millis() as usize / 100 + 2;
    assert!(!lines.is_empty());
    assert!(
        lines.len() <= ceiling,
        "{} lines for a {burst:?} burst",
        lines.len()
    );
    assert!(lines.iter().all(|line| line == "x"), "{lines:?}");
}

#[test]
fn excluded_paths_never_reach_the_marker() {
    let scratch = Scratch::new("excluded");
    fs::create_dir_all(scratch.source().join("photos/@eaDir")).unwrap();
    let fragment = scratch.fragment("[\"*.part\", \"cache/\"]");
    let watcher = spawn(&scratch, &fragment, &[]);
    // source and photos; @eaDir is never watched.
    wait_armed(&watcher, 2);
    std::thread::sleep(Duration::from_millis(150));
    assert!(watches_held(watcher.0.id()) == 2);
    fs::write(
        scratch.source().join("photos/@eaDir/SYNOPHOTO_THUMB_M.jpg"),
        b"x",
    )
    .unwrap();
    fs::write(scratch.source().join("photos/upload.part"), b"x").unwrap();
    fs::create_dir(scratch.source().join("cache")).unwrap();
    fs::write(scratch.source().join("cache/blob"), b"x").unwrap();
    fs::create_dir(scratch.source().join("#recycle")).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        marker_lines(&scratch).is_empty(),
        "{:?}",
        marker_lines(&scratch)
    );
    fs::write(scratch.source().join("photos/kept.jpg"), b"x").unwrap();
    let lines = wait_for_line(&scratch, |lines| !lines.is_empty());
    assert_eq!(lines, ["x"]);
}

#[test]
fn an_overflow_while_stopped_is_written_as_an_overflow_line() {
    let scratch = Scratch::new("overflow");
    let fragment = scratch.fragment("[]");
    let watcher = spawn(&scratch, &fragment, &[]);
    wait_armed(&watcher, 1);
    let queued: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Stopped the way the controller stops a watcher while it consumes the marker: the kernel
    // keeps queueing, and drops what does not fit.
    let pid = Pid::from_raw(watcher.0.id() as i32);
    kill(pid, Signal::SIGSTOP).unwrap();
    for index in 0..queued / 2 + 64 {
        fs::write(scratch.source().join(format!("f{index}")), b"").unwrap();
    }
    kill(pid, Signal::SIGCONT).unwrap();
    let lines = wait_for_line(&scratch, |lines| {
        lines.iter().any(|line| line == "x overflow")
    });
    assert!(lines.iter().any(|line| line == "x overflow"), "{lines:?}");
}

#[test]
fn a_removed_source_writes_a_last_marker_and_exits_by_name() {
    let scratch = Scratch::new("gone");
    let fragment = scratch.fragment("[]");
    let mut watcher = spawn(&scratch, &fragment, &[]);
    wait_armed(&watcher, 1);
    fs::remove_dir(scratch.source()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = watcher.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the watcher outlived its source");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(66));
    assert!(!marker_lines(&scratch).is_empty());
    let errors = fs::read_to_string(scratch.errors()).unwrap();
    assert!(
        errors.starts_with("source_missing: the source was deleted, moved or unmounted"),
        "{errors}"
    );
    assert_eq!(errors.lines().count(), 1, "{errors}");
}

#[test]
fn a_command_line_outside_the_subset_exits_64_with_one_named_line() {
    let output = Command::new(BINARY)
        .args(["--watch", "-m", "-r", "--format", "%w%f", "/tmp"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(errors.starts_with("watch_setup_failed usage: "), "{errors}");
    assert!(output.stdout.is_empty());
    let output = Command::new(BINARY)
        .args(["--watch", "-m", "-r", "/definitely/not/here"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(66));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("source_missing"));
}

#[test]
fn an_unreadable_profile_fragment_is_named_not_ignored() {
    let scratch = Scratch::new("fragment");
    let fragment = scratch.0.join("broken.toml");
    fs::write(&fragment, "excludes = [\n").unwrap();
    let output = Command::new(BINARY)
        .arg("--watch")
        .arg("--profile-fragment")
        .arg(&fragment)
        .args(["-m", "-r"])
        .arg(scratch.source())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(78));
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(
        errors.starts_with("watch_setup_failed rules: profile fragment "),
        "{errors}"
    );
}

#[test]
fn a_watcher_whose_controller_dies_stops_with_it() {
    let scratch = Scratch::new("orphan");
    let pid_file = scratch.0.join("watcher.pid");
    // A shell stands in for the controller: it starts the watcher in the background, records its
    // pid, and is then killed outright, the way a crashed controller would be.
    let mut controller = Command::new("sh")
        .arg("-c")
        .arg("\"$0\" --watch -m -q -r --format x \"$1\" >/dev/null 2>&1 & echo $! > \"$2\"; exec sleep 600")
        .arg(BINARY)
        .arg(scratch.source())
        .arg(&pid_file)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let watcher: u32 = loop {
        if let Some(pid) = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    while watches_held(watcher) < 1 {
        assert!(Instant::now() < deadline, "the watcher never armed");
        std::thread::sleep(Duration::from_millis(20));
    }
    controller.kill().unwrap();
    controller.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        // Gone, or a zombie waiting for whatever adopted it to reap it: either way not running.
        let state = fs::read_to_string(format!("/proc/{watcher}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(") ").map(|(_, rest)| rest.chars().next()))
            .flatten();
        if matches!(state, None | Some('Z') | Some('X')) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the watcher kept running after its controller died (state {state:?})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn probe(scratch: &Scratch, seconds: &str, change: bool) -> (i32, String) {
    let mut child = Command::new(BINARY)
        .args(["--watch-probe", "--seconds", seconds])
        .arg(scratch.source())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut report = String::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if change && line.starts_with("waiting ") {
            fs::write(scratch.source().join("docs/probe.txt"), b"x").unwrap();
        }
        report.push_str(&line);
        report.push('\n');
    }
    let status = child.wait().unwrap();
    (status.code().unwrap(), report)
}

#[test]
fn the_probe_reports_the_size_against_the_limit_and_what_arrives() {
    let scratch = Scratch::new("probe");
    fs::create_dir_all(scratch.source().join("docs/a")).unwrap();
    // Another watcher of this user holds three watches; the probe's budget must count them.
    let fragment = scratch.fragment("[]");
    let other = spawn(&scratch, &fragment, &[]);
    wait_armed(&other, 3);
    let (code, report) = probe(&scratch, "2", true);
    assert_eq!(code, 0, "{report}");
    assert!(report.contains("armed: 3 directories watched"), "{report}");
    let held = report
        .lines()
        .find_map(|line| line.split("held_by_this_user=").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("no held count in {report}"));
    assert!(held >= 3, "{report}");
    assert!(report.contains("docs/probe.txt"), "{report}");
    assert!(
        report.contains("verdict: inotify reports changes"),
        "{report}"
    );
    drop(other);

    let (code, report) = probe(&scratch, "1", false);
    assert_eq!(code, 1, "{report}");
    assert!(
        report.contains("verdict: armed, but no change arrived in 1 s"),
        "{report}"
    );
}

#[test]
fn the_probe_names_a_missing_source() {
    let output = Command::new(BINARY)
        .args(["--watch-probe", "--seconds", "1", "/definitely/not/here"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(66));
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("refused: source_missing"), "{report}");
}

#[cfg(feature = "fault-inject")]
#[test]
fn a_setup_walk_blocked_on_a_directory_exits_named_within_its_deadline() {
    let scratch = Scratch::new("hang");
    fs::create_dir_all(scratch.source().join("fine")).unwrap();
    fs::create_dir_all(scratch.source().join("stuck/slow/below")).unwrap();
    let fragment = scratch.fragment("[]");
    let started = Instant::now();
    let output = Command::new(BINARY)
        .arg("--watch")
        .arg("--profile-fragment")
        .arg(&fragment)
        .args(["--setup-deadline", "1", "-m", "-q", "-r", "--format", "x"])
        .arg(scratch.source())
        .env("SDSYNC_FAULT_BLOCK", "read_dir:*/slow")
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "deadline 1 s, plus spawn slack: {elapsed:?}"
    );
    assert_eq!(output.status.code(), Some(72));
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(
        errors.starts_with("watch_setup_failed deadline=1s: the directory walk did not finish"),
        "{errors}"
    );
    assert!(errors.trim_end().ends_with("/stuck/slow"), "{errors}");
}

#[test]
fn a_lowered_kernel_limit_is_a_named_watch_limit_exit() {
    if std::env::var_os("SDSYNC_TEST_LOWERED_WATCH_LIMIT").is_none() {
        eprintln!("skipped: needs SDSYNC_TEST_LOWERED_WATCH_LIMIT and a lowered sysctl");
        return;
    }
    let read = |path: &str| -> Option<u64> { fs::read_to_string(path).ok()?.trim().parse().ok() };
    let per_user = read("/proc/sys/fs/inotify/max_user_watches").unwrap();
    let limit = read("/proc/sys/user/max_inotify_watches")
        .map_or(per_user, |namespace| per_user.min(namespace));
    assert!(limit <= 4096, "the inotify watch limit is {limit}");
    let scratch = Scratch::new("limit");
    for index in 0..limit {
        fs::create_dir(scratch.source().join(format!("d{index}"))).unwrap();
    }
    let output = Command::new(BINARY)
        .args(["--watch", "-m", "-q", "-r", "--format", "x"])
        .arg(scratch.source())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(73));
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(errors.starts_with("watch_limit dirs="), "{errors}");
    assert!(errors.contains(&format!("limit={limit}")), "{errors}");
    assert!(errors.contains("max_user_watches"), "{errors}");
}
