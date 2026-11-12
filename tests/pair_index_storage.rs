//! The verified-pair index's redb file under faults (perf-design §2.7, §6 "redb fault matrix").
//!
//! Each case damages, locks, hides or fills the file a previous run wrote and asserts the open
//! gives exactly one named condition and either an empty, usable index or none at all: never a
//! panic, and never evidence read from a file that did not verify.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use synology_drive_sync::pair_index::{
    Binding, IndexCommit, IndexMeta, PairIndex, PairRecord, RemoteSignal, SCHEMA, SEMANTICS_EPOCH,
    StoredPair,
};
use synology_drive_sync::pair_index_redb::{
    IndexPaths, MARKER, RedbPairIndex, decode_record, encode_record, prepare_directory,
};

struct Dir(PathBuf);

impl Dir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "sdsync-pairstore-{label}-{}-{}",
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
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn record(index: u64, generation: u64) -> PairRecord {
    PairRecord {
        size: index,
        local_mtime_ns: 1_700_000_000_000_000_000 + index as i64,
        local_ctime_ns: 1_700_000_000_000_000_000 + generation as i64,
        device: 41,
        inode: index,
        remote_mtime: 1_700_000_000,
        remote_ctime: Some(1_700_000_000 + generation as i64),
        remote_crtime: None,
        md5: [index as u8; 16],
        verified_at: 1_700_000_000,
        verified_at_nas: Some(1_700_000_000),
        racy: false,
    }
}

fn key(index: u64) -> String {
    format!("folder-{:03}/file-{index:07}.bin", index % 97)
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
            server: "https://nas.example/ DS920".to_owned(),
        },
        remote_signal: RemoteSignal::Ctime,
        canary_at: 0,
    }
}

fn paths(dir: &Dir) -> IndexPaths {
    prepare_directory(&dir.0).unwrap();
    IndexPaths::new(&dir.0, "photos").unwrap()
}

/// Write `count` pairs of `generation` in one commit and close.
fn write(dir: &Dir, count: u64, generation: u64) {
    let opened = RedbPairIndex::open(paths(dir), 8);
    let mut backend = opened.backend.expect("a usable index");
    backend
        .commit(IndexCommit {
            clear: false,
            remove: Vec::new(),
            upsert: (0..count)
                .map(|index| {
                    (
                        key(index),
                        StoredPair {
                            record: record(index, generation),
                            remote_spelling: (index % 50 == 0).then(|| key(index).to_uppercase()),
                        },
                    )
                })
                .collect(),
            meta: Some(meta()),
        })
        .unwrap();
}

/// Read every pair back, in key order.
fn read_all(backend: &mut RedbPairIndex) -> BTreeMap<String, StoredPair> {
    let mut keys = Vec::new();
    backend
        .for_each_key(&mut |key| keys.push(key.to_owned()))
        .unwrap();
    let mut pairs = BTreeMap::new();
    for key in keys {
        let pair = backend.get(&key).unwrap().expect("listed key has a pair");
        pairs.insert(key, pair);
    }
    backend.finish_reads();
    pairs
}

fn database(dir: &Dir) -> PathBuf {
    dir.0.join("photos.redb")
}

/// redb's debug assertions walk every page at open, before anything verified them, and panic on a
/// page a release build reports as an error. Without this manifest line the fault matrix below
/// would test code that never ships (and fail on panics that never happen in a release).
#[test]
fn redb_is_built_without_debug_assertions_in_every_profile() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("the manifest");
    let section = manifest
        .split("[profile.dev.package.redb]")
        .nth(1)
        .expect("Cargo.toml keeps a [profile.dev.package.redb] section");
    let section = section.split("\n[").next().unwrap_or(section);
    assert!(
        section
            .lines()
            .any(|line| line.trim() == "debug-assertions = false"),
        "[profile.dev.package.redb] must set debug-assertions = false"
    );
}

#[test]
fn pairs_round_trip_through_a_real_file_and_the_directory_is_claimed() {
    let dir = Dir::new("roundtrip");
    write(&dir, 500, 1);
    let mode = fs::metadata(database(&dir)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(dir.0.join(MARKER).is_file());
    let opened = RedbPairIndex::open(paths(&dir), 8);
    assert!(opened.issues.is_empty(), "{:?}", opened.issues);
    let mut backend = opened.backend.unwrap();
    assert_eq!(backend.meta().unwrap(), Some(meta()));
    let pairs = read_all(&mut backend);
    assert_eq!(pairs.len(), 500);
    assert_eq!(pairs[&key(7)].record, record(7, 1));
    assert_eq!(
        pairs[&key(50)].remote_spelling.as_deref(),
        Some(key(50).to_uppercase().as_str())
    );
    let bytes = encode_record(&record(3, 9), false);
    assert_eq!(decode_record(&bytes), Some((record(3, 9), false)));
}

/// What one damaged file must give: a named condition and a usable empty index, or a clean read
/// of exactly what was written. Never a panic, never a partial or wrong read.
fn assert_survives(dir: &Dir, case: &str) {
    let opened = RedbPairIndex::open(paths(dir), 8);
    let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
    let mut backend = opened
        .backend
        .unwrap_or_else(|| panic!("{case}: a damaged file is replaced, not refused: {codes:?}"));
    let pairs = read_all(&mut backend);
    if codes.is_empty() {
        // The damage hit free space: what is read is what was written.
        assert_eq!(pairs.len(), 2_000, "{case}");
        for (index, (key_text, pair)) in pairs.iter().enumerate() {
            let _ = index;
            let number: u64 = key_text
                .rsplit('-')
                .next()
                .and_then(|tail| tail.strip_suffix(".bin"))
                .and_then(|digits| digits.parse().ok())
                .unwrap();
            assert_eq!(pair.record, record(number, 1), "{case}: {key_text}");
        }
    } else {
        assert_eq!(codes, vec!["pair-index-corrupt"], "{case}");
        assert!(pairs.is_empty(), "{case}: damaged evidence is never served");
        let set_aside = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".redb.corrupt-")
            })
            .count();
        assert!(set_aside <= 1, "{case}: one quarantined copy is kept");
    }
}

#[test]
fn a_truncated_file_is_named_set_aside_and_rebuilt() {
    for cut in [4096_u64, 65_536, 1 << 20, 0] {
        let dir = Dir::new("truncated");
        write(&dir, 2_000, 1);
        let length = fs::metadata(database(&dir)).unwrap().len();
        let cut = if cut == 0 {
            length / 2
        } else {
            cut.min(length - 1)
        };
        fs::OpenOptions::new()
            .write(true)
            .open(database(&dir))
            .unwrap()
            .set_len(cut)
            .unwrap();
        let opened = RedbPairIndex::open(paths(&dir), 8);
        let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
        assert_eq!(codes, vec!["pair-index-corrupt"], "cut at {cut}");
        let mut backend = opened.backend.expect("rebuilt empty");
        assert!(read_all(&mut backend).is_empty());
    }
}

#[test]
fn an_overwritten_header_garbage_and_foreign_files_are_named() {
    for (case, damage) in [
        (
            "header",
            Box::new(|file: &mut fs::File| {
                file.seek(SeekFrom::Start(0)).unwrap();
                file.write_all(&[0x5a; 512]).unwrap();
            }) as Box<dyn Fn(&mut fs::File)>,
        ),
        (
            "garbage",
            Box::new(|file: &mut fs::File| {
                file.set_len(0).unwrap();
                file.seek(SeekFrom::Start(0)).unwrap();
                let bytes: Vec<u8> = (0..200_000_u32)
                    .map(|value| (value.wrapping_mul(2_654_435_761) >> 24) as u8)
                    .collect();
                file.write_all(&bytes).unwrap();
            }),
        ),
    ] {
        let dir = Dir::new(case);
        write(&dir, 2_000, 1);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(database(&dir))
            .unwrap();
        damage(&mut file);
        drop(file);
        let opened = RedbPairIndex::open(paths(&dir), 8);
        let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
        assert_eq!(codes, vec!["pair-index-corrupt"], "{case}");
        assert!(opened.backend.is_some(), "{case}: rebuilt");
    }
}

/// Deterministic pseudo-random positions, so a failure reproduces.
fn positions(seed: u64, count: usize, length: u64) -> Vec<u64> {
    let mut state = seed;
    (0..count)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % length
        })
        .collect()
}

#[test]
fn random_page_smashes_and_byte_flips_never_panic_or_misread() {
    for (round, seed) in (0..20_u64).map(|round| (round, 0x9e37_79b9 + round)) {
        for smash in [true, false] {
            let dir = Dir::new("smash");
            write(&dir, 2_000, 1);
            let length = fs::metadata(database(&dir)).unwrap().len();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(database(&dir))
                .unwrap();
            for position in positions(seed, if smash { 1 } else { 8 }, length) {
                file.seek(SeekFrom::Start(if smash {
                    position & !4095
                } else {
                    position
                }))
                .unwrap();
                if smash {
                    file.write_all(&[0xa5; 4096]).unwrap();
                } else {
                    file.write_all(&[(position as u8) ^ 0xff]).unwrap();
                }
            }
            drop(file);
            eprintln!("case: round {round} smash={smash}");
            assert_survives(&dir, &format!("round {round} smash={smash}"));
        }
    }
}

#[test]
fn a_symlink_a_fifo_and_a_shared_mode_are_refused_by_name() {
    let dir = Dir::new("unsafe");
    let outside = Dir::new("unsafe-target");
    let target = outside.0.join("elsewhere");
    fs::write(&target, b"not ours").unwrap();
    prepare_directory(&dir.0).unwrap();
    std::os::unix::fs::symlink(&target, database(&dir)).unwrap();
    let opened = RedbPairIndex::open(paths(&dir), 8);
    assert!(opened.backend.is_none());
    assert_eq!(opened.issues[0].code, "pair-index-unsafe");
    assert_eq!(
        fs::read(&target).unwrap(),
        b"not ours",
        "never written through"
    );
    fs::remove_file(database(&dir)).unwrap();

    let status = Command::new("mkfifo").arg(database(&dir)).status();
    if status.is_ok_and(|status| status.success()) {
        let opened = RedbPairIndex::open(paths(&dir), 8);
        assert!(opened.backend.is_none());
        assert_eq!(opened.issues[0].code, "pair-index-unsafe");
        fs::remove_file(database(&dir)).unwrap();
    }

    write(&dir, 10, 1);
    fs::set_permissions(database(&dir), fs::Permissions::from_mode(0o644)).unwrap();
    let opened = RedbPairIndex::open(paths(&dir), 8);
    assert!(opened.backend.is_none());
    assert_eq!(opened.issues[0].code, "pair-index-unsafe");
    assert!(
        opened.issues[0].cause.contains("644"),
        "{}",
        opened.issues[0].cause
    );
}

#[test]
fn a_read_only_file_is_named_and_left_alone() {
    let dir = Dir::new("readonly");
    write(&dir, 10, 1);
    fs::set_permissions(database(&dir), fs::Permissions::from_mode(0o400)).unwrap();
    // Root reads and writes past mode bits; the case cannot be made there.
    if fs::OpenOptions::new()
        .write(true)
        .open(database(&dir))
        .is_ok()
    {
        return;
    }
    let opened = RedbPairIndex::open(paths(&dir), 8);
    assert!(opened.backend.is_none());
    assert_eq!(opened.issues[0].code, "pair-index-readonly");
}

#[test]
fn a_foreign_directory_is_refused_with_its_contents_named() {
    let dir = Dir::new("foreign");
    fs::write(dir.0.join("holiday.jpg"), b"x").unwrap();
    let issue = prepare_directory(&dir.0).unwrap_err();
    assert_eq!(issue.code, "pair-index-directory-unusable");
    assert!(issue.cause.contains("holiday.jpg"), "{}", issue.cause);
    let shared = Dir::new("shared");
    fs::set_permissions(&shared.0, fs::Permissions::from_mode(0o770)).unwrap();
    let issue = prepare_directory(&shared.0).unwrap_err();
    assert!(
        issue.cause.contains("writable by group or other"),
        "{}",
        issue.cause
    );
}

/// The core waits for a short holder (a dashboard read) and then proceeds.
#[test]
fn the_core_waits_out_a_short_holder() {
    let dir = Dir::new("lock");
    write(&dir, 10, 1);
    let holder = redb::Database::open(database(&dir)).unwrap();
    let released = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        drop(holder);
    });
    let started = Instant::now();
    let opened = RedbPairIndex::open(paths(&dir), 8);
    released.join().unwrap();
    assert!(opened.backend.is_some(), "{:?}", opened.issues);
    assert!(started.elapsed() >= Duration::from_millis(1_500));
}

/// The child of the kill test: commit generation after generation until killed.
#[test]
#[ignore = "run by kill_nine_mid_commit_leaves_all_old_or_all_new"]
fn commit_forever_until_killed() {
    let Some(directory) = std::env::var_os("SDSYNC_TEST_INDEX_WRITER") else {
        return;
    };
    // The parent owns the directory: this process is killed, so it must never remove it.
    let dir = std::mem::ManuallyDrop::new(Dir(PathBuf::from(directory)));
    let mut generation = 2;
    loop {
        write(&dir, 200_000, generation);
        fs::write(dir.0.join("committed"), generation.to_string()).unwrap();
        generation += 1;
    }
}

#[test]
fn kill_nine_mid_commit_leaves_all_old_or_all_new() {
    for delay_ms in [150_u64, 400, 900, 1_700, 2_600] {
        let dir = Dir::new("kill");
        write(&dir, 200_000, 1);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "commit_forever_until_killed",
                "--nocapture",
            ])
            .env("SDSYNC_TEST_INDEX_WRITER", &dir.0)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(delay_ms));
        child.kill().unwrap();
        child.wait().unwrap();
        let opened = RedbPairIndex::open(paths(&dir), 8);
        assert!(
            opened
                .issues
                .iter()
                .all(|issue| issue.code == "pair-index-corrupt"),
            "{:?}",
            opened.issues
        );
        let mut backend = opened.backend.expect("usable after a kill");
        let pairs = read_all(&mut backend);
        if pairs.is_empty() {
            continue;
        }
        let generations: std::collections::BTreeSet<i64> = pairs
            .values()
            .map(|pair| pair.record.local_ctime_ns - 1_700_000_000_000_000_000)
            .collect();
        assert_eq!(
            generations.len(),
            1,
            "kill at {delay_ms} ms left a mix of generations {generations:?}"
        );
        assert_eq!(pairs.len(), 200_000);
    }
}

/// A full volume: the write fails by name and the previous commit stays intact. Needs a small
/// filesystem, given as `SDSYNC_TEST_FULL_DIR` (a size-limited tmpfs); skipped without one.
#[test]
fn a_full_volume_fails_the_store_by_name_and_keeps_the_previous_commit() {
    let Some(full) = std::env::var_os("SDSYNC_TEST_FULL_DIR") else {
        eprintln!("skipped: set SDSYNC_TEST_FULL_DIR to a small tmpfs to run this case");
        return;
    };
    let dir = Dir(Path::new(&full).join(format!("full-{}", std::process::id())));
    fs::create_dir_all(&dir.0).unwrap();
    fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
    write(&dir, 1_000, 1);
    let mut failed = None;
    for round in 2..200 {
        let opened = RedbPairIndex::open(paths(&dir), 8);
        let mut backend = opened.backend.expect("usable");
        let result = backend.commit(IndexCommit {
            upsert: (0..20_000)
                .map(|index| {
                    (
                        format!("bulk-{round}-{index:08}-{}", "x".repeat(64)),
                        StoredPair {
                            record: record(index, 1),
                            remote_spelling: None,
                        },
                    )
                })
                .collect(),
            ..IndexCommit::default()
        });
        if let Err(issue) = result {
            failed = Some(issue);
            break;
        }
    }
    let issue = failed.expect("the volume filled");
    assert_eq!(issue.code, "pair-index-store-failed", "{issue:?}");
    let opened = RedbPairIndex::open(paths(&dir), 8);
    let mut backend = opened.backend.expect("the previous commit is intact");
    let pairs = read_all(&mut backend);
    assert_eq!(pairs[&key(5)].record, record(5, 1));
}

/// D4: the first open of a run verifies the file; a reopen of the file exactly as this process
/// left it does not. One run (read `meta`, read pairs, walk the keys, commit, count) is one full
/// verification, not one per window.
#[test]
fn one_run_verifies_the_file_once() {
    let dir = Dir::new("verify-once");
    write(&dir, 2_000, 1);
    let opened = RedbPairIndex::open(paths(&dir), 8);
    let mut backend = opened.backend.expect("usable");
    assert_eq!(backend.integrity_checks(), 1, "the first open verifies");
    assert_eq!(backend.meta().unwrap(), Some(meta()));
    backend.finish_reads();
    for index in 0..100 {
        assert!(backend.get(&key(index)).unwrap().is_some());
    }
    backend.finish_reads();
    let mut keys = 0;
    backend.for_each_key(&mut |_| keys += 1).unwrap();
    assert_eq!(keys, 2_000);
    backend.finish_reads();
    backend
        .commit(IndexCommit {
            remove: vec![key(0)],
            ..IndexCommit::default()
        })
        .unwrap();
    assert_eq!(backend.stored_pairs(), Some(1_999));
    assert_eq!(
        backend.integrity_checks(),
        1,
        "reopens of an unchanged file"
    );
}

/// D4: a reopen that finds the file changed since this process closed it verifies it again; one
/// that is damaged is set aside, nothing from it is served, and the run goes on with a new file.
#[test]
fn a_reopen_of_a_changed_file_verifies_it_and_damage_sets_it_aside() {
    let dir = Dir::new("reopen-damaged");
    write(&dir, 2_000, 1);
    let mut backend = RedbPairIndex::open(paths(&dir), 8).backend.expect("usable");
    assert_eq!(backend.meta().unwrap(), Some(meta()));
    backend.finish_reads();
    // Between the windows, something else cuts the file short.
    let length = fs::metadata(database(&dir)).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(database(&dir))
        .unwrap()
        .set_len(length / 2)
        .unwrap();
    assert_eq!(backend.get(&key(7)).unwrap(), None, "nothing is served");
    assert!(backend.take_discarded(), "the session is told");
    assert!(
        backend.integrity_checks() >= 2,
        "the changed file was verified"
    );
    let notes = backend.take_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].code, "pair-index-corrupt");
    assert!(
        notes[0].cause.contains("set aside as"),
        "{}",
        notes[0].cause
    );
    backend.finish_reads();
    backend
        .commit(IndexCommit {
            upsert: vec![(
                key(1),
                StoredPair {
                    record: record(1, 2),
                    remote_spelling: None,
                },
            )],
            meta: Some(meta()),
            ..IndexCommit::default()
        })
        .unwrap();
    // The next run reads only what this run wrote.
    let mut next = RedbPairIndex::open(paths(&dir), 8).backend.expect("usable");
    let pairs = read_all(&mut next);
    assert_eq!(pairs.keys().cloned().collect::<Vec<_>>(), vec![key(1)]);
}

/// D4: one record that does not decode used to leave the index unusable on every run until the
/// file was deleted by hand. The file is set aside, named, and rebuilt.
#[test]
fn an_undecodable_record_sets_the_file_aside_instead_of_failing_every_run() {
    let dir = Dir::new("undecodable");
    write(&dir, 10, 1);
    {
        let raw = redb::Database::open(database(&dir)).unwrap();
        let write = raw.begin_write().unwrap();
        {
            let mut pairs = write
                .open_table(redb::TableDefinition::<&[u8], &[u8]>::new("pairs"))
                .unwrap();
            pairs
                .insert(key(3).as_bytes(), b"not a record".as_slice())
                .unwrap();
        }
        write.commit().unwrap();
    }
    for run in 0..2 {
        let opened = RedbPairIndex::open(paths(&dir), 8);
        assert!(opened.issues.is_empty(), "run {run}: {:?}", opened.issues);
        let mut backend = opened.backend.expect("usable");
        assert_eq!(backend.get(&key(3)).unwrap(), None, "run {run}");
        let discarded = backend.take_discarded();
        let notes = backend.take_notes();
        if run == 0 {
            assert!(discarded);
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert_eq!(notes[0].code, "pair-index-corrupt");
            assert!(
                notes[0].cause.contains("is not a valid pair record")
                    && notes[0].cause.contains("set aside as"),
                "{}",
                notes[0].cause
            );
        } else {
            assert!(
                !discarded && notes.is_empty(),
                "the next run is clean: {notes:?}"
            );
        }
        backend.finish_reads();
    }
}

/// N1: a profile name over 64 characters has an index of its own, which the directory claim
/// accepts on the next run, alongside a short-named profile's.
#[test]
fn a_long_profile_name_keeps_an_index_the_directory_accepts() {
    let dir = Dir::new("long-name");
    let long = format!("{}-{}", "nightly_mirror".repeat(4), "x".repeat(20));
    assert!(long.len() > 64);
    prepare_directory(&dir.0).unwrap();
    let long_paths = IndexPaths::new(&dir.0, &long).expect("usable");
    for paths in [
        long_paths.clone(),
        IndexPaths::new(&dir.0, "photos").unwrap(),
    ] {
        let mut backend = RedbPairIndex::open(paths, 8).backend.expect("usable");
        backend
            .commit(IndexCommit {
                upsert: vec![(
                    key(1),
                    StoredPair {
                        record: record(1, 1),
                        remote_spelling: None,
                    },
                )],
                meta: Some(meta()),
                ..IndexCommit::default()
            })
            .unwrap();
    }
    assert!(long_paths.database.is_file());
    let name = long_paths
        .database
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(name.len(), 48 + 1 + 16 + ".redb".len(), "{name}");
    assert!(name.starts_with(&long[..48]), "{name}");
    // The next run claims the directory with both files in it.
    assert!(prepare_directory(&dir.0).unwrap().is_empty());
    let mut backend = RedbPairIndex::open(IndexPaths::new(&dir.0, &long).unwrap(), 8)
        .backend
        .expect("usable");
    assert_eq!(read_all(&mut backend).len(), 1);
}

/// A copy that cannot be removed: a non-empty directory under a copy's name, which
/// `remove_file` refuses whatever the permissions.
fn stuck_copy(dir: &Dir, epoch: u64, old: bool) -> PathBuf {
    let path = dir.0.join(format!("photos.redb.corrupt-{epoch}"));
    fs::create_dir_all(path.join("inside")).unwrap();
    if old {
        fs::File::open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(30 * 86_400))
            .unwrap();
    }
    path
}

/// S3-7: the weekly sweep of quarantined copies ignored a copy it could not remove. It is named
/// once, with the first path and cause and how many more, and the index is still used.
#[test]
fn a_quarantined_copy_the_sweep_cannot_remove_is_named_once() {
    let dir = Dir::new("sweep");
    prepare_directory(&dir.0).unwrap();
    let first = stuck_copy(&dir, 1, true);
    let second = stuck_copy(&dir, 2, true);
    let recent = stuck_copy(&dir, 3, false);
    let notes = prepare_directory(&dir.0).expect("the directory is still usable");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].code, "pair-index-cleanup-failed");
    assert!(
        [first.display().to_string(), second.display().to_string()].contains(&notes[0].path),
        "{notes:?}"
    );
    assert!(
        notes[0].cause.contains("older than 7 days") && notes[0].cause.contains("1 more"),
        "{}",
        notes[0].cause
    );
    assert!(recent.exists(), "a recent copy is not swept");
}

/// S3-7: copies a damaged file's quarantine could not remove are named, and past three a damaged
/// file is deleted instead of set aside, so copies cannot pile up.
#[test]
fn copies_that_cannot_be_removed_are_named_and_bounded() {
    for stuck in [1_u64, 3] {
        let dir = Dir::new("bounded");
        write(&dir, 100, 1);
        for epoch in 0..stuck {
            stuck_copy(&dir, epoch + 1, false);
        }
        fs::OpenOptions::new()
            .write(true)
            .open(database(&dir))
            .unwrap()
            .set_len(4096)
            .unwrap();
        let opened = RedbPairIndex::open(paths(&dir), 8);
        let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
        assert_eq!(
            codes,
            vec!["pair-index-corrupt", "pair-index-cleanup-failed"],
            "{stuck}: {:?}",
            opened.issues
        );
        assert!(
            opened.issues[1]
                .path
                .contains(&dir.0.join("photos.redb.corrupt-").display().to_string()),
            "{:?}",
            opened.issues[1]
        );
        let copies = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".redb.corrupt-")
            })
            .count();
        if stuck < 3 {
            assert!(opened.issues[0].cause.contains("set aside as"));
            assert_eq!(copies as u64, stuck + 1, "the damaged file is kept");
        } else {
            assert!(
                opened.issues[0]
                    .cause
                    .contains("deleted rather than set aside"),
                "{}",
                opened.issues[0].cause
            );
            assert_eq!(copies as u64, stuck, "no copy is added past the bound");
        }
        let mut backend = opened.backend.expect("a new, empty index");
        assert!(read_all(&mut backend).is_empty());
    }
}

/// The panic record, read: a file a previous run's redb panicked on is set aside before redb
/// opens it again, and the record goes with it.
#[test]
fn a_recorded_panic_sets_the_file_aside_before_redb_opens_it() {
    let dir = Dir::new("panic-record");
    write(&dir, 10, 1);
    let record_path = dir.0.join("photos.redb.panicked");
    fs::write(&record_path, "index out of range in btree_cursor.rs\n").unwrap();
    let opened = RedbPairIndex::open(paths(&dir), 8);
    let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
    assert_eq!(codes, vec!["pair-index-corrupt"]);
    assert!(
        opened.issues[0].cause.contains("stopped inside redb")
            && opened.issues[0].cause.contains("btree_cursor"),
        "{}",
        opened.issues[0].cause
    );
    assert!(!record_path.exists(), "the record goes with the file");
    let mut backend = opened.backend.expect("a new, empty index");
    assert!(read_all(&mut backend).is_empty());
}

/// The panic record, written: redb 4.4 panics inside `create_file` on some corruptions of its
/// saved allocator state, before any check can run, and a release build aborts. Each page of a
/// real file is damaged in turn until one makes the open panic; the hook must have recorded it,
/// and the next open must set the file aside instead of panicking again.
#[test]
fn a_panic_inside_redb_is_recorded_and_the_next_open_heals() {
    let dir = Dir::new("panic-heal");
    write(&dir, 2_000, 1);
    let original = fs::read(database(&dir)).unwrap();
    let record_path = dir.0.join("photos.redb.panicked");
    let mut panicked_at = None;
    for page in 1..(original.len() / 4096) {
        let mut damaged = original.clone();
        damaged[page * 4096] ^= 0x01;
        fs::write(database(&dir), &damaged).unwrap();
        let attempt =
            std::panic::catch_unwind(|| RedbPairIndex::open(paths(&dir), 8).backend.is_some());
        if attempt.is_err() {
            panicked_at = Some(page * 4096);
            break;
        }
        // Damage caught by name was set aside: put the file back for the next page.
        for entry in fs::read_dir(&dir.0).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .contains(".redb.corrupt-")
            {
                fs::remove_file(entry.path()).unwrap();
            }
        }
    }
    let Some(position) = panicked_at else {
        eprintln!("no single-byte page damage made redb panic: nothing to heal");
        return;
    };
    assert!(
        record_path.is_file(),
        "the panic at {position} was recorded"
    );
    let opened = RedbPairIndex::open(paths(&dir), 8);
    let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
    assert_eq!(codes, vec!["pair-index-corrupt"], "{:?}", opened.issues);
    assert!(!record_path.exists());
    let mut backend = opened.backend.expect("healed");
    assert!(read_all(&mut backend).is_empty());
}

/// The compaction budget is `2 * COMPACT_BYTES_PER_ENTRY` bytes per pair. Measured here so the
/// number cannot drift below what redb and the record layout really cost: a compacted file built
/// in key order, as every commit writes its pairs, costs at most `COMPACT_BYTES_PER_ENTRY` per
/// pair with paths of 76 and 150 bytes, and one built in an arbitrary order (redb splits its
/// pages half full) still fits the doubled budget with 76-byte paths, so compacting it cannot
/// leave the file over budget for good. (The file on disk before compaction is larger, because
/// redb grows it in doubling steps; the budget is what bounds that.)
#[test]
fn a_compacted_pair_costs_no_more_than_the_compaction_budget_assumes() {
    use synology_drive_sync::pair_index_redb::COMPACT_BYTES_PER_ENTRY;
    const ENTRIES: u64 = 30_000;
    for (key_bytes, in_key_order, ceiling) in [
        (76_usize, true, COMPACT_BYTES_PER_ENTRY),
        (150, true, COMPACT_BYTES_PER_ENTRY),
        (76, false, 2 * COMPACT_BYTES_PER_ENTRY),
    ] {
        let dir = Dir::new("dense");
        let location = paths(&dir);
        {
            let mut backend = RedbPairIndex::open(location.clone(), 8)
                .backend
                .expect("a usable index");
            let mut upsert: Vec<(String, StoredPair)> = (0..ENTRIES)
                .map(|index| {
                    let mut name = format!("Photos/2019/trip-{:03}/IMG_{index:07}", index % 97);
                    while name.len() + 4 < key_bytes {
                        name.push('x');
                    }
                    name.push_str(".jpg");
                    (
                        name,
                        StoredPair {
                            record: record(index, 1),
                            remote_spelling: None,
                        },
                    )
                })
                .collect();
            if in_key_order {
                upsert.sort_by(|a, b| a.0.cmp(&b.0));
            }
            backend
                .commit(IndexCommit {
                    clear: false,
                    remove: Vec::new(),
                    upsert,
                    meta: Some(meta()),
                })
                .unwrap();
        }
        let mut database = redb::Database::open(&location.database).expect("the file opens");
        database.compact().expect("compacts");
        drop(database);
        let per_entry = fs::metadata(&location.database).unwrap().len() / ENTRIES;
        eprintln!(
            "{key_bytes}-byte paths, key order {in_key_order}: {per_entry} bytes per compacted pair"
        );
        assert!(
            per_entry <= ceiling,
            "a compacted pair with {key_bytes}-byte paths (key order {in_key_order}) costs \
             {per_entry} bytes, over the {ceiling} the compaction budget allows: correct \
             COMPACT_BYTES_PER_ENTRY, pair_index_redb.rs's note on it and docs/pair-index.md"
        );
        assert!(
            per_entry >= key_bytes as u64 + 104,
            "{per_entry} bytes is less than the key and record alone: the measurement is wrong"
        );
    }
}

/// What the armv7 miscompile left on disk (`tests/pair_index_codegen.rs`): the magic number and
/// the god byte, and the rest of the header zero.
fn zero_the_header_body(dir: &Dir) {
    use std::os::unix::fs::FileExt as _;
    let file = fs::OpenOptions::new()
        .write(true)
        .open(database(dir))
        .unwrap();
    file.write_all_at(&[0; 310], 10).unwrap();
}

/// The directory's entries and their modification times.
fn listing(dir: &Dir) -> BTreeMap<String, std::time::SystemTime> {
    fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.metadata().unwrap().modified().unwrap(),
            )
        })
        .collect()
}

fn copies(dir: &Dir) -> usize {
    listing(dir)
        .keys()
        .filter(|name| name.contains(".redb.corrupt-"))
        .count()
}

fn sixty_pairs() -> IndexCommit {
    IndexCommit {
        clear: false,
        remove: Vec::new(),
        upsert: (0..60)
            .map(|index| {
                (
                    key(index),
                    StoredPair {
                        record: record(index, 1),
                        remote_spelling: None,
                    },
                )
            })
            .collect(),
        meta: Some(meta()),
    }
}

/// A system that cannot keep the index, as the DS416 on 26.62 could not: every file is found
/// damaged, with the same cause, the next time it is opened. The third such open in a row sets
/// the file aside like the others but builds no new one, and names a lasting condition. Later
/// runs name the same condition and touch nothing, so one quarantined copy is all that stays,
/// and the runs go on without the index.
#[test]
fn an_index_this_system_cannot_keep_is_turned_off_by_name_not_rebuilt_every_run() {
    let dir = Dir::new("unkeepable");
    write(&dir, 50, 1);
    zero_the_header_body(&dir);

    // Run 1: the open finds the file damaged (one) and builds a new one; the commit's reopen
    // finds that one damaged too (two) and commits into a third.
    let opened = RedbPairIndex::open(paths(&dir), 8);
    let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
    assert_eq!(codes, vec!["pair-index-corrupt"]);
    assert!(
        opened.issues[0].cause.contains("page size 0"),
        "{:?}",
        opened.issues
    );
    let mut backend = opened
        .backend
        .expect("rebuilt after the first damaged open");
    assert_eq!(backend.meta().unwrap(), None);
    backend.finish_reads();
    zero_the_header_body(&dir);
    backend
        .commit(sixty_pairs())
        .expect("the second damaged open still rebuilds");
    let codes: Vec<&str> = backend
        .take_notes()
        .iter()
        .map(|issue| issue.code)
        .collect();
    assert_eq!(codes, vec!["pair-index-corrupt"]);
    drop(backend);
    assert_eq!(copies(&dir), 1, "one quarantined copy, as designed");
    assert!(paths(&dir).rebuilds.is_file(), "the count is kept");

    // Run 2: the third damaged open in a row.
    zero_the_header_body(&dir);
    let opened = RedbPairIndex::open(paths(&dir), 8);
    let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
    assert_eq!(codes, vec!["pair-index-corrupt", "pair-index-unkeepable"]);
    assert!(
        opened.backend.is_none(),
        "the run goes on without the index"
    );
    let unkeepable = &opened.issues[1];
    assert_eq!(unkeepable.severity.as_str(), "warning");
    assert!(
        unkeepable.cause.contains("3 times in a row")
            && unkeepable.cause.contains("page size 0")
            && unkeepable.cause.contains("photos.redb.rebuilds"),
        "{unkeepable:?}"
    );
    assert!(
        opened.issues[0].cause.contains("no new index is built"),
        "{:?}",
        opened.issues[0]
    );
    assert!(!database(&dir).exists(), "no new file is built");
    assert_eq!(copies(&dir), 1, "still one quarantined copy");

    // Run 3 and on: the same lasting condition, and nothing in the directory changes.
    let before = listing(&dir);
    for _ in 0..3 {
        let opened = RedbPairIndex::open(paths(&dir), 8);
        let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
        assert_eq!(codes, vec!["pair-index-unkeepable"]);
        assert!(opened.backend.is_none());
    }
    assert_eq!(listing(&dir), before, "a standing verdict touches nothing");
}

/// One damaged file is not a pattern: the open that finds the rebuilt file sound clears the
/// count, so damage months apart never adds up to the lasting condition.
#[test]
fn a_sound_reopen_clears_the_count_of_damaged_opens() {
    let dir = Dir::new("count-cleared");
    write(&dir, 50, 1);
    zero_the_header_body(&dir);
    let opened = RedbPairIndex::open(paths(&dir), 8);
    assert_eq!(opened.issues.len(), 1, "{:?}", opened.issues);
    let mut backend = opened.backend.expect("rebuilt");
    backend.finish_reads();
    assert!(
        paths(&dir).rebuilds.is_file(),
        "one damaged open is counted"
    );
    // The commit's reopen finds the new file as this process left it: sound.
    backend.commit(sixty_pairs()).unwrap();
    assert_eq!(backend.take_notes(), Vec::new());
    drop(backend);
    assert!(!paths(&dir).rebuilds.exists(), "a sound reopen clears it");

    for _ in 0..2 {
        zero_the_header_body(&dir);
        let opened = RedbPairIndex::open(paths(&dir), 8);
        let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
        assert_eq!(
            codes,
            vec!["pair-index-corrupt"],
            "each damage is healed alone"
        );
        let mut backend = opened.backend.expect("rebuilt");
        backend.commit(sixty_pairs()).unwrap();
        drop(backend);
        let opened = RedbPairIndex::open(paths(&dir), 8);
        assert_eq!(opened.issues, Vec::new());
    }
}

/// A verdict another build reached, or one older than the retry period, is tried again from a
/// count of zero; a record that is not one this package writes is named and starts over.
#[test]
fn a_lapsed_or_unreadable_verdict_is_tried_again() {
    let dir = Dir::new("verdict-lapsed");
    write(&dir, 10, 1);
    let rebuilds = paths(&dir).rebuilds;
    fs::write(
        &rebuilds,
        "sdsync.pair-index-rebuilds.v1\nwriter=0.0.1+elsewhere/redb-4.4.0\ncount=9\nsince=1\n\
         last=1\ncause=DB corrupted: Database page size 0 does not match expected 4096\n",
    )
    .unwrap();
    fs::set_permissions(&rebuilds, fs::Permissions::from_mode(0o600)).unwrap();
    let opened = RedbPairIndex::open(paths(&dir), 8);
    assert_eq!(opened.issues, Vec::new());
    assert!(opened.backend.is_some(), "tried again");
    assert!(!rebuilds.exists(), "the lapsed verdict is cleared");
    drop(opened);

    fs::write(&rebuilds, "not a record\n").unwrap();
    fs::set_permissions(&rebuilds, fs::Permissions::from_mode(0o600)).unwrap();
    let opened = RedbPairIndex::open(paths(&dir), 8);
    let codes: Vec<&str> = opened.issues.iter().map(|issue| issue.code).collect();
    assert_eq!(codes, vec!["pair-index-cleanup-failed"]);
    assert!(
        opened.issues[0].cause.contains("starts over")
            && opened.issues[0].path.ends_with("photos.redb.rebuilds"),
        "{:?}",
        opened.issues
    );
    assert!(opened.backend.is_some());
}
