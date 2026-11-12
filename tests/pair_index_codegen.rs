//! The verified-pair index as the release profile compiles it.
//!
//! rustc 1.90 miscompiles redb 4.4 for armv7 at `opt-level = 3` in one codegen unit, which is what
//! `[profile.release]` asks for: `check_integrity()` writes the header back with its page size,
//! region geometry and both commit slots zeroed, keeping only the magic number and the god byte.
//! The file then fails its next open (`Database page size 0 does not match expected 4096`), so
//! every index a package built on an armv7 NAS was set aside and rebuilt on every run and never
//! served a pair. The debug profile every other suite runs in compiles the same source correctly,
//! and so did i686 and x86_64 release builds; the bug only exists in a release build for armv7.
//!
//! So these cases mean something only when this file is built with `--release` for the target a
//! package ships, which CI does for armv7 and i686 under QEMU (the Synology package job). On a
//! debug build they still pass, and pin what a sound build does.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{FileExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use redb::{
    Builder, Database, Durability, ReadableDatabase, ReadableTableMetadata, TableDefinition,
};
use synology_drive_sync::pair_index::{
    Binding, IndexCommit, IndexMeta, PairIndex, PairRecord, RemoteSignal, SCHEMA, SEMANTICS_EPOCH,
    StoredPair,
};
use synology_drive_sync::pair_index_redb::{IndexPaths, RedbPairIndex, prepare_directory};

const PAIRS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("pairs");

struct Dir(PathBuf);

impl Dir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "sdsync-paircodegen-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The page size redb's header records (bytes 12..16), as the file holds it now.
fn recorded_page_size(path: &Path) -> u32 {
    let file = fs::File::open(path).expect("open the database file");
    let mut field = [0_u8; 4];
    file.read_exact_at(&mut field, 12)
        .expect("read the header's page size");
    u32::from_le_bytes(field)
}

fn open(path: &Path) -> Database {
    let mut builder = Builder::new();
    builder.set_cache_size(8 << 20);
    builder.create(path).expect("redb opens the file")
}

/// The exact sequence that lost the header: create, verify (where the miscompiled write
/// happened), close, reopen, commit, close, reopen and verify. The header must be whole after
/// every step, and the pairs must read back.
#[test]
fn redb_keeps_its_header_through_verification_commit_and_reopen() {
    let dir = Dir::new("header");
    let path = dir.0.join("probe.redb");

    let mut database = open(&path);
    assert!(database.check_integrity().expect("a new file verifies"));
    assert_eq!(recorded_page_size(&path), 4096, "after check_integrity");
    drop(database);
    assert_eq!(recorded_page_size(&path), 4096, "after the first close");

    let database = open(&path);
    let mut write = database.begin_write().unwrap();
    write.set_durability(Durability::Immediate).unwrap();
    {
        let mut table = write.open_table(PAIRS).unwrap();
        for index in 0..200_u32 {
            let key = format!("folder/file-{index:05}.bin");
            table
                .insert(key.as_bytes(), [7_u8; 104].as_slice())
                .unwrap();
        }
    }
    write.commit().unwrap();
    assert_eq!(recorded_page_size(&path), 4096, "after a commit");
    drop(database);
    assert_eq!(recorded_page_size(&path), 4096, "after the second close");

    let mut database = open(&path);
    assert!(database.check_integrity().expect("the file verifies again"));
    assert_eq!(
        recorded_page_size(&path),
        4096,
        "after the second check_integrity"
    );
    let read = database.begin_read().unwrap();
    assert_eq!(read.open_table(PAIRS).unwrap().len().unwrap(), 200);
}

fn record(index: u64) -> PairRecord {
    PairRecord {
        size: index,
        local_mtime_ns: 1_700_000_000_000_000_000 + index as i64,
        local_ctime_ns: 1_700_000_000_000_000_000,
        device: 41,
        inode: index,
        remote_mtime: 1_700_000_000,
        remote_ctime: Some(1_700_000_000),
        remote_crtime: None,
        md5: [index as u8; 16],
        verified_at: 1_700_000_000,
        verified_at_nas: Some(1_700_000_000),
        racy: false,
    }
}

fn meta() -> IndexMeta {
    IndexMeta {
        schema: SCHEMA.to_owned(),
        semantics_epoch: SEMANTICS_EPOCH,
        binding: Binding {
            profile: "photos".to_owned(),
            source: "/volume1/photos".to_owned(),
            source_device: 41,
            remote: "/backup/photos".to_owned(),
            server: "https://nas.example/ DS416".to_owned(),
        },
        remote_signal: RemoteSignal::Ctime,
        canary_at: 0,
    }
}

fn paths(dir: &Dir) -> IndexPaths {
    prepare_directory(&dir.0).unwrap();
    IndexPaths::new(&dir.0, "photos").unwrap()
}

/// Two runs through the index's own open, read and commit windows: the second finds the first
/// one's evidence, with nothing named and nothing set aside. On the miscompiled build the first
/// run already set its own new file aside twice.
#[test]
fn a_second_run_reads_the_evidence_the_first_one_stored() {
    let dir = Dir::new("runs");
    let opened = RedbPairIndex::open(paths(&dir), 8);
    assert_eq!(opened.issues, Vec::new(), "the first run's open");
    let mut backend = opened.backend.expect("a usable index");
    assert_eq!(backend.meta().unwrap(), None);
    backend.finish_reads();
    backend
        .commit(IndexCommit {
            clear: false,
            remove: Vec::new(),
            upsert: (0..60)
                .map(|index| {
                    (
                        format!("folder/file-{index:03}.docx"),
                        StoredPair {
                            record: record(index),
                            remote_spelling: None,
                        },
                    )
                })
                .collect(),
            meta: Some(meta()),
        })
        .expect("the first run's commit");
    assert_eq!(backend.take_notes(), Vec::new(), "the first run's commit");
    drop(backend);

    let opened = RedbPairIndex::open(paths(&dir), 8);
    assert_eq!(opened.issues, Vec::new(), "the second run's open");
    let mut backend = opened.backend.expect("a usable index");
    assert_eq!(backend.meta().unwrap(), Some(meta()));
    let stored = backend.get("folder/file-007.docx").unwrap();
    assert_eq!(stored.map(|pair| pair.record), Some(record(7)));
    backend.finish_reads();
    let mut keys = 0;
    backend.for_each_key(&mut |_| keys += 1).unwrap();
    assert_eq!(keys, 60);

    let set_aside: Vec<_> = fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().contains(".corrupt-"))
        .collect();
    assert_eq!(set_aside, Vec::<std::ffi::OsString>::new());
}

/// The manifest line that keeps a release build out of the miscompile. Removing it is only safe
/// once the toolchain CI builds the package with compiles `redb` correctly for armv7 in one
/// codegen unit, which the two cases above, run in release under QEMU, are there to show.
#[test]
fn the_release_profile_builds_redb_outside_the_miscompile() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("the manifest");
    let section = manifest
        .split("[profile.release.package.redb]")
        .nth(1)
        .expect("Cargo.toml keeps a [profile.release.package.redb] section");
    let section = section.split("\n[").next().unwrap_or(section);
    assert!(
        section
            .lines()
            .any(|line| line.trim() == "codegen-units = 16"),
        "[profile.release.package.redb] must set codegen-units = 16"
    );
}
