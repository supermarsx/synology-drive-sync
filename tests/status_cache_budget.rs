//! Size and memory model behind the status digest cache.
//!
//! Two numbers decide whether this cache is affordable on the armv7 target, and neither is
//! guessable, so both are measured here and asserted rather than written down in a comment.
//!
//! # Bytes on disk
//!
//! The cache holds one record per unchanged file, and the shipped ceiling is
//! [`ENTRY_LIMIT`] — the same 20,000 as `SCAN_BUDGET_DEFAULT`, because a cache larger than the walk
//! that fills it could never be used in full. Path length dominates the record, exactly as it
//! dominates the inventories in `scan_memory.rs`, so this measures the deep-path worst case.
//!
//! # Bytes in memory, which is the one that could have gone wrong
//!
//! `scan_memory.rs` pins both inventories at roughly 27 MiB of a 32 MiB request ceiling. That
//! leaves no room for a third structure holding every path again, so the cache does not build one:
//! records are streamed one line at a time against the two maps that already exist, and what was
//! served is signalled by `content_md5` being `Some` rather than by a set of paths.
//!
//! This test measures the heap the cache actually holds during a full warm read. An implementation
//! that materialised its contents — or that returned the served paths as a `BTreeSet<String>`, which
//! an earlier draft of this module did — measures in megabytes here and fails. That is the whole
//! reason this file exists.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use synology_drive_sync::api::{RemoteEntry, RemoteInventory};
use synology_drive_sync::integrity::ContentMd5;
use synology_drive_sync::local::{EntryKind, FileIdentity, LocalEntry, LocalInventory};
use synology_drive_sync::status_cache::{CacheOptions, DEFAULT_MAX_AGE_SECONDS, StatusCache};

/// The whole cache, at its entry ceiling and worst-case path length, must fit in this much disk.
///
/// This is package state on a NAS system volume, not a scratch file, so it is bounded deliberately.
const CACHE_FILE_CEILING_BYTES: u64 = 8 * 1024 * 1024;

/// Heap the cache itself may hold while reading its entire contents.
///
/// Generous next to a line buffer and nowhere near a per-entry structure: at the entry ceiling a
/// map keyed by path would need several megabytes, so anything under this proves the read is
/// streamed. It is not a tight bound on purpose — the point is the order of magnitude.
const CACHE_READ_HEAP_CEILING_BYTES: usize = 256 * 1024;

const ENTRIES: usize = 20_000;
const NOW: i64 = 1_757_000_000;

struct CountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// High-water mark, which is the figure that matters here.
///
/// Live-at-exit would be satisfied by an implementation that builds a map of every entry and drops
/// it before returning — which allocates the megabytes this test exists to forbid, just not
/// permanently. Only a peak can tell a streamed read from a materialised one.
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to the system allocator unchanged and only adjusts counters
// alongside it, so allocation behaviour is identical to `System`.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            let live = LIVE_BYTES.fetch_add(new_size, Ordering::Relaxed) + new_size;
            PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

/// Reset the high-water mark to the current live total, so the next reading is a peak *over the
/// measured region* rather than over the whole process.
fn arm_peak() -> usize {
    let live = live_bytes();
    PEAK_BYTES.store(live, Ordering::Relaxed);
    live
}

fn peak_bytes() -> usize {
    PEAK_BYTES.load(Ordering::Relaxed)
}

/// The same deep-path shape `scan_memory.rs` measures with, so the two files describe one tree.
fn relative_path(index: usize) -> String {
    format!(
        "department-operations/project-archive/2026/quarter-{}/scanned-documents/invoice-statement-{index:06}.pdf",
        index % 4 + 1
    )
}

fn digest(index: usize) -> ContentMd5 {
    ContentMd5::from_bytes(std::array::from_fn(|byte| {
        (index.wrapping_mul(31) + byte) as u8
    }))
}

fn inventories(with_digests: bool) -> (LocalInventory, RemoteInventory) {
    let source = PathBuf::from("/volume1/homes/mirror-bot/exports");
    let mut local = BTreeMap::new();
    let mut remote = BTreeMap::new();
    for index in 0..ENTRIES {
        let relative = relative_path(index);
        local.insert(
            relative.clone(),
            LocalEntry {
                full_path: source.join(&relative),
                relative: relative.clone(),
                kind: EntryKind::File,
                size: 4096,
                mtime_ms: 1_600_000_000_000,
                identity: FileIdentity {
                    device: 2049,
                    inode: 1_000_000 + index as u64,
                },
                content_md5: with_digests.then(|| digest(index)),
            },
        );
        remote.insert(
            relative.clone(),
            RemoteEntry {
                remote_path: format!("/team/export/{relative}"),
                relative,
                kind: EntryKind::File,
                size: 4096,
                mtime_seconds: 1_600_000_000,
                mount_point_type: None,
                content_md5: with_digests.then(|| digest(index)),
            },
        );
    }
    (
        LocalInventory {
            root: source,
            entries: local,
        },
        RemoteInventory {
            root_exists: true,
            entries: remote,
        },
    )
}

fn options(directory: PathBuf, canary: usize) -> CacheOptions {
    CacheOptions {
        directory,
        profile: "exports".to_owned(),
        source: "/volume1/homes/mirror-bot/exports".to_owned(),
        remote: "/team/export".to_owned(),
        compare: "content",
        refresh: false,
        max_age_seconds: DEFAULT_MAX_AGE_SECONDS,
        canary,
    }
}

/// The whole model is asserted in one test on purpose.
///
/// The allocation counter is process-wide, so two tests measuring concurrently would each observe
/// the other's allocations and report noise. This binary therefore holds exactly one test.
#[test]
fn the_cache_fits_its_disk_ceiling_and_is_read_without_a_third_structure() {
    let scratch = std::env::temp_dir().join(format!("sdsync-cache-budget-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).expect("scratch directory");

    // Write a cache at the shipped entry ceiling.
    let (local, remote) = inventories(true);
    let comparison: BTreeSet<String> = local.entries.keys().cloned().collect();
    let cache = StatusCache::open(options(scratch.clone(), 0), NOW).expect("cache opens");
    cache
        .store(&local, &remote, &comparison, false)
        .expect("cache is written");
    drop(local);
    drop(remote);

    let file_bytes = fs::metadata(cache.path()).expect("cache exists").len();
    let per_entry = file_bytes / ENTRIES as u64;
    println!(
        "measured {file_bytes} bytes for {ENTRIES} entries, {per_entry} bytes per entry, against a {} MiB ceiling",
        CACHE_FILE_CEILING_BYTES / (1024 * 1024)
    );
    assert!(
        file_bytes <= CACHE_FILE_CEILING_BYTES,
        "the cache needs {file_bytes} bytes at its entry ceiling, past the {CACHE_FILE_CEILING_BYTES} it is allowed"
    );

    // Read it back against fresh inventories, measuring only what the cache itself holds.
    let (mut fresh_local, mut fresh_remote) = inventories(false);
    let baseline = arm_peak();
    let served = cache.serve(&mut fresh_local, &mut fresh_remote, &comparison);
    let peak = peak_bytes().saturating_sub(baseline);
    let retained = live_bytes().saturating_sub(baseline);

    println!(
        "cache read peaked at {peak} bytes of heap and retained {retained} while serving {} entries",
        served.supplied
    );
    assert_eq!(
        served.supplied, ENTRIES,
        "every stored entry should have been served against an unchanged tree"
    );
    assert!(
        peak <= CACHE_READ_HEAP_CEILING_BYTES,
        "reading the cache peaked at {peak} bytes, past the {CACHE_READ_HEAP_CEILING_BYTES} a streamed read is allowed; something is materialising per-entry state"
    );

    // Digests really were applied, so the measurement above is of a read that did the work.
    assert!(
        fresh_local
            .entries
            .values()
            .all(|entry| entry.content_md5.is_some())
    );
    assert!(
        fresh_remote
            .entries
            .values()
            .all(|entry| entry.content_md5.is_some())
    );

    drop(fresh_local);
    drop(fresh_remote);
    let _ = fs::remove_dir_all(&scratch);
}
