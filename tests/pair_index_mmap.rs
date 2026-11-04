//! D1: a write the file's facts do not show must never be served from the verified-pair index.
//!
//! Linux moves a file's mtime and ctime when a store through a shared mapping first dirties a
//! clean page (`page_mkwrite`), not on later stores into the page while it stays dirty, and the
//! page stays dirty until writeback cleans it (`dirty_expire_centisecs` plus
//! `dirty_writeback_centisecs`, about 35 s by default). A record proven within that window of the
//! last change time may hold content that later changed with every fact unchanged, so it is
//! stored racy and verified live next time. These run the real CLI, with the real window: no
//! test override is set here.
#![cfg(target_os = "linux")]

#[allow(dead_code)]
mod support;

use std::fs;
use std::io::{Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const ROOT: &str = "/team/mapped";

fn run(arguments: &[String]) -> Output {
    let environment = TestDir::new("mmap-environment");
    for directory in ["home", "xdg"] {
        fs::create_dir_all(environment.child(directory)).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
    command
        .args(arguments)
        .env("HOME", environment.child("home"))
        .env("XDG_CONFIG_HOME", environment.child("xdg"));
    // The real window: no inherited override, no inherited profile settings.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SDSYNC_") {
            command.env_remove(name);
        }
    }
    command.output().expect("run synology-drive-sync")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "one JSON document ({error}); stderr: {}; stdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn facts(path: &Path) -> (u64, i64, i64, i64, i64, u64) {
    let metadata = fs::metadata(path).unwrap();
    (
        metadata.size(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.ino(),
    )
}

struct Fixture {
    _dir: TestDir,
    source: PathBuf,
    index: PathBuf,
    password: PathBuf,
    server: MockFileStation,
}

/// Thirty settled pairs, so the index has something to serve besides the file under test.
fn fixture(label: &str) -> Fixture {
    let dir = TestDir::new(label);
    let source = dir.child("source");
    fs::create_dir_all(&source).unwrap();
    let server = MockFileStation::start();
    server.add_directory(ROOT);
    for index in 0..30 {
        let body = format!("{index:040}").into_bytes();
        fs::write(source.join(format!("f{index:02}.bin")), &body).unwrap();
        server.add_file(&format!("{ROOT}/f{index:02}.bin"), &body, 1_700_000_000);
    }
    let password = dir.write("password", PASSWORD);
    let index = dir.child("index");
    Fixture {
        _dir: dir,
        source,
        index,
        password,
        server,
    }
}

impl Fixture {
    fn sync(&self) -> Value {
        let arguments: Vec<String> = [
            "--quiet",
            "--output",
            "json",
            "sync",
            self.source.to_str().unwrap(),
            ROOT,
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            self.password.to_str().unwrap(),
            "--no-vault",
            "--allow-http",
            "--jobs",
            "1",
            "--compare",
            "content",
            "--pair-index",
            self.index.to_str().unwrap(),
            "--pair-index-canary",
            "0",
        ]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect();
        let output = run(&arguments);
        assert_eq!(
            output.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        json(&output)
    }

    fn uploaded(document: &Value) -> u64 {
        // A run with nothing to do reports `{"changed": false}` and no counts.
        document["result"]["uploaded"].as_u64().unwrap_or(0)
    }

    fn index(document: &Value) -> &Value {
        if document["result"]["pair_index"].is_object() {
            &document["result"]["pair_index"]
        } else {
            &document["plan"]["pair_index"]
        }
    }

    fn remote(&self, name: &str) -> Vec<u8> {
        self.server
            .file_contents(&format!("{ROOT}/{name}"))
            .expect("the NAS copy exists")
    }
}

/// How many fresh files the mapped-write test tries before it gives up on provoking the case.
const PROVOKE_ATTEMPTS: usize = 10;

/// One attempt at the case: a store into an already-dirty mapped page that leaves every fact the
/// index keys on unchanged.
enum Attempt {
    /// The facts did not move across the second store, the content did, and the next run was
    /// checked: this is the case, and it passed.
    Provoked,
    /// The case did not arise (writeback cleaned the page first, or the filesystem stamps every
    /// store): nothing was proven either way, and the reason is named.
    NotProvoked(String),
}

/// A shared mapping of one file, unmapped when dropped.
struct Mapping {
    base: *mut u8,
    length: usize,
    _file: fs::File,
}

impl Mapping {
    fn new(path: &Path, length: usize) -> Self {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        // SAFETY: a fresh shared mapping of a file this test owns and sized to `length`; it is
        // unmapped in `Drop` and not used after.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED, "mmap");
        Self {
            base: base.cast(),
            length,
            _file: file,
        }
    }

    fn store(&self, offset: usize, byte: u8) {
        assert!(offset < self.length);
        // SAFETY: `offset` is inside the mapping, checked above.
        unsafe { self.base.add(offset).write_volatile(byte) };
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, not used after this.
        unsafe { libc::munmap(self.base.cast(), self.length) };
    }
}

fn mapped_write_attempt(attempt: usize) -> Attempt {
    let fixture = fixture(&format!("mmap-{attempt}"));
    let mapped = fixture.source.join("db.bin");
    fs::write(&mapped, vec![b'A'; 4096]).unwrap();
    // Without this the page `write(2)` just dirtied is still dirty, the first store through the
    // mapping meets a dirty page and moves no time, and the case under test cannot arise: the
    // first store must be the one that faults a clean page writable.
    fs::File::open(&mapped).unwrap().sync_all().unwrap();
    let mapping = Mapping::new(&mapped, 4096);
    // The first store faults the clean page writable: mtime and ctime move, now.
    mapping.store(0, b'B');
    let first = fs::read(&mapped).unwrap();
    fixture
        .server
        .add_file(&format!("{ROOT}/db.bin"), &first, 1_700_000_000);
    // Past the old two-second window, well inside the writeback window.
    std::thread::sleep(Duration::from_millis(2_500));

    let proving = fixture.sync();
    assert_eq!(Fixture::uploaded(&proving), 0, "{proving}");
    assert_eq!(Fixture::index(&proving)["stored"], 31, "{proving}");

    let before = facts(&mapped);
    // A second store into the still-dirty page: no fault, so no time update.
    mapping.store(1, b'C');
    let after = facts(&mapped);
    let changed = fs::read(&mapped).unwrap();
    assert_ne!(changed, first, "the content changed");
    if before != after {
        return Attempt::NotProvoked(format!(
            "the second store moved a fact the index keys on (before {before:?}, after {after:?}): \
             the page was written back in between, or this filesystem stamps every mapped store"
        ));
    }

    let next = fixture.sync();
    assert_eq!(
        fixture.remote("db.bin"),
        changed,
        "every fact was unchanged ({before:?}) and the content was not: the change must reach \
         the NAS: {next}"
    );
    assert_eq!(Fixture::uploaded(&next), 1, "{next}");
    assert!(Fixture::index(&next)["racy"].as_u64() >= Some(1), "{next}");
    Attempt::Provoked
}

/// The review-int probe, as a regression test: a store through a shared mapping into a page that
/// is already dirty changes the content with size, mtime, ctime and inode unchanged. Proven 2.5 s
/// after the change time, the old two-second window stored the pair as settled and the next run
/// served the stale digest: the change was never uploaded.
///
/// The case only arises when the second store meets a dirty page and the first met a clean one,
/// so each attempt starts from a synced file and checks that no fact moved across the second
/// store. An attempt where one did proves nothing and is retried on fresh files; if no attempt
/// arises, the test fails rather than pass without having tested the rule.
#[test]
fn a_store_into_a_dirty_mapped_page_is_never_served_stale() {
    let mut not_provoked = Vec::new();
    for attempt in 1..=PROVOKE_ATTEMPTS {
        match mapped_write_attempt(attempt) {
            Attempt::Provoked => {
                eprintln!("PROVOKED on attempt {attempt} of {PROVOKE_ATTEMPTS}");
                return;
            }
            Attempt::NotProvoked(reason) => {
                eprintln!("attempt {attempt}: not provoked: {reason}");
                not_provoked.push(reason);
            }
        }
    }
    panic!(
        "the mapped-write case could not be provoked in {PROVOKE_ATTEMPTS} attempts, so the racy \
         rule was not tested here (it is not passing): {not_provoked:#?}"
    );
}

/// A normal `write(2)` again within the window, size and mtime kept: whatever the change time
/// shows, a pair proven inside the window is never served, so the rewrite is uploaded.
#[test]
fn a_file_written_again_within_the_window_is_never_served() {
    let fixture = fixture("rewrite");
    let path = fixture.source.join("notes.txt");
    fs::write(&path, b"first version!").unwrap();
    fixture.server.add_file(
        &format!("{ROOT}/notes.txt"),
        b"first version!",
        1_700_000_000,
    );
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(2_500));

    let proving = fixture.sync();
    assert_eq!(Fixture::uploaded(&proving), 0, "{proving}");
    assert_eq!(Fixture::index(&proving)["stored"], 31, "{proving}");

    // The same size, written in place, with the modification time put back.
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b"second version").unwrap();
    file.set_modified(mtime).unwrap();
    drop(file);

    let next = fixture.sync();
    assert_eq!(fixture.remote("notes.txt"), b"second version", "{next}");
    assert_eq!(Fixture::uploaded(&next), 1, "{next}");
}
