//! Disposable digest cache and aggregate rollup for the status query.
//!
//! # What this is, and what it deliberately is not
//!
//! A status query in [`crate::plan::CompareMode::Content`] spends almost all of its time obtaining
//! two digests per file: the local MD5, which costs a full read of every byte, and File Station's
//! server-side MD5, which costs a task start plus at least one status poll — two serial round trips
//! per file. In the steady state both answers are the same as last time, because
//! [`crate::plan::select_comparison_remote_digests`] only asks about pairs whose size *and*
//! modification time already agree on both sides.
//!
//! This module stores those digests so an unchanged file need not be re-read and re-hashed. It is
//! a **cache, not a ledger**: deleting it is always safe and only ever costs time. Every guarantee
//! the package makes holds with the cache absent, stale, truncated, or corrupt, because those cases
//! all degrade to exactly one behaviour — recompute from live evidence.
//!
//! # Evidence, not verdicts
//!
//! An entry stores two independently recorded MD5s and the metadata identity they were observed
//! under. It does **not** store "this file is in sync". The verdict is still derived on every query
//! by [`crate::plan`], from digests, using the same `compare_files` the cold path uses — so the
//! comparison logic has exactly one implementation and the cache cannot drift from it.
//!
//! # MD5 only, by construction
//!
//! Neither CRC32 nor SHA-256 is stored, and the format has no field that could hold one. A record
//! reaches the engine only through [`ContentMd5::from_bytes`], which leaves the strong digests
//! absent, so a cache-sourced fingerprint returns `false` from `has_full_proof`, `None` from
//! `full_match`, and at best [`crate::integrity::ContentMatch::Md5Only`] from `compare_content`.
//!
//! That is what keeps deletion and server-copy safe. Both authorise destroying or duplicating
//! remote data and both require `ContentMatch::Strong`, so a cache entry is *structurally
//! incapable* of satisfying their guard: wiring one in by mistake produces a refused mutation, not
//! an unverified one. Two weaker barriers sit above it — neither path constructs a reader, and
//! `select_strong_remote_digests` selects entries with no local counterpart, which is disjoint from
//! the set this cache holds — but the type is the barrier that survives careless editing.
//!
//! # The one failure this cannot prevent
//!
//! A file whose bytes change while its size, modification time and inode all stay identical (an
//! in-place `cp -p` from a same-length revision, a `touch -r` after an edit, a filesystem rollback)
//! will be served a stale digest and shown as in sync when it is not. That is a real regression in
//! *display* accuracy against the uncached path, and it is stated plainly rather than argued away.
//!
//! Sync correctness is unaffected, because **no mutation path reads this cache** — `plan`, `run`,
//! `resync`, delete and server-copy are untouched code that never constructs a reader, so the next
//! run hashes the file live, sees the difference, and uploads it. The window is additionally
//! bounded by four independent mechanisms: the next sync run moves the remote modification time and
//! invalidates the entry, [`DEFAULT_MAX_AGE_SECONDS`] expires it outright, a package upgrade retires
//! the whole file, and an explicit refresh discards it. The user-visible bound on that window is
//! `oldest_evidence_epoch`, which is why the reported age is the *oldest* contributing observation
//! and not the newest: it is the only claim that is true of every row in the answer.
//!
//! # The canary
//!
//! A design in which the cache can be broadly wrong and nothing notices would be the dangerous one.
//! Each warm pass therefore re-verifies a small absolute sample of served entries against live
//! evidence. It is not a sweep — [`DEFAULT_CANARY`] entries against a 20,000-entry tree would take
//! hundreds of runs to cover it — it is a detector for *systematic* error, which it catches almost
//! immediately. A single mismatch discards the entire file and forces a cold pass rather than
//! repairing the offending entry, because repairing it would hide exactly the case worth finding.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::RemoteInventory;
use crate::integrity::ContentMd5;
use crate::local::{EntryKind, FileIdentity, LocalInventory};
use crate::plan::StatusStats;

/// Format identity. A reader that does not recognise this exact string treats the file as absent.
pub const SCHEMA: &str = "sdsync.status-cache.v1";

/// Asserted on read. Its only legal value; see the module comment on why nothing stronger is
/// storable. A future format carrying strong digests must choose a new [`SCHEMA`], not a new value
/// here, so that an old reader cannot silently accept it.
pub const DIGEST_STRENGTH: &str = "md5-only";

/// Proves the file was written to completion. Renaming already makes a torn file unreachable; this
/// also catches a temporary file truncated by a full filesystem before the rename.
pub const END_SENTINEL: &str = "sdsync.status-cache.v1.end";

/// Schema of the aggregate rollup, which is a separate document with a separate lifetime.
pub const ROLLUP_SCHEMA: &str = "sdsync.status-rollup.v1";

/// Entries retained per profile, matching [`crate::local::SCAN_BUDGET_DEFAULT`]: a cache larger
/// than the walk that fills it could never be used in full. Measured worst case at this ceiling is
/// about 5.2 MB, for 99-byte paths.
pub const ENTRY_LIMIT: usize = crate::local::SCAN_BUDGET_DEFAULT;

/// Refuse to read a file larger than this. The entry ceiling already bounds what this module
/// writes, so a larger file did not come from here and is not worth the work of finding that out.
pub const FILE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Bound on the whole cache directory across every profile. Past it, whole profile files are
/// removed oldest-first; they are disposable, so the crude unit is the right one.
pub const DIRECTORY_LIMIT_BYTES: u64 = 32 * 1024 * 1024;

/// Evidence older than this is treated as absent, so a tree that never changes is still re-verified
/// end to end on a bounded cadence. A scheduled run refreshes entries long before this bites; it is
/// the backstop for a package that syncs only on demand.
pub const DEFAULT_MAX_AGE_SECONDS: i64 = 14 * 24 * 60 * 60;

/// Served entries re-verified live per pass. Absolute rather than proportional: its job is to
/// detect systematic error, and 25 independent probes do that regardless of tree size, whereas a
/// percentage would scale the cost back up with the thing being avoided.
pub const DEFAULT_CANARY: usize = 25;

/// Never trust an entry whose recorded modification time is this close to the moment the cache was
/// written. Git's racy-index rule: a file rewritten within the same clock tick as the observation
/// can carry an unchanged timestamp, and no amount of metadata comparison can see it.
const RACY_WINDOW_SECONDS: i64 = 1;

/// How a status answer was produced, reported so a caller can label its own confidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheState {
    /// No cache directory was configured. Behaviour is identical to the package before this existed.
    Off,
    /// A cache is configured but supplied nothing: first run, wrong header, or expired.
    Cold,
    /// At least one digest was served from stored evidence.
    Warm,
    /// Stored evidence was deliberately ignored and recomputed.
    Refreshed,
    /// Stored evidence was found to be wrong and the file was discarded. Always accompanied by a
    /// full cold recomputation, so the answer is live; the state exists to make the fault visible.
    Unusable,
}

impl CacheState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Cold => "cold",
            Self::Warm => "warm",
            Self::Refreshed => "refreshed",
            Self::Unusable => "unusable",
        }
    }
}

/// What a status answer's digests cost, for the `cache` object in the status output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheReport {
    pub state: CacheState,
    pub digests_reused: usize,
    pub digests_computed: usize,
    /// The oldest observation any served digest rests on; the honest "as of" for the whole answer.
    pub oldest_evidence_epoch: Option<i64>,
    pub canary_checked: usize,
}

impl CacheReport {
    pub fn off() -> Self {
        Self {
            state: CacheState::Off,
            digests_reused: 0,
            digests_computed: 0,
            oldest_evidence_epoch: None,
            canary_checked: 0,
        }
    }
}

/// Everything needed to decide whether a stored file describes *this* query.
#[derive(Clone, Debug)]
pub struct CacheOptions {
    pub directory: PathBuf,
    pub profile: String,
    pub source: String,
    pub remote: String,
    pub compare: &'static str,
    /// Ignore stored evidence, recompute live, and rewrite. The refresh button.
    pub refresh: bool,
    pub max_age_seconds: i64,
    pub canary: usize,
}

/// One record, as stored. Field names carry the digest strength so a future reader learns it from
/// the file rather than by tracing a type.
#[derive(Debug, Serialize)]
struct Record<'a> {
    /// Source-relative path. Records are written in ascending order of this field.
    p: &'a str,
    /// Size, shared: an entry exists only for a pair whose sizes agreed.
    s: u64,
    /// Local modification time in milliseconds.
    m: i64,
    /// Local filesystem identity as `device:inode`, or `0:0` where the platform exposes none.
    n: String,
    lmd5: String,
    /// Remote modification time in seconds, which is all File Station reports.
    rm: i64,
    rmd5: String,
    /// When this pair was observed. Bounds staleness and drives `oldest_evidence_epoch`.
    t: i64,
}

/// The borrowed form used on read. `Cow` rather than `&str` because a path containing a quote or a
/// backslash arrives escaped, and refusing to parse it would discard the whole file over one name.
#[derive(Debug, Deserialize)]
struct RecordRef<'a> {
    #[serde(borrow)]
    p: std::borrow::Cow<'a, str>,
    s: u64,
    m: i64,
    #[serde(borrow)]
    n: std::borrow::Cow<'a, str>,
    #[serde(borrow)]
    lmd5: std::borrow::Cow<'a, str>,
    rm: i64,
    #[serde(borrow)]
    rmd5: std::borrow::Cow<'a, str>,
    t: i64,
}

#[derive(Debug, Deserialize, Serialize)]
struct Header {
    schema: String,
    digest_strength: String,
    /// Package version. A mismatch retires the file, following the precedent the DSM response cache
    /// already sets: an upgrade cannot leave evidence behind whose meaning may have changed, and
    /// nobody has to remember to bump a format number for that to hold.
    binary: String,
    platform: String,
    profile: String,
    source: String,
    remote: String,
    compare: String,
    generated_at: i64,
    /// Records written last time. Only used to size the canary stride in a single pass; a wrong
    /// value costs a differently sized sample, never a wrong answer.
    entries: usize,
}

/// One served entry held back for live re-verification.
#[derive(Clone, Debug)]
pub struct CanaryProbe {
    pub relative: String,
    cached_local: [u8; 16],
    cached_remote: [u8; 16],
}

/// What [`StatusCache::serve`] supplied, and what it deliberately withheld.
///
/// Deliberately holds no list of served paths. A `BTreeSet<String>` of them would store every path
/// a third time — measured at roughly 250 bytes per entry, so about 5 MB at the scan budget — on
/// top of the two inventories that `tests/scan_memory.rs` already pins at 27 MiB of a 32 MiB
/// ceiling. That set would breach the ceiling on the exact platform this cache exists to help.
///
/// The information is not lost, because the inventories already carry it: an entry whose
/// `content_md5` is `Some` after this ran is one the cache supplied. Callers read that instead.
#[derive(Debug, Default)]
pub struct Served {
    /// How many pairs had both digests filled from stored evidence.
    pub supplied: usize,
    /// Entries that had usable evidence but were withheld so live work re-derives them, bounded by
    /// [`CacheOptions::canary`]. Verified afterwards by [`StatusCache::verify_canary`].
    pub probes: Vec<CanaryProbe>,
    pub oldest_evidence_epoch: Option<i64>,
}

/// A reader and writer bound to one profile's cache file.
#[derive(Debug)]
pub struct StatusCache {
    options: CacheOptions,
    path: PathBuf,
    now: i64,
}

impl StatusCache {
    /// Bind to a profile's cache, or decline.
    ///
    /// Declines rather than fails: an unusable directory means the query runs exactly as it did
    /// before this module existed. `None` is never an error the caller reports.
    pub fn open(options: CacheOptions, now: i64) -> Option<Self> {
        if !name_is_safe(&options.profile) {
            return None;
        }
        if !directory_is_private(&options.directory) {
            return None;
        }
        let path = options
            .directory
            .join(format!("{}.ndjson", options.profile));
        Some(Self { options, path, now })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this pass was told to ignore stored evidence. Reported rather than inferred: a
    /// refreshed answer and a cold one both reuse nothing, and a caller showing "as of" needs to
    /// tell "there was nothing to reuse" from "we chose not to".
    pub fn is_refresh(&self) -> bool {
        self.options.refresh
    }

    fn rollup_path(&self) -> PathBuf {
        self.options
            .directory
            .join(format!("{}.rollup.json", self.options.profile))
    }

    /// Fill both inventories' digests from stored evidence, for the comparison set only.
    ///
    /// Only pairs in `comparison` are eligible, which is what keeps the cache incapable of
    /// inventing an attention state: every other pairing has already been decided by metadata that
    /// was read live this run. The single question a served entry can answer is "these two agree on
    /// size and modification time; do their bytes agree?", and it can only answer it affirmatively.
    pub fn serve(
        &self,
        local: &mut LocalInventory,
        remote: &mut RemoteInventory,
        comparison: &BTreeSet<String>,
    ) -> Served {
        let mut served = Served::default();
        if self.read_into(&mut served, local, remote, comparison) {
            return served;
        }
        // A rejected file must leave both inventories exactly as a missing one would. Rolling back
        // here rather than at each rejection is what makes that hold for every rejection there is,
        // including ones added later: the read path only has to stop, never to tidy up.
        //
        // Clearing every digest rather than only the ones served is correct because this runs
        // before any other populator, so a digest present here can only have come from this file.
        // It also needs no record of which paths were touched, which is the point -- see [`Served`].
        for entry in local.entries.values_mut() {
            entry.content_md5 = None;
        }
        for entry in remote.entries.values_mut() {
            entry.content_md5 = None;
        }
        Served::default()
    }

    /// Read stored evidence into `served`, reporting whether the whole file was usable.
    ///
    /// Returning `false` means the file is not trusted at all. Anything already applied to the
    /// inventories is undone by [`Self::serve`], so this may stop at any point without unwinding.
    fn read_into(
        &self,
        served: &mut Served,
        local: &mut LocalInventory,
        remote: &mut RemoteInventory,
        comparison: &BTreeSet<String>,
    ) -> bool {
        if self.options.refresh || comparison.is_empty() {
            return true;
        }
        let Some(reader) = self.open_validated() else {
            return true;
        };
        let mut reader = reader;
        let mut line = String::new();

        // The header decides whether any of the rest describes this query at all.
        let Some(header) = read_header(&mut reader, &mut line) else {
            return false;
        };
        if !self.header_matches(&header) {
            return true;
        }

        let stride = canary_stride(header.entries, self.options.canary);
        let offset = if stride == 0 {
            0
        } else {
            (header.generated_at.rem_euclid(stride as i64)) as usize
        };
        let expiry = self.now.saturating_sub(self.options.max_age_seconds);
        let racy_floor = header
            .generated_at
            .saturating_sub(RACY_WINDOW_SECONDS)
            .saturating_mul(1000);

        let mut previous: Option<String> = None;
        let mut eligible = 0_usize;
        let mut ended = false;

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => return false,
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed == END_SENTINEL {
                ended = true;
                break;
            }
            let Ok(record) = serde_json::from_str::<RecordRef<'_>>(trimmed) else {
                // One unparseable line means the file is not what this module wrote. Abandon all of
                // it rather than reason about which half is trustworthy.
                return false;
            };
            // Ordering is part of the format: it is what lets the writer merge without holding the
            // whole file, and a violation is the cheapest available proof of a mangled file.
            if previous
                .as_deref()
                .is_some_and(|last| record.p.as_ref() <= last)
            {
                return false;
            }
            previous = Some(record.p.clone().into_owned());

            if record.t > self.now || record.t < expiry {
                continue;
            }
            if record.m >= racy_floor {
                continue;
            }
            let (Some(local_md5), Some(remote_md5)) =
                (parse_md5(&record.lmd5), parse_md5(&record.rmd5))
            else {
                continue;
            };
            if !comparison.contains(record.p.as_ref()) {
                continue;
            }
            let Some(local_entry) = local.entries.get(record.p.as_ref()) else {
                continue;
            };
            if local_entry.kind != EntryKind::File
                || local_entry.size != record.s
                || local_entry.mtime_ms != record.m
                || encode_identity(local_entry.identity) != record.n
            {
                continue;
            }
            let Some(remote_entry) = remote.entries.get(record.p.as_ref()) else {
                continue;
            };
            if remote_entry.kind != EntryKind::File
                || remote_entry.size != record.s
                || remote_entry.mtime_seconds != record.rm
            {
                continue;
            }

            let relative = record.p.into_owned();
            if stride > 0
                && eligible % stride == offset
                && served.probes.len() < self.options.canary
            {
                // Withheld: the live pass will re-derive both digests and `verify_canary` will
                // compare them against what was stored here.
                served.probes.push(CanaryProbe {
                    relative,
                    cached_local: local_md5,
                    cached_remote: remote_md5,
                });
            } else {
                if let Some(entry) = local.entries.get_mut(&relative) {
                    entry.content_md5 = Some(ContentMd5::from_bytes(local_md5));
                }
                if let Some(entry) = remote.entries.get_mut(&relative) {
                    entry.content_md5 = Some(ContentMd5::from_bytes(remote_md5));
                }
                served.oldest_evidence_epoch = Some(
                    served
                        .oldest_evidence_epoch
                        .map_or(record.t, |oldest: i64| oldest.min(record.t)),
                );
                served.supplied += 1;
            }
            eligible += 1;
        }

        // A file without its sentinel was truncated, so nothing read from it is trusted.
        ended
    }

    /// Compare the withheld probes against the digests the live pass just computed.
    ///
    /// Returns the first disagreement. A caller seeing `Some` must discard the file and recompute
    /// the whole answer cold: the sample is a detector for systematic error, so one wrong entry is
    /// evidence about the file, not about that entry.
    pub fn verify_canary(
        probes: &[CanaryProbe],
        local: &LocalInventory,
        remote: &RemoteInventory,
    ) -> Option<String> {
        for probe in probes {
            let live_local = local
                .entries
                .get(&probe.relative)
                .and_then(|entry| entry.content_md5);
            let live_remote = remote
                .entries
                .get(&probe.relative)
                .and_then(|entry| entry.content_md5);
            // A probe whose live digest is missing proves nothing either way and is not a failure:
            // the file may have been deleted between the walk and the hash.
            if let Some(live) = live_local
                && live.as_bytes() != &probe.cached_local
            {
                return Some(probe.relative.clone());
            }
            if let Some(live) = live_remote
                && live.as_bytes() != &probe.cached_remote
            {
                return Some(probe.relative.clone());
            }
        }
        None
    }

    /// Remove the cache file. Used when the canary fails, and safe to call at any time.
    pub fn discard(&self) {
        let _ = fs::remove_file(&self.path);
    }

    /// Rewrite the cache as the union of this run's observations and the still-usable stored ones.
    ///
    /// The union is required for correctness under a scoped query: rewriting only what this run saw
    /// would silently discard evidence for the rest of the tree. Both sides are in ascending path
    /// order, so this is a two-way merge that holds one record at a time rather than the file.
    ///
    /// Only pairs whose digests **agreed** are recorded. An entry therefore means "these two MD5s
    /// were observed equal under this metadata"; a differing pair is about to be re-read by a sync
    /// anyway, so storing it would buy nothing and widen what an entry claims.
    pub fn store(
        &self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        comparison: &BTreeSet<String>,
        scoped: bool,
    ) -> std::io::Result<()> {
        let fresh = self.observations(local, remote, comparison);
        let carried = self.carried_records(local, scoped, &fresh);

        let temporary = self.options.directory.join(format!(
            ".tmp.{}.{}.ndjson",
            self.options.profile,
            std::process::id()
        ));
        let file = create_private(&temporary)?;
        let mut writer = BufWriter::new(file);

        let total = fresh.len().saturating_add(carried.len()).min(ENTRY_LIMIT);
        let header = Header {
            schema: SCHEMA.to_owned(),
            digest_strength: DIGEST_STRENGTH.to_owned(),
            binary: env!("SDSYNC_VERSION").to_owned(),
            platform: std::env::consts::OS.to_owned(),
            profile: self.options.profile.clone(),
            source: self.options.source.clone(),
            remote: self.options.remote.clone(),
            compare: self.options.compare.to_owned(),
            generated_at: self.now,
            entries: total,
        };
        let write = (|| -> std::io::Result<()> {
            serde_json::to_writer(&mut writer, &header)?;
            writer.write_all(b"\n")?;

            let mut written = 0_usize;
            let mut fresh_iterator = fresh.iter().peekable();
            let mut carried_iterator = carried.iter().peekable();
            loop {
                let take_fresh = match (fresh_iterator.peek(), carried_iterator.peek()) {
                    (None, None) => break,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (Some((left, _)), Some((right, _))) => left <= right,
                };
                let (relative, stored) = if take_fresh {
                    let entry = fresh_iterator.next().expect("peeked");
                    // A path observed this run supersedes the stored one for the same path.
                    if carried_iterator
                        .peek()
                        .is_some_and(|(other, _)| other == &entry.0)
                    {
                        carried_iterator.next();
                    }
                    entry
                } else {
                    // Past the ceiling, this run's observations are still recorded and carried-over
                    // ones stop. Fresh entries are bounded by the scan budget, so they alone can
                    // never breach it, and dropping the oldest evidence first is the right bias.
                    if written >= ENTRY_LIMIT {
                        carried_iterator.next();
                        continue;
                    }
                    carried_iterator.next().expect("peeked")
                };
                if written >= ENTRY_LIMIT {
                    break;
                }
                serde_json::to_writer(
                    &mut writer,
                    &Record {
                        p: relative,
                        s: stored.size,
                        m: stored.mtime_ms,
                        n: encode_identity(stored.identity),
                        lmd5: hex16(&stored.local_md5),
                        rm: stored.remote_mtime_seconds,
                        rmd5: hex16(&stored.remote_md5),
                        t: stored.observed_at,
                    },
                )?;
                writer.write_all(b"\n")?;
                written += 1;
            }
            writer.write_all(END_SENTINEL.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()
        })();

        if let Err(error) = write {
            drop(writer);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(writer);
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        self.enforce_directory_limit();
        Ok(())
    }

    /// This run's agreeing pairs, in ascending path order.
    fn observations(
        &self,
        local: &LocalInventory,
        remote: &RemoteInventory,
        comparison: &BTreeSet<String>,
    ) -> BTreeMap<String, StoredEntry> {
        let mut observations = BTreeMap::new();
        for relative in comparison {
            let Some(local_entry) = local.entries.get(relative) else {
                continue;
            };
            let Some(remote_entry) = remote.entries.get(relative) else {
                continue;
            };
            if local_entry.kind != EntryKind::File || remote_entry.kind != EntryKind::File {
                continue;
            }
            let (Some(local_md5), Some(remote_md5)) =
                (local_entry.content_md5, remote_entry.content_md5)
            else {
                continue;
            };
            if local_md5.as_bytes() != remote_md5.as_bytes() {
                continue;
            }
            if local_entry.size != remote_entry.size {
                continue;
            }
            observations.insert(
                relative.clone(),
                StoredEntry {
                    size: local_entry.size,
                    mtime_ms: local_entry.mtime_ms,
                    identity: local_entry.identity,
                    local_md5: *local_md5.as_bytes(),
                    remote_mtime_seconds: remote_entry.mtime_seconds,
                    remote_md5: *remote_md5.as_bytes(),
                    observed_at: self.now,
                },
            );
        }
        observations
    }

    /// Stored entries worth keeping: still inside the age ceiling, and not superseded.
    ///
    /// An unscoped query saw the whole tree, so a stored path absent from the local inventory has
    /// been deleted and is dropped. A scoped query saw only part of it, where absence means "out of
    /// scope" and dropping would throw away good evidence.
    fn carried_records(
        &self,
        local: &LocalInventory,
        scoped: bool,
        fresh: &BTreeMap<String, StoredEntry>,
    ) -> BTreeMap<String, StoredEntry> {
        let mut carried = BTreeMap::new();
        let Some(mut reader) = self.open_validated() else {
            return carried;
        };
        let mut line = String::new();
        let Some(header) = read_header(&mut reader, &mut line) else {
            return carried;
        };
        if !self.header_matches(&header) {
            return carried;
        }
        let expiry = self.now.saturating_sub(self.options.max_age_seconds);
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => return BTreeMap::new(),
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed == END_SENTINEL {
                break;
            }
            let Ok(record) = serde_json::from_str::<RecordRef<'_>>(trimmed) else {
                return BTreeMap::new();
            };
            if record.t > self.now || record.t < expiry {
                continue;
            }
            if fresh.contains_key(record.p.as_ref()) {
                continue;
            }
            if !scoped && !local.entries.contains_key(record.p.as_ref()) {
                continue;
            }
            let (Some(local_md5), Some(remote_md5)) =
                (parse_md5(&record.lmd5), parse_md5(&record.rmd5))
            else {
                continue;
            };
            carried.insert(
                record.p.into_owned(),
                StoredEntry {
                    size: record.s,
                    mtime_ms: record.m,
                    identity: decode_identity(&record.n),
                    local_md5,
                    remote_mtime_seconds: record.rm,
                    remote_md5,
                    observed_at: record.t,
                },
            );
        }
        carried
    }

    /// Write the aggregate rollup for this profile.
    ///
    /// A separate document with a separate lifetime, so either file can be deleted alone. It holds
    /// no evidence and nothing ever reads it back as an input: it is the *last answer*, rendered,
    /// and its only consumer is a display that stamps it with its own age.
    ///
    /// Refused for a scoped query. [`StatusStats`] describes the scope it was computed for, and
    /// writing a subtree's totals as a profile total would be a confidently wrong number.
    pub fn store_rollup(
        &self,
        stats: &StatusStats,
        report: &CacheReport,
        scoped: bool,
    ) -> std::io::Result<()> {
        if scoped {
            return Ok(());
        }
        let document = RollupDocument {
            schema: ROLLUP_SCHEMA.to_owned(),
            binary: env!("SDSYNC_VERSION").to_owned(),
            profile: self.options.profile.clone(),
            source: self.options.source.clone(),
            remote: self.options.remote.clone(),
            compare: self.options.compare.to_owned(),
            observed_at_epoch: self.now,
            observation: RollupObservation {
                // False when a scan budget stopped either walk, which makes every count below a
                // floor. A reader must render these as "4,000+", never "4,000": understating what
                // is still pending is the direction that tells someone they are caught up when they
                // are not.
                complete: stats.complete,
                budget: crate::local::SCAN_BUDGET_DEFAULT,
                digests_reused: report.digests_reused,
                digests_computed: report.digests_computed,
                oldest_evidence_epoch: report.oldest_evidence_epoch,
            },
            state: RollupState::from_stats(stats),
        };

        let target = self.rollup_path();
        let temporary = self.options.directory.join(format!(
            ".tmp.{}.{}.rollup.json",
            self.options.profile,
            std::process::id()
        ));
        let file = create_private(&temporary)?;
        let mut writer = BufWriter::new(file);
        let write = (|| -> std::io::Result<()> {
            serde_json::to_writer(&mut writer, &document)?;
            writer.write_all(b"\n")?;
            writer.flush()
        })();
        if let Err(error) = write {
            drop(writer);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(writer);
        if let Err(error) = fs::rename(&temporary, &target) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(())
    }

    fn header_matches(&self, header: &Header) -> bool {
        header.schema == SCHEMA
            && header.digest_strength == DIGEST_STRENGTH
            && header.binary == env!("SDSYNC_VERSION")
            && header.platform == std::env::consts::OS
            && header.profile == self.options.profile
            && header.source == self.options.source
            && header.remote == self.options.remote
            && header.compare == self.options.compare
            && header.generated_at <= self.now
    }

    /// Open the cache file only if it still looks like a private package file.
    ///
    /// The cache is *believed*, which makes it an input, and inputs get validated. These are the
    /// checks `private_secret_file` applies in the package's own shell: a regular file, not a
    /// symbolic link, a single link, and no access for group or other.
    ///
    /// There is deliberately no owner comparison, because obtaining the process UID would require
    /// `unsafe`, which this crate forbids. It is not needed: a directory this process can open that
    /// is not group- or other-writable can only have been written by this user or by root, and root
    /// is outside any threat model this check could serve.
    fn open_validated(&self) -> Option<BufReader<fs::File>> {
        let metadata = fs::symlink_metadata(&self.path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        if metadata.len() > FILE_LIMIT_BYTES {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
                return None;
            }
        }
        fs::File::open(&self.path).ok().map(BufReader::new)
    }

    /// Keep the whole directory inside its byte ceiling by removing whole profile files, oldest
    /// first. Crude on purpose: every file here is disposable, and a partial eviction would need
    /// bookkeeping that could itself be wrong.
    ///
    /// Necessary because the package's own `cache_prune` filters on plain files at the top of
    /// `$cache_root` and cannot see into this subdirectory, so nothing else bounds these bytes.
    fn enforce_directory_limit(&self) {
        let Ok(entries) = fs::read_dir(&self.options.directory) else {
            return;
        };
        let mut files: Vec<(i64, u64, PathBuf)> = Vec::new();
        let mut total = 0_u64;
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|duration| i64::try_from(duration.as_secs()).ok())
                .unwrap_or(0);
            total = total.saturating_add(metadata.len());
            files.push((modified, metadata.len(), entry.path()));
        }
        if total <= DIRECTORY_LIMIT_BYTES {
            return;
        }
        files.sort_by_key(|(modified, _, _)| *modified);
        for (_, length, path) in files {
            if total <= DIRECTORY_LIMIT_BYTES {
                break;
            }
            // Never evict the file this run just wrote; it is the freshest evidence there is.
            if path == self.path {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(length);
            }
        }
    }
}

/// One record's payload, shared by the read and write paths so the two cannot disagree on shape.
#[derive(Clone, Copy, Debug)]
struct StoredEntry {
    size: u64,
    mtime_ms: i64,
    identity: FileIdentity,
    local_md5: [u8; 16],
    remote_mtime_seconds: i64,
    remote_md5: [u8; 16],
    observed_at: i64,
}

fn read_header(reader: &mut BufReader<fs::File>, line: &mut String) -> Option<Header> {
    line.clear();
    if reader.read_line(line).ok()? == 0 {
        return None;
    }
    serde_json::from_str::<Header>(line.trim_end_matches(['\n', '\r'])).ok()
}

/// Spacing between canary picks, sized from the record count the header claims.
fn canary_stride(entries: usize, canary: usize) -> usize {
    if canary == 0 {
        return 0;
    }
    entries.max(1).div_ceil(canary).max(1)
}

fn create_private(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A leftover from a killed run of this same PID. Replacing it is safe: the name is
            // private to this process and nothing else may write it.
            fs::remove_file(path)?;
            options.open(path)
        }
        Err(error) => Err(error),
    }
}

/// A directory is usable only if it is a real directory, not a symbolic link, and grants no write
/// access to group or other. See [`StatusCache::open_validated`] for why ownership is not compared.
fn directory_is_private(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.mode() & 0o022 != 0 {
            return false;
        }
    }
    true
}

/// The profile name becomes a filename, so it is constrained exactly as the package's own
/// `cache_name_is_safe` constrains a cache key: no traversal, no separators, nothing hidden.
///
/// The DSM manager validates profile names before this is ever reached. Re-checking here is what
/// makes the property hold for any other caller too, rather than depending on one of them.
fn name_is_safe(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name.starts_with('.') || name.contains("..") {
        return false;
    }
    name.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn encode_identity(identity: FileIdentity) -> String {
    format!("{}:{}", identity.device, identity.inode)
}

fn decode_identity(value: &str) -> FileIdentity {
    let mut parts = value.splitn(2, ':');
    let device = parts.next().and_then(|part| part.parse().ok());
    let inode = parts.next().and_then(|part| part.parse().ok());
    match (device, inode) {
        (Some(device), Some(inode)) => FileIdentity { device, inode },
        _ => FileIdentity::default(),
    }
}

fn hex16(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble is a hex digit"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("nibble is a hex digit"));
    }
    out
}

fn parse_md5(value: &str) -> Option<[u8; 16]> {
    if value.len() != 32 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    let source = value.as_bytes();
    for (index, output) in bytes.iter_mut().enumerate() {
        let high = (source[index * 2] as char).to_digit(16)?;
        let low = (source[index * 2 + 1] as char).to_digit(16)?;
        *output = ((high << 4) | low) as u8;
    }
    Some(bytes)
}

// ---------------------------------------------------------------------------
// The aggregate rollup
//
// A separate document from the digest cache, with a separate lifetime, holding
// the totals from the last unscoped pass. It exists so a dashboard can render
// the whole picture without walking anything: reading it opens one small file
// and touches nothing else -- no digest cache, no source file, no network.
//
// It is a byproduct, not a summary. `plan::accumulate` already folds every
// entry into `StatusStats` during the walk that produces the answer, so this
// persists a value the engine computed rather than adding a pass. Nothing ever
// sums the digest cache to produce it, and nothing ever reads it back as an
// input to a decision.
//
// The types below are used for both writing and reading, so the two cannot
// drift into disagreeing about the shape.
// ---------------------------------------------------------------------------

/// Schema of the composed cross-profile figure, which is derived at read time
/// and never stored: a second stored copy of a total is a second thing that can
/// be wrong about it.
pub const ROLLUP_AGGREGATE_SCHEMA: &str = "sdsync.status-rollup-aggregate.v1";

/// A count of files and the bytes they occupy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilesAndBytes {
    pub files: usize,
    pub bytes: u64,
}

/// A count of files alone, where a byte total would have to be invented.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Files {
    pub files: usize,
}

/// A count of entries, which may be directories as well as files.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Entries {
    pub entries: usize,
}

/// What the last pass observed, in the engine's own quantities.
///
/// Every field maps one-to-one onto a [`StatusStats`] field. The single composite,
/// `would_transfer`, has an inherited definition rather than a parallel one: its bytes are
/// `StatusStats::transfer_bytes`, documented there as accumulated exactly as
/// `SyncPlan::upload_bytes` is, so it answers "how far behind am I" with the arithmetic a real run
/// would do. Its three components sit beside it so the composition stays visible.
///
/// Note what is *not* here: any count of what a run transferred. That is an **event**, it lives in
/// the run report with its own timestamp, and conflating it with a **state** is the confusion this
/// naming exists to prevent. The word "synced" appears nowhere, because it means both.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RollupState {
    pub in_sync: FilesAndBytes,
    pub would_transfer: FilesAndBytes,
    pub differs: Files,
    pub missing_remote: Files,
    pub remote_only: Entries,
    pub type_conflicts: Entries,
    pub excluded: Entries,
    pub directories: Entries,
    pub total_entries: usize,
    pub attention_entries: usize,
}

impl RollupState {
    pub fn from_stats(stats: &StatusStats) -> Self {
        Self {
            in_sync: FilesAndBytes {
                files: stats.in_sync_files,
                bytes: stats.in_sync_bytes,
            },
            would_transfer: FilesAndBytes {
                files: stats.differing_files + stats.missing_remote_files + stats.type_conflicts,
                bytes: stats.transfer_bytes,
            },
            differs: Files {
                files: stats.differing_files,
            },
            missing_remote: Files {
                files: stats.missing_remote_files,
            },
            remote_only: Entries {
                entries: stats.remote_only_entries,
            },
            type_conflicts: Entries {
                entries: stats.type_conflicts,
            },
            excluded: Entries {
                entries: stats.excluded_entries,
            },
            directories: Entries {
                entries: stats.directories,
            },
            total_entries: stats.total_entries,
            attention_entries: stats.attention_entries,
        }
    }

    fn add(&mut self, other: &Self) {
        self.in_sync.files += other.in_sync.files;
        self.in_sync.bytes = self.in_sync.bytes.saturating_add(other.in_sync.bytes);
        self.would_transfer.files += other.would_transfer.files;
        self.would_transfer.bytes = self
            .would_transfer
            .bytes
            .saturating_add(other.would_transfer.bytes);
        self.differs.files += other.differs.files;
        self.missing_remote.files += other.missing_remote.files;
        self.remote_only.entries += other.remote_only.entries;
        self.type_conflicts.entries += other.type_conflicts.entries;
        self.excluded.entries += other.excluded.entries;
        self.directories.entries += other.directories.entries;
        self.total_entries += other.total_entries;
        self.attention_entries += other.attention_entries;
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RollupObservation {
    /// False when a scan budget stopped either walk, which makes every count in
    /// [`RollupState`] a floor. A reader must render those as "4,000+", never "4,000".
    pub complete: bool,
    pub budget: usize,
    pub digests_reused: usize,
    pub digests_computed: usize,
    pub oldest_evidence_epoch: Option<i64>,
}

/// One profile's stored rollup.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RollupDocument {
    pub schema: String,
    pub binary: String,
    pub profile: String,
    pub source: String,
    pub remote: String,
    pub compare: String,
    pub observed_at_epoch: i64,
    pub observation: RollupObservation,
    pub state: RollupState,
}

/// The whole-NAS figure, composed from the per-profile documents at read time.
///
/// O(profiles), not O(tree): a handful of small files, no walk, no network. Never stored, so there
/// is no second source of truth that can drift from the first.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RollupAggregate {
    pub schema: String,
    pub generated_at_epoch: i64,
    pub profiles_total: usize,
    pub profiles_observed: usize,
    /// Named, not merely counted: a reader that cannot say *which* profile is missing cannot tell
    /// the user what to do about it.
    pub profiles_never_observed: Vec<String>,
    /// True only when every expected profile was observed and every one of them finished its walk.
    /// Otherwise every figure in `total` is a floor.
    pub complete: bool,
    /// The **oldest** contributing observation, not the newest. The newest would describe the
    /// freshest part of the answer and imply it of all of it; the oldest is the only claim true of
    /// every profile in the total.
    pub observed_at_epoch: Option<i64>,
    /// Set when two profiles cover overlapping trees, which makes a naive sum double-count.
    pub overlapping_profiles: bool,
    /// The summed figures, or `None` when they cannot be stated honestly.
    ///
    /// A `None` here must be *rendered*, not omitted: "unavailable because profiles overlap" tells
    /// the user why, while a missing row invites them to add the per-profile figures by hand and
    /// reach the same wrong answer themselves.
    pub total: Option<RollupState>,
    /// Why `total` is absent, in words a reader can show.
    pub total_unavailable_reason: Option<String>,
    /// Per-profile rows, in profile order, for a reader that renders them individually.
    pub profiles: Vec<RollupDocument>,
}

/// Compose the whole-NAS figure from every rollup in `directory`.
///
/// `expected` names the profiles that ought to be present. Supplying it is what lets a
/// never-observed profile force the total incomplete rather than silently contributing nothing —
/// contributing zero would understate what is pending, which is the direction that tells someone
/// they are caught up when they are not. When it is `None`, `complete` is false and says so,
/// because a total cannot be complete over a set nobody has stated.
///
/// Reads nothing but the rollup files themselves. No digest cache, no source file, no network.
pub fn compose_rollups(directory: &Path, expected: Option<&[String]>, now: i64) -> RollupAggregate {
    let mut documents: BTreeMap<String, RollupDocument> = BTreeMap::new();
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.to_string_lossy().ends_with(".rollup.json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(document) = serde_json::from_str::<RollupDocument>(&text) else {
                continue;
            };
            // A document from another format, or another build whose quantities may have changed
            // meaning, is not evidence about this one. Skipping it leaves the profile unobserved,
            // which forces the total incomplete rather than quietly dropping it from the sum.
            if document.schema != ROLLUP_SCHEMA || document.binary != env!("SDSYNC_VERSION") {
                continue;
            }
            documents.insert(document.profile.clone(), document);
        }
    }

    let never_observed: Vec<String> = expected
        .map(|names| {
            names
                .iter()
                .filter(|name| !documents.contains_key(*name))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    let overlapping = documents.values().enumerate().any(|(index, left)| {
        documents.values().skip(index + 1).any(|right| {
            trees_overlap(&left.source, &right.source) || trees_overlap(&left.remote, &right.remote)
        })
    });

    let every_walk_finished = documents
        .values()
        .all(|document| document.observation.complete);
    let complete = expected.is_some() && never_observed.is_empty() && every_walk_finished;

    let observed_at_epoch = documents
        .values()
        .map(|document| document.observed_at_epoch)
        .min();

    let (total, total_unavailable_reason) = if overlapping {
        (
            None,
            Some(
                "two or more profiles cover overlapping trees, so a combined total would count \
                 the same files more than once"
                    .to_owned(),
            ),
        )
    } else if documents.is_empty() {
        (None, Some("no profile has been observed yet".to_owned()))
    } else {
        let mut summed = RollupState::default();
        for document in documents.values() {
            summed.add(&document.state);
        }
        (Some(summed), None)
    };

    RollupAggregate {
        schema: ROLLUP_AGGREGATE_SCHEMA.to_owned(),
        generated_at_epoch: now,
        profiles_total: expected.map_or(documents.len(), <[String]>::len),
        profiles_observed: documents.len(),
        profiles_never_observed: never_observed,
        complete,
        observed_at_epoch,
        overlapping_profiles: overlapping,
        total,
        total_unavailable_reason,
        profiles: documents.into_values().collect(),
    }
}

/// Whether two roots cover any of the same tree.
///
/// Equal, or one a path-boundary prefix of the other. Deliberately textual: these are the paths the
/// profiles were configured with, and resolving them would mean touching the filesystem, which is
/// the one thing the aggregate read path must not do.
fn trees_overlap(left: &str, right: &str) -> bool {
    let left = left.trim_end_matches(['/', '\\']);
    let right = right.trim_end_matches(['/', '\\']);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }
    let (shorter, longer) = if left.len() < right.len() {
        (left, right)
    } else {
        (right, left)
    };
    longer
        .strip_prefix(shorter)
        .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RemoteEntry;
    use crate::integrity::ContentMatch;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const NOW: i64 = 1_757_000_000;

    /// A way of corrupting a stored file, for the table in `damaged_files_degrade_to_a_cold_pass`.
    type Damage = fn(&str) -> String;

    /// A change to one side's metadata, for the table in `any_metadata_movement_withholds_the_entry`.
    type Movement = fn(&mut LocalInventory, &mut RemoteInventory);

    static NONCE: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sdsync-status-cache-{}-{nonce}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options(directory: &Path) -> CacheOptions {
        CacheOptions {
            directory: directory.to_owned(),
            profile: "photos".to_owned(),
            source: "/volume1/photos".to_owned(),
            remote: "/team/photos".to_owned(),
            compare: "content",
            refresh: false,
            max_age_seconds: DEFAULT_MAX_AGE_SECONDS,
            canary: 0,
        }
    }

    fn digest(seed: u8) -> ContentMd5 {
        ContentMd5::from_bytes([seed; 16])
    }

    /// A local and remote pair agreeing on size and modification time, which is the only shape the
    /// comparison set — and therefore the cache — ever contains.
    fn pair(
        local: &mut LocalInventory,
        remote: &mut RemoteInventory,
        relative: &str,
        size: u64,
        mtime_ms: i64,
        inode: u64,
    ) {
        local.entries.insert(
            relative.to_owned(),
            crate::local::LocalEntry {
                relative: relative.to_owned(),
                full_path: PathBuf::from("/volume1/photos").join(relative),
                kind: EntryKind::File,
                size,
                mtime_ms,
                identity: FileIdentity {
                    device: 2049,
                    inode,
                },
                content_md5: None,
            },
        );
        remote.entries.insert(
            relative.to_owned(),
            RemoteEntry {
                relative: relative.to_owned(),
                remote_path: format!("/team/photos/{relative}"),
                kind: EntryKind::File,
                size,
                mtime_seconds: mtime_ms.div_euclid(1000),
                mount_point_type: None,
                content_md5: None,
            },
        );
    }

    fn inventories() -> (LocalInventory, RemoteInventory) {
        (
            LocalInventory {
                root: PathBuf::from("/volume1/photos"),
                entries: BTreeMap::new(),
            },
            RemoteInventory {
                root_exists: true,
                entries: BTreeMap::new(),
            },
        )
    }

    fn comparison_of(local: &LocalInventory) -> BTreeSet<String> {
        local.entries.keys().cloned().collect()
    }

    /// Populate digests as a live pass would, then store, producing a warm cache.
    fn warm(
        cache: &StatusCache,
        local: &mut LocalInventory,
        remote: &mut RemoteInventory,
        seed: u8,
    ) {
        for entry in local.entries.values_mut() {
            entry.content_md5 = Some(digest(seed));
        }
        for entry in remote.entries.values_mut() {
            entry.content_md5 = Some(digest(seed));
        }
        let comparison = comparison_of(local);
        cache
            .store(local, remote, &comparison, false)
            .expect("store");
    }

    #[test]
    fn round_trip_serves_every_unchanged_pair() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        pair(&mut local, &mut remote, "a.bin", 10, 1_000_000, 11);
        pair(&mut local, &mut remote, "b/c.bin", 20, 2_000_000, 12);
        warm(&cache, &mut local, &mut remote, 0xAB);

        let (mut fresh_local, mut fresh_remote) = inventories();
        pair(
            &mut fresh_local,
            &mut fresh_remote,
            "a.bin",
            10,
            1_000_000,
            11,
        );
        pair(
            &mut fresh_local,
            &mut fresh_remote,
            "b/c.bin",
            20,
            2_000_000,
            12,
        );
        let comparison = comparison_of(&fresh_local);
        let later = StatusCache::open(options(&scratch.0), NOW + 60).expect("cache opens");
        let served = later.serve(&mut fresh_local, &mut fresh_remote, &comparison);

        assert_eq!(served.supplied, 2);
        assert_eq!(served.oldest_evidence_epoch, Some(NOW));
        for entry in fresh_local.entries.values() {
            assert_eq!(entry.content_md5, Some(digest(0xAB)));
        }
        for entry in fresh_remote.entries.values() {
            assert_eq!(entry.content_md5, Some(digest(0xAB)));
        }
    }

    /// The evidence a cache can supply is MD5 and nothing else, so a fingerprint it produced cannot
    /// satisfy a guard that requires strong proof. This is what keeps deletion and server-copy safe
    /// by construction rather than by anyone remembering a rule.
    #[test]
    fn served_digests_can_never_prove_content_equality_strongly() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        pair(&mut local, &mut remote, "a.bin", 10, 1_000_000, 11);
        warm(&cache, &mut local, &mut remote, 0x41);

        let (mut fresh_local, mut fresh_remote) = inventories();
        pair(
            &mut fresh_local,
            &mut fresh_remote,
            "a.bin",
            10,
            1_000_000,
            11,
        );
        let comparison = comparison_of(&fresh_local);
        let later = StatusCache::open(options(&scratch.0), NOW + 1).expect("cache opens");
        assert_eq!(
            later
                .serve(&mut fresh_local, &mut fresh_remote, &comparison)
                .supplied,
            1
        );

        let local_digest = fresh_local.entries["a.bin"].content_md5.expect("served");
        let remote_digest = fresh_remote.entries["a.bin"].content_md5.expect("served");
        assert!(!local_digest.has_full_proof());
        assert!(!remote_digest.has_full_proof());
        assert_eq!(local_digest.full_match(&remote_digest), None);
        assert_eq!(
            local_digest.compare_content(&remote_digest),
            ContentMatch::Md5Only
        );
    }

    /// Every way a stored file can be wrong degrades to the same behaviour as no file at all.
    #[test]
    fn damaged_files_degrade_to_a_cold_pass() {
        let cases: [(&str, Damage); 5] = [
            ("truncated before the sentinel", |text| {
                text.lines()
                    .filter(|line| *line != END_SENTINEL)
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n"
            }),
            ("an unparseable record", |text| {
                text.replace("\"lmd5\"", "\"lmd5\" \"broken\"")
            }),
            ("a header from another package version", |text| {
                text.replace(env!("SDSYNC_VERSION"), "0.0.0-other")
            }),
            ("a header naming another source", |text| {
                text.replace("/volume1/photos", "/volume1/elsewhere")
            }),
            ("records out of order", |text| {
                let mut lines: Vec<&str> = text.lines().collect();
                lines.swap(1, 2);
                lines.join("\n") + "\n"
            }),
        ];

        for (label, damage) in cases {
            let scratch = Scratch::new();
            let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
            let (mut local, mut remote) = inventories();
            pair(&mut local, &mut remote, "a.bin", 10, 1_000_000, 11);
            pair(&mut local, &mut remote, "b.bin", 20, 2_000_000, 12);
            warm(&cache, &mut local, &mut remote, 0x11);

            let text = fs::read_to_string(cache.path()).expect("cache is readable");
            fs::write(cache.path(), damage(&text)).expect("damage is written");

            let (mut fresh_local, mut fresh_remote) = inventories();
            pair(
                &mut fresh_local,
                &mut fresh_remote,
                "a.bin",
                10,
                1_000_000,
                11,
            );
            pair(
                &mut fresh_local,
                &mut fresh_remote,
                "b.bin",
                20,
                2_000_000,
                12,
            );
            let comparison = comparison_of(&fresh_local);
            let later = StatusCache::open(options(&scratch.0), NOW + 1).expect("cache opens");
            let served = later.serve(&mut fresh_local, &mut fresh_remote, &comparison);

            assert!(
                served.supplied == 0,
                "{label} should have supplied nothing, supplied {:?}",
                served.supplied
            );
            // Nothing may be left half-applied either: a rejected file leaves both inventories
            // exactly as a missing file would.
            for entry in fresh_local.entries.values() {
                assert_eq!(
                    entry.content_md5, None,
                    "{label} left a local digest behind"
                );
            }
            for entry in fresh_remote.entries.values() {
                assert_eq!(
                    entry.content_md5, None,
                    "{label} left a remote digest behind"
                );
            }
        }
    }

    /// Each of these signals that the pair may no longer be what was observed, so none of them may
    /// be answered from stored evidence.
    #[test]
    fn any_metadata_movement_withholds_the_entry() {
        let mutations: [(&str, Movement); 5] = [
            ("local size", |local, _| {
                local.entries.get_mut("a.bin").unwrap().size = 11;
            }),
            ("local modification time", |local, _| {
                local.entries.get_mut("a.bin").unwrap().mtime_ms = 1_000_001;
            }),
            ("local filesystem identity", |local, _| {
                local.entries.get_mut("a.bin").unwrap().identity.inode = 99;
            }),
            ("remote modification time", |_, remote| {
                remote.entries.get_mut("a.bin").unwrap().mtime_seconds += 1;
            }),
            ("remote size", |_, remote| {
                remote.entries.get_mut("a.bin").unwrap().size = 11;
            }),
        ];

        for (label, mutate) in mutations {
            let scratch = Scratch::new();
            let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
            let (mut local, mut remote) = inventories();
            pair(&mut local, &mut remote, "a.bin", 10, 1_000_000, 11);
            warm(&cache, &mut local, &mut remote, 0x22);

            let (mut fresh_local, mut fresh_remote) = inventories();
            pair(
                &mut fresh_local,
                &mut fresh_remote,
                "a.bin",
                10,
                1_000_000,
                11,
            );
            mutate(&mut fresh_local, &mut fresh_remote);
            let comparison = comparison_of(&fresh_local);
            let later = StatusCache::open(options(&scratch.0), NOW + 1).expect("cache opens");
            let served = later.serve(&mut fresh_local, &mut fresh_remote, &comparison);
            assert!(
                served.supplied == 0,
                "a changed {label} was still answered from the cache"
            );
        }
    }

    #[test]
    fn evidence_past_the_age_ceiling_is_absent() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        pair(&mut local, &mut remote, "a.bin", 10, 1_000_000, 11);
        warm(&cache, &mut local, &mut remote, 0x33);

        let (mut fresh_local, mut fresh_remote) = inventories();
        pair(
            &mut fresh_local,
            &mut fresh_remote,
            "a.bin",
            10,
            1_000_000,
            11,
        );
        let comparison = comparison_of(&fresh_local);
        let expired = StatusCache::open(options(&scratch.0), NOW + DEFAULT_MAX_AGE_SECONDS + 1)
            .expect("opens");
        assert!(
            expired
                .serve(&mut fresh_local, &mut fresh_remote, &comparison)
                .supplied
                == 0
        );
    }

    /// Git's racy rule. A file whose recorded modification time sits inside the window the cache was
    /// written in cannot be distinguished from one rewritten during that same tick.
    #[test]
    fn evidence_inside_the_racy_window_is_withheld() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        // Modified in the same second the cache is written.
        pair(&mut local, &mut remote, "a.bin", 10, NOW * 1000, 11);
        warm(&cache, &mut local, &mut remote, 0x44);

        let (mut fresh_local, mut fresh_remote) = inventories();
        pair(
            &mut fresh_local,
            &mut fresh_remote,
            "a.bin",
            10,
            NOW * 1000,
            11,
        );
        let comparison = comparison_of(&fresh_local);
        let later = StatusCache::open(options(&scratch.0), NOW + 600).expect("cache opens");
        assert!(
            later
                .serve(&mut fresh_local, &mut fresh_remote, &comparison)
                .supplied
                == 0
        );
    }

    /// A pair outside the comparison set has already been decided by metadata read live this run.
    /// The cache must not touch it, which is what stops it suppressing a state it was never asked
    /// about.
    #[test]
    fn entries_outside_the_comparison_set_are_never_served() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        pair(&mut local, &mut remote, "a.bin", 10, 1_000_000, 11);
        pair(&mut local, &mut remote, "b.bin", 20, 2_000_000, 12);
        warm(&cache, &mut local, &mut remote, 0x55);

        let (mut fresh_local, mut fresh_remote) = inventories();
        pair(
            &mut fresh_local,
            &mut fresh_remote,
            "a.bin",
            10,
            1_000_000,
            11,
        );
        pair(
            &mut fresh_local,
            &mut fresh_remote,
            "b.bin",
            20,
            2_000_000,
            12,
        );
        let comparison: BTreeSet<String> = ["a.bin".to_owned()].into_iter().collect();
        let later = StatusCache::open(options(&scratch.0), NOW + 1).expect("cache opens");
        let served = later.serve(&mut fresh_local, &mut fresh_remote, &comparison);

        assert_eq!(served.supplied, comparison.len());
        assert!(fresh_local.entries["a.bin"].content_md5.is_some());
        assert!(fresh_local.entries["b.bin"].content_md5.is_none());
        assert!(fresh_remote.entries["b.bin"].content_md5.is_none());
    }

    /// A pair whose digests disagreed is not evidence that anything matched, so it is not recorded.
    #[test]
    fn only_agreeing_pairs_are_recorded() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        pair(&mut local, &mut remote, "agrees.bin", 10, 1_000_000, 11);
        pair(&mut local, &mut remote, "differs.bin", 20, 2_000_000, 12);
        local.entries.get_mut("agrees.bin").unwrap().content_md5 = Some(digest(1));
        remote.entries.get_mut("agrees.bin").unwrap().content_md5 = Some(digest(1));
        local.entries.get_mut("differs.bin").unwrap().content_md5 = Some(digest(2));
        remote.entries.get_mut("differs.bin").unwrap().content_md5 = Some(digest(3));
        let comparison = comparison_of(&local);
        cache
            .store(&local, &remote, &comparison, false)
            .expect("store");

        let text = fs::read_to_string(cache.path()).expect("readable");
        assert!(text.contains("agrees.bin"));
        assert!(!text.contains("differs.bin"));
    }

    #[test]
    fn refresh_ignores_stored_evidence_but_still_rewrites() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        pair(&mut local, &mut remote, "a.bin", 10, 1_000_000, 11);
        warm(&cache, &mut local, &mut remote, 0x66);

        let mut refreshing = options(&scratch.0);
        refreshing.refresh = true;
        let refresher = StatusCache::open(refreshing, NOW + 1).expect("cache opens");
        let (mut fresh_local, mut fresh_remote) = inventories();
        pair(
            &mut fresh_local,
            &mut fresh_remote,
            "a.bin",
            10,
            1_000_000,
            11,
        );
        let comparison = comparison_of(&fresh_local);
        assert!(
            refresher
                .serve(&mut fresh_local, &mut fresh_remote, &comparison)
                .supplied
                == 0
        );
        assert!(refresher.is_refresh());

        warm(&refresher, &mut fresh_local, &mut fresh_remote, 0x77);
        let text = fs::read_to_string(refresher.path()).expect("readable");
        assert!(text.contains(&"77".repeat(16)));
    }

    /// A scoped pass sees only part of the tree, so evidence for the rest must survive its rewrite.
    /// An unscoped pass saw everything, so a stored path with no local file has been deleted.
    #[test]
    fn a_rewrite_carries_evidence_a_scoped_pass_could_not_see() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        pair(&mut local, &mut remote, "kept/a.bin", 10, 1_000_000, 11);
        pair(&mut local, &mut remote, "scoped/b.bin", 20, 2_000_000, 12);
        warm(&cache, &mut local, &mut remote, 0x88);

        // A scoped pass over "scoped/" only.
        let (mut scoped_local, mut scoped_remote) = inventories();
        pair(
            &mut scoped_local,
            &mut scoped_remote,
            "scoped/b.bin",
            20,
            2_000_000,
            12,
        );
        let scoped_cache = StatusCache::open(options(&scratch.0), NOW + 5).expect("cache opens");
        for entry in scoped_local.entries.values_mut() {
            entry.content_md5 = Some(digest(0x99));
        }
        for entry in scoped_remote.entries.values_mut() {
            entry.content_md5 = Some(digest(0x99));
        }
        let comparison = comparison_of(&scoped_local);
        scoped_cache
            .store(&scoped_local, &scoped_remote, &comparison, true)
            .expect("store");

        let text = fs::read_to_string(cache.path()).expect("readable");
        assert!(
            text.contains("kept/a.bin"),
            "a scoped rewrite discarded out-of-scope evidence"
        );
        assert!(text.contains("scoped/b.bin"));

        // An unscoped pass that no longer sees "kept/a.bin" means the file is gone.
        let unscoped = StatusCache::open(options(&scratch.0), NOW + 6).expect("cache opens");
        unscoped
            .store(&scoped_local, &scoped_remote, &comparison, false)
            .expect("store");
        let text = fs::read_to_string(cache.path()).expect("readable");
        assert!(
            !text.contains("kept/a.bin"),
            "a deleted file kept its stored evidence"
        );
    }

    /// The canary compares withheld entries against live evidence. A disagreement is reported so the
    /// caller can discard everything, which is the only response that catches systematic error.
    #[test]
    fn the_canary_reports_stored_evidence_that_disagrees() {
        let scratch = Scratch::new();
        let mut sampling = options(&scratch.0);
        sampling.canary = 4;
        let cache = StatusCache::open(sampling.clone(), NOW).expect("cache opens");
        let (mut local, mut remote) = inventories();
        for index in 0..8_u64 {
            pair(
                &mut local,
                &mut remote,
                &format!("file-{index}.bin"),
                10 + index,
                1_000_000 + index as i64,
                11 + index,
            );
        }
        warm(&cache, &mut local, &mut remote, 0xAA);

        let (mut fresh_local, mut fresh_remote) = inventories();
        for index in 0..8_u64 {
            pair(
                &mut fresh_local,
                &mut fresh_remote,
                &format!("file-{index}.bin"),
                10 + index,
                1_000_000 + index as i64,
                11 + index,
            );
        }
        let comparison = comparison_of(&fresh_local);
        let later = StatusCache::open(sampling, NOW + 1).expect("cache opens");
        let served = later.serve(&mut fresh_local, &mut fresh_remote, &comparison);
        assert!(
            !served.probes.is_empty(),
            "no entry was withheld for verification"
        );
        assert!(
            served.supplied < 8,
            "every entry was served, so nothing was checked"
        );

        // A live pass that agrees leaves the cache trusted.
        for probe in &served.probes {
            fresh_local
                .entries
                .get_mut(&probe.relative)
                .unwrap()
                .content_md5 = Some(digest(0xAA));
            fresh_remote
                .entries
                .get_mut(&probe.relative)
                .unwrap()
                .content_md5 = Some(digest(0xAA));
        }
        assert_eq!(
            StatusCache::verify_canary(&served.probes, &fresh_local, &fresh_remote),
            None
        );

        // One that disagrees names the entry, which is the signal to discard the whole file.
        let victim = served.probes[0].relative.clone();
        fresh_local.entries.get_mut(&victim).unwrap().content_md5 = Some(digest(0xBB));
        assert_eq!(
            StatusCache::verify_canary(&served.probes, &fresh_local, &fresh_remote),
            Some(victim)
        );
    }

    #[test]
    fn a_profile_name_that_is_not_a_safe_file_name_is_refused() {
        let scratch = Scratch::new();
        for name in ["", "..", "../escape", "a/b", ".hidden", "with space"] {
            let mut candidate = options(&scratch.0);
            candidate.profile = name.to_owned();
            assert!(
                StatusCache::open(candidate, NOW).is_none(),
                "profile name {name:?} was accepted as a file name"
            );
        }
    }

    #[test]
    fn a_missing_directory_declines_rather_than_failing() {
        let scratch = Scratch::new();
        let mut absent = options(&scratch.0);
        absent.directory = scratch.0.join("does-not-exist");
        assert!(StatusCache::open(absent, NOW).is_none());
    }

    /// The rollup is a separate document describing the whole profile, so a scoped pass — whose
    /// totals describe only a subtree — must not write one.
    #[test]
    fn the_rollup_is_self_contained_and_refuses_a_scoped_pass() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let stats = rollup_stats();
        let report = CacheReport {
            state: CacheState::Warm,
            digests_reused: 7,
            digests_computed: 3,
            oldest_evidence_epoch: Some(NOW - 900),
            canary_checked: 2,
        };

        cache
            .store_rollup(&stats, &report, true)
            .expect("scoped write");
        let rollup = scratch.0.join("photos.rollup.json");
        assert!(
            !rollup.exists(),
            "a scoped pass wrote a whole-profile rollup"
        );

        cache
            .store_rollup(&stats, &report, false)
            .expect("unscoped write");
        let text = fs::read_to_string(&rollup).expect("readable");
        let document: serde_json::Value = serde_json::from_str(&text).expect("json");

        assert_eq!(document["schema"], ROLLUP_SCHEMA);
        assert_eq!(document["profile"], "photos");
        assert_eq!(document["observed_at_epoch"], NOW);
        assert_eq!(document["observation"]["complete"], true);
        assert_eq!(document["observation"]["digests_reused"], 7);
        assert_eq!(document["observation"]["oldest_evidence_epoch"], NOW - 900);
        // A state, and a different quantity from anything a run transferred.
        assert_eq!(document["state"]["in_sync"]["files"], 4);
        assert_eq!(document["state"]["in_sync"]["bytes"], 400);
        // The engine's own arithmetic, not a parallel definition.
        assert_eq!(document["state"]["would_transfer"]["bytes"], 46);
        assert_eq!(document["state"]["would_transfer"]["files"], 4);
        assert_eq!(document["state"]["differs"]["files"], 2);
        assert_eq!(document["state"]["missing_remote"]["files"], 1);
        // Nothing here may be called "synced": the state above and the count of files a run moved
        // are different quantities, and one field name serving both is how they get confused.
        assert!(!text.contains("synced"));
    }

    /// A truncated walk makes every count a floor, and the rollup has to say so: a reader that
    /// renders these as exact totals would understate what is still pending, which is the direction
    /// that tells someone they are caught up when they are not.
    #[test]
    fn a_truncated_walk_is_recorded_as_incomplete() {
        let scratch = Scratch::new();
        let cache = StatusCache::open(options(&scratch.0), NOW).expect("cache opens");
        let mut stats = rollup_stats();
        stats.complete = false;
        cache
            .store_rollup(&stats, &CacheReport::off(), false)
            .expect("write");
        let document: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(scratch.0.join("photos.rollup.json")).expect("readable"),
        )
        .expect("json");
        assert_eq!(document["observation"]["complete"], false);
    }

    fn rollup_stats() -> StatusStats {
        let mut stats = StatusStats::empty(crate::plan::CompareMode::Content);
        stats.in_sync_files = 4;
        stats.in_sync_bytes = 400;
        stats.differing_files = 2;
        stats.missing_remote_files = 1;
        stats.remote_only_entries = 3;
        stats.type_conflicts = 1;
        stats.excluded_entries = 5;
        stats.directories = 6;
        stats.transfer_bytes = 46;
        stats.total_entries = 11;
        stats.attention_entries = 4;
        stats.complete = true;
        stats
    }

    /// One profile's rollup, as a real pass would have written it.
    struct Rollup<'a> {
        profile: &'a str,
        source: &'a str,
        remote: &'a str,
        in_sync: usize,
        pending: usize,
        complete: bool,
        observed_at: i64,
    }

    impl Default for Rollup<'_> {
        fn default() -> Self {
            Self {
                profile: "photos",
                source: "/volume1/photos",
                remote: "/team/photos",
                in_sync: 100,
                pending: 3,
                complete: true,
                observed_at: NOW,
            }
        }
    }

    fn write_rollup(directory: &Path, spec: Rollup<'_>) {
        let mut candidate = options(directory);
        candidate.profile = spec.profile.to_owned();
        candidate.source = spec.source.to_owned();
        candidate.remote = spec.remote.to_owned();
        let cache = StatusCache::open(candidate, spec.observed_at).expect("cache opens");
        let mut stats = StatusStats::empty(crate::plan::CompareMode::Content);
        stats.in_sync_files = spec.in_sync;
        stats.in_sync_bytes = spec.in_sync as u64 * 100;
        stats.differing_files = spec.pending;
        stats.transfer_bytes = spec.pending as u64 * 10;
        stats.total_entries = spec.in_sync + spec.pending;
        stats.attention_entries = spec.pending;
        stats.complete = spec.complete;
        cache
            .store_rollup(&stats, &CacheReport::off(), false)
            .expect("rollup is written");
    }

    #[test]
    fn the_aggregate_sums_profiles_that_do_not_overlap() {
        let scratch = Scratch::new();
        write_rollup(&scratch.0, Rollup::default());
        write_rollup(
            &scratch.0,
            Rollup {
                profile: "documents",
                source: "/volume1/documents",
                remote: "/team/documents",
                in_sync: 40,
                pending: 1,
                observed_at: NOW - 600,
                ..Rollup::default()
            },
        );

        let expected = ["photos".to_owned(), "documents".to_owned()];
        let aggregate = compose_rollups(&scratch.0, Some(&expected), NOW);

        assert!(aggregate.complete);
        assert!(!aggregate.overlapping_profiles);
        assert_eq!(aggregate.profiles_total, 2);
        assert_eq!(aggregate.profiles_observed, 2);
        assert!(aggregate.profiles_never_observed.is_empty());
        // The oldest contributor, not the newest: it is the only age true of every figure summed.
        assert_eq!(aggregate.observed_at_epoch, Some(NOW - 600));
        let total = aggregate.total.expect("a total over disjoint profiles");
        assert_eq!(total.in_sync.files, 140);
        assert_eq!(total.would_transfer.files, 4);
        assert_eq!(total.attention_entries, 4);
    }

    /// Two profiles covering the same tree would be counted twice. A total that silently overstates
    /// coverage is the failure this whole design exists to prevent, so it is withheld — and the
    /// reason is carried so a reader can say why rather than leaving a gap.
    #[test]
    fn overlapping_profiles_withhold_the_total_and_say_why() {
        let scratch = Scratch::new();
        write_rollup(
            &scratch.0,
            Rollup {
                profile: "whole",
                source: "/volume1/media",
                remote: "/team/media",
                pending: 2,
                ..Rollup::default()
            },
        );
        write_rollup(
            &scratch.0,
            Rollup {
                profile: "subtree",
                source: "/volume1/media/photos",
                remote: "/team/media/photos",
                in_sync: 40,
                pending: 1,
                ..Rollup::default()
            },
        );

        let expected = ["whole".to_owned(), "subtree".to_owned()];
        let aggregate = compose_rollups(&scratch.0, Some(&expected), NOW);

        assert!(aggregate.overlapping_profiles);
        assert!(aggregate.total.is_none());
        let reason = aggregate
            .total_unavailable_reason
            .expect("a withheld total must carry its reason");
        assert!(
            reason.contains("overlapping"),
            "the reason must be renderable to a user: {reason}"
        );
        // The per-profile rows survive: only the combination is unsound, not the parts.
        assert_eq!(aggregate.profiles.len(), 2);
    }

    /// A profile with no stored totals contributes nothing and is named. Contributing zero would
    /// understate what is pending, which is the direction that tells someone they are caught up when
    /// they are not.
    #[test]
    fn a_never_observed_profile_is_named_and_makes_the_total_incomplete() {
        let scratch = Scratch::new();
        write_rollup(&scratch.0, Rollup::default());

        let expected = ["photos".to_owned(), "archive".to_owned()];
        let aggregate = compose_rollups(&scratch.0, Some(&expected), NOW);

        assert!(!aggregate.complete);
        assert_eq!(aggregate.profiles_total, 2);
        assert_eq!(aggregate.profiles_observed, 1);
        assert_eq!(aggregate.profiles_never_observed, ["archive"]);
        // The total is still offered, because a floor is useful; `complete` is what says it is one.
        let total = aggregate.total.expect("observed profiles still sum");
        assert_eq!(total.in_sync.files, 100);
    }

    /// A budget-truncated walk makes every count a floor, and that has to survive the sum.
    #[test]
    fn a_truncated_profile_makes_the_whole_total_incomplete() {
        let scratch = Scratch::new();
        write_rollup(
            &scratch.0,
            Rollup {
                complete: false,
                ..Rollup::default()
            },
        );
        let expected = ["photos".to_owned()];
        let aggregate = compose_rollups(&scratch.0, Some(&expected), NOW);
        assert!(
            !aggregate.complete,
            "a truncated walk must not produce a total presented as exact"
        );
    }

    /// Without a stated expectation there is no way to know a profile is missing, so the result must
    /// not claim completeness it cannot have checked.
    #[test]
    fn an_unstated_profile_set_can_never_be_complete() {
        let scratch = Scratch::new();
        write_rollup(
            &scratch.0,
            Rollup {
                pending: 0,
                ..Rollup::default()
            },
        );
        let aggregate = compose_rollups(&scratch.0, None, NOW);
        assert!(
            !aggregate.complete,
            "completeness over a set nobody stated cannot be asserted"
        );
        assert_eq!(aggregate.profiles_observed, 1);
    }

    /// A document from another build may use the same field names for quantities that have changed
    /// meaning. Skipping it leaves that profile unobserved, which is the safe direction.
    #[test]
    fn a_rollup_from_another_build_is_not_counted() {
        let scratch = Scratch::new();
        write_rollup(&scratch.0, Rollup::default());
        let path = scratch.0.join("photos.rollup.json");
        let text = fs::read_to_string(&path).expect("readable");
        fs::write(&path, text.replace(env!("SDSYNC_VERSION"), "0.0.0-other")).expect("rewritten");

        let expected = ["photos".to_owned()];
        let aggregate = compose_rollups(&scratch.0, Some(&expected), NOW);
        assert_eq!(aggregate.profiles_observed, 0);
        assert_eq!(aggregate.profiles_never_observed, ["photos"]);
        assert!(!aggregate.complete);
    }

    #[test]
    fn an_empty_directory_reports_nothing_observed_rather_than_a_zero_total() {
        let scratch = Scratch::new();
        let aggregate = compose_rollups(&scratch.0, Some(&["photos".to_owned()]), NOW);
        assert_eq!(aggregate.profiles_observed, 0);
        assert!(
            aggregate.total.is_none(),
            "an unobserved NAS must not report a total of zero pending files"
        );
        assert!(aggregate.total_unavailable_reason.is_some());
    }

    #[test]
    fn tree_overlap_is_detected_only_at_a_path_boundary() {
        assert!(trees_overlap("/volume1/media", "/volume1/media"));
        assert!(trees_overlap("/volume1/media", "/volume1/media/photos"));
        assert!(trees_overlap("/volume1/media/photos", "/volume1/media"));
        assert!(trees_overlap("/volume1/media/", "/volume1/media"));
        // A shared textual prefix that is not a path boundary is a different tree.
        assert!(!trees_overlap("/volume1/media", "/volume1/media-archive"));
        assert!(!trees_overlap("/volume1/a", "/volume1/b"));
        assert!(!trees_overlap("", "/volume1/a"));
    }

    /// The stored document and the type that reads it must agree, or a rollup written by one build
    /// becomes unreadable to the next for no reason anyone can see.
    #[test]
    fn a_stored_rollup_round_trips_through_its_own_type() {
        let scratch = Scratch::new();
        write_rollup(
            &scratch.0,
            Rollup {
                in_sync: 7,
                pending: 2,
                ..Rollup::default()
            },
        );
        let text = fs::read_to_string(scratch.0.join("photos.rollup.json")).expect("readable");
        let document: RollupDocument = serde_json::from_str(&text).expect("reads as its own type");
        assert_eq!(document.schema, ROLLUP_SCHEMA);
        assert_eq!(document.state.in_sync.files, 7);
        assert_eq!(document.state.would_transfer.files, 2);
        assert_eq!(
            serde_json::to_value(&document).expect("re-serializes"),
            serde_json::from_str::<serde_json::Value>(&text).expect("original parses"),
            "the writer and the reader disagree about the document shape"
        );
    }

    #[test]
    fn hex_round_trips_every_byte() {
        let bytes: [u8; 16] = std::array::from_fn(|index| (index as u8).wrapping_mul(17));
        assert_eq!(parse_md5(&hex16(&bytes)), Some(bytes));
        assert_eq!(parse_md5("nonsense"), None);
        assert_eq!(parse_md5(&"g".repeat(32)), None);
    }

    #[test]
    fn identity_round_trips_and_rejects_nonsense() {
        let identity = FileIdentity {
            device: 2049,
            inode: 123_456,
        };
        assert_eq!(decode_identity(&encode_identity(identity)), identity);
        assert_eq!(decode_identity("nonsense"), FileIdentity::default());
        assert!(!FileIdentity::default().is_known());
    }
}
