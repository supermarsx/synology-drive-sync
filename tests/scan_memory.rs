//! Memory model behind `local::SCAN_BUDGET_DEFAULT`.
//!
//! The 200-row page cap bounds a status *response*; it does not bound the *walk* that produces it.
//! A scoped scan materializes a local and a remote inventory in full before anything is compared,
//! so the walk's ceiling is a memory ceiling, and on the armv7 target that ceiling is the binding
//! constraint rather than time.
//!
//! This measures the real heap cost of one scanned entry on both sides with a counting allocator,
//! then asserts that the shipped budget keeps both inventories together under
//! [`REQUEST_MEMORY_CEILING_BYTES`]. Raising the budget without re-deriving it fails here, so the
//! constant and the evidence behind it cannot drift apart.
//!
//! # What the bytes are made of
//!
//! Measured for a 99-byte relative path, per local+remote entry pair. Windows and Linux/glibc agree
//! to within 1% (**1,466** and **1,451**), so the figure is the data structures, not the allocator.
//! It projects to 27 MiB of the 32 MiB ceiling at the shipped budget.
//!
//! The itemised breakdown that used to live here totalled 1,336 and is **not** re-derived below,
//! because it no longer adds up and re-deriving it honestly means attributing every struct change
//! since it was written. What is known, measured rather than reasoned:
//!
//! ```text
//! LocalEntry::identity            30   FileIdentity, added for the status digest cache
//! everything else              1,436   including ~100 unattributed to any itemised line
//!                             ------
//! per pair                     1,466
//! ```
//!
//! The `identity` figure was obtained by substitution — measuring with the field shrunk to two
//! bytes and differencing — so it is evidence, not arithmetic. Note the ~2x amplification it shows:
//! a 16-byte struct field costs about 30 bytes per entry because a `BTreeMap` node reserves value
//! slots for 11 entries whether or not it holds 11. Anyone adding a field here should expect to pay
//! roughly double what `size_of` suggests.
//!
//! Three consequences worth knowing before retuning:
//!
//! - The path text is held six times per pair (key, `relative`, and a derived absolute path, on
//!   each side) for one logical path. That is where a compact representation would win.
//! - Cost tracks path length, so "entries" is not a fixed amount of memory: shallow entries cost
//!   roughly half what deep ones do, so the budget is set from the deep-path worst case.
//! - The status digest cache does **not** add a third structure here. It is consumed one record at
//!   a time against these two maps, so its own memory is a single line buffer regardless of how
//!   many entries it holds. A cache materialised as a third `BTreeMap` would breach this ceiling,
//!   which is why it is not one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use synology_drive_sync::api::{RemoteEntry, RemoteInventory};
use synology_drive_sync::local::{EntryKind, LocalEntry, LocalInventory, SCAN_BUDGET_DEFAULT};

/// Both inventories for one request must fit in this much heap.
const REQUEST_MEMORY_CEILING_BYTES: usize = 32 * 1024 * 1024;

struct CountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to the system allocator unchanged and only adjusts a counter
// alongside it, so allocation behaviour is identical to `System`.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            LIVE_BYTES.fetch_add(new_size, Ordering::Relaxed);
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

/// A relative path shaped like real payload rather than a short synthetic name.
///
/// Deep, descriptive folder trees are what a Drive-backed share actually holds, and path length
/// dominates per-entry cost: every entry stores its relative path twice on the remote side (the
/// relative and the absolute File Station path) and twice on the local side (the relative and a
/// `PathBuf`), plus once more as the map key.
fn relative_path(index: usize) -> String {
    format!(
        "department-operations/project-archive/2026/quarter-{}/scanned-documents/invoice-statement-{index:06}.pdf",
        index % 4 + 1
    )
}

fn local_inventory(count: usize) -> LocalInventory {
    let source = PathBuf::from("/volume1/homes/mirror-bot/exports");
    let mut entries = BTreeMap::new();
    for index in 0..count {
        let relative = relative_path(index);
        entries.insert(
            relative.clone(),
            LocalEntry {
                full_path: source.join(&relative),
                relative,
                kind: EntryKind::File,
                size: 4096,
                mtime_ms: 1_762_000_000_000,
                identity: Default::default(),
                content_md5: None,
            },
        );
    }
    LocalInventory {
        root: source,
        entries,
    }
}

fn remote_inventory(count: usize) -> RemoteInventory {
    let mut entries = BTreeMap::new();
    for index in 0..count {
        let relative = relative_path(index);
        entries.insert(
            relative.clone(),
            RemoteEntry {
                remote_path: format!("/team/export/{relative}"),
                relative,
                kind: EntryKind::File,
                size: 4096,
                mtime_seconds: 1_762_000_000,
                mount_point_type: None,
                content_md5: None,
            },
        );
    }
    RemoteInventory {
        root_exists: true,
        entries,
    }
}

/// Heap bytes held by one local plus one remote entry for the same path.
fn measured_bytes_per_entry_pair() -> usize {
    // A large enough sample that per-map overhead and allocator rounding average out.
    const SAMPLE: usize = 20_000;

    let baseline = live_bytes();
    let local = local_inventory(SAMPLE);
    let remote = remote_inventory(SAMPLE);
    let peak = live_bytes();

    assert_eq!(local.entries.len(), SAMPLE);
    assert_eq!(remote.entries.len(), SAMPLE);
    let held = peak.saturating_sub(baseline);
    drop(local);
    drop(remote);

    held.div_ceil(SAMPLE)
}

/// The whole model is asserted in one test on purpose.
///
/// The allocation counter is process-wide, so two tests measuring concurrently would each observe
/// the other's allocations and report noise. This binary therefore holds exactly one test.
#[test]
fn the_scan_budget_is_derived_from_a_measured_per_request_memory_ceiling() {
    let per_pair = measured_bytes_per_entry_pair();
    let projected = per_pair.saturating_mul(SCAN_BUDGET_DEFAULT);

    println!(
        "measured {per_pair} bytes per local+remote entry pair; \
         SCAN_BUDGET_DEFAULT = {SCAN_BUDGET_DEFAULT} projects to {} MiB of a {} MiB ceiling",
        projected / (1024 * 1024),
        REQUEST_MEMORY_CEILING_BYTES / (1024 * 1024),
    );

    // Guards the model itself: if entry layout changes enough to move this materially, the budget
    // needs re-deriving rather than silently coming to mean something else.
    assert!(
        (400..=4_000).contains(&per_pair),
        "per-entry-pair cost of {per_pair} bytes is outside the range the budget was derived \
         against; re-derive local::SCAN_BUDGET_DEFAULT from this measurement"
    );

    assert!(
        projected <= REQUEST_MEMORY_CEILING_BYTES,
        "a scan at the shipped budget would hold {projected} bytes, over the \
         {REQUEST_MEMORY_CEILING_BYTES}-byte per-request ceiling. Either lower \
         local::SCAN_BUDGET_DEFAULT or re-derive the ceiling; do not raise the budget without \
         re-running this measurement."
    );

    // Pinned from below as well, so the constant stays derived from the ceiling rather than
    // chosen arbitrarily low and quietly truncating scans that would have fit.
    assert!(
        projected.saturating_mul(4) >= REQUEST_MEMORY_CEILING_BYTES,
        "the budget projects to only {projected} bytes against a {REQUEST_MEMORY_CEILING_BYTES}-byte \
         ceiling, which truncates scans that would fit; re-derive it from the measurement"
    );
}
