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

/// The review-int probe, as a regression test: a store through a shared mapping into a page that
/// is already dirty changes the content with size, mtime, ctime and inode unchanged. Proven 2.5 s
/// after the change time, the old two-second window stored the pair as settled and the next run
/// served the stale digest: the change was never uploaded.
#[test]
fn a_store_into_a_dirty_mapped_page_is_never_served_stale() {
    let fixture = fixture("mmap");
    let mapped = fixture.source.join("db.bin");
    fs::write(&mapped, vec![b'A'; 4096]).unwrap();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&mapped)
        .unwrap();
    // SAFETY: a fresh 4096-byte shared mapping of a 4096-byte file this test owns, unmapped below.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED, "mmap");
    let base = base.cast::<u8>();
    // The first store faults the clean page writable: mtime and ctime move, now.
    // SAFETY: offset 0 is inside the mapping.
    unsafe { base.write_volatile(b'B') };
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
    // SAFETY: offset 1 is inside the mapping.
    unsafe { base.add(1).write_volatile(b'C') };
    let after = facts(&mapped);
    let changed = fs::read(&mapped).unwrap();
    assert_ne!(changed, first, "the content changed");
    eprintln!(
        "facts before={before:?} after={after:?} (equal: {})",
        before == after
    );

    let next = fixture.sync();
    assert_eq!(
        fixture.remote("db.bin"),
        changed,
        "the change must reach the NAS: {next}"
    );
    assert_eq!(Fixture::uploaded(&next), 1, "{next}");
    assert!(Fixture::index(&next)["racy"].as_u64() >= Some(1), "{next}");
    // SAFETY: the mapping made above, not used after this.
    unsafe { libc::munmap(base.cast(), 4096) };
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
